//! The `Repository` CRD — a namespaced kopia repository. ADR-0003 §3.1.

use crate::backend::Backend;
use crate::common::{
    CatalogBounds, CreateBehavior, DeletionProtectionSpec, Encryption, FailurePolicy,
    IdentityDefaults, MoverDefaults, NamespaceDeletePolicy, RepositoryMode, ScheduleDefaults,
    default_namespace_delete_policy, default_repository_mode,
};
use crate::maintenance::RepositoryMaintenanceSpec;
use crate::server::{ServerSpec, ServerStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A kopia repository owned by one namespace, referenced by `SnapshotPolicy`s and `Restore`s.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "Repository",
    namespaced,
    status = "RepositoryStatus",
    shortname = "kopiarepo",
    category = "kopiur",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Backend","type":"string","jsonPath":".status.backend"}"#,
    printcolumn = r#"{"name":"Server","type":"string","jsonPath":".status.server.endpoint"}"#,
    printcolumn = r#"{"name":"IndexBlobs","type":"integer","jsonPath":".status.storageStats.indexBlobCount","priority":1}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
// §7/§15: create-time-immutability transition rules in the CRD schema (apiserver +
// CI), complementing the webhook checks. The `create.*` rules only bite when `create`
// is present on both sides. `encryption` (the password Secret reference) is deliberately
// NOT locked: kopia fixes only the resolved password value in the repo format, never the
// Secret name/key, and the reference is not a reliable proxy (a rename with identical
// content must not be rejected — that broke GitOps). See `validate::diff_immutable_repo_fields`.
// Each leaf is `has()`-guarded: CEL field access on an absent optional key raises a
// "no such key" error (which fails the WHOLE rule → 422 on *every* update, blocking
// the controller's finalizer/status writes), so we compare presence first and only
// dereference when set — the common `create: {enabled: true}` case (no splitter/
// hash/encryption/ecc) must reconcile, not wedge. Mirrors the webhook's None-vs-Some
// semantics in `validate::diff_immutable_repo_fields`.
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "!has(self.create) || !has(oldSelf.create) || (has(self.create.splitter) == has(oldSelf.create.splitter) && (!has(self.create.splitter) || self.create.splitter == oldSelf.create.splitter))", "message": "create.splitter is immutable after creation"},
    {"rule": "!has(self.create) || !has(oldSelf.create) || (has(self.create.hash) == has(oldSelf.create.hash) && (!has(self.create.hash) || self.create.hash == oldSelf.create.hash))", "message": "create.hash is immutable after creation"},
    {"rule": "!has(self.create) || !has(oldSelf.create) || (has(self.create.encryption) == has(oldSelf.create.encryption) && (!has(self.create.encryption) || self.create.encryption == oldSelf.create.encryption))", "message": "create.encryption is immutable after creation"},
    {"rule": "!has(self.create) || !has(oldSelf.create) || (has(self.create.ecc) == has(oldSelf.create.ecc) && (!has(self.create.ecc) || self.create.ecc == oldSelf.create.ecc))", "message": "create.ecc is immutable after creation"}
]))]
#[serde(rename_all = "camelCase")]
pub struct RepositorySpec {
    /// Exactly one storage backend.
    pub backend: Backend,
    /// Repository password (a Secret reference).
    pub encryption: Encryption,
    /// What to do when the repository does not yet exist (absent means it must already exist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<CreateBehavior>,
    /// Tuning for the bootstrap/discovery mover Job (`<name>-discovery`) that
    /// connects/creates an object-store repository the operator cannot reach
    /// in-process (and re-runs for catalog re-scans).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<BootstrapSpec>,
    /// Base mover configuration inherited by every mover this repository spawns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover_defaults: Option<MoverDefaults>,
    /// Scheduling defaults (e.g. `timezone`) inherited by consumers that don't set
    /// their own equivalent field — verification, replication, and maintenance
    /// schedules today; set once here instead of repeating it on every cron.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_defaults: Option<ScheduleDefaults>,
    /// Bounds materialization of `origin: discovered` `Snapshot` CRs from the kopia catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogBounds>,
    /// Identity defaults (CEL `*Expr`) applied when consumers don't override.
    /// Set `cluster` when this repository's backend is shared across clusters
    /// (e.g. one bucket backed up from more than one Kubernetes cluster) — the
    /// default kopia identity hostname then becomes `<namespace>.<cluster>`
    /// instead of bare `<namespace>`, so same-named namespaces on different
    /// clusters never collide. Same semantics as `ClusterRepository`'s field of
    /// the same name (ADR-0004 §5); a consumer's explicit `spec.identity` still
    /// wins over anything here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_defaults: Option<IdentityDefaults>,
    /// Optional kopia web-UI server, exposed via a `Service` in this namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerSpec>,
    /// Maintenance control; when absent or enabled the reconciler creates and owns a `Maintenance` CR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<RepositoryMaintenanceSpec>,
    /// What happens to this repository's snapshots when a consuming namespace is deleted.
    #[serde(default = "default_namespace_delete_policy")]
    #[schemars(default = "default_namespace_delete_policy")]
    pub on_namespace_delete: NamespaceDeletePolicy,
    /// Mass-deletion circuit breaker for this repository's Snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_protection: Option<DeletionProtectionSpec>,
    /// Access mode: `ReadWrite` (default) or `ReadOnly` (serves restores only).
    #[serde(default = "default_repository_mode")]
    #[schemars(default = "default_repository_mode")]
    pub mode: RepositoryMode,
    /// Pause this repository: skip connect/bootstrap and maintenance projection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspend: bool,
    /// Repository health thresholds (tunes the index-blob-count warning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<RepositoryHealthSpec>,
    /// Mutable kopia repository parameters, re-applied on bootstrap whenever they drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<RepositoryParameters>,
}

/// Tuning for the bootstrap/discovery mover Job, shared by `Repository` and
/// `ClusterRepository`. Bootstrap connects (or, with `create`, creates) an
/// object-store repository the operator cannot reach in-process.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapSpec {
    /// Failure policy for the bootstrap Job. `activeDeadlineSeconds` caps how long
    /// a connect may run before the Job is marked failed (default 120s); raise it
    /// for a slow backend — e.g. an rclone remote whose repository metadata and
    /// indexes load through kopia's embedded `rclone serve`/WebDAV bridge.
    /// `backoffLimit` bounds retries. `podStartupDeadlineSeconds` is accepted for
    /// shape parity but is not honored by the bootstrap Job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_policy: Option<FailurePolicy>,
}

/// Mutable kopia repository parameters (`kopia repository set-parameters`), shared by
/// `Repository` and `ClusterRepository`.
///
/// Deliberately NOT part of `spec.create` ([`crate::common::CreateBehavior`]): that block's
/// whole contract is create-time-fixed-and-immutable, enforced by CEL and the webhook, and
/// these are mutable on a live repository — re-applying them to an existing repo is the
/// entire point.
///
/// Every field is optional with no kopiur-side default: **absent means "don't touch it"**,
/// so declaring nothing here changes nothing about how a repository behaves. Note the
/// consequence for GitOps — *removing* a value you previously set does not restore kopia's
/// default, it leaves the repository at whatever you last applied. Set it back explicitly.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryParameters {
    /// Epoch-manager tuning — how fast kopia closes epochs and therefore how fast index
    /// blobs get compacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<EpochParameters>,
    /// Object-lock blob retention (S3/Azure/GCS) — ransomware protection. Absent means
    /// "don't touch it"; use `disabled: true` to actively turn it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_retention: Option<BlobRetention>,
}

/// Object-lock blob retention (`kopia repository set-parameters --retention-mode/--retention-period`).
///
/// Externally tagged, so exactly one variant exists by construction. That is load-bearing
/// rather than stylistic: kopia rejects a mode without a period and a period without a mode
/// ("both retention mode and period must be provided when setting blob retention
/// properties"), and pairing the period *with* the mode in the type makes that error
/// unrepresentable instead of merely validated.
///
/// Requires a backend that supports object lock — S3, Azure, or GCS — **and** a bucket with
/// object lock enabled at creation time (it cannot be turned on afterwards). kopia's flags do
/// not create it. On an unsupported backend `set-parameters` hard-fails with
/// `blob-retention: unsupported put-blob option`, so admission rejects those backends.
///
/// # What this does and does not protect
///
/// The lock is applied when a blob is **written**. Kopiur does not enable kopia's
/// `maintenance set --extend-object-locks`, so locks on blobs that are still needed are never
/// extended: a blob written today under `period: 720h` becomes deletable again in 30 days.
/// Treat it as a rolling floor, not cumulative immutability, and size the period to exceed
/// your longest recovery window. Blobs written *before* retention was enabled — including the
/// repository format blob — are never retroactively locked.
///
/// ```
/// use kopiur_api::repository::{BlobRetention, RetentionWindow};
///
/// // Externally tagged: the wire form is `{ "governance": { "period": "720h" } }`.
/// let r: BlobRetention = serde_json::from_value(serde_json::json!({
///     "governance": { "period": "720h" }
/// }))
/// .unwrap();
/// assert_eq!(r, BlobRetention::Governance(RetentionWindow { period: "720h".into() }));
/// assert_eq!(r.kind_str(), "Governance");
///
/// // Disabling carries no period — kopia ignores it, and the type says so.
/// let off: BlobRetention = serde_json::from_value(serde_json::json!({ "disabled": true }))
///     .unwrap();
/// assert_eq!(off.kind_str(), "Disabled");
/// ```
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BlobRetention {
    /// `GOVERNANCE` — locked against ordinary deletes, but a sufficiently privileged
    /// identity can still shorten or remove the lock. The safe default for most clusters.
    Governance(RetentionWindow),
    /// `COMPLIANCE` — **nobody can shorten or remove the lock before it expires**, including
    /// the account root. An oversized period is an unfixable storage-cost commitment; there
    /// is no recovery path short of deleting the bucket after expiry.
    Compliance(RetentionWindow),
    /// Actively disable retention (`--retention-mode=none`). Must be `true`.
    ///
    /// A `bool` rather than a unit variant because an externally-tagged unit variant
    /// serializes as the bare string `"Disabled"`, mixing string and object forms in one
    /// `oneOf` and breaking the structural schema. Same shape, and same reason, as
    /// [`crate::cluster_repository::AllowedNamespaces::All`].
    ///
    /// This is distinct from omitting `blobRetention` entirely: absent means "leave the
    /// repository alone", so deleting the block from a manifest can never silently strip
    /// ransomware protection someone configured deliberately.
    Disabled(bool),
}

impl BlobRetention {
    /// Stable discriminant string for status/metrics/printcolumns.
    pub fn kind_str(&self) -> &'static str {
        match self {
            BlobRetention::Governance(_) => "Governance",
            BlobRetention::Compliance(_) => "Compliance",
            BlobRetention::Disabled(_) => "Disabled",
        }
    }

    /// The declared retention window, or `None` when retention is being disabled.
    pub fn window(&self) -> Option<&RetentionWindow> {
        match self {
            BlobRetention::Governance(w) | BlobRetention::Compliance(w) => Some(w),
            BlobRetention::Disabled(_) => None,
        }
    }
}

/// How long a blob stays locked once written.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetentionWindow {
    /// Go-style duration; kopia's minimum is `24h` and there is no maximum.
    ///
    /// Accepts `h`/`m`/`s` (or a bare number of seconds) — **not `d`**. Note that kopia's own
    /// CLI *does* take `30d`, so a period copied from kopia's documentation is rejected here;
    /// write 30 days as `720h`. Kopiur deliberately keeps one duration grammar across every
    /// CRD field rather than matching kopia's per-flag variations.
    pub period: String,
}

/// kopia epoch-manager parameters. Absent fields are left at whatever the repository
/// already has (kopia's defaults, noted per field).
///
/// An epoch may only advance once it is **older than `minDuration`** AND has accumulated
/// `advanceOnCount` blobs or `advanceOnSizeMB` of index data. `minDuration` is a floor, so
/// kopia's 24h default means a busy fleet — say 17 hourly policies at ~60 index blobs/hour
/// — is forced to ~1700 blobs before an epoch may close, and kopia only compacts two epochs
/// behind. The repository then permanently carries thousands of uncompacted index blobs,
/// tripping `IndexBlobHealth` and slowing every mover's connect. Lowering `minDuration` is
/// the lever for that (#258).
///
/// `cleanupSafetyMargin` is deliberately **observable but not settable**: its job is to stop
/// kopia deleting index blobs a concurrent writer still needs, and there is no safe generic
/// advice for lowering it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EpochParameters {
    /// Minimum epoch age before it may advance (kopia default `24h`). A Go-style duration
    /// (`6h`, `90m`). The advance **gate** — no blob count closes an epoch younger than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_duration: Option<String>,
    /// How often clients re-read epoch state (kopia default `20m`). Go-style duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_frequency: Option<String>,
    /// Index blobs in an epoch that trigger an advance, once older than `minDuration`
    /// (kopia default `20`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advance_on_count: Option<i64>,
    /// Total index size in an epoch that triggers an advance, once older than `minDuration`
    /// (kopia default `10` MiB).
    ///
    /// Named `MiB`, not `MB`, with an explicit rename rather than the derived camelCase
    /// (`advanceOnSizeMb`, which reads as *megabit*). The unit is genuinely mebibytes —
    /// kopia's `--epoch-advance-on-size-mb` multiplies by 1048576, so `10` is 10485760
    /// bytes — even though kopia's own log renders the result as "MB". That ambiguity is
    /// this field's main hazard; the API surface should not reproduce it.
    #[serde(
        default,
        rename = "advanceOnSizeMiB",
        skip_serializing_if = "Option::is_none"
    )]
    pub advance_on_size_mb: Option<i64>,
    /// Epochs between full index checkpoints (kopia default `7`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_frequency: Option<i64>,
    /// Parallelism for epoch cleanup deletions (kopia default `4`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_parallelism: Option<i64>,
}

/// The epoch parameters the repository ACTUALLY reports, mirrored into `status` from
/// `kopia repository status` at the last bootstrap.
///
/// A separate type from [`EpochParameters`] rather than a reuse, for three reasons: the CRD
/// spec type cannot hold kopia's full output (it omits `enabled` and `cleanupSafetyMargin`);
/// `Option` would mean opposite things in the two positions (spec `None` = "don't touch",
/// status `None` = "kopia didn't report it"); and non-`Option` fields encode "observed means
/// complete" in the type. Same reasoning as [`StorageStats`] being its own type.
///
/// This is what makes the apply honest: it is best-effort, so a failed apply stays visible
/// here as drift from `spec` instead of silently doing nothing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObservedEpochParameters {
    /// Whether kopia's epoch manager is enabled on this repository at all.
    pub enabled: bool,
    /// Observed minimum epoch age, as a Go-style duration.
    pub min_duration: String,
    /// Observed epoch-state refresh frequency, as a Go-style duration.
    pub refresh_frequency: String,
    /// Observed cleanup safety margin, as a Go-style duration. Reported for diagnosis;
    /// not settable through `spec.parameters`.
    pub cleanup_safety_margin: String,
    /// Observed index-blob count that triggers an epoch advance.
    pub advance_on_count: i64,
    /// Observed total index size (MiB) that triggers an epoch advance.
    #[serde(rename = "advanceOnSizeMiB")]
    pub advance_on_size_mb: i64,
    /// Observed epochs between full index checkpoints.
    pub checkpoint_frequency: i64,
    /// Observed epoch-cleanup delete parallelism.
    pub delete_parallelism: i64,
}

/// Observed kopia repository parameters, mirrored into `status`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObservedRepositoryParameters {
    /// The epoch parameters the repository reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<ObservedEpochParameters>,
    /// The blob retention the repository reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_retention: Option<ObservedBlobRetention>,
}

/// The blob retention the repository ACTUALLY reports, mirrored into `status` from
/// `kopia repository status` at the last bootstrap.
///
/// A separate type from [`BlobRetention`] for the same reasons [`ObservedEpochParameters`] is
/// separate from [`EpochParameters`]: the spec type is a closed union that cannot express
/// "kopia reported nothing", and non-`Option` fields encode "observed means complete".
///
/// `mode` is a plain `String`, not the spec enum, because kopia reports an empty mode when
/// retention is off — a value the union deliberately cannot hold. Reporting kopia's own word
/// verbatim also keeps the mirror honest if kopia ever adds a mode kopiur doesn't model.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObservedBlobRetention {
    /// Whether the repository currently has blob retention in force (kopia's
    /// `IsRetentionEnabled()`: a non-empty mode AND a non-zero period).
    pub enabled: bool,
    /// Observed retention mode exactly as kopia reports it (`GOVERNANCE`, `COMPLIANCE`, or
    /// empty when off).
    pub mode: String,
    /// Observed retention period, as a Go-style duration (`720h`). Empty-equivalent (`0s`)
    /// when retention is off.
    pub period: String,
}

/// Repository health thresholds, shared by `Repository` and `ClusterRepository`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryHealthSpec {
    /// Index-blob count above which the reconciler raises the `IndexBlobHealth` warning (`0` disables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_index_blob_warn_threshold")]
    pub index_blob_warn_threshold: Option<i64>,
    /// Periodic backend health probe (on by default): re-connect the repository
    /// on a timer to confirm the kopia repository still exists at the backend.
    /// With the default `onFailure: Degrade` it doubles as the repository
    /// circuit breaker — see `probe.onFailure`. Disable with
    /// `probe.enabled: false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<RepositoryHealthProbeSpec>,
}

/// schemars default for [`RepositoryHealthSpec::index_blob_warn_threshold`] —
/// [`DEFAULT_INDEX_BLOB_WARN_THRESHOLD`](crate::consts::DEFAULT_INDEX_BLOB_WARN_THRESHOLD).
/// Returns the field's `Option` type so schemars 1 emits the schema `default:`
/// (which the apiserver materializes on admission). Safe because
/// `resolve_index_blob_warn_threshold` resolves an absent field to exactly this
/// constant — server-side defaulting changes the stored shape, not behavior.
fn default_index_blob_warn_threshold() -> Option<i64> {
    Some(crate::consts::DEFAULT_INDEX_BLOB_WARN_THRESHOLD)
}

/// Backend health probe, shared by `Repository` and `ClusterRepository`. On by
/// default.
///
/// Once a repository reaches `Ready`, the operator trusts that pinned status and
/// — for object-store / volume-backed backends — never re-checks the backend on
/// its steady-state heartbeat. If the kopia repository is wiped or becomes
/// unreachable, nothing notices until a backup runs and fails. The probe
/// re-connects the backend every [`interval`](Self::interval) and surfaces the
/// result as the `BackendReachable` condition + a Warning event.
///
/// What happens past [`failureThreshold`](Self::failure_threshold) consecutive
/// failures depends on [`onFailure`](Self::on_failure): with the default
/// `Degrade`, the repository moves to `Degraded` and backups, maintenance, and
/// replication are **paused** until a re-connect succeeds (the repository
/// circuit breaker); with `Alert`, the repository stays `Ready` and only the
/// condition + event + metric fire.
///
/// **Never destructive.** A wiped repository and a transient outage look alike,
/// and silently recreating an empty repository over a real one destroys
/// restorability — so the probe never auto-recreates. Acting on a
/// `RepositoryVanished` alert (a deliberate re-create) is a human decision.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryHealthProbeSpec {
    /// Whether the probe runs (default `true`). `false` disables probing — and
    /// with it the circuit breaker, since the probe is its only sensor: a wiped
    /// or unreachable backend then goes unnoticed until the next backup fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_health_probe_enabled")]
    pub enabled: Option<bool>,
    /// How often to re-probe the backend (Go-style duration like `30m` or `1h`;
    /// minimum `30s`, default `30m`). Inert when `enabled: false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_health_probe_interval")]
    pub interval: Option<String>,
    /// How many *consecutive* failing probes to require before the failure is
    /// acted on (default `3`): the loud `BackendReachable=False` condition +
    /// event fire, and — under `onFailure: Degrade` — the repository moves to
    /// `Degraded`. Debounces a single transient blip from alarming, tripping
    /// the breaker, or nudging a destructive manual recreate. Any success
    /// resets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_health_probe_failure_threshold")]
    pub failure_threshold: Option<i64>,
    /// What sustained probe failure (past `failureThreshold`) does to the
    /// repository (default `Degrade`). `Degrade` moves it to `Degraded`,
    /// pausing backups, maintenance, and replication until a re-connect
    /// succeeds — recovery is automatic. `Alert` keeps the repository `Ready`
    /// and only raises the condition + Warning event + metric (the pre-breaker
    /// behavior); backups keep running against the failing backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_probe_on_failure")]
    pub on_failure: Option<ProbeOnFailure>,
}

/// What sustained backend-probe failure (past `failureThreshold`) does to the
/// repository (default `Degrade`). `Degrade` moves it to `Degraded`, pausing
/// backups, maintenance, and replication until a re-connect succeeds —
/// recovery is automatic. `Alert` keeps the repository `Ready` and only raises
/// the `BackendReachable` condition + Warning event + metric; backups keep
/// running against the failing backend. Neither ever auto-recreates.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum ProbeOnFailure {
    /// Past `failureThreshold` consecutive failing probes, move the repository
    /// to `Degraded` and pause backups, maintenance, and replication until a
    /// re-connect succeeds (the repository circuit breaker). Recovery is
    /// automatic — any successful connect returns the repository to `Ready`.
    #[default]
    Degrade,
    /// Alert-only: the repository stays `Ready` and only the `BackendReachable`
    /// condition, a Warning event, and the failure metric are raised. Backups
    /// keep running (and failing) against the unhealthy backend.
    Alert,
}

/// schemars default for [`RepositoryHealthProbeSpec::enabled`] —
/// [`DEFAULT_HEALTH_PROBE_ENABLED`](crate::consts::DEFAULT_HEALTH_PROBE_ENABLED)
/// (`true`, #345). Returns the field's `Option` type so schemars 1 emits the
/// schema `default:`. Safe to materialize server-side because
/// [`RepositoryHealthProbeSpec::enabled`] resolves an absent field to exactly
/// this constant. Note the schema default only materializes when the
/// `health.probe` object is *present*; the absent-parent case is resolved by
/// the same `enabled()` seam, so the two paths agree.
fn default_health_probe_enabled() -> Option<bool> {
    Some(crate::consts::DEFAULT_HEALTH_PROBE_ENABLED)
}

/// schemars default for [`RepositoryHealthProbeSpec::on_failure`] —
/// [`ProbeOnFailure::Degrade`], matching `effective_on_failure`'s absent →
/// `Degrade` resolution. Returns the field's `Option` type so schemars 1 emits
/// the schema `default:` (`"Degrade"` on the wire).
fn default_probe_on_failure() -> Option<ProbeOnFailure> {
    Some(ProbeOnFailure::Degrade)
}

/// schemars default for [`RepositoryHealthProbeSpec::interval`] — the string
/// form of [`DEFAULT_HEALTH_PROBE_INTERVAL`](crate::consts::DEFAULT_HEALTH_PROBE_INTERVAL)
/// (`30m`). `effective_interval` resolves an absent/unparseable value to that
/// same duration, so materializing `30m` is behavior-preserving; the field is
/// inert unless `probe.enabled`. A unit test pins `"30m"` to the constant.
fn default_health_probe_interval() -> Option<String> {
    Some("30m".to_string())
}

/// schemars default for [`RepositoryHealthProbeSpec::failure_threshold`] —
/// [`DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD`](crate::consts::DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD)
/// (`3`), matching `effective_failure_threshold`'s absent→CONST resolution.
fn default_health_probe_failure_threshold() -> Option<i64> {
    Some(crate::consts::DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD)
}

impl RepositoryHealthProbeSpec {
    /// Whether the backend health probe runs (`spec.health.probe.enabled`).
    ///
    /// **This resolver is the load-bearing default (#345), not the CRD schema
    /// `default:`** — a nested schema default only materializes when its parent
    /// object is present, and most specs omit `spec.health`/`probe` entirely.
    /// Absent spec/probe/field ⇒
    /// [`DEFAULT_HEALTH_PROBE_ENABLED`](crate::consts::DEFAULT_HEALTH_PROBE_ENABLED)
    /// (`true`); an explicit `Some(v)` ⇒ `v`. Every caller resolves through this
    /// single seam, so `enabled: false` is the one way to opt out.
    pub fn enabled(health: Option<&RepositoryHealthSpec>) -> bool {
        health
            .and_then(|h| h.probe.as_ref())
            .and_then(|p| p.enabled)
            .unwrap_or(crate::consts::DEFAULT_HEALTH_PROBE_ENABLED)
    }

    /// The effective `onFailure` policy: `spec.health.probe.onFailure` when set,
    /// else [`ProbeOnFailure::Degrade`] — the circuit breaker is the default,
    /// matching the schema `default:` (which only materializes when the `probe`
    /// object is present; this resolver covers the absent-parent case).
    pub fn effective_on_failure(health: Option<&RepositoryHealthSpec>) -> ProbeOnFailure {
        health
            .and_then(|h| h.probe.as_ref())
            .and_then(|p| p.on_failure)
            .unwrap_or_default()
    }

    /// The effective probe cadence used **when the probe is enabled**:
    /// `interval` when set and parseable, else [`DEFAULT_HEALTH_PROBE_INTERVAL`].
    /// (The webhook rejects an unparseable value, so the fallback only covers
    /// objects admitted before the validator existed.)
    ///
    /// [`DEFAULT_HEALTH_PROBE_INTERVAL`]: crate::consts::DEFAULT_HEALTH_PROBE_INTERVAL
    pub fn effective_interval(health: Option<&RepositoryHealthSpec>) -> std::time::Duration {
        health
            .and_then(|h| h.probe.as_ref())
            .and_then(|p| p.interval.as_deref())
            .and_then(crate::duration::parse_go_duration)
            .unwrap_or(crate::consts::DEFAULT_HEALTH_PROBE_INTERVAL)
    }

    /// The effective consecutive-failure threshold before the loud condition is
    /// raised: `failureThreshold` when set (clamped to at least 1), else
    /// [`DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD`].
    ///
    /// [`DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD`]: crate::consts::DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD
    pub fn effective_failure_threshold(health: Option<&RepositoryHealthSpec>) -> i64 {
        health
            .and_then(|h| h.probe.as_ref())
            .and_then(|p| p.failure_threshold)
            .map(|t| t.max(1))
            .unwrap_or(crate::consts::DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD)
    }
}

/// Resolve the effective index-blob warning threshold from an optional
/// `spec.health`. Pure, so it's shared by the admission webhook, the controller,
/// and tests without forking the default/disable semantics:
///
/// * absent spec or unset field ⇒
///   [`DEFAULT_INDEX_BLOB_WARN_THRESHOLD`](crate::consts::DEFAULT_INDEX_BLOB_WARN_THRESHOLD),
/// * `Some(0)` ⇒ `0` (the sentinel that disables the warning),
/// * `Some(n)` ⇒ `n`.
///
/// ```
/// use kopiur_api::repository::{resolve_index_blob_warn_threshold, RepositoryHealthSpec};
/// use kopiur_api::consts::DEFAULT_INDEX_BLOB_WARN_THRESHOLD;
///
/// assert_eq!(resolve_index_blob_warn_threshold(None), DEFAULT_INDEX_BLOB_WARN_THRESHOLD);
/// let h = RepositoryHealthSpec { index_blob_warn_threshold: Some(0), ..Default::default() };
/// assert_eq!(resolve_index_blob_warn_threshold(Some(&h)), 0); // disabled
/// let h = RepositoryHealthSpec { index_blob_warn_threshold: Some(250), ..Default::default() };
/// assert_eq!(resolve_index_blob_warn_threshold(Some(&h)), 250);
/// ```
pub fn resolve_index_blob_warn_threshold(health: Option<&RepositoryHealthSpec>) -> i64 {
    health
        .and_then(|h| h.index_blob_warn_threshold)
        .unwrap_or(crate::consts::DEFAULT_INDEX_BLOB_WARN_THRESHOLD)
}

/// Lifecycle phase of a repository. A freshly admitted CR starts in `Pending`.
///
/// ```
/// use kopiur_api::repository::RepositoryPhase;
///
/// assert_eq!(serde_json::to_value(RepositoryPhase::Ready).unwrap(), "Ready");
/// // An unrecognized phase from a newer operator decodes instead of erroring.
/// let p: RepositoryPhase = serde_json::from_value(serde_json::json!("Upgrading")).unwrap();
/// assert_eq!(p, RepositoryPhase::Unknown("Upgrading".into()));
/// assert_eq!(serde_json::to_value(&p).unwrap(), "Upgrading");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum RepositoryPhase {
    /// Accepted by the API server but not yet reconciled.
    #[default]
    Pending,
    /// Connecting to (or creating) the kopia repository.
    Initializing,
    /// Connected and healthy.
    Ready,
    /// Temporarily not fully operational, and self-healing: a retryable
    /// bootstrap/connect failure is being retried, or the backend health probe
    /// exceeded its `failureThreshold` (the circuit breaker is open — backups,
    /// maintenance, and replication are paused until a re-connect succeeds).
    /// See the `BackendReachable` and `Ready` conditions for the cause.
    Degraded,
    /// Connect/create failed; see conditions for the actionable reason.
    Failed,
    /// A phase string this build does not recognize (newer operator, or legacy
    /// stored data). Decode-compat only — hidden from the CRD schema, never
    /// produced by this build. Never treated as `Ready`: consumers that gate on
    /// "is the repository usable" must hold, not proceed.
    Unknown(String),
}

crate::common::phase_serde!(
    RepositoryPhase,
    "Lifecycle phase of a repository. A freshly admitted CR starts in `Pending`."
);

impl crate::common::PhaseLabel for RepositoryPhase {
    const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Initializing,
        Self::Ready,
        Self::Degraded,
        Self::Failed,
    ];
    fn label(&self) -> &str {
        match self {
            Self::Pending => "Pending",
            Self::Initializing => "Initializing",
            Self::Ready => "Ready",
            Self::Degraded => "Degraded",
            Self::Failed => "Failed",
            Self::Unknown(s) => s,
        }
    }
    fn unknown(raw: String) -> Self {
        Self::Unknown(raw)
    }
}

/// Observed state of a `Repository`, carrying resolved values pinned by the reconciler.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryStatus {
    /// Current lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RepositoryPhase>,
    /// `metadata.generation` of the `spec` last reconciled; drives staleness detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// `resourceVersion` of the password Secret observed at the last connect attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_credential_version: Option<String>,
    /// Kopia repository unique ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_id: Option<String>,
    /// Mirror of `spec.backend` discriminant for the print column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Repository size and snapshot counts from the last catalog scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_stats: Option<StorageStats>,
    /// Catalog-materialization status (how many discovered `Snapshot`s, last refresh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogStatus>,
    /// Resolved kopia server endpoint/auth, pinned by the reconciler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerStatus>,
    /// Last reverify-request token honored from a `Snapshot`'s re-probe nudge
    /// (RFC3339); the loop guard that keeps each request a one-shot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reverify_at: Option<String>,
    /// Backend health-probe state (`spec.health.probe`), when enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<RepositoryHealthStatus>,
    /// The kopia repository parameters actually observed at the last bootstrap. Compare
    /// against `spec.parameters` to see whether a declared value landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<ObservedRepositoryParameters>,
    /// Standard Kubernetes conditions (e.g. `Connected`, `MaintenanceOwned`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// Backend health-probe state, shared by `Repository` and `ClusterRepository`.
/// Pinned by the reconciler when `spec.health.probe` is enabled so the next
/// reconcile can tell whether a probe is due and how many consecutive failures
/// have accrued (the debounce that keeps a transient blip from raising the loud
/// `RepositoryVanished` / `BackendReachable=False` condition).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryHealthStatus {
    /// RFC 3339 timestamp of the last completed probe (success or failure); drives
    /// the `health_probe_due` timer so the probe re-fires on cadence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_at: Option<String>,
    /// RFC 3339 timestamp of the last *successful* probe (backend reachable, repo present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_healthy_at: Option<String>,
    /// Consecutive failing probes accrued; reset to zero on any success. The loud
    /// condition is raised only once this reaches the failure threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_probe_failures: Option<i64>,
    /// RFC 3339 timestamp of the first failure in the current failing streak.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_failure_at: Option<String>,
    /// RFC 3339 timestamp at which the last backend health probe was *launched*
    /// (the bootstrap Job created for it), cleared when its result is finalized.
    ///
    /// This is the **launch-side** rate limit, and it is what makes the probe
    /// terminate. `lastProbeAt` is written only at *finalize*, so gating the
    /// launch on that alone recycles the bootstrap Job forever: the gate destroys
    /// every Job whose completion would have cleared it (#273). Never compared
    /// against `lastProbeAt` — see `kopiur_controller::health::probe_action`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_attempt_at: Option<String>,
}

/// Aggregate repository storage figures from the last catalog scan.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageStats {
    /// Total snapshots present in the repository (across all identities).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_count: Option<i64>,
    /// Human-readable total on-disk size (e.g. `412Gi`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size: Option<String>,
    /// Logical bytes under management (the integer form of `total_size`): the sum,
    /// over each distinct snapshot source, of the most-recent snapshot's logical
    /// size. Exposed to backup preflight as `repository.sizeBytes`. This is
    /// repository *total size*, not backend free space (object stores don't report
    /// remaining capacity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size_bytes: Option<i64>,
    /// RFC 3339 timestamp these stats were last observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at: Option<String>,
    /// Number of content-index blobs (`kopia index list`) observed at the last bootstrap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_blob_count: Option<i64>,
}

/// Status of catalog materialization for `origin: discovered` `Snapshot` CRs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogStatus {
    /// How many `Snapshot` CRs were materialized from the catalog scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovered_backup_count: Option<i64>,
    /// RFC 3339 timestamp of the last catalog refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh_at: Option<String>,
    /// Snapshots in the last complete listing classified as another cluster's
    /// (see `catalog.foreignSnapshots`); never materialized under `Ignore`.
    /// As of `catalog.lastRefreshAt` — enable `periodicRefresh` to keep it
    /// current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreign_snapshot_count: Option<i64>,
    /// The RFC3339 token VALUE of the `kopiur.home-operations.com/catalog-scan-requested-at`
    /// annotation last honored by a completed catalog scan. Compared by
    /// **equality** against the live annotation to decide whether a requested
    /// on-demand scan is still pending — deliberately NOT a timestamp comparison
    /// against `lastRefreshAt` (a periodic refresh completing after the request
    /// was made would otherwise look like it honored the request even though it
    /// started before the annotation was set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_request_honored: Option<String>,
    /// RFC 3339 timestamp of the last bootstrap/scan attempt initiated BECAUSE OF
    /// a pending `catalog-scan-requested-at` token (i.e. the token arm was the
    /// reason the attempt fired). Used only to rate-limit token-driven attempts
    /// on a Ready-but-unreachable repository — it is never compared against the
    /// token for retirement (that's `scanRequestHonored`, by equality).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_request_attempt_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{PhaseLabel, RepositoryMode};
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn repository_phase_all_covers_every_variant_uniquely() {
        // Mirrors the `SnapshotPhase` tripwire: every variant is in ALL with a
        // unique, non-empty label, so the metrics reset set and any consumer
        // iterating phases can never silently miss one.
        let labels: Vec<&str> = RepositoryPhase::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(RepositoryPhase::ALL.len(), 5);
        assert!(labels.iter().all(|l| !l.is_empty()));
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "phase labels must be unique");
        assert!(RepositoryPhase::ALL.contains(&RepositoryPhase::default()));
    }

    #[test]
    fn repository_schema_emits_context_free_defaults() {
        // brume's complaint: "everything is usually null for the defaults" in the
        // CRD/JSON schema. These context-free constants must surface as a schema
        // `default:` (visible in kubectl explain / the YAML language server, and
        // consumed by the generated field reference). The apiserver materializes
        // them server-side, which is safe ONLY because each field's resolver maps
        // absent → exactly this value (see the paired default fns).
        let crd = Repository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let spec = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
        assert_eq!(
            spec["properties"]["health"]["properties"]["indexBlobWarnThreshold"]["default"],
            serde_json::json!(1000)
        );
        assert_eq!(
            spec["properties"]["health"]["properties"]["probe"]["properties"]["enabled"]["default"],
            serde_json::json!(crate::consts::DEFAULT_HEALTH_PROBE_ENABLED),
            "the probe must be default-ON in the schema (#345)"
        );
        assert_eq!(
            spec["properties"]["health"]["properties"]["probe"]["properties"]["interval"]["default"],
            serde_json::json!("30m")
        );
        assert_eq!(
            spec["properties"]["health"]["properties"]["probe"]["properties"]["failureThreshold"]["default"],
            serde_json::json!(3)
        );
        assert_eq!(
            spec["properties"]["health"]["properties"]["probe"]["properties"]["onFailure"]["default"],
            serde_json::json!("Degrade"),
            "the breaker (Degrade) must be the schema default for onFailure (#345)"
        );
        assert_eq!(
            spec["properties"]["catalog"]["properties"]["refreshInterval"]["default"],
            serde_json::json!("1h")
        );
        assert_eq!(
            spec["properties"]["server"]["properties"]["service"]["properties"]["port"]["default"],
            serde_json::json!(51515)
        );
    }

    #[test]
    fn health_probe_interval_schema_default_matches_the_duration_constant() {
        // The schema default is a STRING ("30m") but the controller resolves an
        // absent value to the Duration constant. If someone changes the constant
        // without the string (or vice-versa), server-side defaulting would
        // materialize a value that no longer equals the resolver's fallback.
        let s = default_health_probe_interval().expect("some");
        assert_eq!(
            crate::duration::parse_go_duration(&s),
            Some(crate::consts::DEFAULT_HEALTH_PROBE_INTERVAL),
            "default_health_probe_interval() string must parse to DEFAULT_HEALTH_PROBE_INTERVAL"
        );
        assert_eq!(
            default_health_probe_failure_threshold(),
            Some(crate::consts::DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD)
        );
        assert_eq!(
            default_index_blob_warn_threshold(),
            Some(crate::consts::DEFAULT_INDEX_BLOB_WARN_THRESHOLD)
        );
        assert_eq!(
            default_health_probe_enabled(),
            Some(crate::consts::DEFAULT_HEALTH_PROBE_ENABLED)
        );
        assert_eq!(default_probe_on_failure(), Some(ProbeOnFailure::Degrade));
    }

    #[test]
    fn mode_suspend_and_ecc_roundtrip() {
        // ADR-0005 §11/§14(e)/§13(a): mode, suspend, and create.ecc parse the
        // cluster's way and round-trip.
        let yaml = r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s } }
create:
  enabled: true
  encryption: AES256-GCM-HMAC-SHA256
  ecc:
    algorithm: REED-SOLOMON-CRC32
    overheadPercent: 2
mode: ReadOnly
suspend: true
"#;
        let spec: RepositorySpec = from_yaml(yaml);
        assert_eq!(spec.mode, RepositoryMode::ReadOnly);
        assert!(!spec.mode.allows_writes());
        assert!(spec.suspend);
        let ecc = spec.create.as_ref().unwrap().ecc.as_ref().expect("ecc");
        assert_eq!(ecc.algorithm.as_deref(), Some("REED-SOLOMON-CRC32"));
        assert_eq!(ecc.overhead_percent, Some(2));

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["mode"], "ReadOnly");
        assert_eq!(json["suspend"], true);
        let reparsed: RepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn bootstrap_failure_policy_round_trips() {
        let spec: RepositorySpec = from_yaml(
            r#"
backend: { rclone: { remotePath: "mydrive:backups", startupTimeout: 2m } }
encryption: { passwordSecretRef: { name: s } }
bootstrap:
  failurePolicy:
    activeDeadlineSeconds: 600
    backoffLimit: 1
"#,
        );
        let fp = spec
            .bootstrap
            .as_ref()
            .and_then(|b| b.failure_policy.as_ref())
            .expect("bootstrap.failurePolicy");
        assert_eq!(fp.active_deadline_seconds, Some(600));
        assert_eq!(fp.backoff_limit, Some(1));
        // Absent bootstrap stays None.
        let bare: RepositorySpec = from_yaml(
            r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s } }
"#,
        );
        assert!(bare.bootstrap.is_none());
    }

    #[test]
    fn health_threshold_parses_and_resolver_honors_default_and_disable() {
        // Absent spec.health → default threshold.
        let bare: RepositorySpec = from_yaml(
            r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s } }
"#,
        );
        assert!(bare.health.is_none());
        assert_eq!(
            resolve_index_blob_warn_threshold(bare.health.as_ref()),
            crate::consts::DEFAULT_INDEX_BLOB_WARN_THRESHOLD
        );

        // Explicit override parses the cluster's way and resolves verbatim.
        let tuned: RepositorySpec = from_yaml(
            r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s } }
health:
  indexBlobWarnThreshold: 250
"#,
        );
        assert_eq!(
            resolve_index_blob_warn_threshold(tuned.health.as_ref()),
            250
        );

        // 0 is the disable sentinel (not "fall back to default").
        let disabled: RepositorySpec = from_yaml(
            r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s } }
health:
  indexBlobWarnThreshold: 0
"#,
        );
        assert_eq!(
            resolve_index_blob_warn_threshold(disabled.health.as_ref()),
            0
        );
    }

    #[test]
    fn deletion_protection_threshold_schema_default_matches_the_constant() {
        // Mirrors repository_schema_emits_context_free_defaults: a context-free
        // default is safe to server-side-materialize because
        // effective_mass_deletion_threshold maps absent → this same value.
        let crd = Repository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let spec = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
        assert_eq!(
            spec["properties"]["deletionProtection"]["properties"]["threshold"]["default"],
            serde_json::json!(crate::consts::DEFAULT_MASS_DELETION_THRESHOLD)
        );
        assert_eq!(
            crate::consts::effective_mass_deletion_threshold(None),
            crate::consts::DEFAULT_MASS_DELETION_THRESHOLD
        );
    }

    #[test]
    fn deletion_protection_round_trips_and_zero_disables() {
        use crate::common::DeletionProtectionSpec;

        let spec: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             deletionProtection:\n  threshold: 25\n",
        );
        assert_eq!(
            spec.deletion_protection.as_ref().and_then(|d| d.threshold),
            Some(25)
        );
        assert_eq!(
            crate::consts::effective_mass_deletion_threshold(spec.deletion_protection.as_ref()),
            25
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["deletionProtection"]["threshold"], 25);
        let reparsed: RepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // `Some(0)` is the disable sentinel — it passes through, not "fall back to default".
        let disabled = DeletionProtectionSpec { threshold: Some(0) };
        assert_eq!(
            crate::consts::effective_mass_deletion_threshold(Some(&disabled)),
            0
        );

        // Absent deletionProtection stays None and is elided (no stored-object churn).
        let bare: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n",
        );
        assert!(bare.deletion_protection.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("deletionProtection")
                .is_none(),
            "absent deletionProtection must be elided"
        );
    }

    #[test]
    fn storage_stats_index_blob_count_roundtrips() {
        let stats = StorageStats {
            snapshot_count: Some(12),
            total_size: None,
            total_size_bytes: Some(442_000_000),
            last_observed_at: None,
            index_blob_count: Some(1448),
        };
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["indexBlobCount"], 1448);
        assert_eq!(json["totalSizeBytes"], 442_000_000_i64);
        let back: StorageStats = serde_json::from_value(json).unwrap();
        assert_eq!(back, stats);
    }

    #[test]
    fn catalog_status_foreign_snapshot_count_roundtrips() {
        let status: CatalogStatus = from_yaml(
            "discoveredBackupCount: 42\nlastRefreshAt: 2026-06-01T00:00:00Z\nforeignSnapshotCount: 7\n",
        );
        assert_eq!(status.foreign_snapshot_count, Some(7));
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["foreignSnapshotCount"], 7);
        let back: CatalogStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back, status);

        // Absent stays None and is elided (no stored-object churn).
        let bare: CatalogStatus = from_yaml("{}\n");
        assert!(bare.foreign_snapshot_count.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("foreignSnapshotCount")
                .is_none(),
            "absent foreignSnapshotCount must be elided"
        );
    }

    #[test]
    fn catalog_status_scan_request_honored_roundtrips() {
        let status: CatalogStatus = from_yaml(
            "lastRefreshAt: 2026-06-01T00:00:00Z\nscanRequestHonored: 2026-06-01T00:00:00Z\n",
        );
        assert_eq!(
            status.scan_request_honored.as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["scanRequestHonored"], "2026-06-01T00:00:00Z");
        let back: CatalogStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back, status);

        // Absent stays None and is elided.
        let bare: CatalogStatus = from_yaml("{}\n");
        assert!(bare.scan_request_honored.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("scanRequestHonored")
                .is_none(),
            "absent scanRequestHonored must be elided"
        );
    }

    #[test]
    fn catalog_status_scan_request_attempt_at_roundtrips() {
        let status: CatalogStatus = from_yaml(
            "lastRefreshAt: 2026-06-01T00:00:00Z\nscanRequestAttemptAt: 2026-06-01T00:05:00Z\n",
        );
        assert_eq!(
            status.scan_request_attempt_at.as_deref(),
            Some("2026-06-01T00:05:00Z")
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["scanRequestAttemptAt"], "2026-06-01T00:05:00Z");
        let back: CatalogStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back, status);

        // Absent stays None and is elided.
        let bare: CatalogStatus = from_yaml("{}\n");
        assert!(bare.scan_request_attempt_at.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("scanRequestAttemptAt")
                .is_none(),
            "absent scanRequestAttemptAt must be elided"
        );
    }

    #[test]
    fn repository_crd_exposes_index_blobs_print_column() {
        let crd = Repository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let cols = json["spec"]["versions"][0]["additionalPrinterColumns"]
            .as_array()
            .expect("printer columns present");
        assert!(
            cols.iter().any(|c| c["name"] == "IndexBlobs"
                && c["jsonPath"] == ".status.storageStats.indexBlobCount"),
            "Repository must surface the IndexBlobs print column"
        );
    }

    #[test]
    fn repository_crd_carries_immutability_transition_rules() {
        // §7/§15: the spec schema carries the create.{splitter,hash,encryption,ecc}
        // immutability transition rules — but NOT an `encryption` (password Secret ref)
        // rule: the reference is mutable (kopia fixes only the resolved value, so a
        // rename with identical content must pass).
        let crd = Repository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let rules = json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["x-kubernetes-validations"]
            .as_array()
            .expect("spec.x-kubernetes-validations present");
        let has = |needle: &str| {
            rules
                .iter()
                .any(|r| r["rule"].as_str().is_some_and(|s| s.contains(needle)))
        };
        assert!(
            !has("self.encryption == oldSelf.encryption"),
            "the password Secret ref must NOT be locked (a rename must be allowed)"
        );
        assert!(has("self.create.splitter == oldSelf.create.splitter"));
        assert!(has("self.create.hash == oldSelf.create.hash"));
        assert!(has("self.create.ecc == oldSelf.create.ecc"));
    }

    #[test]
    fn create_immutability_rules_guard_each_optional_leaf_with_has() {
        // Regression (e2e): a `create.*` immutability rule that dereferences the leaf
        // without a `has()` guard (`self.create.splitter == oldSelf.create.splitter`)
        // raises a CEL "no such key" error whenever `create` is present but the
        // optional leaf is absent — the common `create: {enabled: true}` case. That
        // error fails the WHOLE rule → the apiserver 422s *every* update, so the
        // controller can never add its finalizer or write status and the Repository
        // wedges below Ready. Each `create.*` leaf must therefore be `has()`-guarded.
        let crd = Repository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let rules = json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["x-kubernetes-validations"]
            .as_array()
            .expect("spec.x-kubernetes-validations present");
        for leaf in ["splitter", "hash", "encryption", "ecc"] {
            let rule = rules
                .iter()
                .find_map(|r| {
                    let s = r["rule"].as_str()?;
                    s.contains(&format!("self.create.{leaf} == oldSelf.create.{leaf}"))
                        .then_some(s)
                })
                .unwrap_or_else(|| panic!("missing create.{leaf} immutability rule"));
            assert!(
                rule.contains(&format!("has(self.create.{leaf})"))
                    && rule.contains(&format!("has(oldSelf.create.{leaf})")),
                "create.{leaf} immutability rule must `has()`-guard the leaf on BOTH sides \
                 (else `create: {{enabled: true}}` 422s every update); got: {rule}"
            );
        }
    }

    #[test]
    fn mode_defaults_to_readwrite_and_emits_openapi_default() {
        // Absent ⇒ ReadWrite (parses) and the schema carries `default: ReadWrite`.
        let spec: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\nencryption: { passwordSecretRef: { name: s } }\n",
        );
        assert_eq!(spec.mode, RepositoryMode::ReadWrite);
        assert!(!spec.suspend);
        // Materialized (not skip-elided), so it round-trips into the stored object.
        assert_eq!(serde_json::to_value(&spec).unwrap()["mode"], "ReadWrite");

        let crd = Repository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let default = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["mode"]["default"];
        assert_eq!(default, "ReadWrite");
    }

    #[test]
    fn health_probe_helpers_default_and_parse() {
        use crate::consts::{
            DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD, DEFAULT_HEALTH_PROBE_INTERVAL,
        };
        // Absent spec / absent probe ⇒ ENABLED (#345: the probe is default-on;
        // this resolver — not the nested schema default, which cannot fire when
        // the parent object is absent — is the load-bearing default), with the
        // interval/threshold defaults.
        assert!(RepositoryHealthProbeSpec::enabled(None));
        assert_eq!(
            RepositoryHealthProbeSpec::effective_interval(None),
            DEFAULT_HEALTH_PROBE_INTERVAL
        );
        assert_eq!(
            RepositoryHealthProbeSpec::effective_failure_threshold(None),
            DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD
        );
        assert_eq!(
            RepositoryHealthProbeSpec::effective_on_failure(None),
            ProbeOnFailure::Degrade,
            "absent onFailure must resolve to the breaker default"
        );

        // A present-but-empty probe object resolves identically (the `Option`
        // layers must agree with the absent-parent path).
        let empty = RepositoryHealthSpec {
            probe: Some(RepositoryHealthProbeSpec::default()),
            ..Default::default()
        };
        assert!(RepositoryHealthProbeSpec::enabled(Some(&empty)));
        assert_eq!(
            RepositoryHealthProbeSpec::effective_on_failure(Some(&empty)),
            ProbeOnFailure::Degrade
        );

        // Parses Go-duration string from the wire, NOT a {secs,nanos} object.
        let spec: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             health:\n  probe:\n    enabled: true\n    interval: 45m\n    failureThreshold: 5\n",
        );
        assert!(RepositoryHealthProbeSpec::enabled(spec.health.as_ref()));
        assert_eq!(
            RepositoryHealthProbeSpec::effective_interval(spec.health.as_ref()),
            std::time::Duration::from_secs(45 * 60)
        );
        assert_eq!(
            RepositoryHealthProbeSpec::effective_failure_threshold(spec.health.as_ref()),
            5
        );

        // An explicit `enabled: false` is THE opt-out: it resolves false and
        // SURVIVES serialization (an Option<bool> elides only None — eliding
        // false would silently re-enable the probe on the next round-trip).
        let disabled: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             health:\n  probe:\n    enabled: false\n    interval: 1h\n",
        );
        assert!(!RepositoryHealthProbeSpec::enabled(
            disabled.health.as_ref()
        ));
        let json = serde_json::to_value(&disabled).unwrap();
        assert_eq!(
            json["health"]["probe"]["enabled"],
            serde_json::json!(false),
            "enabled: false must survive serialization"
        );
        let reparsed: RepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(disabled, reparsed);
        assert!(!RepositoryHealthProbeSpec::enabled(
            reparsed.health.as_ref()
        ));

        // Absent `enabled` inside a present probe stays None and is elided.
        let tuned_only: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             health:\n  probe:\n    interval: 1h\n",
        );
        assert!(RepositoryHealthProbeSpec::enabled(
            tuned_only.health.as_ref()
        ));
        assert!(
            serde_json::to_value(&tuned_only).unwrap()["health"]["probe"]
                .get("enabled")
                .is_none(),
            "absent enabled must be elided (no stored-object churn)"
        );
    }

    #[test]
    fn probe_on_failure_round_trips_and_rejects_unknown_variants() {
        // Explicit `Alert` parses the cluster's way and round-trips as a string.
        let alert: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             health:\n  probe:\n    onFailure: Alert\n",
        );
        assert_eq!(
            RepositoryHealthProbeSpec::effective_on_failure(alert.health.as_ref()),
            ProbeOnFailure::Alert
        );
        let json = serde_json::to_value(&alert).unwrap();
        assert_eq!(json["health"]["probe"]["onFailure"], "Alert");
        let reparsed: RepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(alert, reparsed);

        // Explicit `Degrade` round-trips too.
        let degrade: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             health:\n  probe:\n    onFailure: Degrade\n",
        );
        assert_eq!(
            serde_json::to_value(&degrade).unwrap()["health"]["probe"]["onFailure"],
            "Degrade"
        );

        // Absent onFailure stays None (elided; the resolver supplies Degrade).
        let absent: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             health:\n  probe:\n    enabled: true\n",
        );
        assert!(
            serde_json::to_value(&absent).unwrap()["health"]["probe"]
                .get("onFailure")
                .is_none(),
            "absent onFailure must be elided"
        );
        assert_eq!(
            RepositoryHealthProbeSpec::effective_on_failure(absent.health.as_ref()),
            ProbeOnFailure::Degrade
        );

        // Unknown variant is rejected at decode (the closed enum IS the
        // validator; the CRD schema enforces the same set at admission).
        let v: serde_json::Value = serde_yaml::from_str(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             health:\n  probe:\n    onFailure: Recreate\n",
        )
        .unwrap();
        assert!(
            serde_json::from_value::<RepositorySpec>(v).is_err(),
            "unknown onFailure variant must be rejected"
        );
    }

    #[test]
    fn schedule_defaults_timezone_round_trips() {
        let spec: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             scheduleDefaults:\n  timezone: America/New_York\n",
        );
        assert_eq!(
            spec.schedule_defaults
                .as_ref()
                .and_then(|d| d.timezone.as_deref()),
            Some("America/New_York")
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["scheduleDefaults"]["timezone"], "America/New_York");
        let reparsed: RepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent scheduleDefaults stays None and is elided (no stored-object churn).
        let bare: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n",
        );
        assert!(bare.schedule_defaults.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("scheduleDefaults")
                .is_none(),
            "absent scheduleDefaults must be elided"
        );
    }

    #[test]
    fn identity_defaults_cluster_round_trips_on_repository() {
        // M5: `RepositorySpec.identityDefaults` mirrors `ClusterRepositorySpec`'s
        // field of the same name — same shape, same round-trip behavior (see
        // `cluster_repository::tests::identity_defaults_cluster_round_trips`).
        let spec: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             identityDefaults:\n  cluster: east\n  hostnameExpr: namespace\n",
        );
        let id = spec.identity_defaults.as_ref().expect("identityDefaults");
        assert_eq!(id.cluster.as_deref(), Some("east"));
        assert_eq!(id.hostname_expr.as_deref(), Some("namespace"));
        assert!(id.username_expr.is_none());

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["identityDefaults"]["cluster"], "east");
        assert_eq!(json["identityDefaults"]["hostnameExpr"], "namespace");
        let reparsed: RepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent identityDefaults stays None and is elided (no stored-object churn).
        let bare: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n",
        );
        assert!(bare.identity_defaults.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("identityDefaults")
                .is_none(),
            "absent identityDefaults must be elided"
        );
    }

    #[test]
    fn catalog_foreign_snapshots_round_trips_on_repository() {
        use crate::common::ForeignSnapshots;

        let spec: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             catalog:\n  foreignSnapshots: Fallback\n",
        );
        assert_eq!(
            spec.catalog.as_ref().and_then(|c| c.foreign_snapshots),
            Some(ForeignSnapshots::Fallback)
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["catalog"]["foreignSnapshots"], "Fallback");
        let reparsed: RepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        let spec: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             catalog:\n  foreignSnapshots: Ignore\n",
        );
        assert_eq!(
            spec.catalog.as_ref().and_then(|c| c.foreign_snapshots),
            Some(ForeignSnapshots::Ignore)
        );

        // Absent stays None and is elided.
        let bare: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             catalog: {}\n",
        );
        assert!(bare.catalog.as_ref().unwrap().foreign_snapshots.is_none());
        assert!(
            serde_json::to_value(&bare).unwrap()["catalog"]
                .get("foreignSnapshots")
                .is_none(),
            "absent catalog.foreignSnapshots must be elided"
        );
    }

    #[test]
    fn catalog_adoption_round_trips_on_repository() {
        use crate::common::SnapshotAdoption;

        let spec: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             catalog:\n  adoption: Ignore\n",
        );
        assert_eq!(
            spec.catalog.as_ref().and_then(|c| c.adoption),
            Some(SnapshotAdoption::Ignore)
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["catalog"]["adoption"], "Ignore");
        let reparsed: RepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent stays None and is elided; no schema default (context-dependent).
        let bare: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             catalog: {}\n",
        );
        assert!(bare.catalog.as_ref().unwrap().adoption.is_none());
        assert!(
            serde_json::to_value(&bare).unwrap()["catalog"]
                .get("adoption")
                .is_none(),
            "absent catalog.adoption must be elided"
        );

        let crd = Repository::crd();
        let crd_json = serde_json::to_value(&crd).unwrap();
        let prop = &crd_json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["catalog"]["properties"]["adoption"];
        assert!(
            prop.get("default").is_none(),
            "catalog.adoption must NOT carry a schema default: {prop}"
        );
    }
}
