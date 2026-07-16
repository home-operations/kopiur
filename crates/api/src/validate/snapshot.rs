use super::*;
use crate::error::{ValidationError, ValidationResult};
use crate::snapshot::{Origin, SnapshotSpec};
use crate::snapshot_policy::{CopyMethod, Hook, SnapshotPolicySpec};
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
    // `copyMethod: Direct` + `readOnly: false` is the one combination that reaches the
    // workload's own volume. `readOnly: false` is only ever set to make `fsGroup` apply,
    // and the kubelet applies it by recursively chgrp-ing the mount and adding
    // group-write. Under Snapshot/Clone that walk rewrites a throwaway staged PVC and is
    // free; under Direct it permanently rewrites live production data while the workload
    // runs — and the mover ships `fsGroup: 65532` by DEFAULT, so a user who sets one bool
    // to fix a permissions error would have their data re-grouped with no other signal.
    // Databases that refuse an over-permissive data directory (postgres, redis) fail to
    // restart afterwards. Require the intent to be stated; it is not inferable.
    for (i, source) in spec.sources.iter().enumerate() {
        if crate::snapshot_policy::source_mutates_live_volume(spec.copy_method, source)
            && !source.acknowledge_live_mutation.unwrap_or(false)
        {
            errs.push(ValidationError::InvalidFieldValue {
                field: format!("spec.sources[{i}].readOnly"),
                reason: "copyMethod: Direct with readOnly: false mounts the LIVE source volume \
                         read-write, so the kubelet will recursively chgrp its contents to the \
                         mover's fsGroup (65532 by default) and make them group-writable — \
                         permanently, while the workload is running. Prefer copyMethod: \
                         Snapshot/Clone, which applies fsGroup to a throwaway staged copy and \
                         never touches your data. If you do mean to rewrite the live volume, \
                         set acknowledgeLiveMutation: true on this source"
                    .to_string(),
            });
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
    // `snapshot create --upload-limit-mb` (M4 flag sweep, issue #216): a count
    // knob, must be at least 1 (0 or negative disables the flag's own purpose).
    if let Some(u) = &spec.upload
        && let Some(mb) = u.limit_mb
        && let Some(e) = require_min("SnapshotPolicy spec.upload.limitMb", mb, 1)
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
            // The flat `verification.quick.cron` shape moved under `quick.schedule`
            // (GitHub #174) — a required schedule would break decode of old persisted
            // objects, so `schedule` is Option and this validator is the gate that
            // rejects a re-apply of the old shape with an actionable pointer.
            match &q.schedule {
                None => errs.push(ValidationError::InvalidFieldValue {
                    field: "spec.verification.quick.schedule".to_string(),
                    reason: "the flat `verification.quick.cron` shape moved to \
                             `verification.quick.schedule.cron` (matching `deep.schedule`). \
                             Move your cron/jitter/timezone fields under `schedule:`."
                        .to_string(),
                }),
                Some(s) => {
                    if let Err(e) = validate_cron(&s.cron) {
                        errs.push(e);
                    }
                    if let Err(e) = validate_timezone(s.timezone.as_deref()) {
                        errs.push(e);
                    }
                    if let Err(e) = validate_jitter(
                        "spec.verification.quick.schedule.jitter",
                        s.jitter.as_deref(),
                    ) {
                        errs.push(e);
                    }
                }
            }
            // `kopia snapshot verify` tuning knobs: counts must be at least 1.
            // `maxErrors` is deliberately unconstrained — 0 is a valid, meaningful
            // value (kopia's own default, "stop at the first error").
            if let Some(p) = q.parallel
                && let Some(e) = require_min(
                    "SnapshotPolicy spec.verification.quick.parallel",
                    p.into(),
                    1,
                )
            {
                errs.push(e);
            }
            if let Some(p) = q.file_parallelism
                && let Some(e) = require_min(
                    "SnapshotPolicy spec.verification.quick.fileParallelism",
                    p.into(),
                    1,
                )
            {
                errs.push(e);
            }
            if let Some(p) = q.file_queue_length
                && let Some(e) = require_min(
                    "SnapshotPolicy spec.verification.quick.fileQueueLength",
                    p.into(),
                    1,
                )
            {
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
            if let Err(e) = validate_jitter(
                "spec.verification.deep.schedule.jitter",
                d.schedule.jitter.as_deref(),
            ) {
                errs.push(e);
            }
            // `restore --parallel` under the hood (deep verify IS a restore).
            if let Some(p) = d.parallel
                && let Some(e) = require_min(
                    "SnapshotPolicy spec.verification.deep.parallel",
                    p.into(),
                    1,
                )
            {
                errs.push(e);
            }
        }
        if let Some(expr) = &v.success_expr
            && let Err(e) = crate::success_expr::validate_success_expr(expr)
        {
            errs.push(e);
        }
    }
    errs.extend(validate_staging(spec));
    // Preflight: the timeout must parse, check names must be unique + non-blank, and
    // each check expression must compile + trial-evaluate to a bool with no
    // out-of-scope variable — rejected at admission rather than at the first run.
    if let Some(pf) = &spec.preflight {
        if let Some(t) = &pf.timeout
            && crate::duration::parse_go_duration(t).is_none()
        {
            errs.push(ValidationError::InvalidFieldValue {
                field: "spec.preflight.timeout".to_string(),
                reason: format!(
                    "{t:?} is not a valid duration. Use a Go-style duration like 10m or 1h; omit \
                     for the default (10m), or 0 to hold indefinitely"
                ),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for (i, c) in pf.checks.iter().enumerate() {
            let name = c.name.trim();
            if name.is_empty() {
                errs.push(ValidationError::MissingRequiredField {
                    field: format!("spec.preflight.checks[{i}].name"),
                });
            } else if !seen.insert(name.to_string()) {
                errs.push(ValidationError::InvalidFieldValue {
                    field: format!("spec.preflight.checks[{i}].name"),
                    reason: format!(
                        "duplicate preflight check name {name:?}; names must be unique"
                    ),
                });
            }
            if let Err(e) = crate::preflight::validate_preflight_expr(&c.expr) {
                errs.push(e);
            }
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

/// Validate `spec.staging` (+ its interplay with `copyMethod` and the sources):
///
///   * `timeout` must parse — rejected at admission rather than silently falling
///     back to the default at the first backup run.
///   * `accessModes` entries must be canonical/unique, and `ReadWriteOncePod`
///     sole ([`validate_access_modes`]).
///   * The staged-PVC override fields (`storageClassName`/`accessModes`) must have
///     a staged PVC to act on — rejected for `copyMethod: Direct` (no staged PVC
///     at all), an NFS source (never staged), and `pvcSelector` sources (staging
///     is skipped for selector expansion). The pre-existing `timeout` and
///     `volumeSnapshotClassName` stay deliberately lenient in those combinations —
///     tightening them now would reject already-persisted objects on re-apply.
fn validate_staging(spec: &SnapshotPolicySpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let Some(st) = &spec.staging else {
        return errs;
    };
    if let Some(t) = &st.timeout
        && crate::duration::parse_go_duration(t).is_none()
    {
        errs.push(ValidationError::InvalidFieldValue {
            field: "spec.staging.timeout".to_string(),
            reason: format!(
                "{t:?} is not a valid duration. Use a Go-style duration like 10m or 1h; omit \
                 for the default (10m), or 0 to wait for the VolumeSnapshot indefinitely"
            ),
        });
    }
    errs.extend(validate_access_modes(
        "spec.staging.accessModes",
        &st.access_modes,
    ));
    // A ReadOnlyMany staged PVC cannot be mounted read-write: the kubelet fails the
    // mount and the backup dies at run time with an opaque error. Catch it here.
    if st.access_modes.contains(&PvcAccessMode::ReadOnlyMany)
        && let Some(i) = spec
            .sources
            .iter()
            .position(|s| !crate::snapshot_policy::source_read_only(s))
    {
        errs.push(ValidationError::InvalidFieldValue {
            field: format!("spec.sources[{i}].readOnly"),
            reason: "readOnly: false cannot be honored when spec.staging.accessModes is \
                     [ReadOnlyMany]: the staged PVC is read-only, so mounting it read-write \
                     fails at the kubelet and the backup never starts. Drop ReadOnlyMany (a \
                     read-write staged PVC is what lets the kubelet apply fsGroup), or drop \
                     readOnly: false. The same conflict exists — invisibly here, because the \
                     class is only resolvable in-cluster — with a read-only staged class such \
                     as a rook-ceph CephFS class with backingSnapshot: \"true\""
                .to_string(),
        });
    }
    let overrides: Vec<&str> = [
        (
            "spec.staging.storageClassName",
            st.storage_class_name.is_some(),
        ),
        ("spec.staging.accessModes", !st.access_modes.is_empty()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect();
    if overrides.is_empty() {
        return errs;
    }
    let overrides = overrides.join(" / ");
    match spec.copy_method {
        CopyMethod::Direct => errs.push(ValidationError::InvalidFieldValue {
            field: overrides.clone(),
            reason: "copyMethod: Direct mounts the live source PVC — there is no staged PVC \
                     to override. Remove the staged-PVC override(s), or use copyMethod: \
                     Snapshot/Clone."
                .to_string(),
        }),
        CopyMethod::Snapshot | CopyMethod::Clone => {}
    }
    if spec.sources.iter().any(|s| s.nfs.is_some()) {
        errs.push(ValidationError::InvalidFieldValue {
            field: overrides.clone(),
            reason: "an NFS source is read directly and never staged, so a staged-PVC \
                     override is meaningless with it; remove the override(s) or use a PVC \
                     source for copyMethod: Snapshot/Clone"
                .to_string(),
        });
    }
    if spec.sources.iter().any(|s| s.pvc_selector.is_some()) {
        errs.push(ValidationError::InvalidFieldValue {
            field: overrides,
            reason: "pvcSelector sources are not CSI-staged (staging applies to single-PVC \
                     sources only), so a staged-PVC override would be silently ignored; \
                     remove the override(s) or use a `pvc:` source"
                .to_string(),
        });
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
    if let Err(e) = validate_jitter("spec.schedule.jitter", spec.schedule.jitter.as_deref()) {
        errs.push(e);
    }
    errs
}
