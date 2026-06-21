use super::*;
use crate::error::{ValidationError, ValidationResult};
use crate::snapshot::{Origin, SnapshotSpec};
use crate::snapshot_policy::{Hook, SnapshotPolicySpec};
use crate::snapshot_schedule::SnapshotScheduleSpec;

/// Validate a `SnapshotPolicy` spec, accumulating all problems.
pub fn validate_backup_config(spec: &SnapshotPolicySpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_repository_ref(&spec.repository) {
        errs.push(e);
    }
    if spec.sources.is_empty() {
        errs.push(ValidationError::MissingRequiredField {
            field: "spec.sources (at least one source required)".to_string(),
        });
    }
    for source in &spec.sources {
        if let Err(e) = validate_source(source) {
            errs.push(e);
        }
    }
    // Identity shape (kopia's username@hostname:path contract). The explicit
    // overrides are validated here — client-free, so this runs on every admission
    // even when the webhook has no kube client. CEL-resolved values and the
    // name/namespace defaults are validated where they are resolved
    // (`resolve_identity`), and again defensively at reconcile time.
    if let Some(id) = &spec.identity {
        if let Some(u) = &id.username
            && let Err(e) = validate_identity_component("spec.identity.username", u)
        {
            errs.push(e);
        }
        if let Some(h) = &id.hostname
            && let Err(e) = validate_identity_component("spec.identity.hostname", h)
        {
            errs.push(e);
        }
    }
    for (i, source) in spec.sources.iter().enumerate() {
        if let Some(p) = &source.source_path_override
            && let Err(e) =
                validate_source_path(&format!("spec.sources[{i}].sourcePathOverride"), p)
        {
            errs.push(e);
        }
    }
    // `volumeSnapshotClassName` only applies when a PVC source is CSI-snapshotted/cloned
    // (`copyMethod: Snapshot`/`Clone`). An NFS source has no PVC to snapshot, so pairing
    // it with an explicit class is a configuration mistake — reject it at admission with
    // an actionable message rather than silently ignoring the class. (`copyMethod` itself
    // can't be rejected for NFS: it defaults to `Snapshot` implicitly and an NFS source
    // is simply read directly.)
    if spec.volume_snapshot_class_name.is_some() && spec.sources.iter().any(|s| s.nfs.is_some()) {
        errs.push(ValidationError::InvalidFieldValue {
            field: "spec.volumeSnapshotClassName".to_string(),
            reason: "an NFS source cannot be CSI-snapshotted, so volumeSnapshotClassName is \
                     meaningless with it; remove volumeSnapshotClassName (NFS is read directly), \
                     or use a PVC source for copyMethod: Snapshot/Clone"
                .to_string(),
        });
    }
    if let Some(m) = &spec.mover
        && let Err(e) = validate_mover(m, "SnapshotPolicy mover")
    {
        errs.push(e);
    }
    // Data-loss guard: a retention that selects no snapshots prunes EVERY Snapshot the
    // moment it runs. `retention: None` means "don't prune" (safe) and is not flagged;
    // only an explicit but empty/all-zero retention is the trap.
    if let Some(r) = &spec.retention
        && retention_keeps_nothing(r)
    {
        errs.push(ValidationError::InvalidFieldValue {
            field: "spec.retention".to_string(),
            reason: "keeps no snapshots — every keep* bucket is unset or 0, so GFS retention \
                     would prune every Snapshot immediately (data loss). Set at least one bucket \
                     (e.g. keepLatest: 1), or omit spec.retention entirely to disable pruning."
                .to_string(),
        });
    }
    // Verification (ADR-0005 §4): override schedules must parse, and the optional
    // `successExpr` (ADR-0005 §15) must compile + trial-evaluate to a bool with no
    // out-of-scope variable — rejected at admission rather than at first verify run.
    if let Some(v) = &spec.verification {
        if let Some(q) = &v.quick {
            if let Err(e) = validate_cron(&q.cron) {
                errs.push(e);
            }
            if let Err(e) = validate_timezone(q.timezone.as_deref()) {
                errs.push(e);
            }
        }
        if let Some(d) = &v.deep {
            if let Err(e) = validate_cron(&d.schedule.cron) {
                errs.push(e);
            }
            if let Err(e) = validate_timezone(d.schedule.timezone.as_deref()) {
                errs.push(e);
            }
        }
        if let Some(expr) = &v.success_expr
            && let Err(e) = crate::success_expr::validate_success_expr(expr)
        {
            errs.push(e);
        }
    }
    // Hooks (ADR §4.8): per-hook shape problems are caught at admission rather
    // than at the first backup run (where a quiesce hook failing on a typo would
    // abort the backup).
    if let Some(h) = &spec.hooks {
        for (list, hooks) in [
            ("beforeSnapshot", &h.before_snapshot),
            ("afterSnapshot", &h.after_snapshot),
        ] {
            for (i, hook) in hooks.iter().enumerate() {
                if let Err(e) = validate_hook(list, i, hook) {
                    errs.push(e);
                }
            }
        }
    }
    errs
}

/// Validate one hook entry — the controller executes these with the SAME parsers
/// (Go-style `timeout`, URL/method for `httpRequest`), so a value admitted here
/// can never fail to parse at run time. Exhaustive over [`Hook`].
fn validate_hook(list: &str, index: usize, hook: &Hook) -> ValidationResult {
    let field = |leaf: &str| format!("spec.hooks.{list}[{index}].{leaf}");
    let check_timeout = |leaf: &str, t: Option<&str>| -> ValidationResult {
        if let Some(t) = t
            && crate::duration::parse_go_duration(t).is_none()
        {
            return Err(ValidationError::InvalidFieldValue {
                field: field(leaf),
                reason: format!(
                    "{t:?} is not a valid Go-style duration; use a positive number with an \
                     s/m/h suffix (e.g. 90s, 2m) — how long the hook may run before it is \
                     treated as failed"
                ),
            });
        }
        Ok(())
    };
    match hook {
        Hook::WorkloadExec(h) => {
            if h.command.is_empty() {
                return Err(ValidationError::MissingRequiredField {
                    field: field("workloadExec.command"),
                });
            }
            check_timeout("workloadExec.timeout", h.timeout.as_deref())
        }
        Hook::RunJob(h) => check_timeout("runJob.timeout", h.timeout.as_deref()),
        Hook::HttpRequest(h) => {
            if !(h.url.starts_with("http://") || h.url.starts_with("https://")) {
                return Err(ValidationError::InvalidFieldValue {
                    field: field("httpRequest.url"),
                    reason: format!(
                        "{:?} must be an absolute http:// or https:// URL the controller can \
                         reach (e.g. http://notifier.tools.svc:8080/fire)",
                        h.url
                    ),
                });
            }
            if let Some(m) = &h.method {
                const METHODS: [&str; 7] =
                    ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
                if !METHODS.contains(&m.to_ascii_uppercase().as_str()) {
                    return Err(ValidationError::InvalidFieldValue {
                        field: field("httpRequest.method"),
                        reason: format!(
                            "{m:?} is not an HTTP method; use one of GET, POST (default), PUT, \
                             PATCH, DELETE, HEAD, OPTIONS"
                        ),
                    });
                }
            }
            check_timeout("httpRequest.timeout", h.timeout.as_deref())
        }
    }
}

/// Validate a `Snapshot` spec for a given origin, accumulating all problems.
///
/// `origin` is supplied by the caller because the canonical value lives in
/// `status.origin` / the `kopiur.home-operations.com/origin` label, not in `spec` (ADR §3.4).
pub fn validate_backup(spec: &SnapshotSpec, origin: Origin) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_backup_deletion_policy(origin, spec.deletion_policy) {
        errs.push(e);
    }
    if let Some(fp) = &spec.failure_policy
        && let Err(e) = validate_failure_policy(fp, "Snapshot")
    {
        errs.push(e);
    }
    errs
}

/// Exactly one of `policyRef` / `policySelector` is set on a `SnapshotSchedule`
/// (ADR-0005 §10). Neither ⇒ `MissingRequiredField`; both ⇒ `MutuallyExclusive`.
/// Pure so the XOR decision is unit-tested directly.
pub fn validate_schedule_policy_target(spec: &SnapshotScheduleSpec) -> ValidationResult {
    match (spec.policy_ref.is_some(), spec.policy_selector.is_some()) {
        (true, true) => Err(ValidationError::MutuallyExclusive {
            a: "policyRef".to_string(),
            b: "policySelector".to_string(),
            context: "SnapshotSchedule".to_string(),
        }),
        (false, false) => Err(ValidationError::MissingRequiredField {
            field: "exactly one of spec.policyRef or spec.policySelector".to_string(),
        }),
        _ => Ok(()),
    }
}

/// Validate a `SnapshotSchedule` spec, accumulating all problems.
pub fn validate_backup_schedule(spec: &SnapshotScheduleSpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Err(e) = validate_schedule_policy_target(spec) {
        errs.push(e);
    }
    if let Err(e) = validate_cron(&spec.schedule.cron) {
        errs.push(e);
    }
    if let Err(e) = validate_timezone(spec.schedule.timezone.as_deref()) {
        errs.push(e);
    }
    errs
}
