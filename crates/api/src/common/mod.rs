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
/// enumerates every **canonical** variant and [`PhaseLabel::label`] is an
/// exhaustive match. The controller's `kopiur_resource_phase` gauge uses these
/// to set the active phase to 1 and the rest to 0 (and to clear all on
/// deletion), so both the label string and the reset set come from the enum
/// itself rather than a stringly-typed table that can silently drift
/// (ADR §5.5 type-safety thesis).
///
/// Every phase enum also carries an `Unknown(String)` decode-compat variant
/// (see the crate-internal `phase_serde!` macro below). It is deliberately **absent**
/// from [`PhaseLabel::ALL`]: `ALL` is the CRD schema's admissible set and the
/// metric label domain, whereas `Unknown` only ever comes back off the wire
/// from a newer operator's write. `label()` therefore returns `&str`, not
/// `&'static str` — `Unknown` echoes the stored string verbatim.
pub trait PhaseLabel: Clone + PartialEq + 'static {
    /// Every canonical variant, in declaration order. Never contains `Unknown`.
    const ALL: &'static [Self];

    /// The stable metric/wire label string for this variant (exhaustive
    /// `match`); the `Unknown` arm echoes the stored value verbatim so a
    /// read-modify-write never mutates a phase this build does not understand.
    fn label(&self) -> &str;

    /// Build the decode-compat fallback for a non-canonical stored string.
    /// Implemented by the `phase_serde!` macro's host enum.
    fn unknown(raw: String) -> Self;

    /// Parse a **canonical** label; `None` for anything else (including a value
    /// that would decode to `Unknown`). Derived from `ALL` + `label()` so a new
    /// variant is parseable the moment it is declared.
    fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|v| v.label() == s).cloned()
    }

    /// The canonical label set, for the CRD schema `enum` and "valid values"
    /// messages. One definition, derived from `ALL`.
    fn canonical() -> Vec<&'static str> {
        Self::ALL.iter().map(PhaseLabel::label).collect()
    }
}

/// Give a phase enum the `Unknown`-tolerant wire contract: `Serialize` echoes
/// [`PhaseLabel::label`], `Deserialize` falls back to `Unknown(raw)` instead of
/// erroring, and `JsonSchema` publishes **only** the canonical values.
///
/// Why the fallback exists (the graceful-decode convention, mirroring
/// [`PvcAccessMode`]): a phase string this build does not know — written by a
/// newer operator during a rolling upgrade, or by a future controller into a CR
/// an older CLI then lists — must never fail the typed watch/list for the whole
/// Kind. One un-decodable object would otherwise wedge every other object's
/// reconciliation (and, for `kubectl kopiur doctor`, turn a real problem into a
/// silent green).
///
/// # What consumers do with `Unknown` — a deliberate three-way split
///
/// `Unknown` is never terminal, never schedulable, never reapable, and never a
/// success. What follows from that is NOT uniform, and the differences are the
/// point rather than an oversight:
///
/// 1. **Read-only classifications HOLD.** Anything deciding "is this finished /
///    reapable / retention-eligible / a success" answers *no*, and stays out of
///    every set whose members get deleted or whose absence silences an alert.
///    The CLI's `--wait` paths keep waiting rather than exiting 0 or 1 on an
///    outcome they cannot substantiate.
/// 2. **Reconcilers whose re-drive is IDEMPOTENT self-heal by overwriting.**
///    `Restore` (its source is pinned in `status.resolved` and never
///    re-resolved, so re-driving restores the same snapshot to the same
///    target), `Repository`/`ClusterRepository` (connect is idempotent), and a
///    `Maintenance` manual run (its Job name is keyed on the request timestamp)
///    all re-derive the phase from observed state and write it, replacing the
///    value they could not read. Parking instead would strand the object with
///    no way out — remember the fallback also catches legacy stored values that
///    no future upgrade will ever explain. Each names the phase first via the
///    controller's `io::warn_unreadable_phase`: deliberate, never silent.
/// 3. **Reconcilers whose re-drive would DUPLICATE irreversible work hold.**
///    `Snapshot` is the one: a `Snapshot` IS its run, and re-driving one mints a
///    second mover Job and a second kopia snapshot. That is the exact hazard the
///    one-shot discipline exists for, so the reconciler holds (log + slow
///    requeue) instead. Idempotence, not read-vs-write, is what separates (2)
///    from (3).
///
/// A fourth case is created by (3) composing with a fail-closed gate: a
/// `SnapshotSchedule` whose concurrency gate is held by an `Unknown`-phase run
/// stops firing permanently under `concurrencyPolicy: Forbid`, and neither the
/// schedule nor the run looks unhealthy. That one is not resolved by weakening
/// the hold (the hold is right) but by SURFACING it: a registered structural
/// gate ([`crate::gates::STRUCTURAL_GATES`], `ScheduleRunnable=False`) plus a
/// Warning Event — a condition rather than a log, because unlike a per-pass
/// warning it has a real transition to record and a diagnostic to feed.
///
/// # `$desc`
///
/// The CRD-schema `description`. It is spelled out here rather than taken from
/// the doc comment because a manual `JsonSchema` impl cannot see doc comments;
/// keep it byte-identical to what the derive used to emit or `mise run
/// gen-check` will (correctly) fail. It therefore deliberately DIVERGES from
/// the enum's rustdoc: the rustdoc is free to explain `Unknown` to Rust
/// readers, while `$desc` must stay frozen at the pre-`Unknown` wording,
/// because changing it would rewrite the published CRD for no behavioral
/// reason. Treat `$desc` as a schema artifact, not documentation.
///
/// Crate-internal on purpose (`pub(crate) use` below, not `#[macro_export]`):
/// it expands to impls of THIS crate's traits for THIS crate's enums and would
/// commit `kopiur-api` to a public macro contract nothing outside needs.
macro_rules! phase_serde {
    ($ty:ty, $desc:literal) => {
        impl ::serde::Serialize for $ty {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str($crate::common::PhaseLabel::label(self))
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $ty {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                Ok(<Self as $crate::common::PhaseLabel>::parse(&s)
                    .unwrap_or_else(|| <Self as $crate::common::PhaseLabel>::unknown(s)))
            }
        }

        impl ::schemars::JsonSchema for $ty {
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                stringify!($ty).into()
            }
            fn json_schema(_: &mut ::schemars::SchemaGenerator) -> ::schemars::Schema {
                // Canonical values only: `Unknown` is a decode-compat artifact,
                // never an admissible write. The apiserver keeps rejecting
                // anything outside this set.
                //
                // Shape matters: this is a `oneOf` of `const`s, NOT a flat
                // `enum`, because that is exactly what `#[derive(JsonSchema)]`
                // emits for a documented unit-only enum — and the two are not
                // interchangeable downstream. `Option<Self>` runs schemars'
                // `allow_null`, which appends a literal `null` to a flat `enum`
                // but wraps a `oneOf` in `anyOf[.., null]`; kube's CRD rewriter
                // then folds that back into `enum: [..] + nullable: true` with
                // no bogus `null` member. Flattening this to `"enum"` silently
                // changes every generated CRD (`mise run gen-check` catches it).
                ::schemars::json_schema!({
                    "description": $desc,
                    "oneOf": <Self as $crate::common::PhaseLabel>::canonical()
                        .into_iter()
                        .map(|v| ::serde_json::json!({ "type": "string", "const": v }))
                        .collect::<Vec<_>>(),
                })
            }
        }
    };
}

pub(crate) use phase_serde;

/// Reference to a key within a `Secret`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    /// Name of the `Secret`.
    pub name: String,
    /// Namespace of the `Secret`; absent = same namespace as the referrer. A
    /// `ClusterRepository` is cluster-scoped and has no namespace of its own, so when IT
    /// reads the `Secret` (to connect, to bootstrap, or to run its repository server) an
    /// absent namespace means the operator's namespace (`KOPIUR_NAMESPACE`). A workload
    /// mover (Snapshot/Restore/Maintenance) still needs the `Secret` in its OWN namespace —
    /// `envFrom` is namespace-local — so put it there, or use `credentialProjection`, which
    /// needs this namespace set EXPLICITLY to know what to copy. Set it whenever anything
    /// other than the operator itself reads the `Secret`.
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
    /// Namespace of the `Secret`; absent = same namespace as the referrer. A
    /// `ClusterRepository` is cluster-scoped and has no namespace of its own, so when IT
    /// reads the `Secret` (to connect, to bootstrap, or to run its repository server) an
    /// absent namespace means the operator's namespace (`KOPIUR_NAMESPACE`). A workload
    /// mover (Snapshot/Restore/Maintenance) still needs the `Secret` in its OWN namespace —
    /// `envFrom` is namespace-local — so put it there, or use `credentialProjection`, which
    /// needs this namespace set EXPLICITLY to know what to copy. Set it whenever anything
    /// other than the operator itself reads the `Secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Reference to a key within a `ConfigMap` (e.g. a CA bundle). The ref carries no
/// `namespace` field: it always resolves in the referrer's namespace for a namespaced
/// `Repository`. A `ClusterRepository` is cluster-scoped and has no namespace of its own,
/// so for it the ref resolves in the operator's namespace (`KOPIUR_NAMESPACE`) — put the
/// `ConfigMap` there.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapKeyRef {
    /// Name of the `ConfigMap` holding the value (e.g. a CA bundle). Resolved in the
    /// referrer's namespace for a namespaced `Repository`, and in the operator's
    /// namespace (`KOPIUR_NAMESPACE`) for a `ClusterRepository` (cluster-scoped, no
    /// namespace of its own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_name: Option<String>,
    /// Which key inside the `ConfigMap` to read; defaults to `ca.crt` when unset.
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

/// A normalized, comparable repository key for a consumer's [`RepositoryRef`]
/// resolved from `owner_namespace` (the consuming CR's namespace). Two
/// references are "the same repository" only when their keys match. Pure +
/// exhaustive over [`RepositoryKind`]. Hoisted from the webhook's
/// identity-collision module so the validator, the webhook, and child-naming
/// all normalize identically (the webhook re-exports this).
///
/// - `Repository` → `"Repository/<effective-ns>/<name>"` (effective-ns is
///   `ref.namespace` or the owner's namespace).
/// - `ClusterRepository` → `"ClusterRepository/<name>"` (namespace-free).
///
/// ```
/// use kopiur_api::common::{RepositoryKind, RepositoryRef, repo_key};
///
/// let r = RepositoryRef { kind: RepositoryKind::Repository, name: "nas".into(), namespace: None };
/// assert_eq!(repo_key(&r, "backups"), "Repository/backups/nas");
/// let c = RepositoryRef { kind: RepositoryKind::ClusterRepository, name: "shared".into(), namespace: None };
/// assert_eq!(repo_key(&c, "backups"), "ClusterRepository/shared");
/// ```
pub fn repo_key(repo: &RepositoryRef, owner_namespace: &str) -> String {
    match repo.kind {
        RepositoryKind::Repository => {
            let ns = repo.namespace.as_deref().unwrap_or(owner_namespace);
            format!("Repository/{ns}/{}", repo.name)
        }
        RepositoryKind::ClusterRepository => format!("ClusterRepository/{}", repo.name),
    }
}

/// Normalize a [`RepositoryRef`] against the namespace it resolves relative to
/// (`owner_namespace`, the consuming CR's namespace): a namespaced `Repository`
/// ref carries its EFFECTIVE namespace explicitly (so a later reader can
/// re-resolve it from anywhere — e.g. after the owning recipe is gone); a
/// cluster-scoped `ClusterRepository` ref carries none (the webhook forbids
/// one). This is the one normal form every pin uses — `status.resolved.repository`
/// (the controller's run-time pin) and `Snapshot.spec.repository` (the
/// mint-time pin a multi-repo fan-out child or replication copy CR carries) —
/// so [`repo_key`] over a normalized ref is namespace-independent. Exhaustive
/// over [`RepositoryKind`].
pub fn normalized_repository_ref(r: &RepositoryRef, owner_namespace: &str) -> RepositoryRef {
    match r.kind {
        RepositoryKind::Repository => RepositoryRef {
            kind: RepositoryKind::Repository,
            name: r.name.clone(),
            namespace: Some(
                r.namespace
                    .clone()
                    .unwrap_or_else(|| owner_namespace.to_string()),
            ),
        },
        RepositoryKind::ClusterRepository => RepositoryRef {
            kind: RepositoryKind::ClusterRepository,
            name: r.name.clone(),
            namespace: None,
        },
    }
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
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateBehavior {
    /// Create the repository if it does not exist yet — on by default. Repository
    /// create/connect is idempotent: pointing `create` at an already-initialized
    /// repository just connects to it (it never re-creates or clobbers), so the
    /// only effect of the default is that a genuinely-absent repository is
    /// bootstrapped instead of erroring. Set `false` for a strictly read-only or
    /// externally-managed repository the operator must never create.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
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

impl Default for CreateBehavior {
    /// Mirrors the serde/schema default: create-on-first-use is on.
    fn default() -> Self {
        CreateBehavior {
            enabled: true,
            encryption: None,
            splitter: None,
            hash: None,
            ecc: None,
        }
    }
}

/// Whether the operator should create the repository when it does not yet exist.
///
/// Pure resolver shared by the controller and tests so the "absent means create"
/// default cannot fork: an absent `spec.create` resolves to `true` (create on
/// first use), and an explicit `create.enabled` is honored as written. Repository
/// create/connect is idempotent, so create-on is the least-surprise default; set
/// `create.enabled: false` to opt out.
///
/// ```
/// use kopiur_api::common::{create_enabled, CreateBehavior};
///
/// assert!(create_enabled(None)); // absent → create on
/// assert!(create_enabled(Some(&CreateBehavior::default())));
/// let off = CreateBehavior { enabled: false, ..CreateBehavior::default() };
/// assert!(!create_enabled(Some(&off)));
/// ```
pub fn create_enabled(create: Option<&CreateBehavior>) -> bool {
    create.map(|c| c.enabled).unwrap_or(true)
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
    /// The mover operation that failed, as a stable label (e.g.
    /// `repository connect`, `snapshot create`) — the values of the mover's
    /// `KopiaOp::as_str()`. Distinguishes a repository-level connect failure
    /// from a source-level failure (a broken PVC), which share
    /// `kopiaErrorClass` values (e.g. `NotFound`). Absent on failures that
    /// occurred outside a kopia invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
}

/// CEL expressions evaluated at admission to derive consumer identity when a
/// `SnapshotPolicy` doesn't override. Shared by `Repository` and
/// `ClusterRepository` (M5 gave the namespaced kind the same surface the
/// cluster-scoped kind has had since M1) — both mean the same thing: this
/// repository's backend is (or may be) shared, so its consumers need a
/// hostname/username recipe beyond the bare per-namespace default.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdentityDefaults {
    /// This cluster's identity suffix for repositories shared across clusters
    /// (an RFC 1123 label, at most 32 characters; dots are rejected — the first
    /// `.` in a hostname is the namespace/cluster delimiter). When set, the
    /// default kopia identity hostname becomes `<namespace>.<cluster>` instead
    /// of `<namespace>`, so two clusters backing up same-named namespaces write
    /// distinct identities (and one cluster's retention prune can no longer
    /// touch the other's snapshots). Also exposed to `hostnameExpr`/
    /// `usernameExpr` as the CEL variable `cluster`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    /// CEL expression for the kopia identity hostname (e.g. `"namespace"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname_expr: Option<String>,
    /// CEL expression for the kopia identity username (e.g. `"namespace + '-' + policyName"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_expr: Option<String>,
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

/// A PersistentVolumeClaim access mode as a closed set — `ReadWriteOnce`,
/// `ReadOnlyMany`, `ReadWriteMany`, `ReadWriteOncePod` — so a typo is rejected by
/// the CRD schema itself instead of surfacing as a provisioner error at the first
/// backup or restore run.
///
/// Deliberately **no `Default`** (unlike other unit enums here): everywhere this
/// type appears, an absent/empty list means "inherit from context" — the source
/// PVC's modes for a staged PVC, `ReadWriteOnce` for a restore-created PVC — so
/// there is no context-free default value to name.
///
/// The extra [`PvcAccessMode::Unknown`] variant exists ONLY so values persisted
/// before this field was schema-enforced still **deserialize** instead of erroring
/// the typed watch stream for the whole Kind (one legacy CR must never wedge every
/// other CR's reconciliation). It is hidden from the CRD schema — the apiserver
/// rejects non-canonical strings on every new write — and
/// [`crate::validate::validate_access_modes`] rejects it loudly per-CR with the
/// offending value quoted.
///
/// ```
/// use kopiur_api::common::PvcAccessMode;
///
/// // Canonical values round-trip as bare k8s strings.
/// assert_eq!(serde_json::to_value(PvcAccessMode::ReadOnlyMany).unwrap(), "ReadOnlyMany");
/// let m: PvcAccessMode = serde_json::from_value(serde_json::json!("ReadWriteOnce")).unwrap();
/// assert_eq!(m, PvcAccessMode::ReadWriteOnce);
///
/// // A legacy/bogus stored string decodes (never a watcher-poisoning error) into
/// // `Unknown`, preserving the value verbatim for the rejection message — and it
/// // re-serializes to the same string, so a read-modify-write never mutates it.
/// let m: PvcAccessMode = serde_json::from_value(serde_json::json!("ReadWriteOnze")).unwrap();
/// assert_eq!(m, PvcAccessMode::Unknown("ReadWriteOnze".into()));
/// assert_eq!(serde_json::to_value(&m).unwrap(), "ReadWriteOnze");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PvcAccessMode {
    /// Mounted read-write by a single node (`RWO`).
    ReadWriteOnce,
    /// Mounted read-only by many nodes (`ROX`) — e.g. a CephFS `backingSnapshot`
    /// shallow-clone staged PVC.
    ReadOnlyMany,
    /// Mounted read-write by many nodes (`RWX`).
    ReadWriteMany,
    /// Mounted read-write by a single **pod** (`RWOP`); the apiserver requires it
    /// to be the sole mode on a PVC.
    ReadWriteOncePod,
    /// A non-canonical stored value (pre-schema-enforcement legacy data). Never
    /// admissible on a new write (not in the CRD schema); consumers reject it via
    /// [`crate::validate::validate_access_modes`] with the value quoted, instead
    /// of a serde error that would poison the typed watcher.
    Unknown(String),
}

impl PvcAccessMode {
    /// The four canonical Kubernetes access-mode strings — the CRD schema `enum`
    /// and the "valid values" list in rejection messages, from one source.
    pub const CANONICAL: [&'static str; 4] = [
        "ReadWriteOnce",
        "ReadOnlyMany",
        "ReadWriteMany",
        "ReadWriteOncePod",
    ];

    /// The k8s wire string (exhaustive; `Unknown` echoes the stored value verbatim).
    pub fn mode_str(&self) -> &str {
        match self {
            PvcAccessMode::ReadWriteOnce => "ReadWriteOnce",
            PvcAccessMode::ReadOnlyMany => "ReadOnlyMany",
            PvcAccessMode::ReadWriteMany => "ReadWriteMany",
            PvcAccessMode::ReadWriteOncePod => "ReadWriteOncePod",
            PvcAccessMode::Unknown(s) => s,
        }
    }

    /// Parse a **canonical** k8s access-mode string; `None` for anything else.
    /// The migrate tool uses this to refuse a non-canonical VolSync value up
    /// front instead of passing it through to a doomed `kubectl apply`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ReadWriteOnce" => Some(PvcAccessMode::ReadWriteOnce),
            "ReadOnlyMany" => Some(PvcAccessMode::ReadOnlyMany),
            "ReadWriteMany" => Some(PvcAccessMode::ReadWriteMany),
            "ReadWriteOncePod" => Some(PvcAccessMode::ReadWriteOncePod),
            _ => None,
        }
    }
}

impl Serialize for PvcAccessMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.mode_str())
    }
}

impl<'de> Deserialize<'de> for PvcAccessMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(PvcAccessMode::parse(&s).unwrap_or(PvcAccessMode::Unknown(s)))
    }
}

impl JsonSchema for PvcAccessMode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PvcAccessMode".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Only the canonical values: `Unknown` is a decode-compat artifact for
        // legacy stored data, never an admissible write.
        schemars::json_schema!({
            "type": "string",
            "description": "A Kubernetes PersistentVolumeClaim access mode.",
            "enum": PvcAccessMode::CANONICAL,
        })
    }
}

/// Lifecycle of the underlying kopia snapshot when its `Snapshot` CR is deleted.
/// Produced backups default to `Delete`; discovered snapshots are forced to `Retain`.
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

/// What the deletion of a `SnapshotSchedule` does to the `Snapshot` CRs it
/// produced (which Kubernetes GC cascade-deletes via their ownerReference).
/// Default `Retain`: the CRs are removed but their kopia snapshots survive and
/// the catalog rediscovers them as `origin: discovered`. `Delete` opts into the
/// cascade: each Snapshot's own `deletionPolicy` applies.
///
/// Deliberately 2-variant (not reusing [`DeletionPolicy`]): an `Orphan` in
/// cascade position would differ from `Retain` only in per-CR event/metric
/// bookkeeping — an invalid state made unrepresentable. The guard's `Retain`
/// is exactly `DeletionPolicy::Retain`'s semantics (CR removed, kopia snapshot
/// stays, catalog rediscovers it), deliberately NOT the `Orphan` event storm
/// (no per-CR "orphaned" event/metric for every produced Snapshot).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum ScheduleDeletePolicy {
    /// Keep the kopia snapshots: a produced Snapshot whose effective
    /// deletionPolicy is `Delete` is downgraded to retain when its owning
    /// schedule is gone (the fail-safe default).
    #[default]
    Retain,
    /// Cascade: each produced Snapshot's own `deletionPolicy` applies even when
    /// the owning schedule is gone (subject to the mass-deletion breaker).
    Delete,
}

/// What the deletion of a `SnapshotPolicy` does to the `Snapshot` CRs carrying
/// its config label (the recipe's produced/adopted rows — NOT its kopia
/// snapshot history in the abstract, which is exactly what `Retain` preserves).
/// Default `Retain`: the CRs are removed but every kopia snapshot survives
/// (rediscoverable/adoptable by a future `SnapshotPolicy`, including this one
/// re-created). `Delete` opts into the cascade: each CR's own `deletionPolicy`
/// applies, as EXTERNAL deletions subject to the per-repository mass-deletion
/// breaker (`deletionProtection.threshold`).
///
/// Deliberately 2-variant (not reusing [`DeletionPolicy`]), mirroring
/// [`ScheduleDeletePolicy`]: an `Orphan` in cascade position would differ from
/// `Retain` only in per-CR event/metric bookkeeping — an invalid state made
/// unrepresentable. The guard's `Retain` is exactly `DeletionPolicy::Retain`'s
/// semantics (CR removed, kopia snapshot stays, catalog rediscovers it),
/// deliberately NOT the `Orphan` event storm (no per-CR "orphaned" event/metric
/// for every one of a deleted policy's Snapshots). Not [`ScheduleDeletePolicy`]
/// itself: that type's doc contract is schedule-specific (its `Delete` arm talks
/// about a *schedule* being gone/replaced), and a `SnapshotPolicy` deletion is a
/// distinct trigger with its own semantics worth documenting on its own type.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum PolicyDeletePolicy {
    /// Keep the kopia snapshots: a Snapshot whose effective deletionPolicy is
    /// `Delete` is downgraded to retain when its owning `SnapshotPolicy` is gone
    /// (the fail-safe default).
    #[default]
    Retain,
    /// Cascade: each Snapshot's own `deletionPolicy` applies even though the
    /// owning `SnapshotPolicy` is gone (subject to the mass-deletion breaker).
    Delete,
}

/// Mass-deletion circuit breaker for this repository's Snapshots.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeletionProtectionSpec {
    /// Pending EXTERNAL destructive Snapshot deletions (deletionTimestamp set,
    /// effective deletionPolicy Delete, not operator-pruned) that trip the
    /// breaker for this repository: at or above this, those deletions are HELD
    /// (finalizers wait) until acknowledged via the
    /// `kopiur.home-operations.com/allow-mass-deletion` annotation.
    /// `0` disables the breaker. Default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_mass_deletion_threshold")]
    pub threshold: Option<u32>,
}

/// schemars default for [`DeletionProtectionSpec::threshold`] —
/// [`DEFAULT_MASS_DELETION_THRESHOLD`](crate::consts::DEFAULT_MASS_DELETION_THRESHOLD)
/// (`10`), matching `effective_mass_deletion_threshold`'s absent→CONST
/// resolution. Returns the field's `Option` type so schemars 1 emits the
/// schema `default:`.
fn default_mass_deletion_threshold() -> Option<u32> {
    Some(crate::consts::DEFAULT_MASS_DELETION_THRESHOLD)
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
    /// IANA timezone the cron is evaluated in (e.g. `America/Chicago`); absent uses
    /// the enclosing schedule's timezone, else the controller default (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Resolve an optional IANA timezone name to a concrete zone, defaulting to UTC.
/// An unparseable name falls back to UTC defensively — the admission webhook rejects
/// bad names up front via `validate::validate_timezone`, so reconcile-time resolution
/// should never see one.
pub fn resolve_tz(name: Option<&str>) -> chrono_tz::Tz {
    name.and_then(|s| s.parse::<chrono_tz::Tz>().ok())
        .unwrap_or(chrono_tz::Tz::UTC)
}

/// Repo-level scheduling defaults, inherited at reconcile time by consumers that
/// don't set their own equivalent field (ADR §2.2 principle 10: sub-object, not a
/// leaf field, so future defaults — e.g. jitter — slot in without API breakage).
///
/// Consumed by `SnapshotPolicy` verification, `RepositoryReplication`,
/// `Maintenance` scheduling, and `SnapshotSchedule` (the recurring-backup cron) —
/// all of which resolve their repository in-reconciler via
/// [`crate::common::RepositoryRef`]. The `SnapshotSchedule` consumer resolves its
/// target policy's repository default at slot-computation time (see
/// [`effective_timezone`]) and is re-triggered by a repository referent watch.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleDefaults {
    /// IANA timezone name applied to every consuming cron that doesn't set its own
    /// `timezone` (e.g. `America/New_York`). Set once here instead of repeating it
    /// on every `SnapshotPolicy.verification`, `RepositoryReplication.schedule`, and
    /// `Maintenance.schedule` cron.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Resolve a consuming cron's own optional IANA timezone against a repository-level
/// default, falling back to UTC (mirrors [`resolve_tz`]): `own` wins when set, else
/// `repo_default` (typically `Repository`/`ClusterRepository`
/// `spec.scheduleDefaults.timezone`), else UTC. An unparseable name at whichever
/// level is selected falls back to UTC defensively, same as `resolve_tz` — the
/// admission webhook rejects bad names up front for both levels via
/// `validate::validate_timezone`, so reconcile-time resolution should never see one.
///
/// ```
/// use kopiur_api::common::resolve_tz_with_default;
///
/// // The schedule's own timezone wins, even over a repo default.
/// assert_eq!(
///     resolve_tz_with_default(Some("America/Chicago"), Some("UTC")),
///     "America/Chicago".parse::<chrono_tz::Tz>().unwrap(),
/// );
/// // Absent own timezone falls through to the repo default.
/// assert_eq!(
///     resolve_tz_with_default(None, Some("America/New_York")),
///     "America/New_York".parse::<chrono_tz::Tz>().unwrap(),
/// );
/// // Both absent → UTC.
/// assert_eq!(resolve_tz_with_default(None, None), chrono_tz::Tz::UTC);
/// ```
pub fn resolve_tz_with_default(own: Option<&str>, repo_default: Option<&str>) -> chrono_tz::Tz {
    resolve_tz(own.or(repo_default))
}

/// Matched `SnapshotSchedule` target policies disagreed on their repositories'
/// `scheduleDefaults.timezone`, so [`effective_timezone`] could not pick one
/// unambiguously and fell back to UTC. The controller surfaces this as a status
/// condition recommending an explicit `spec.schedule.timezone`.
///
/// This case only arises for the `policySelector` fan-out form (a single
/// `policyRef` has exactly one repository, so it can never disagree with itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimezoneAmbiguity {
    /// The distinct candidate zones (IANA names, sorted) that disagreed, for the
    /// human-readable condition message.
    pub candidates: Vec<String>,
}

/// **Pure.** Decide the effective timezone a `SnapshotSchedule`'s cron is
/// evaluated in, given the schedule's own `spec.schedule.timezone` (`own`) and the
/// `scheduleDefaults.timezone` of each *matched* target policy's repository
/// (`policy_repo_defaults`, one entry per matched policy; `None` = that repo sets
/// no default). Mirrors [`resolve_tz`] fallback semantics (an unparseable name
/// degrades to UTC — the webhook rejects bad names up front).
///
/// Rules (the reconciler does the GETs and passes the data in):
/// - `own` set → that zone wins, no lookups, never ambiguous.
/// - `own` unset, **no** matched policies → UTC, not ambiguous.
/// - `own` unset, all matched policies resolve to **one** zone → that zone.
/// - `own` unset, matched policies resolve to **differing** zones → UTC plus a
///   [`TimezoneAmbiguity`] (recommend an explicit `spec.schedule.timezone`).
///
/// A single `policyRef` therefore never yields ambiguity (one repository). Repos
/// with no default resolve to UTC, so mixing "a zone" with "no default" is a
/// genuine disagreement and is reported.
///
/// ```
/// use kopiur_api::common::effective_timezone;
///
/// // Own timezone wins outright.
/// let (tz, amb) = effective_timezone(Some("America/Chicago"), &[]);
/// assert_eq!(tz.name(), "America/Chicago");
/// assert!(amb.is_none());
///
/// // Unset own, one agreeing default across matched policies.
/// let defs = [Some("Europe/Berlin".to_string()), Some("Europe/Berlin".to_string())];
/// let (tz, amb) = effective_timezone(None, &defs);
/// assert_eq!(tz.name(), "Europe/Berlin");
/// assert!(amb.is_none());
///
/// // Unset own, disagreeing defaults → UTC + ambiguity signal.
/// let defs = [Some("Europe/Berlin".to_string()), None];
/// let (tz, amb) = effective_timezone(None, &defs);
/// assert_eq!(tz, chrono_tz::Tz::UTC);
/// assert!(amb.is_some());
/// ```
pub fn effective_timezone(
    own: Option<&str>,
    policy_repo_defaults: &[Option<String>],
) -> (chrono_tz::Tz, Option<TimezoneAmbiguity>) {
    if own.is_some() {
        return (resolve_tz(own), None);
    }
    // No matched policies → nothing to inherit from.
    if policy_repo_defaults.is_empty() {
        return (chrono_tz::Tz::UTC, None);
    }
    // Resolve each matched policy's repo default to a concrete zone, then reduce to
    // the distinct set. A repo with no default resolves to UTC (via `resolve_tz`),
    // so it can legitimately disagree with a repo that sets one.
    let mut zones: Vec<chrono_tz::Tz> = policy_repo_defaults
        .iter()
        .map(|d| resolve_tz(d.as_deref()))
        .collect();
    zones.sort_by(|a, b| a.name().cmp(b.name()));
    zones.dedup();
    if zones.len() == 1 {
        (zones[0], None)
    } else {
        let candidates = zones.iter().map(|z| z.name().to_string()).collect();
        (chrono_tz::Tz::UTC, Some(TimezoneAmbiguity { candidates }))
    }
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
