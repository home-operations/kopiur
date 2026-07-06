//! The `Repository` CRD — a namespaced kopia repository. ADR-0003 §3.1.

use crate::backend::Backend;
use crate::common::{
    CatalogBounds, CreateBehavior, Encryption, FailurePolicy, MoverDefaults, NamespaceDeletePolicy,
    RepositoryMode, ScheduleDefaults, default_namespace_delete_policy, default_repository_mode,
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
    /// Tuning for the one-shot bootstrap Job that connects/creates an object-store
    /// repository the operator cannot reach in-process.
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
}

/// Tuning for the one-shot bootstrap Job, shared by `Repository` and
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

/// Repository health thresholds, shared by `Repository` and `ClusterRepository`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryHealthSpec {
    /// Index-blob count above which the reconciler raises the `IndexBlobHealth` warning (`0` disables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_index_blob_warn_threshold")]
    pub index_blob_warn_threshold: Option<i64>,
    /// Opt-in periodic backend health probe: re-connect a `Ready` repository on a
    /// timer to confirm the kopia repository still exists at the backend.
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

/// Opt-in backend health probe, shared by `Repository` and `ClusterRepository`.
///
/// Once a repository reaches `Ready`, the operator trusts that pinned status and
/// — for object-store / volume-backed backends — never re-checks the backend on
/// its steady-state heartbeat. If the kopia repository is wiped or becomes
/// unreachable, nothing notices until a backup runs and fails. Enabling this
/// probe re-connects the backend every [`interval`](Self::interval) and surfaces
/// the result as a condition + Warning event (the repository **stays `Ready`** —
/// this is alert-only; it never auto-recreates and never pauses backups).
///
/// **Alert-only by design.** A wiped repository and a transient outage look alike,
/// and silently recreating an empty repository over a real one destroys
/// restorability — so the probe only *reports*. Acting on the alert (a deliberate
/// re-create) is a human decision.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryHealthProbeSpec {
    /// Turn the probe on. Off by default — existing repositories keep their
    /// current behavior until a user opts in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enabled: bool,
    /// How often to re-probe the backend (Go-style duration like `30m` or `1h`;
    /// minimum `30s`, default `30m`). Inert unless `enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_health_probe_interval")]
    pub interval: Option<String>,
    /// How many *consecutive* failing probes to require before raising the loud
    /// condition + event (default `3`). Debounces a single transient blip from
    /// alarming or nudging a destructive manual recreate. Any success resets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_health_probe_failure_threshold")]
    pub failure_threshold: Option<i64>,
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
    /// Whether the backend health probe is opted in (`spec.health.probe.enabled`).
    /// Off by default, so an existing `Ready` repository keeps its behavior.
    pub fn enabled(health: Option<&RepositoryHealthSpec>) -> bool {
        health
            .and_then(|h| h.probe.as_ref())
            .is_some_and(|p| p.enabled)
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
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum RepositoryPhase {
    /// Accepted by the API server but not yet reconciled.
    #[default]
    Pending,
    /// Connecting to (or creating) the kopia repository.
    Initializing,
    /// Connected and healthy.
    Ready,
    /// Reachable, but a sub-operation (e.g. maintenance) is failing; see conditions.
    Degraded,
    /// Connect/create failed; see conditions for the actionable reason.
    Failed,
}

impl crate::common::PhaseLabel for RepositoryPhase {
    const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Initializing,
        Self::Ready,
        Self::Degraded,
        Self::Failed,
    ];
    fn label(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Initializing => "Initializing",
            Self::Ready => "Ready",
            Self::Degraded => "Degraded",
            Self::Failed => "Failed",
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::RepositoryMode;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

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
            spec["properties"]["health"]["properties"]["probe"]["properties"]["interval"]["default"],
            serde_json::json!("30m")
        );
        assert_eq!(
            spec["properties"]["health"]["properties"]["probe"]["properties"]["failureThreshold"]["default"],
            serde_json::json!(3)
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
        // Absent spec / absent probe ⇒ disabled, defaults.
        assert!(!RepositoryHealthProbeSpec::enabled(None));
        assert_eq!(
            RepositoryHealthProbeSpec::effective_interval(None),
            DEFAULT_HEALTH_PROBE_INTERVAL
        );
        assert_eq!(
            RepositoryHealthProbeSpec::effective_failure_threshold(None),
            DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD
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
        // `enabled: false` is skip-serialized (no stored-object churn).
        let disabled: RepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s } }\n\
             health:\n  probe:\n    interval: 1h\n",
        );
        assert!(!RepositoryHealthProbeSpec::enabled(
            disabled.health.as_ref()
        ));
        let json = serde_json::to_value(&disabled).unwrap();
        assert!(
            json["health"]["probe"].get("enabled").is_none(),
            "enabled: false must be elided"
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
}
