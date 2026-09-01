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
                "{s} must be >= 0 — a negative deadline marks every slot expired the instant \
                 it fires (SkipExpired forever), so the schedule never runs. Omit the field for \
                 no deadline, or use 0 to fire only exactly on time"
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
