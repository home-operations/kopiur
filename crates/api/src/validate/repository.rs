use super::*;
use crate::backend::{Backend, RepoVolume};
use crate::cluster_repository::{AllowedNamespaces, ClusterRepositorySpec};
use crate::common::{MoverDefaults, RepositoryKind, RepositoryRef, Retention};
use crate::error::{ValidationError, ValidationResult};
use crate::maintenance::{MaintenanceSpec, RepositoryMaintenanceSpec};
use crate::repository::{RepositoryHealthSpec, RepositorySpec};
use crate::repository_replication::RepositoryReplicationSpec;
use std::collections::BTreeMap;

/// A `RepositoryRef` is well-formed: a `ClusterRepository` reference is by name
/// only, so `namespace` MUST be absent (ADR §3.2/§3.3). A namespaced `Repository`
/// reference may carry a namespace (cross-namespace references are allowed).
///
/// ```
/// use kopiur_api::common::RepositoryRef;
/// use kopiur_api::validate::validate_repository_ref;
/// use kopiur_api::ValidationError;
///
/// // OK: a namespaced Repository reference may name a namespace.
/// let ok: RepositoryRef = serde_json::from_value(serde_json::json!({
///     "kind": "Repository", "name": "nas-primary", "namespace": "backups",
/// }))
/// .unwrap();
/// assert!(validate_repository_ref(&ok).is_ok());
///
/// // Err: a ClusterRepository is referenced by name alone — a namespace is forbidden.
/// let bad: RepositoryRef = serde_json::from_value(serde_json::json!({
///     "kind": "ClusterRepository", "name": "shared", "namespace": "oops",
/// }))
/// .unwrap();
/// assert_eq!(
///     validate_repository_ref(&bad).unwrap_err(),
///     ValidationError::ClusterRepoNamespaceForbidden { namespace: "oops".to_string() },
/// );
/// ```
pub fn validate_repository_ref(r: &RepositoryRef) -> ValidationResult {
    match r.kind {
        RepositoryKind::ClusterRepository => match &r.namespace {
            Some(ns) => Err(ValidationError::ClusterRepoNamespaceForbidden {
                namespace: ns.clone(),
            }),
            None => Ok(()),
        },
        RepositoryKind::Repository => Ok(()),
    }
}

/// A consumer namespace is permitted by a `ClusterRepository`'s tenancy gate
/// (ADR §3.2/§4.3).
///
/// - `List`     → membership test.
/// - `All(true)`→ always allowed; `All(false)` is meaningless and denies.
/// - `Selector` → matched against `labels` (the consumer namespace's labels). The
///   `crates/api` crate cannot fetch a `Namespace` object, so the caller (webhook)
///   must supply the labels. **If `labels` is `None` we fail closed** with
///   [`ValidationError::SelectorLabelsUnavailable`] rather than guess — the webhook
///   never trusts unfiltered input (ADR §3.2). Selector matching here is a simple
///   `matchLabels` superset test (the common case); `matchExpressions` is treated
///   as "no constraint" for now and documented as such.
pub fn validate_consumer_against_cluster_repo(
    consumer_namespace: &str,
    repo_name: &str,
    allowed: &AllowedNamespaces,
    labels: Option<&BTreeMap<String, String>>,
) -> ValidationResult {
    match allowed {
        AllowedNamespaces::All(true) => Ok(()),
        AllowedNamespaces::All(false) => Err(ValidationError::ConsumerNamespaceNotAllowed {
            namespace: consumer_namespace.to_string(),
            repo: repo_name.to_string(),
        }),
        AllowedNamespaces::List(names) => {
            if names.iter().any(|n| n == consumer_namespace) {
                Ok(())
            } else {
                Err(ValidationError::ConsumerNamespaceNotAllowed {
                    namespace: consumer_namespace.to_string(),
                    repo: repo_name.to_string(),
                })
            }
        }
        AllowedNamespaces::Selector(sel) => {
            let Some(labels) = labels else {
                return Err(ValidationError::SelectorLabelsUnavailable {
                    namespace: consumer_namespace.to_string(),
                    repo: repo_name.to_string(),
                });
            };
            let match_labels = sel.match_labels.clone().unwrap_or_default();
            // Every required label must be present with the required value.
            let matches = match_labels
                .iter()
                .all(|(k, v)| labels.get(k).map(|got| got == v).unwrap_or(false));
            if matches {
                Ok(())
            } else {
                Err(ValidationError::ConsumerNamespaceNotAllowed {
                    namespace: consumer_namespace.to_string(),
                    repo: repo_name.to_string(),
                })
            }
        }
    }
}

/// A `Snapshot`'s `deletionPolicy` is legal for its origin (ADR §4.5).
///
/// `origin: discovered` forces `Retain`: `None` (defaults to `Retain`) and an
/// explicit `Retain` pass; `Delete`/`Orphan` are rejected. Other origins accept any
/// policy.
pub fn validate_backup_deletion_policy(
    origin: crate::snapshot::Origin,
    policy: Option<crate::common::DeletionPolicy>,
) -> ValidationResult {
    use crate::common::DeletionPolicy;
    use crate::snapshot::Origin;
    if origin != Origin::Discovered {
        return Ok(());
    }
    match policy {
        None | Some(DeletionPolicy::Retain) => Ok(()),
        Some(other) => Err(ValidationError::DiscoveredMustRetain {
            got: format!("{other:?}"),
        }),
    }
}

/// `spec.health` rules shared by `Repository` and `ClusterRepository`
/// (ADR-0005 §13). The index-blob warning threshold must be non-negative: a
/// negative count is nonsensical, and `0` is the documented sentinel that
/// disables the warning (so it is allowed). `context` names the kind for the
/// message ("Repository" / "ClusterRepository").
pub fn validate_repository_health(
    health: Option<&RepositoryHealthSpec>,
    context: &str,
) -> ValidationResult {
    if let Some(h) = health
        && let Some(t) = h.index_blob_warn_threshold
        && t < 0
    {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{context} health.indexBlobWarnThreshold"),
            reason: format!(
                "must be >= 0 (got {t}); 0 disables the index-blob warning, a positive \
                 value is the count above which a Warning is raised"
            ),
        });
    }
    Ok(())
}

/// Whether a [`Retention`] selects **no** snapshots — every bucket unset or `0`. The
/// controller only prunes when `spec.retention` is `Some` ([`crate::retention::select_kept`]
/// over the buckets), so a `Some(keeps-nothing)` retention prunes *every* `Snapshot`
/// immediately: silent data loss. (`retention: None` is the safe "don't prune" case and is
/// NOT flagged.)
pub(crate) fn retention_keeps_nothing(r: &Retention) -> bool {
    [
        r.keep_latest,
        r.keep_hourly,
        r.keep_daily,
        r.keep_weekly,
        r.keep_monthly,
        r.keep_annual,
    ]
    .into_iter()
    .all(|bucket| bucket.unwrap_or(0) == 0)
}

/// A `Repository` spec does not carry kopia-side (repo-level) retention policy,
/// which would conflict with CR-driven GFS retention (ADR §4.4 exclusivity).
///
/// The current [`RepositorySpec`] deliberately models no inline retention field, so
/// this **always passes today**. It exists as the enforcement hook so that if a
/// future field (e.g. `spec.policy.keepDaily`) is ever added, wiring it here is the
/// one obvious place — and the rule is already named and tested. Be pragmatic: we
/// do not invent a field to reject.
pub fn validate_repository_no_inline_retention(_spec: &RepositorySpec) -> ValidationResult {
    // No inline-retention field exists on RepositorySpec. If one is added later,
    // return Err(ValidationError::InlineRetentionForbidden { field: "<name>" }) here.
    Ok(())
}

/// Validate a `spec.maintenance` block on a `Repository`/`ClusterRepository`,
/// accumulating problems (ADR §3.7):
/// - any override schedule's quick/full crons must parse (same parser as runtime);
/// - `namespace` is **cluster-scope only** — it selects where the namespaced
///   managed `Maintenance` lands for a `ClusterRepository`, and is forbidden on a
///   namespaced `Repository` (whose `Maintenance` always lives in its own ns).
///
/// `cluster_scoped` is the only thing that differs between the two repository
/// kinds, so one validator serves both call sites.
pub fn validate_repository_maintenance(
    maintenance: &RepositoryMaintenanceSpec,
    cluster_scoped: bool,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Some(schedule) = &maintenance.schedule {
        if let Err(e) = validate_cron(&schedule.quick.cron) {
            errs.push(e);
        }
        if let Err(e) = validate_cron(&schedule.full.cron) {
            errs.push(e);
        }
        for tz in [
            schedule.timezone.as_deref(),
            schedule.quick.timezone.as_deref(),
            schedule.full.timezone.as_deref(),
        ] {
            if let Err(e) = validate_timezone(tz) {
                errs.push(e);
            }
        }
    }
    if !cluster_scoped && let Some(ns) = &maintenance.namespace {
        errs.push(ValidationError::MaintenanceNamespaceOnNamespacedRepo {
            namespace: ns.clone(),
        });
    }
    errs
}

/// Accumulate every create-time-immutable field that changed between `old` and
/// `new` repository specs (ADR-0005 §7). Shared by both repository kinds via the
/// thin [`validate_repository_immutability`] / [`validate_cluster_repository_immutability`]
/// wrappers, which pass the `encryption` password ref + the `create.{splitter,hash,
/// encryption}` algorithms — the fields kopia bakes into the repository format.
///
/// Pure: the webhook supplies `old`/`new` from the admission request's old/new
/// objects; CREATE has no old object, so this is only wired into the UPDATE path.
fn diff_immutable_repo_fields(
    old_create: Option<&crate::common::CreateBehavior>,
    new_create: Option<&crate::common::CreateBehavior>,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    // NOTE: `encryption` (the password Secret *reference*) is deliberately NOT immutable.
    // kopia bakes only the resolved password *value* and the `create.*` algorithms into
    // the repository format — never the Secret name/namespace/key. Locking the reference
    // was both over-strict (a Secret rename with identical content was rejected, breaking
    // GitOps) and under-strict (editing a Secret's content in place — the actual password
    // change kopia would reject — sailed through). kopia also supports `change-password`,
    // so the password is operationally mutable; a genuinely wrong ref surfaces at connect
    // time, not at admission. We only enforce the create-time algorithms below.
    // The create-time kopia algorithms. Compared field-wise so the message names the
    // exact field. `create` itself may be absent on either side (absent ⇒ None algos).
    let old_splitter = old_create.and_then(|c| c.splitter.as_deref());
    let new_splitter = new_create.and_then(|c| c.splitter.as_deref());
    if old_splitter != new_splitter {
        errs.push(ValidationError::Immutable {
            field: "create.splitter".to_string(),
        });
    }
    let old_hash = old_create.and_then(|c| c.hash.as_deref());
    let new_hash = new_create.and_then(|c| c.hash.as_deref());
    if old_hash != new_hash {
        errs.push(ValidationError::Immutable {
            field: "create.hash".to_string(),
        });
    }
    let old_enc = old_create.and_then(|c| c.encryption.as_deref());
    let new_enc = new_create.and_then(|c| c.encryption.as_deref());
    if old_enc != new_enc {
        errs.push(ValidationError::Immutable {
            field: "create.encryption".to_string(),
        });
    }
    // ECC (Reed-Solomon parity) is baked into the repository format at create time
    // (ADR-0005 §13(a)) — immutable post-create like the other create knobs.
    let old_ecc = old_create.and_then(|c| c.ecc.as_ref());
    let new_ecc = new_create.and_then(|c| c.ecc.as_ref());
    if old_ecc != new_ecc {
        errs.push(ValidationError::Immutable {
            field: "create.ecc".to_string(),
        });
    }
    errs
}

/// Reject changes to create-time-immutable `Repository` fields on UPDATE (ADR-0005
/// §7): `create.splitter`, `create.hash`, `create.encryption`, `create.ecc`. Returns
/// every changed field so a user sees them all at once. Empty ⇒ no immutable change.
///
/// `encryption` (the password Secret reference) is intentionally NOT in this set — only
/// the resolved password value is fixed in the kopia format, and the reference is not a
/// reliable proxy for it (see [`diff_immutable_repo_fields`]). Renaming the Secret is fine.
///
/// ```
/// use kopiur_api::repository::RepositorySpec;
/// use kopiur_api::validate::validate_repository_immutability;
/// # use kopiur_api::backend::{Backend, FilesystemBackend};
/// # use kopiur_api::common::{CreateBehavior, Encryption, SecretKeyRef};
/// # fn spec(splitter: Option<&str>) -> RepositorySpec {
/// #     RepositorySpec {
/// #         backend: Backend::Filesystem(FilesystemBackend { path: "/r".into(), volume: None }),
/// #         encryption: Encryption { password_secret_ref: SecretKeyRef { name: "s".into(), namespace: None, key: None } },
/// #         create: Some(CreateBehavior { enabled: true, encryption: None, splitter: splitter.map(String::from), hash: None, ecc: None }),
/// #         mover_defaults: None, catalog: None, server: None, maintenance: None, on_namespace_delete: Default::default(), mode: Default::default(), suspend: false, health: None,
/// #     }
/// # }
/// // Unchanged splitter → accepted.
/// assert!(validate_repository_immutability(&spec(Some("FIXED-4M")), &spec(Some("FIXED-4M"))).is_empty());
/// // Changed splitter → rejected.
/// assert!(!validate_repository_immutability(&spec(Some("FIXED-4M")), &spec(Some("DYNAMIC"))).is_empty());
/// ```
pub fn validate_repository_immutability(
    old: &RepositorySpec,
    new: &RepositorySpec,
) -> Vec<ValidationError> {
    diff_immutable_repo_fields(old.create.as_ref(), new.create.as_ref())
}

/// Reject changes to create-time-immutable `ClusterRepository` fields on UPDATE
/// (ADR-0005 §7). Same field set as [`validate_repository_immutability`].
pub fn validate_cluster_repository_immutability(
    old: &ClusterRepositorySpec,
    new: &ClusterRepositorySpec,
) -> Vec<ValidationError> {
    diff_immutable_repo_fields(old.create.as_ref(), new.create.as_ref())
}

/// Validate a `Repository` spec, accumulating all problems (ADR §3.1).
pub fn validate_repository(spec: &RepositorySpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_repository_no_inline_retention(spec) {
        errs.push(e);
    }
    if let Err(e) = validate_backend(&spec.backend) {
        errs.push(e);
    }
    if let Some(m) = &spec.maintenance {
        errs.extend(validate_repository_maintenance(m, false));
    }
    if let Some(c) = &spec.catalog {
        errs.extend(validate_catalog_bounds(c, false));
    }
    if let Some(md) = &spec.mover_defaults
        && let Some(res) = &md.resources
        && let Err(e) = validate_resources(res, "Repository moverDefaults")
    {
        errs.push(e);
    }
    if let Some(server) = &spec.server {
        errs.extend(validate_server(server, spec.mode));
    }
    if let Err(e) = validate_repository_health(spec.health.as_ref(), "Repository") {
        errs.push(e);
    }
    errs
}

/// The actionable admission warning for an inline-NFS filesystem repo whose
/// `moverDefaults` grant write access only via `fsGroup`. **`fsGroup` is silently
/// ignored on NFS** (the kubelet doesn't recursively chown in-tree NFS mounts), so
/// the mover/server/bootstrap reach the export as the unprivileged uid and the
/// repo `connect`/`create` fails with `permission denied`. Non-blocking (a user
/// fixing it NAS-side via Mapall can ignore it). Kept short for the admission
/// response (kube truncates very long warnings).
pub const NFS_FSGROUP_WARNING: &str = "NFS filesystem repo: fsGroup is ignored on NFS — \
     grant the mover write access via moverDefaults.podSecurityContext.supplementalGroups \
     (with a group-writable export), securityContext.runAsUser, or NAS-side Mapall";

/// Whether `moverDefaults` configures an NFS-effective write identity — i.e. a
/// `runAsUser` (container or pod) that owns the export, or a `supplementalGroups`
/// the export is group-writable by. `fsGroup` deliberately does **not** count: it
/// is a no-op on NFS.
fn nfs_write_identity_configured(mover_defaults: Option<&MoverDefaults>) -> bool {
    let Some(md) = mover_defaults else {
        return false;
    };
    let container_uid = md.security_context.as_ref().and_then(|sc| sc.run_as_user);
    let pod_uid = md
        .pod_security_context
        .as_ref()
        .and_then(|psc| psc.run_as_user);
    let suppl_groups = md
        .pod_security_context
        .as_ref()
        .and_then(|psc| psc.supplemental_groups.as_ref())
        .is_some_and(|g| !g.is_empty());
    container_uid.is_some() || pod_uid.is_some() || suppl_groups
}

/// Non-blocking admission warnings for a `Repository`/`ClusterRepository`. Shared
/// by both handlers (the rules can't fork). Today: the inline-NFS + `fsGroup`-only
/// footgun (see [`NFS_FSGROUP_WARNING`]). Takes the resolved `backend` +
/// `moverDefaults` so it serves both kinds without re-deriving them.
pub fn repository_warnings(
    backend: &Backend,
    mover_defaults: Option<&MoverDefaults>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let inline_nfs = matches!(
        backend,
        Backend::Filesystem(fs) if matches!(fs.volume, Some(RepoVolume::Nfs(_)))
    );
    if inline_nfs && !nfs_write_identity_configured(mover_defaults) {
        warnings.push(NFS_FSGROUP_WARNING.to_string());
    }
    warnings
}

/// Validate `spec.catalog` (ADR §3.1/§3.2): the refresh interval must parse and
/// respect the floor, the retain bounds must be enforceable, and
/// `fallbackNamespace` only means something on a cluster-scoped repository
/// (`cluster_scoped`). One validator for both kinds so the rules cannot fork.
pub fn validate_catalog_bounds(
    catalog: &crate::common::CatalogBounds,
    cluster_scoped: bool,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Some(raw) = catalog.refresh_interval.as_deref() {
        match crate::duration::parse_go_duration(raw) {
            None => errs.push(ValidationError::InvalidFieldValue {
                field: "catalog.refreshInterval".to_string(),
                reason: format!(
                    "{raw:?} is not a valid duration. Use a Go-style duration like 30s, 5m, or \
                     1h; omit the field for the default (1h)"
                ),
            }),
            Some(d) if d < crate::consts::MIN_CATALOG_REFRESH_INTERVAL => {
                errs.push(ValidationError::InvalidFieldValue {
                    field: "catalog.refreshInterval".to_string(),
                    reason: format!(
                        "{raw:?} is shorter than the 30s minimum. Each re-scan of an \
                         object-store repository runs a mover Job; use 30s or more (default 1h)"
                    ),
                });
            }
            Some(_) => {}
        }
    }
    if let Some(retain) = &catalog.retain {
        if let Some(n) = retain.per_identity
            && n < 0
        {
            errs.push(ValidationError::InvalidFieldValue {
                field: "catalog.retain.perIdentity".to_string(),
                reason: format!(
                    "{n} is negative. Use a positive count of discovered Snapshot CRs to keep \
                     per identity, 0 to disable discovered-Snapshot materialization, or omit \
                     the field to materialize everything"
                ),
            });
        }
        if let Some(d) = retain.max_age_days
            && d < 1
        {
            errs.push(ValidationError::InvalidFieldValue {
                field: "catalog.retain.maxAgeDays".to_string(),
                reason: format!(
                    "{d} is not a usable age bound. Use a positive number of days (snapshots \
                     older than this get no discovered Snapshot CR), or omit the field for no \
                     age bound"
                ),
            });
        }
    }
    if !cluster_scoped && catalog.fallback_namespace.is_some() {
        errs.push(ValidationError::InvalidFieldValue {
            field: "catalog.fallbackNamespace".to_string(),
            reason: "only a ClusterRepository places discovered Snapshots across namespaces; a \
                     namespaced Repository always materializes into its own namespace — remove \
                     the field (or move the repository to a ClusterRepository)"
                .to_string(),
        });
    }
    errs
}

/// Validate a `RepositoryReplication` spec, accumulating all problems (ADR-0005
/// §13(d)): the `sourceRef` is well-formed, the schedule cron parses, the
/// destination backend's content is valid, and (when a mover is set) it's
/// well-formed. The "destination differs from source" rule needs the resolved
/// source backend, which this pure validator cannot fetch — the webhook resolves it
/// and calls [`replication_destination_differs`] separately.
pub fn validate_repository_replication(spec: &RepositoryReplicationSpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_repository_ref(&spec.source_ref) {
        errs.push(e);
    }
    if let Err(e) = validate_cron(&spec.schedule.cron) {
        errs.push(e);
    }
    if let Err(e) = validate_timezone(spec.schedule.timezone.as_deref()) {
        errs.push(e);
    }
    if let Err(e) = validate_backend(&spec.destination) {
        errs.push(e);
    }
    if let Some(m) = &spec.mover
        && let Err(e) = validate_mover(m, "RepositoryReplication mover")
    {
        errs.push(e);
    }
    errs
}

/// Whether a replication's `destination` backend differs from its source
/// repository's backend (ADR-0005 §13(d)). Replicating a repository to *itself* is a
/// no-op (or a loop), so the webhook rejects it. Pure so the decision is unit-tested;
/// the webhook resolves the source backend (it has a client) and calls this. A
/// "same" destination is detected structurally: same backend kind AND the same
/// identifying target (bucket+prefix / path / container / host+path / url / remote).
///
/// ```
/// use kopiur_api::backend::{Backend, FilesystemBackend, S3Backend};
/// use kopiur_api::validate::replication_destination_differs;
///
/// let fs_a = Backend::Filesystem(FilesystemBackend { path: "/a".into(), volume: None });
/// let fs_b = Backend::Filesystem(FilesystemBackend { path: "/b".into(), volume: None });
/// // Different paths → differ.
/// assert!(replication_destination_differs(&fs_a, &fs_b));
/// // Same path → same target (would be a self-replication).
/// assert!(!replication_destination_differs(&fs_a, &fs_a));
/// // Different backend kinds always differ.
/// let s3 = Backend::S3(S3Backend { bucket: "b".into(), prefix: None, endpoint: None, region: None, auth: None, tls: None });
/// assert!(replication_destination_differs(&fs_a, &s3));
/// ```
pub fn replication_destination_differs(
    source: &crate::backend::Backend,
    dest: &crate::backend::Backend,
) -> bool {
    backend_target_key(source) != backend_target_key(dest)
}

/// A structural identity key for a backend (kind + identifying target), used by
/// [`replication_destination_differs`] to decide whether two backends point at the
/// same storage. Exhaustive over [`crate::backend::Backend`] so a new backend cannot
/// compile until its key is defined.
fn backend_target_key(backend: &crate::backend::Backend) -> String {
    use crate::backend::Backend;
    let kind = backend.kind_str();
    let target = match backend {
        Backend::Filesystem(f) => f.path.clone(),
        Backend::S3(s) => format!("{}/{}", s.bucket, s.prefix.clone().unwrap_or_default()),
        Backend::Azure(a) => format!("{}/{}", a.container, a.prefix.clone().unwrap_or_default()),
        Backend::Gcs(g) => format!("{}/{}", g.bucket, g.prefix.clone().unwrap_or_default()),
        Backend::B2(b) => format!("{}/{}", b.bucket, b.prefix.clone().unwrap_or_default()),
        Backend::Sftp(s) => format!("{}:{}", s.host, s.path),
        Backend::WebDav(w) => w.url.clone(),
        Backend::Rclone(r) => r.remote_path.clone(),
    };
    format!("{kind}:{target}")
}

/// Validate a `Maintenance` spec, accumulating all problems (ADR §3.7).
pub fn validate_maintenance(spec: &MaintenanceSpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_repository_ref(&spec.repository) {
        errs.push(e);
    }
    if let Err(e) = validate_cron(&spec.schedule.quick.cron) {
        errs.push(e);
    }
    if let Err(e) = validate_cron(&spec.schedule.full.cron) {
        errs.push(e);
    }
    for tz in [
        spec.schedule.timezone.as_deref(),
        spec.schedule.quick.timezone.as_deref(),
        spec.schedule.full.timezone.as_deref(),
    ] {
        if let Err(e) = validate_timezone(tz) {
            errs.push(e);
        }
    }
    if let Some(m) = &spec.mover {
        if let Err(e) = forbid_pvc_consumer(
            m,
            "maintenance",
            "Use an explicit mover.securityContext instead.",
        ) {
            errs.push(e);
        }
        if let Err(e) = validate_mover(m, "Maintenance mover") {
            errs.push(e);
        }
    }
    if let Some(fp) = &spec.failure_policy
        && let Err(e) = validate_failure_policy(fp, "Maintenance")
    {
        errs.push(e);
    }
    errs
}

/// Validate a `ClusterRepository` spec, accumulating all problems (ADR §3.2).
///
/// `All(false)` is rejected as meaningless (SKILL: "`false` is rejected by webhook").
pub fn validate_cluster_repository(spec: &ClusterRepositorySpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let AllowedNamespaces::All(false) = spec.allowed_namespaces {
        errs.push(ValidationError::MissingRequiredField {
            field: "allowedNamespaces.all must be true to grant access (false is meaningless)"
                .to_string(),
        });
    }
    if let Err(e) = validate_backend(&spec.backend) {
        errs.push(e);
    }
    if let Some(m) = &spec.maintenance {
        errs.extend(validate_repository_maintenance(m, true));
    }
    // Identity CEL expressions must compile + trial-evaluate to a string at admission
    // (ADR-0004 §5), so a typo / out-of-scope variable is rejected on `kubectl apply`.
    if let Some(id) = &spec.identity_defaults {
        if let Some(expr) = &id.hostname_expr
            && let Err(e) = crate::identity::validate_identity_expr(expr)
        {
            errs.push(e);
        }
        if let Some(expr) = &id.username_expr
            && let Err(e) = crate::identity::validate_identity_expr(expr)
        {
            errs.push(e);
        }
    }
    if let Some(c) = &spec.catalog {
        errs.extend(validate_catalog_bounds(c, true));
    }
    if let Some(md) = &spec.mover_defaults
        && let Some(res) = &md.resources
        && let Err(e) = validate_resources(res, "ClusterRepository moverDefaults")
    {
        errs.push(e);
    }
    if let Some(server) = &spec.server {
        if server.namespace.trim().is_empty() {
            errs.push(ValidationError::ServerNamespaceRequired);
        }
        errs.extend(validate_server(&server.server, spec.mode));
    }
    if let Err(e) = validate_repository_health(spec.health.as_ref(), "ClusterRepository") {
        errs.push(e);
    }
    errs
}
