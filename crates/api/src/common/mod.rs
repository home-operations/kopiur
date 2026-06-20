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

/// Reference to a key within a `Secret` in the same namespace as the referrer,
/// unless `namespace` is given (required for cluster-scoped CRs — ADR §3.2).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    /// Name of the `Secret`.
    pub name: String,
    /// Namespace of the `Secret`. Absent = same namespace as the referrer;
    /// required for cluster-scoped CRs which have no own namespace (ADR §3.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Which key inside the `Secret` to read. Defaults are documented per-field on
    /// the consuming struct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Reference to an entire `Secret` (the operator reads well-known keys from it,
/// e.g. `AWS_ACCESS_KEY_ID`). See ADR §3.1 backend `auth.secretRef`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    /// Name of the `Secret`.
    pub name: String,
    /// Namespace of the `Secret`. Absent = same namespace as the referrer;
    /// required for cluster-scoped CRs (ADR §3.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Reference to a key within a `ConfigMap` (e.g. a CA bundle). ADR §3.1 `tls.caBundleRef`.
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

/// TLS settings for object-store backends. ADR §3.1.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    /// CA bundle (PEM) used to verify the endpoint's certificate, sourced from a
    /// `ConfigMap`. Preferred over `insecureSkipVerify` for self-signed endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_bundle_ref: Option<ConfigMapKeyRef>,
    /// Skip TLS certificate verification (still uses TLS). Maps to kopia's
    /// `--disable-tls-verification`. For self-signed endpoints; prefer
    /// `caBundleRef` in production.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure_skip_verify: bool,
    /// Disable TLS entirely and talk plain HTTP. Maps to kopia's `--disable-tls`.
    /// Needed for HTTP-only endpoints (e.g. an in-cluster MinIO/RustFS service);
    /// kopia's S3 path otherwise assumes HTTPS. Off by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_tls: bool,
}

/// Which kind of repository a consumer CR references. ADR §3.2/§3.3.
///
/// This is a closed enum: a consumer's `repository.kind` is always exactly one
/// of these two values, so reconcilers `match` it exhaustively.
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

/// Discriminated reference from a consumer CR (`SnapshotPolicy`, `Snapshot`,
/// `Restore`, `Maintenance`) to a `Repository` or `ClusterRepository`. ADR §3.2.
///
/// When `kind == ClusterRepository`, `namespace` MUST be absent — enforced by the
/// admission webhook (`api::validate`), since the type system cannot express
/// "this field is forbidden only for one variant of a sibling field".
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

/// Repository encryption settings. A sub-object so future rotation fields
/// (`rotation`, `previousPasswords`) slot in without breakage (ADR §4.11).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Encryption {
    /// Always a Secret ref; never inline. ADR §3.1.
    pub password_secret_ref: SecretKeyRef,
}

/// Opt-in projection of a repository's credential `Secret`(s) into the namespace
/// where each mover Job runs. **Default off.** ADR §3.1/§4.11.
///
/// Kopiur's baseline contract — like VolSync and K8up — is that the credential
/// Secret already exists in the namespace where a mover runs (it loads creds via
/// namespace-local `envFrom`). For a shared `ClusterRepository` whose Secret is
/// pinned to one namespace, that means placing a copy in each consuming namespace.
/// When `enabled`, the operator does that for you: before each run it reads the
/// source Secret(s) and writes a kopiur-managed copy into the Job's namespace,
/// owned by the consuming CR (garbage-collected with it) and refreshed from source
/// every run. (Cross-namespace secret distribution is opt-in across the ecosystem;
/// keeping it off by default preserves the namespace-as-trust-boundary posture.)
///
/// Even when enabled, projection is a no-op where the source Secret already lives
/// in the Job's namespace (the common namespaced-`Repository` layout): there is
/// nothing to copy, so the operator just verifies it is present. It only actually
/// copies for the cross-namespace case (a shared `ClusterRepository`).
///
/// A sub-object (not a bare `bool`) so future knobs (key remapping, a copy-name
/// template, immutability) slot in without API breakage (ADR §4.11).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredentialProjection {
    /// When true, the operator copies the repository's credential Secret(s) into
    /// the namespace of each mover Job that uses this repository. Off by default.
    #[serde(default)]
    pub enabled: bool,
}

/// Behavior when the repository does not yet exist. ADR §3.1 `create`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateBehavior {
    /// Create the repository if it does not exist yet. Off by default, so a typo'd
    /// backend can't silently spin up a brand-new empty repository.
    #[serde(default)]
    pub enabled: bool,
    /// kopia encryption algorithm for a freshly-created repository (e.g.
    /// `AES256-GCM-HMAC-SHA256`); only consulted at creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<String>,
    /// kopia object splitter for a freshly-created repository; creation-time only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub splitter: Option<String>,
    /// kopia content hash algorithm for a freshly-created repository; creation-time only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Reed-Solomon ECC parity guarding repo blobs against backend bit-rot
    /// (`kopia repository create --ecc=... --ecc-overhead-percent=...`). Creation-time
    /// only and immutable post-create (ADR-0005 §13(a), gated by §7). ADR-0005 §13(a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecc: Option<Ecc>,
}

/// Reed-Solomon error-correcting-code parity for a freshly-created repository
/// (ADR-0005 §13(a)). Both fields creation-time-fixed; immutable post-create (§7).
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

/// GFS retention policy. The single successful-retention driver (ADR §4.4).
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

/// Identity overrides — what kopia records as `username@hostname:path`. ADR §3.3/§4.2.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// Override the `username` portion of `username@hostname:path`; absent uses the
    /// resolved default (the repository's `identityDefaults` CEL expression, or the
    /// object name). Used verbatim and pinned at admission (ADR §4.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Override the `hostname` portion of `username@hostname:path`; absent uses the
    /// resolved default (the repository's `identityDefaults` CEL expression, or the
    /// namespace). Used verbatim and pinned at admission (ADR §4.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// Byte cap for `status.logTail` (and the stderr tail inside
/// [`FailureBlock`]): the mover truncates to the LAST `MAX_LOG_TAIL_BYTES`
/// bytes before patching status, so a noisy kopia run can't bloat etcd. Full
/// logs live in the mover Job's pod. ADR §3.4/§4.10.
pub const MAX_LOG_TAIL_BYTES: usize = 4096;

/// A structured terminal-failure block written by the mover to `status.failure`
/// (ADR §4.10): the kopia error class, a human-readable message, the last
/// stderr lines, and a retry recommendation. Defined in `kopiur-api` (not the
/// mover) so the field names cannot drift from the CRD structural schema — a
/// mismatched name is silently pruned by the API server.
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

/// Fully-resolved identity pinned into status; never re-rendered after admission. ADR §4.2.
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

/// Per-run failure controls passed through to the mover `Job`. ADR §3.4/§4.10 (G6).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailurePolicy {
    /// Passed through to the mover `Job.spec.backoffLimit` — how many times a failed
    /// run is retried before the Job is marked failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_limit: Option<i32>,
    /// Passed through to the mover `Job.spec.activeDeadlineSeconds` — wall-clock cap
    /// after which a still-running run is killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_deadline_seconds: Option<i64>,
    /// How long (seconds) a mover **pod** may sit in a non-starting state — a container
    /// `CreateContainerConfigError` / `ImagePullBackOff` / `InvalidImageName`, or
    /// `Unschedulable` — before the controller fails the run with an actionable reason,
    /// rather than waiting out `active_deadline_seconds` (which can be many hours and
    /// is meant for *long-running*, not *wedged*, work). A wedged pod never reaches a
    /// terminal phase, so `backoffLimit` never trips — this is the only thing that bounds
    /// it. Unset uses the built-in [`DEFAULT_POD_STARTUP_DEADLINE_SECONDS`].
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

/// Reference to a `SnapshotPolicy` CR (used by `Snapshot.spec.policyRef` and
/// `SnapshotSchedule.spec.policyRef`). May cross namespaces, subject to RBAC. ADR §3.4/§3.5.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRef {
    /// Name of the referenced `SnapshotPolicy`.
    pub name: String,
    /// Namespace of the `SnapshotPolicy`; absent = same namespace as the referrer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Generic name/namespace reference to another namespaced object — e.g. a `Snapshot`
/// CR (`Restore.spec.source.snapshotRef`) or a PVC (`Restore.spec.target.pvcRef`). ADR §3.6.
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
/// Shared by `SnapshotPolicy.spec.defaultDeletionPolicy` and `Snapshot.spec.deletionPolicy`.
/// ADR-0003 §4.5 / ADR-0001 §4.5.
///
/// The reconciler distinguishes the three cases with an exhaustive `match` — Rust
/// enforces that any new variant added later must be handled in every match site,
/// preventing the class of bug where a new policy slips into production without a
/// corresponding reconcile branch.
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
    /// Default for `origin: scheduled`/`manual`. Finalizer runs
    /// `kopia snapshot delete <id>` then removes the finalizer.
    #[default]
    Delete,
    /// Default for `origin: discovered`. CR is removed; snapshot stays.
    /// Forced via webhook for discovered snapshots; cannot be overridden.
    Retain,
    /// CR is removed without contacting the repository at all (escape hatch
    /// for "the bucket is gone, just let me delete the CR"). Status records
    /// `orphaned: true` for the snapshot ID before removal.
    Orphan,
}

/// What happens to a repository's snapshots when a consuming **namespace** is
/// deleted. Closed enum, default `Orphan` (fail-safe). ADR-0005 §5.
///
/// A `kubectl delete ns` must not silently destroy off-site backup history (and
/// must not hang the namespace teardown on N `kopia snapshot delete` calls). So the
/// repository owner opts *in* to cascade-delete; the default releases ownership
/// (removes the finalizer) without touching the snapshots. This is distinct from a
/// single `kubectl delete snapshot`, which still honors that `Snapshot`'s own
/// `deletionPolicy`.
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
    /// Release ownership (remove the `Snapshot` finalizers) without deleting the
    /// underlying kopia snapshots. The fail-safe default — `kubectl delete ns` keeps
    /// history.
    #[default]
    Orphan,
    /// Cascade: when a namespace is deleted, the per-`Snapshot` `deletionPolicy`
    /// applies (so produced snapshots are `kopia snapshot delete`d). Opt-in only.
    Delete,
}

/// Repository access mode (ADR-0005 §11). A `ReadOnly` repository serves restores
/// only — no backups, no maintenance — for decommissioning a backend or migrating
/// between repositories without risking writes. Maps to kopia's read-only
/// connection. Closed enum, default `ReadWrite`.
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
    /// Read-only: restores only. Backup Jobs and maintenance are refused. §11.
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

/// A single cron entry with optional deterministic jitter. Shared by `Maintenance`'s
/// quick/full schedules. ADR §3.7. `jitter` is a Go-style duration string (e.g. `30m`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CronSpec {
    /// The cron expression, parsed by `croner`. May contain an `H` placeholder for
    /// deterministic per-schedule jitter (ADR §3.7).
    pub cron: String,
    /// Optional deterministic jitter window as a Go-style duration string (e.g.
    /// `30m`), derived from `(scheduleUID, slot)` so it is stable across restarts.
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
