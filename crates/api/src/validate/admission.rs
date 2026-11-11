//! **Admission-only** validators — the rules the WEBHOOK enforces and the
//! controller deliberately does not.
//!
//! Every shared aggregate in this module's siblings (`validate_backup_schedule`,
//! `validate_maintenance`, `validate_repository_replication`, …) is re-run as a
//! HARD STOP at the top of the corresponding reconciler. That makes those
//! aggregates a ratchet in one direction only: adding a rule to one does not just
//! refuse the next bad edit, it **bricks every already-stored CR** that happens to
//! violate it — the object stops reconciling, and backups stop running, with no
//! user action having taken place.
//!
//! So a rule that TIGHTENS an existing field lives here instead, called only from
//! `kopiur-webhook`'s per-kind handlers. A stored object keeps reconciling under
//! whatever it was admitted with; the next `kubectl apply` is what has to satisfy
//! the tighter rule. (A rule about a brand-new field needs none of this: no stored
//! object can carry a field that did not exist, so it goes in the shared aggregate
//! where the controller re-checks it too.)
//!
//! Everything here is pure and spec-only — same crate, same unit tests, no
//! `kube::Client` — so the split is about *where it is called from*, not about a
//! second dialect of validation living in the webhook crate.

use crate::error::{ValidationError, ValidationResult};
use crate::maintenance::MaintenanceSpec;
use crate::repository_replication::RepositoryReplicationSpec;
use crate::snapshot_policy::SnapshotPolicySpec;
use crate::snapshot_replication::SnapshotReplicationSpec;
use crate::snapshot_schedule::SnapshotScheduleSpec;

use super::validate_jitter_bounds;

/// Run [`validate_jitter_bounds`] over an optional jitter field, pushing any
/// problem onto `errs`. Absent jitter is always fine.
fn push_jitter_bounds(errs: &mut Vec<ValidationError>, field: &str, jitter: Option<&str>) {
    if let Some(j) = jitter
        && let Err(e) = validate_jitter_bounds(field, j)
    {
        errs.push(e);
    }
}

/// `startingDeadlineSeconds` must not be negative.
///
/// A negative deadline is not "no deadline" — the miss check is
/// `now - slot > deadline`, so a negative value marks EVERY slot expired the
/// instant it fires. The schedule then skips every run forever while reporting
/// itself perfectly healthy: the silent-wedge shape. Omit the field for no
/// deadline; `0` (fire only exactly on time) is legitimate and accepted.
///
/// Admission-only: `SnapshotSchedule`'s reconciler re-runs
/// `validate_backup_schedule` as a hard stop, and a stored schedule carrying a
/// negative value must keep reconciling (badly, but visibly) rather than stop dead.
fn validate_starting_deadline_seconds(seconds: Option<i64>) -> ValidationResult {
    if let Some(s) = seconds
        && s < 0
    {
        return Err(ValidationError::InvalidFieldValue {
            field: "spec.schedule.startingDeadlineSeconds".to_string(),
            reason: format!(
                "{s} must be >= 0, or the schedule never runs. Fix: omit the field for no \
                 deadline, or use 0 to fire only exactly on time"
            ),
        });
    }
    Ok(())
}

/// Admission-only extras for a `SnapshotSchedule`: the jitter 24h cap and the
/// non-negative `startingDeadlineSeconds` rule. Both TIGHTEN fields that already
/// exist, so neither may join `validate_backup_schedule`.
pub fn validate_backup_schedule_admission_extras(
    spec: &SnapshotScheduleSpec,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    push_jitter_bounds(
        &mut errs,
        "spec.schedule.jitter",
        spec.schedule.jitter.as_deref(),
    );
    if let Err(e) = validate_starting_deadline_seconds(spec.schedule.starting_deadline_seconds) {
        errs.push(e);
    }
    errs
}

/// Admission-only extras for a `SnapshotPolicy`: the jitter 24h cap on both
/// verification tiers. The PARSE half already lives in `validate_backup_config`
/// (and has since these fields shipped); only the bound is new, so only the bound
/// is admission-only.
pub fn validate_backup_config_admission_extras(spec: &SnapshotPolicySpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let Some(v) = &spec.verification else {
        return errs;
    };
    push_jitter_bounds(
        &mut errs,
        "spec.verification.quick.schedule.jitter",
        v.quick
            .as_ref()
            .and_then(|q| q.schedule.as_ref())
            .and_then(|s| s.jitter.as_deref()),
    );
    push_jitter_bounds(
        &mut errs,
        "spec.verification.deep.schedule.jitter",
        v.deep.as_ref().and_then(|d| d.schedule.jitter.as_deref()),
    );
    errs
}

/// Admission-only extras for a `Maintenance`: parse AND bound both jitter windows.
///
/// Unlike the other kinds, `validate_maintenance` never validated these fields at
/// all — it covers the crons and the timezone only — so a garbage window has always
/// been accepted and silently degraded to *no jitter* at reconcile. That means
/// stored objects carrying garbage exist, and the parse half is a tightening too:
/// both halves are admission-only, so those objects keep maintaining themselves
/// (unspread) until someone edits them.
pub fn validate_maintenance_admission_extras(spec: &MaintenanceSpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    push_jitter_bounds(
        &mut errs,
        "spec.schedule.quick.jitter",
        spec.schedule.quick.jitter.as_deref(),
    );
    push_jitter_bounds(
        &mut errs,
        "spec.schedule.full.jitter",
        spec.schedule.full.jitter.as_deref(),
    );
    errs
}

/// Admission-only extras for a `RepositoryReplication`: parse AND bound the
/// schedule jitter. Same history as `Maintenance` — `validate_repository_replication`
/// checks the cron and timezone but never the jitter, so both halves are new
/// rejections over a field stored objects already carry.
pub fn validate_repository_replication_admission_extras(
    spec: &RepositoryReplicationSpec,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    push_jitter_bounds(
        &mut errs,
        "spec.schedule.jitter",
        spec.schedule.jitter.as_deref(),
    );
    errs
}

/// Admission-only extras for a `SnapshotReplication`: the jitter 24h cap. The parse
/// half already lives in `validate_snapshot_replication`.
pub fn validate_snapshot_replication_admission_extras(
    spec: &SnapshotReplicationSpec,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    push_jitter_bounds(
        &mut errs,
        "spec.schedule.jitter",
        spec.schedule.jitter.as_deref(),
    );
    errs
}

/// The capacity bound on the ORIGINAL `contentCacheSizeMb`/`metadataCacheSizeMb`
/// fields (#613): with a `capacity` on the same layer they must fit in it exactly
/// like `{ size: … }` does — each within the capacity, the pair within 90% of it.
///
/// Admission-only, and only when the cache block CHANGED: those fields predate the
/// bound, so stored objects carrying an oversized budget exist. The shared
/// validator and the controller hand such values to kopia unchanged
/// ([`crate::common::LegacyBudgets::PassThrough`]); this refuses the next apply
/// that edits the cache block while keeping them. Skipping an unchanged block also
/// keeps the controller's own metadata writes (finalizers, labels) on such an
/// object from being refused. `old` is the same block in the previous object
/// (`None` on CREATE).
pub fn validate_cache_legacy_budgets(
    cache: Option<&crate::common::CacheDefaults>,
    old: Option<&crate::common::CacheDefaults>,
    field: &str,
) -> Option<ValidationError> {
    // With a capacity on the layer the full admission view applies: the bound on
    // `*CacheSizeMb`, and a capacity large enough to bound BOTH caches (#707
    // review). Both tighten pre-existing fields, hence admission-only.
    let cache = cache?;
    if old == Some(cache) || cache.capacity.is_none() {
        return None;
    }
    crate::common::resolve_cache_budgets(
        cache,
        cache.capacity.as_deref(),
        crate::common::LegacyBudgets::Enforce,
        field,
    )
    .err()
}

/// The controller owner reference naming a kopiur `Repository`/`ClusterRepository`
/// — the parent an operator-managed `Maintenance` was projected from. `None` for
/// anything else. Owner references are user-editable, so this only NAMES the
/// parent; the webhook must fetch it (and match its uid) before treating the
/// object as a projection (see [`validate_maintenance_cache_legacy_budgets`]).
pub fn repository_controller_ref(
    owner_refs: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference],
) -> Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference> {
    owner_refs.iter().find(|o| {
        o.controller == Some(true)
            && o.api_version.starts_with(&format!("{}/", crate::GROUP))
            && matches!(o.kind.as_str(), "Repository" | "ClusterRepository")
    })
}

/// [`validate_cache_legacy_budgets_for`] for a `Maintenance`, skipped for a
/// VERIFIED projection of a repository.
///
/// The operator's managed `Maintenance` copies its repository's
/// `spec.maintenance.mover`, which was admitted on the repository itself — so a
/// stored repository whose block predates the bound must still get its
/// maintenance when it is (re-)enabled. Refusing the operator's CREATE (no old
/// object to compare against) would leave maintenance silently never starting
/// (#707 review).
///
/// `verified_parent_cache` is `Some(cache)` only when the webhook resolved the
/// [`repository_controller_ref`] parent, its uid matched, and `cache` is that
/// repository's own `spec.maintenance.mover.cache`. The exemption applies only when
/// it EQUALS this object's cache block: owner references are user-editable, so a
/// user-authored `Maintenance` claiming a repository owner but carrying a cache the
/// repository does not have is still checked (#714 review).
pub fn validate_maintenance_cache_legacy_budgets(
    spec: &MaintenanceSpec,
    old: Option<&MaintenanceSpec>,
    verified_parent_cache: Option<Option<&crate::common::CacheDefaults>>,
) -> Vec<ValidationError> {
    if verified_parent_cache.is_some_and(|parent| parent == mover_cache(spec.mover.as_ref())) {
        return Vec::new();
    }
    validate_cache_legacy_budgets_for(spec, old)
}

/// A `SnapshotReplication` connects BOTH repositories, and each keeps its own kopia
/// cache on the one cache volume, so a sized cache must hold two caches of at
/// least 1MiB + 1MiB each: 6Mi (#714 review). Admission-only and only when the
/// block changed, because `capacity` is a pre-existing field.
pub fn validate_snapshot_replication_cache_minimum(
    spec: &SnapshotReplicationSpec,
    old: Option<&SnapshotReplicationSpec>,
) -> Vec<ValidationError> {
    const MIN_MIB: i64 = 6;
    let cache = mover_cache(spec.mover.as_ref());
    let Some(capacity) = cache.and_then(|c| c.capacity.as_deref()) else {
        return Vec::new();
    };
    if old.is_some_and(|o| mover_cache(o.mover.as_ref()) == cache) {
        return Vec::new();
    }
    match crate::common::quantity_to_mib(capacity) {
        Some(mib) if mib < MIN_MIB => vec![ValidationError::InvalidFieldValue {
            field: "spec.mover.cache.capacity".to_string(),
            reason: format!(
                "`{capacity}` is too small: a snapshot replication keeps a kopia cache for both \
                 repositories on this volume, which needs at least {MIN_MIB}Mi. Fix: raise \
                 `capacity`, or remove it for an unsized emptyDir cache"
            ),
        }],
        _ => Vec::new(),
    }
}

/// Every mover cache block a spec carries, with its field path — so the webhook
/// can apply [`validate_cache_legacy_budgets`] to any kind uniformly. Order is
/// stable per kind (the old object's blocks are matched by position).
pub trait CacheBlocks {
    /// `(field path, cache block)` for each place this kind configures a mover cache.
    fn cache_blocks(&self) -> Vec<(&'static str, Option<&crate::common::CacheDefaults>)>;
}

fn mover_cache(m: Option<&crate::common::MoverSpec>) -> Option<&crate::common::CacheDefaults> {
    m.and_then(|m| m.cache.as_ref())
}

macro_rules! mover_cache_blocks {
    ($($ty:ty),* $(,)?) => {$(
        impl CacheBlocks for $ty {
            fn cache_blocks(&self) -> Vec<(&'static str, Option<&crate::common::CacheDefaults>)> {
                vec![("spec.mover.cache", mover_cache(self.mover.as_ref()))]
            }
        }
    )*};
}
mover_cache_blocks!(
    SnapshotPolicySpec,
    crate::snapshot::SnapshotSpec,
    crate::restore::RestoreSpec,
    MaintenanceSpec,
    RepositoryReplicationSpec,
    SnapshotReplicationSpec,
);

macro_rules! repository_cache_blocks {
    ($($ty:ty),* $(,)?) => {$(
        impl CacheBlocks for $ty {
            fn cache_blocks(&self) -> Vec<(&'static str, Option<&crate::common::CacheDefaults>)> {
                vec![
                    (
                        "spec.moverDefaults.cache",
                        self.mover_defaults.as_ref().and_then(|m| m.cache.as_ref()),
                    ),
                    (
                        "spec.maintenance.mover.cache",
                        mover_cache(self.maintenance.as_ref().and_then(|m| m.mover.as_ref())),
                    ),
                ]
            }
        }
    )*};
}
repository_cache_blocks!(
    crate::repository::RepositorySpec,
    crate::cluster_repository::ClusterRepositorySpec,
);

/// [`validate_cache_legacy_budgets`] over every cache block of `spec`, against the
/// same blocks of `old` (the previous object on UPDATE, `None` on CREATE).
pub fn validate_cache_legacy_budgets_for<T: CacheBlocks>(
    spec: &T,
    old: Option<&T>,
) -> Vec<ValidationError> {
    let old_blocks = old.map(CacheBlocks::cache_blocks);
    spec.cache_blocks()
        .into_iter()
        .enumerate()
        .filter_map(|(i, (field, cache))| {
            let prev = old_blocks
                .as_ref()
                .and_then(|b| b.get(i))
                .and_then(|(_, c)| *c);
            validate_cache_legacy_budgets(cache, prev, field)
        })
        .collect()
}

// (`validate_cache_legacy_budgets` above is kind-agnostic: the webhook applies it
// to every kind's cache block, `Repository`/`ClusterRepository` included.)
//
// `Repository`/`ClusterRepository` deliberately have NO entry here. Everything
// this change adds to them — `scheduleDefaults.jitter` (parse + bounds) and
// `moverDefaults.podLabels`/`podAnnotations` (reserved keys, via
// `super::validate_pod_metadata`) — concerns BRAND-NEW fields, so no stored object
// can carry a value the rules reject and the rules are safe in
// `validate_repository`/`validate_cluster_repository`, where the controller
// re-checks them too. An empty extras fn per kind would be pure ceremony; the next
// Repository *tightening* is what earns one.
//
// Worth recording the asymmetry the shared placement leaves behind: the
// `Repository` reconciler re-runs only a PARTIAL validator set (see
// `controller/src/repository.rs`), while `ClusterRepository` re-runs the full
// aggregate. So the new `scheduleDefaults.jitter` rule is enforced at reconcile for
// one kind and not the other. That is acceptable precisely because it is
// admission that matters here — the webhook covers both kinds identically, and the
// reconcile-side re-check is a defense-in-depth pass, not the gate.

/// The legacy `spec.extraArgs` (`kopia policy set` escape hatch) checked against
/// the kopia flag table (#708): a flag Kopiur models, owns or forbids is refused.
///
/// Admission-only, and only when the list CHANGED: `extraArgs` predates the
/// table, so stored policies carrying a now-refused flag exist. They keep backing
/// up exactly as before (the mover still passes the list) and report the problem
/// through an `UnsupportedKopiaArgs` condition built from
/// [`legacy_extra_args_findings`]; this refuses the next apply that edits the
/// list while keeping one. `old` is the previous object's spec (`None` on CREATE).
pub fn validate_legacy_extra_args(
    spec: &SnapshotPolicySpec,
    old: Option<&SnapshotPolicySpec>,
) -> Vec<ValidationError> {
    if old.is_some_and(|o| o.extra_args == spec.extra_args) {
        return Vec::new();
    }
    super::snapshot::kopia_arg_errors(
        "spec.extraArgs",
        crate::kopia_flags::KopiaCommand::PolicySet,
        &spec.extra_args,
    )
}

/// The refused args in a policy's legacy `spec.extraArgs`, regardless of when
/// they were admitted — what the controller surfaces as the
/// `UnsupportedKopiaArgs` condition on a stored object it keeps running.
pub fn legacy_extra_args_findings(
    spec: &SnapshotPolicySpec,
) -> Vec<crate::kopia_flags::ExtraArgViolation> {
    crate::kopia_flags::check_extra_args(
        crate::kopia_flags::KopiaCommand::PolicySet,
        &spec.extra_args,
    )
}
