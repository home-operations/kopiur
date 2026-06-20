//! Shared sub-objects reused across multiple CRDs.
//!
//! Per ADR-0003 §2.2 (principle 10) and §4.11, every credential, policy, and
//! identity surface is modeled as a sub-object so future fields slot in without
//! API breakage. Leaf Kubernetes types (`LabelSelector`, `ResourceRequirements`,
//! `PodSecurityContext`) are reused from `k8s-openapi` rather than re-invented.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod cache;
mod mover;
mod secctx;

pub use cache::*;
pub use mover::*;
pub use secctx::*;

/// serde `default` for a `bool` field whose absent value is `true`. Used by
/// "enabled by default, opt out explicitly" surfaces (e.g.
/// `RepositoryMaintenanceSpec.enabled`). `bool::default()` is `false`, so a
/// default-true field cannot lean on `#[serde(default)]` alone.
pub(crate) fn default_true() -> bool {
    true
}

/// A lifecycle-phase enum that can be rendered as a metric label.
///
/// The single source of truth for a CRD's phase labels: [`PhaseLabel::ALL`]
/// enumerates every variant and [`PhaseLabel::label`] is an exhaustive match.
/// The controller's `kopiur_resource_phase` gauge uses these to set the active
/// phase to 1 and the rest to 0 (and to clear all on deletion), so both the
/// label string and the reset set come from the enum itself rather than a
/// stringly-typed table that can silently drift (ADR §5.5 type-safety thesis).
pub trait PhaseLabel: Copy + PartialEq + 'static {
    /// Every variant, in declaration order.
    const ALL: &'static [Self];
    /// The stable metric label string for this variant (exhaustive `match`).
    fn label(&self) -> &'static str;
}

/// Reference to a key within a `Secret`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    /// Name of the `Secret`.
    pub name: String,
    /// Namespace of the `Secret`; absent = same namespace as the referrer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Which key inside the `Secret` to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Reference to an entire `Secret` (the operator reads well-known keys from it).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    /// Name of the `Secret`.
    pub name: String,
    /// Namespace of the `Secret`; absent = same namespace as the referrer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Reference to a key within a `ConfigMap` (e.g. a CA bundle).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapKeyRef {
    /// Name of the `ConfigMap` holding the value (e.g. a CA bundle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_name: Option<String>,
    /// Which key inside the `ConfigMap` to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// TLS settings for object-store backends.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    /// CA bundle (PEM) used to verify the endpoint's certificate, sourced from a `ConfigMap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_bundle_ref: Option<ConfigMapKeyRef>,
    /// Skip TLS certificate verification (still uses TLS); maps to kopia's `--disable-tls-verification`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure_skip_verify: bool,
    /// Disable TLS entirely and talk plain HTTP; maps to kopia's `--disable-tls`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_tls: bool,
}

/// Which kind of repository a consumer CR references (`Repository` or `ClusterRepository`).
///
/// ```
/// use kopiur_api::common::RepositoryKind;
///
/// // Defaults to the namespaced `Repository`, so a same-namespace ref needs no `kind`.
/// assert_eq!(RepositoryKind::default(), RepositoryKind::Repository);
/// // Serializes to the bare CRD kind name (no payload — a plain string).
/// assert_eq!(
///     serde_json::to_value(RepositoryKind::ClusterRepository).unwrap(),
///     "ClusterRepository"
/// );
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum RepositoryKind {
    /// The namespaced `Repository` CRD; the default when `kind` is omitted.
    #[default]
    Repository,
    /// The cluster-scoped `ClusterRepository` CRD; namespace is meaningless for it.
    ClusterRepository,
}

/// Reference from a consumer CR to a `Repository` or `ClusterRepository`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryRef {
    /// Which repository CRD this points at; defaults to [`RepositoryKind::Repository`].
    #[serde(default)]
    pub kind: RepositoryKind,
    /// Name of the referenced `Repository`/`ClusterRepository`.
    pub name: String,
    /// Cross-namespace `Repository` reference; ignored/forbidden for `ClusterRepository`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Repository encryption settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Encryption {
    /// Repository password, always a Secret reference (never inline).
    pub password_secret_ref: SecretKeyRef,
}

/// Opt-in projection of a repository's credential `Secret`(s) into each mover Job's namespace.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredentialProjection {
    /// Copy the repository's credential Secret(s) into the namespace of each mover Job; off by default.
    #[serde(default)]
    pub enabled: bool,
}

/// Behavior when the repository does not yet exist.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateBehavior {
    /// Create the repository if it does not exist yet; off by default.
    #[serde(default)]
    pub enabled: bool,
    /// kopia encryption algorithm for a freshly-created repository (creation-time only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<String>,
    /// kopia object splitter for a freshly-created repository (creation-time only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub splitter: Option<String>,
    /// kopia content hash algorithm for a freshly-created repository (creation-time only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Reed-Solomon ECC parity for a freshly-created repository (creation-time only, immutable after).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecc: Option<Ecc>,
}

/// Reed-Solomon error-correcting-code parity for a freshly-created repository.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Ecc {
    /// ECC algorithm, e.g. `REED-SOLOMON-CRC32` (`--ecc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    /// Parity overhead as a percentage (`--ecc-overhead-percent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhead_percent: Option<i64>,
}

/// GFS retention policy — how many snapshots to keep per time bucket.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    /// Keep the N most-recent snapshots regardless of age.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_latest: Option<u32>,
    /// Keep one snapshot per hour for the most-recent N hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_hourly: Option<u32>,
    /// Keep one snapshot per day for the most-recent N days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_daily: Option<u32>,
    /// Keep one snapshot per week for the most-recent N weeks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_weekly: Option<u32>,
    /// Keep one snapshot per month for the most-recent N months.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_monthly: Option<u32>,
    /// Keep one snapshot per year for the most-recent N years.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_annual: Option<u32>,
}

/// Identity overrides — what kopia records as `username@hostname:path`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// Override the `username` portion of `username@hostname:path`; absent uses the resolved default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Override the `hostname` portion of `username@hostname:path`; absent uses the resolved default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// Byte cap for `status.logTail` (and the stderr tail inside
/// [`FailureBlock`]): the mover truncates to the LAST `MAX_LOG_TAIL_BYTES`
/// bytes before patching status, so a noisy kopia run can't bloat etcd. Full
/// logs live in the mover Job's pod. ADR §3.4/§4.10.
pub const MAX_LOG_TAIL_BYTES: usize = 4096;

/// A structured terminal-failure block written by the mover to `status.failure`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailureBlock {
    /// kopia error class (e.g. `RepositoryUnavailable`, `AuthFailure`).
    pub kopia_error_class: String,
    /// A short human-readable message: what failed, why, and how to fix it.
    pub message: String,
    /// The last lines of kopia's stderr, if any were captured (bounded by
    /// [`MAX_LOG_TAIL_BYTES`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    /// The process exit code, if one was reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether retrying the same operation unchanged could succeed.
    pub retry_recommended: bool,
}

/// Fully-resolved identity pinned into status; never re-rendered after admission.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedIdentity {
    /// The final `username` kopia records, fixed at admission.
    pub username: String,
    /// The final `hostname` kopia records, fixed at admission.
    pub hostname: String,
    /// The resolved snapshot source path, when applicable (`username@hostname:path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// Per-run failure controls passed through to the mover `Job`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailurePolicy {
    /// Mover `Job.spec.backoffLimit` — retries before a failed run is marked failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_limit: Option<i32>,
    /// Mover `Job.spec.activeDeadlineSeconds` — wall-clock cap after which a running run is killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_deadline_seconds: Option<i64>,
    /// Seconds a non-starting (wedged) mover pod may sit before the run is failed; default 300s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_startup_deadline_seconds: Option<i64>,
}

/// Default grace before a non-starting (wedged) mover pod fails its run — 5 minutes.
/// Long enough to absorb a slow image pull or a brief `Unschedulable` while an RWO volume
/// detaches from another node, short enough that a genuinely-broken pod (e.g. an impossible
/// securityContext, a missing image) surfaces as `Failed` fast instead of hanging for hours.
pub const DEFAULT_POD_STARTUP_DEADLINE_SECONDS: i64 = 300;

/// The effective pod-startup deadline (seconds) for a mover Job: the recipe's
/// `failurePolicy.podStartupDeadlineSeconds`, or [`DEFAULT_POD_STARTUP_DEADLINE_SECONDS`]
/// when unset. Shared by **every** reconciler that fast-fails a wedged mover (Snapshot,
/// Restore, Maintenance) so the same default is applied identically on all three.
pub fn pod_startup_deadline_seconds(failure_policy: Option<&FailurePolicy>) -> i64 {
    failure_policy
        .and_then(|fp| fp.pod_startup_deadline_seconds)
        .unwrap_or(DEFAULT_POD_STARTUP_DEADLINE_SECONDS)
}

/// Reference to a `SnapshotPolicy` CR.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRef {
    /// Name of the referenced `SnapshotPolicy`.
    pub name: String,
    /// Namespace of the `SnapshotPolicy`; absent = same namespace as the referrer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Generic name/namespace reference to another namespaced object (e.g. a `Snapshot` CR or PVC).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectRef {
    /// Name of the referenced object.
    pub name: String,
    /// Namespace of the referenced object; absent = same namespace as the referrer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Lifecycle of the underlying kopia snapshot when its `Snapshot` CR is deleted.
///
/// ```
/// use kopiur_api::common::DeletionPolicy;
///
/// // Produced backups default to deleting the snapshot with the CR.
/// assert_eq!(DeletionPolicy::default(), DeletionPolicy::Delete);
/// // Variants serialize to their bare PascalCase names (plain string enum).
/// assert_eq!(serde_json::to_value(DeletionPolicy::Retain).unwrap(), "Retain");
/// assert_eq!(serde_json::to_value(DeletionPolicy::Orphan).unwrap(), "Orphan");
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum DeletionPolicy {
    /// Finalizer runs `kopia snapshot delete <id>` then removes the finalizer; default for produced snapshots.
    #[default]
    Delete,
    /// CR is removed; the kopia snapshot stays. Forced for discovered snapshots.
    Retain,
    /// CR is removed without contacting the repository at all (escape hatch).
    Orphan,
}

/// What happens to a repository's snapshots when a consuming **namespace** is deleted; default `Orphan`.
///
/// ```
/// use kopiur_api::common::NamespaceDeletePolicy;
///
/// // Fail-safe: a deleted namespace orphans (keeps) snapshots by default.
/// assert_eq!(NamespaceDeletePolicy::default(), NamespaceDeletePolicy::Orphan);
/// // Bare PascalCase strings (plain unit enum).
/// assert_eq!(serde_json::to_value(NamespaceDeletePolicy::Delete).unwrap(), "Delete");
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum NamespaceDeletePolicy {
    /// Release ownership without deleting the kopia snapshots; the fail-safe default.
    #[default]
    Orphan,
    /// Cascade: each `Snapshot`'s own `deletionPolicy` applies when the namespace is deleted.
    Delete,
}

/// Repository access mode; `ReadOnly` serves restores only (no backups, no maintenance).
///
/// ```
/// use kopiur_api::common::RepositoryMode;
///
/// assert_eq!(RepositoryMode::default(), RepositoryMode::ReadWrite);
/// assert_eq!(serde_json::to_value(RepositoryMode::ReadOnly).unwrap(), "ReadOnly");
/// // ReadOnly forbids writes (backups + maintenance); restores are allowed.
/// assert!(!RepositoryMode::ReadOnly.allows_writes());
/// assert!(RepositoryMode::ReadWrite.allows_writes());
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum RepositoryMode {
    /// Normal read-write repository (default): backups, restores, maintenance.
    #[default]
    ReadWrite,
    /// Read-only: restores only. Backup Jobs and maintenance are refused.
    ReadOnly,
}

impl RepositoryMode {
    /// Whether this mode permits write operations (backup Jobs + maintenance).
    /// Pure + exhaustive so the single definition lives in one tested place.
    pub fn allows_writes(&self) -> bool {
        match self {
            RepositoryMode::ReadWrite => true,
            RepositoryMode::ReadOnly => false,
        }
    }
}

/// serde/schemars `default` for the repository `mode` field — `ReadWrite`
/// (ADR-0005 §11). Named fn so it backs BOTH serde + schemars defaults.
pub(crate) fn default_repository_mode() -> RepositoryMode {
    RepositoryMode::ReadWrite
}

/// serde/schemars `default` for the repository `on_namespace_delete` field —
/// `Orphan` (ADR-0005 §5). A named fn so it backs BOTH `#[serde(default = ...)]`
/// and `#[schemars(default = ...)]`, emitting a real OpenAPI `default:`.
pub(crate) fn default_namespace_delete_policy() -> NamespaceDeletePolicy {
    NamespaceDeletePolicy::Orphan
}

/// A single cron entry with optional deterministic jitter.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CronSpec {
    /// The cron expression, parsed by `croner`; may contain an `H` placeholder for deterministic jitter.
    pub cron: String,
    /// Optional deterministic jitter window as a Go-style duration string (e.g. `30m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter: Option<String>,
}

impl RepositoryRef {
    /// True if this reference points at the given repository.
    ///
    /// `owner_namespace` is the namespace of the resource that holds the ref
    /// (e.g. the `Maintenance` CR's own namespace), used to resolve a namespaced
    /// `Repository` reference that omits `namespace`. The match is exhaustive over
    /// [`RepositoryKind`] (ADR §5.5):
    ///
    /// - [`RepositoryKind::Repository`]: kind+name must match AND the effective
    ///   namespace (`self.namespace` or `owner_namespace`) must equal
    ///   `target_namespace`.
    /// - [`RepositoryKind::ClusterRepository`]: kind+name must match; namespace is
    ///   ignored on both sides (cluster-scoped).
    ///
    /// `target_namespace` is `None` for a `ClusterRepository` target.
    ///
    /// ```
    /// use kopiur_api::common::{RepositoryKind, RepositoryRef};
    ///
    /// // A namespaced ref that omits `namespace` resolves against the owner's namespace.
    /// let r = RepositoryRef { kind: RepositoryKind::Repository, name: "nas".into(), namespace: None };
    /// assert!(r.resolves_to("apps", RepositoryKind::Repository, "nas", Some("apps")));
    /// assert!(!r.resolves_to("apps", RepositoryKind::Repository, "nas", Some("other")));
    ///
    /// // A cluster-scoped target ignores namespace entirely.
    /// let cr = RepositoryRef {
    ///     kind: RepositoryKind::ClusterRepository,
    ///     name: "hetzner".into(),
    ///     namespace: None,
    /// };
    /// assert!(cr.resolves_to("apps", RepositoryKind::ClusterRepository, "hetzner", None));
    /// // Kind must match even when names collide.
    /// assert!(!r.resolves_to("apps", RepositoryKind::ClusterRepository, "nas", None));
    /// ```
    pub fn resolves_to(
        &self,
        owner_namespace: &str,
        target_kind: RepositoryKind,
        target_name: &str,
        target_namespace: Option<&str>,
    ) -> bool {
        if self.kind != target_kind || self.name != target_name {
            return false;
        }
        match self.kind {
            RepositoryKind::Repository => {
                Some(self.namespace.as_deref().unwrap_or(owner_namespace)) == target_namespace
            }
            RepositoryKind::ClusterRepository => true,
        }
    }
}

#[cfg(test)]
mod tests;
