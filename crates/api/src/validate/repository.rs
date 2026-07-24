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
/// explicit `Retain` pass; `Delete`/`Orphan` are rejected. `discovered`'s
/// underlying kopia snapshot was never created by the operator, so it must
/// never be the thing that deletes it. `adopted` is the one exception: an
/// adopted row was deliberately re-attached to a `SnapshotPolicy` precisely so
/// GFS retention (and any `deletionPolicy`) governs it like a produced backup —
/// any policy is allowed. `scheduled`/`manual` are unchanged (any policy).
pub fn validate_backup_deletion_policy(
    origin: crate::snapshot::Origin,
    policy: Option<crate::common::DeletionPolicy>,
) -> ValidationResult {
    use crate::common::DeletionPolicy;
    use crate::snapshot::Origin;
    match origin {
        Origin::Discovered => match policy {
            None | Some(DeletionPolicy::Retain) => Ok(()),
            Some(other) => Err(ValidationError::DiscoveredMustRetain {
                got: format!("{other:?}"),
            }),
        },
        Origin::Adopted | Origin::Scheduled | Origin::Manual => Ok(()),
    }
}

/// `spec.parameters` is well-formed and applicable (#258). Shared by both repository
/// kinds via `context`, exactly like [`validate_repository_health`].
///
/// Two classes of rule:
///
/// - **Grammar.** Every duration must parse, and every count must be positive. The
///   grammar check matters more here than elsewhere: these are the first CRD durations
///   that reach a kopia CLI, and this module's contract is that a value the webhook
///   admits never fails at reconcile time.
/// - **Applicability.** A `mode: ReadOnly` repository can never apply them — kopia
///   hard-errors `set-parameters` on a read-only connection — so declaring them there is
///   a configuration mistake. Reject it rather than silently ignore the block, matching
///   how `volumeSnapshotClassName` + an NFS source is handled.
pub fn validate_repository_parameters(
    parameters: Option<&crate::repository::RepositoryParameters>,
    mode: crate::common::RepositoryMode,
    context: &str,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let Some(epoch) = parameters.and_then(|p| p.epoch.as_ref()) else {
        return errs;
    };
    if !mode.allows_writes() {
        errs.push(ValidationError::InvalidFieldValue {
            field: format!("{context} spec.parameters.epoch"),
            reason: "a ReadOnly repository cannot apply repository parameters: \
                     `kopia repository set-parameters` rewrites the repository-global format \
                     blob and fails outright on a read-only connection. Remove \
                     spec.parameters, or set mode: ReadWrite on the cluster that owns this \
                     repository (in a multi-cluster layout, declare the parameters there — \
                     they are a property of the repository, not of each consumer)"
                .to_string(),
        });
    }
    let mut duration = |field: &str, raw: &Option<String>| {
        let Some(raw) = raw.as_deref() else { return };
        let field = format!("{context} spec.parameters.epoch.{field}");
        match crate::duration::parse_go_duration(raw) {
            None => errs.push(ValidationError::InvalidFieldValue {
                field,
                reason: format!(
                    "{raw:?} is not a valid duration. Use a Go-style duration with a single \
                     unit, like 6h, 90m, or 30s; omit the field to leave kopia's current \
                     value untouched"
                ),
            }),
            // kopia stores these as a Go `time.Duration` — an i64 NANOSECOND count, so it
            // tops out near 292 years, and `parse_go_duration` happily accepts far more
            // than that (`"999999999999999999"` is a valid bare-seconds value). Bound it
            // here rather than let the drift comparator's `as i64` wrap it to a negative
            // number, and to keep this module's contract: a value the webhook admits must
            // never fail at reconcile time.
            Some(d) if i64::try_from(d.as_nanos()).is_err() => {
                errs.push(ValidationError::InvalidFieldValue {
                    field,
                    reason: format!(
                        "{raw:?} is too large: kopia stores epoch durations as a 64-bit \
                         nanosecond count, so the maximum is roughly 292 years. Use a \
                         realistic epoch duration (hours, e.g. 6h)"
                    ),
                });
            }
            Some(_) => {}
        }
    };
    duration("minDuration", &epoch.min_duration);
    duration("refreshFrequency", &epoch.refresh_frequency);

    let mut positive = |field: &str, v: Option<i64>| {
        if let Some(v) = v
            && v <= 0
        {
            errs.push(ValidationError::InvalidFieldValue {
                field: format!("{context} spec.parameters.epoch.{field}"),
                reason: format!(
                    "must be > 0 (got {v}); omit the field to leave kopia's current value \
                     untouched"
                ),
            });
        }
    };
    positive("advanceOnCount", epoch.advance_on_count);
    positive("advanceOnSizeMiB", epoch.advance_on_size_mb);
    positive("checkpointFrequency", epoch.checkpoint_frequency);
    positive("deleteParallelism", epoch.delete_parallelism);
    errs
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
    if let Some(probe) = health.and_then(|h| h.probe.as_ref()) {
        if let Some(raw) = probe.interval.as_deref() {
            match crate::duration::parse_go_duration(raw) {
                None => {
                    return Err(ValidationError::InvalidFieldValue {
                        field: format!("{context} health.probe.interval"),
                        reason: format!(
                            "{raw:?} is not a valid duration. Use a Go-style duration like 30s, \
                             5m, or 1h; omit the field for the default (30m)"
                        ),
                    });
                }
                Some(d) if d < crate::consts::MIN_HEALTH_PROBE_INTERVAL => {
                    return Err(ValidationError::InvalidFieldValue {
                        field: format!("{context} health.probe.interval"),
                        reason: format!(
                            "{raw:?} is shorter than the 30s minimum. Each probe runs a mover \
                             Job; use 30s or more (default 30m)"
                        ),
                    });
                }
                Some(_) => {}
            }
        }
        if let Some(t) = probe.failure_threshold
            && t < 1
        {
            return Err(ValidationError::InvalidFieldValue {
                field: format!("{context} health.probe.failureThreshold"),
                reason: format!(
                    "must be >= 1 (got {t}); it is the number of consecutive failing probes \
                     required before the warning is raised"
                ),
            });
        }
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
/// #         bootstrap: None, mover_defaults: None, schedule_defaults: None, catalog: None, identity_defaults: None, server: None, maintenance: None, on_namespace_delete: Default::default(), mode: Default::default(), suspend: false, health: None, parameters: None, deletion_protection: None,
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
    // Identity CEL expressions must compile + trial-evaluate to a string at admission
    // (ADR-0004 §5), so a typo / out-of-scope variable is rejected on `kubectl apply`.
    // Mirrors `validate_cluster_repository`'s identical block.
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
        // `cluster` becomes part of the default hostname (`<namespace>.<cluster>`)
        // and `classify_hostname` splits on the first `.`, so it must be a clean
        // RFC 1123 label with no dot of its own (M1/M5).
        if let Some(cluster) = &id.cluster
            && let Err(e) = validate_cluster_name(cluster)
        {
            errs.push(e);
        }
    }
    errs.extend(validate_foreign_snapshots_cluster_coupling(
        spec.catalog.as_ref(),
        spec.identity_defaults
            .as_ref()
            .and_then(|id| id.cluster.as_deref()),
    ));
    if let Some(md) = &spec.mover_defaults
        && let Some(res) = &md.resources
        && let Err(e) = validate_resources(res, "Repository moverDefaults")
    {
        errs.push(e);
    }
    if let Some(server) = &spec.server {
        errs.extend(validate_server(server, spec.mode));
    }
    errs.extend(validate_repository_parameters(
        spec.parameters.as_ref(),
        spec.mode,
        "Repository",
    ));
    if let Err(e) = validate_repository_health(spec.health.as_ref(), "Repository") {
        errs.push(e);
    }
    if let Some(b) = &spec.bootstrap
        && let Some(fp) = &b.failure_policy
        && let Err(e) = validate_failure_policy(fp, "Repository spec.bootstrap")
    {
        errs.push(e);
    }
    if let Err(e) = validate_timezone(
        spec.schedule_defaults
            .as_ref()
            .and_then(|d| d.timezone.as_deref()),
    ) {
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
    // `foreignSnapshots: Fallback` needs somewhere to land, and only a
    // ClusterRepository can place discovered Snapshots outside their own
    // namespace — mirrors the fallbackNamespace rule directly above.
    if matches!(
        catalog.foreign_snapshots,
        Some(crate::common::ForeignSnapshots::Fallback)
    ) {
        if catalog.fallback_namespace.is_none() {
            errs.push(ValidationError::InvalidFieldValue {
                field: "catalog.foreignSnapshots".to_string(),
                reason: "Fallback requires catalog.fallbackNamespace to be set (there is \
                         nowhere to materialize a foreign snapshot otherwise); set \
                         fallbackNamespace, or use Ignore"
                    .to_string(),
            });
        }
        if !cluster_scoped {
            errs.push(ValidationError::InvalidFieldValue {
                field: "catalog.foreignSnapshots".to_string(),
                reason: "Fallback is only meaningful on a ClusterRepository; a namespaced \
                         Repository already materializes into its own namespace; use Ignore or \
                         omit"
                    .to_string(),
            });
        }
    }
    errs
}

/// The `identityDefaults.cluster` × `catalog.foreignSnapshots` cross-field
/// rules (multi-cluster shared-repo): classifying a snapshot as another
/// cluster's is undecidable without a cluster identity (a), and adopting one
/// must never silently switch off an already-configured fallback collector
/// (d). Shared by both repository kinds — `cluster` is the resolved
/// `identityDefaults.cluster` value, `None` when the repository has no cluster
/// identity set (or, on a namespaced `Repository`, no `identityDefaults` set at
/// all). Without a cluster, rule (d) is a no-op (it requires one to fire) while
/// rule (a) still rejects any `foreignSnapshots` set there.
pub fn validate_foreign_snapshots_cluster_coupling(
    catalog: Option<&crate::common::CatalogBounds>,
    cluster: Option<&str>,
) -> Vec<ValidationError> {
    let Some(catalog) = catalog else {
        return Vec::new();
    };
    let mut errs = Vec::new();
    let has_cluster = cluster.is_some_and(|c| !c.is_empty());
    if catalog.foreign_snapshots.is_some() && !has_cluster {
        errs.push(ValidationError::ForeignSnapshotsRequiresCluster);
    }
    if has_cluster && catalog.fallback_namespace.is_some() && catalog.foreign_snapshots.is_none() {
        errs.push(ValidationError::ForeignSnapshotsChoiceRequired);
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
    if let Some(m) = &spec.mover {
        if let Err(e) = validate_mover(m, "RepositoryReplication mover") {
            errs.push(e);
        }
        // A replication mover copies blobs repo→repo and never touches a workload's files, so
        // `repository_replication.rs` never resolves inheritance — it passes the explicit
        // contexts straight to `resolve_mover`. The field was therefore ACCEPTED and silently
        // dropped: the manifest claimed the mover ran as the workload, and it did not. Reject
        // it instead of ignoring it.
        if let Err(e) = super::forbid_inherit(
            m,
            "RepositoryReplication spec",
            "is not honored by a replication mover, which copies repository blobs and never \
             reads a workload's files — there is no workload whose identity it could take. \
             Remove it; set mover.securityContext explicitly if the destination backend needs \
             a particular UID/GID (e.g. a filesystem repository on an NFS export).",
        ) {
            errs.push(e);
        }
    }
    if let Some(sync) = &spec.sync {
        if let Some(p) = sync.parallel
            && let Some(e) = require_min("RepositoryReplication spec.sync.parallel", p.into(), 1)
        {
            errs.push(e);
        }
        if let Some(s) = sync.max_download_speed_bytes_per_second
            && let Some(e) = require_min(
                "RepositoryReplication spec.sync.maxDownloadSpeedBytesPerSecond",
                s,
                1,
            )
        {
            errs.push(e);
        }
        if let Some(s) = sync.max_upload_speed_bytes_per_second
            && let Some(e) = require_min(
                "RepositoryReplication spec.sync.maxUploadSpeedBytesPerSecond",
                s,
                1,
            )
        {
            errs.push(e);
        }
    }
    errs
}

/// Whether a replication's `destination` backend differs from its source
/// repository's backend (ADR-0005 §13(d)). Replicating a repository to *itself* is a
/// no-op (or a loop), so the webhook rejects it. Pure so the decision is unit-tested;
/// the webhook resolves the source backend (it has a client) and calls this. A
/// "same" destination is detected structurally by [`backend_target_key`]: same
/// backend kind AND the same identifying target — which for S3 includes the
/// endpoint and region (not just bucket+prefix), for Azure the storage account,
/// and for a filesystem the backing volume, so two distinct providers that share
/// a bucket/container/path name are NOT mistaken for the same repository (#248).
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
/// // Same bucket name at two DIFFERENT S3 endpoints → distinct targets (#248).
/// let s3_nas = Backend::S3(S3Backend { bucket: "kopiur".into(), prefix: None, endpoint: Some("nas.example:3000".into()), region: None, auth: None, tls: None });
/// let s3_e2 = Backend::S3(S3Backend { bucket: "kopiur".into(), prefix: None, endpoint: Some("t3u7.fra3.idrivee2-58.com".into()), region: Some("eu-central-2".into()), auth: None, tls: None });
/// assert!(replication_destination_differs(&s3_nas, &s3_e2));
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
///
/// Each arm must fold in EVERY field that distinguishes the storage *target*
/// (never credentials) — dropping one makes two genuinely-distinct destinations
/// collide onto the same key, so [`replication_destination_differs`] wrongly
/// reports a self-replication and the webhook rejects a valid `RepositoryReplication`
/// (issue #248): two different S3 providers sharing the bucket name `kopiur`
/// resolved to the same `s3:kopiur/` key when only `bucket`+`prefix` were keyed.
/// Fields are labelled (`endpoint=…;region=…`) so distinct tuples can't concatenate
/// into an identical string. Auth/TLS are deliberately excluded: the same bucket
/// reached with different credentials is still the same storage.
fn backend_target_key(backend: &crate::backend::Backend) -> String {
    use crate::backend::{Backend, RepoVolume};
    let kind = backend.kind_str();
    let target = match backend {
        Backend::Filesystem(f) => {
            // `path` is a mount path INSIDE the mover pod and is commonly the
            // same default (`/repo`) across repositories, so the backing volume
            // (a distinct PVC or NFS export) is what actually distinguishes two
            // filesystem targets.
            let vol = match &f.volume {
                None => "none".to_string(),
                Some(RepoVolume::Pvc(p)) => format!("pvc={}", p.name),
                Some(RepoVolume::Nfs(n)) => format!("nfs={}:{}", n.server, n.path),
            };
            format!("path={};volume={vol}", f.path)
        }
        Backend::S3(s) => format!(
            "endpoint={};region={};bucket={};prefix={}",
            s.endpoint.clone().unwrap_or_default(),
            s.region.clone().unwrap_or_default(),
            s.bucket,
            s.prefix.clone().unwrap_or_default(),
        ),
        Backend::Azure(a) => format!(
            "account={};container={};prefix={}",
            a.storage_account.clone().unwrap_or_default(),
            a.container,
            a.prefix.clone().unwrap_or_default(),
        ),
        // GCS/B2 bucket names are globally unique (no endpoint/account to key),
        // so bucket+prefix is the complete target identity.
        Backend::Gcs(g) => format!(
            "bucket={};prefix={}",
            g.bucket,
            g.prefix.clone().unwrap_or_default()
        ),
        Backend::B2(b) => format!(
            "bucket={};prefix={}",
            b.bucket,
            b.prefix.clone().unwrap_or_default()
        ),
        Backend::Sftp(s) => format!(
            "host={};port={};path={}",
            s.host,
            s.port.map(|p| p.to_string()).unwrap_or_default(),
            s.path,
        ),
        Backend::WebDav(w) => format!("url={}", w.url),
        Backend::Rclone(r) => format!("remotePath={}", r.remote_path),
        Backend::Gdrive(g) => format!("folderId={}", g.folder_id),
    };
    format!("{kind}:{target}")
}

/// Validate a `Maintenance` spec, accumulating all problems (ADR §3.7).
pub fn validate_maintenance(spec: &MaintenanceSpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_repository_ref(&spec.repository) {
        errs.push(e);
    }
    // `ownership.ownerAliases` become kopia identity components once run
    // through `kopia_lease_identity` (M6), so each alias gets the identity
    // shape rule — the same validator the resolved hostname/username go
    // through. `owner` itself is deliberately NOT tightened here: it predates
    // this rule, stored CRs may carry arbitrary strings the lease sanitizer
    // already handles, and the controller re-validates defensively on every
    // reconcile — a new rejection would hard-stop a working Maintenance.
    // Aliases are new with this rule, so no stored object can regress.
    for (i, alias) in spec.ownership.owner_aliases.iter().enumerate() {
        if let Err(e) = validate_identity_component(&format!("ownership.ownerAliases[{i}]"), alias)
        {
            errs.push(e);
        }
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
        if let Err(e) = forbid_snapshot_inherit(
            m,
            "maintenance",
            "a maintenance mover operates on the repository, not on a snapshot's data, so \
             there is no recorded identity to reproduce; `snapshot` is restore-only. Use an \
             explicit mover.securityContext instead.",
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
        // `cluster` becomes part of the default hostname (`<namespace>.<cluster>`)
        // and `classify_hostname` splits on the first `.`, so it must be a clean
        // RFC 1123 label with no dot of its own (M1).
        if let Some(cluster) = &id.cluster
            && let Err(e) = validate_cluster_name(cluster)
        {
            errs.push(e);
        }
    }
    if let Some(c) = &spec.catalog {
        errs.extend(validate_catalog_bounds(c, true));
    }
    errs.extend(validate_foreign_snapshots_cluster_coupling(
        spec.catalog.as_ref(),
        spec.identity_defaults
            .as_ref()
            .and_then(|id| id.cluster.as_deref()),
    ));
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
    errs.extend(validate_repository_parameters(
        spec.parameters.as_ref(),
        spec.mode,
        "ClusterRepository",
    ));
    if let Err(e) = validate_repository_health(spec.health.as_ref(), "ClusterRepository") {
        errs.push(e);
    }
    if let Some(b) = &spec.bootstrap
        && let Some(fp) = &b.failure_policy
        && let Err(e) = validate_failure_policy(fp, "ClusterRepository spec.bootstrap")
    {
        errs.push(e);
    }
    if let Err(e) = validate_timezone(
        spec.schedule_defaults
            .as_ref()
            .and_then(|d| d.timezone.as_deref()),
    ) {
        errs.push(e);
    }
    errs
}
