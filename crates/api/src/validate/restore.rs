use super::*;
use crate::error::{ValidationError, ValidationResult};
use crate::restore::{PathSelection, RestoreSource, RestoreSpec, RestoreTarget};

/// A `Restore` spec is internally consistent (ADR §3.6/§4.6 / ADR-0005 §9).
///
/// The externally-tagged `RestoreSource`/`RestoreTarget` enums already guarantee
/// **exactly one** variant — that is a compile-time/serde invariant, not re-checked
/// here (a `Restore` with no `target` now fails to deserialize entirely, ADR-0005 §9).
/// We validate the cross-field rules the enums can't express:
/// - `source.identity` requires `spec.repository` (nothing else can derive it).
/// - if `target: pvc`, the template must name the PVC (`name` non-empty).
/// - `target: populator` forbids `mover.inheritSecurityContextFrom`: no workload pod
///   exists at provision time to inherit from (ADR-0005 §9 / ADR §4.7) — point the
///   user at `moverDefaults` / an explicit `securityContext` instead.
pub fn validate_restore(spec: &RestoreSpec) -> ValidationResult {
    // Exactly-one-variant on `source`/`target` is guaranteed by the enums; see
    // RestoreSource / RestoreTarget (both required, externally tagged).
    if matches!(spec.source, RestoreSource::Identity(_)) && spec.repository.is_none() {
        return Err(ValidationError::RestoreSourceRepositoryRequired);
    }
    // `asOf` / `waitTimeout` are parsed at reconcile time with the SAME parsers
    // used here, so a value the webhook admits can never fail to parse later.
    // Exhaustive over the source so a new variant must declare its rules.
    match &spec.source {
        RestoreSource::SnapshotRef(_) => {}
        RestoreSource::FromPolicy(c) => {
            validate_as_of("restore.source.fromPolicy.asOf", c.as_of.as_deref())?;
            // The per-PVC source-path override (#443). Same shape check as
            // `IdentitySource::sourcePath` and as the resolved path the identity
            // kernel emits, so a value the webhook admits can never be rejected
            // later by `resolve_identity`'s own `validate_source_path`.
            if let Some(p) = c.source_path.as_deref() {
                crate::validate::validate_source_path("restore.source.fromPolicy.sourcePath", p)?;
            }
        }
        RestoreSource::Identity(i) => {
            validate_as_of("restore.source.identity.asOf", i.as_of.as_deref())?;
            // `snapshotID` pins an exact snapshot — combining it with the
            // relative selectors would silently ignore one of them.
            if i.snapshot_id.is_some() && i.as_of.is_some() {
                return Err(ValidationError::MutuallyExclusive {
                    a: "source.identity.snapshotID".to_string(),
                    b: "source.identity.asOf".to_string(),
                    context: "snapshotID pins an exact snapshot; asOf selects by time".to_string(),
                });
            }
            if i.snapshot_id.is_some() && i.offset.is_some_and(|o| o != 0) {
                return Err(ValidationError::MutuallyExclusive {
                    a: "source.identity.snapshotID".to_string(),
                    b: "source.identity.offset".to_string(),
                    context: "snapshotID pins an exact snapshot; offset selects by position"
                        .to_string(),
                });
            }
        }
    }
    if let Some(wt) = spec.policy.as_ref().and_then(|p| p.wait_timeout.as_deref()) {
        let Some(wait) = crate::duration::parse_go_duration(wt) else {
            return Err(ValidationError::InvalidFieldValue {
                field: "restore.policy.waitTimeout".to_string(),
                reason: format!(
                    "{wt:?} is not a valid duration. Fix: use a Go-style duration like 90s, 5m, \
                     or 1h"
                ),
            });
        };
        // For an object-store `fromPolicy`/`identity` restore the wait is polled
        // INSIDE the mover Job, so it must fit within an explicit
        // `activeDeadlineSeconds` (else the Job is killed mid-wait, before
        // onMissingSnapshot applies). When the deadline is unset the controller's
        // generous default (hours) always dwarfs a sane waitTimeout, so this only
        // guards an explicit, too-small deadline. `snapshotRef` waits in the
        // controller, but the same bound is harmless and keeps the rule simple.
        if let Some(deadline) = spec
            .failure_policy
            .as_ref()
            .and_then(|f| f.active_deadline_seconds)
            && deadline > 0
            && wait.as_secs() as i64 >= deadline
        {
            return Err(ValidationError::InvalidFieldValue {
                field: "restore.policy.waitTimeout".to_string(),
                reason: format!(
                    "{wt:?} ({}s) must be shorter than failurePolicy.activeDeadlineSeconds \
                     ({deadline}s), or the restore Job is killed before onMissingSnapshot \
                     applies. Fix: lower waitTimeout or raise activeDeadlineSeconds",
                    wait.as_secs()
                ),
            });
        }
    }
    match &spec.target {
        RestoreTarget::Pvc(t) if t.name.trim().is_empty() => {
            return Err(ValidationError::MissingRequiredField {
                field: "restore.target.pvc.name".to_string(),
            });
        }
        // `target.pvc` makes the operator CREATE the PVC, so it must know the
        // size — a guessed default could be smaller than the restored data.
        RestoreTarget::Pvc(t) if t.capacity.as_deref().is_none_or(|c| c.trim().is_empty()) => {
            return Err(ValidationError::MissingRequiredField {
                field: "restore.target.pvc.capacity".to_string(),
            });
        }
        RestoreTarget::Populator(_) => {
            // Exhaustive over the inherit variant: the live-pod modes cannot work
            // (no workload pod exists at provision time), but `snapshot` reads the
            // backup's RECORDED identity in the controller before the Job — it needs
            // no live pod, so it is deliberately ALLOWED with a populator target.
            if let Some(m) = &spec.mover {
                use crate::common::InheritSecurityContextFrom;
                match &m.inherit_security_context_from {
                    None | Some(InheritSecurityContextFrom::Snapshot(_)) => {}
                    Some(InheritSecurityContextFrom::WorkloadSelector(_))
                    | Some(InheritSecurityContextFrom::PvcConsumer(_)) => {
                        return Err(ValidationError::InvalidFieldValue {
                            field: "restore.mover.inheritSecurityContextFrom".to_string(),
                            reason: "is not allowed with target.populator (no workload pod \
                                     exists yet). Fix: use inheritSecurityContextFrom: \
                                     { snapshot: {} }, or set mover.securityContext explicitly"
                                .to_string(),
                        });
                    }
                }
            }
        }
        // A stream target writes no PVC: it pipes ONE virtual file out of the
        // snapshot into a command's stdin. Validate the same things the backup-side
        // stream source validates, with the same words.
        RestoreTarget::StreamExec(t) => {
            crate::validate::validate_stream_file_name(
                "restore.target.streamExec.fileName",
                &t.file_name,
            )?;
            crate::validate::validate_stream_exec(
                "restore.target.streamExec.workloadExec",
                &t.workload_exec,
            )?;
        }
        RestoreTarget::Pvc(_) | RestoreTarget::PvcRef(_) => {}
    }
    // Partial restore (#689). Exhaustive over the target: a stream target already
    // selects exactly one virtual file (`fileName`), so a path selection on top of
    // it would be silently ignored — reject it instead.
    if let Some(sel) = &spec.selection {
        match &spec.target {
            RestoreTarget::StreamExec(_) => {
                return Err(ValidationError::MutuallyExclusive {
                    a: "restore.selection".to_string(),
                    b: "restore.target.streamExec".to_string(),
                    context: "streamExec already restores exactly one file (fileName); \
                              Fix: drop spec.selection, or restore into a pvcRef/pvc target"
                        .to_string(),
                });
            }
            RestoreTarget::Pvc(_) | RestoreTarget::PvcRef(_) | RestoreTarget::Populator(_) => {}
        }
        validate_path_selection(sel)?;
    }
    // Access modes on a create-target PVC: canonical/unique/RWOP-sole. Fail-fast on
    // the first problem (this validator's contract); the accumulate wrapper reports
    // the rest. A legacy stored value decodes as `PvcAccessMode::Unknown` (never a
    // watcher-poisoning serde error) and is rejected HERE, per-CR, with the value
    // quoted — the controller calls this defensively on every reconcile, so the
    // rejection reaches the user as a Warning Event + structural backoff.
    if let RestoreTarget::Pvc(t) = &spec.target
        && let Some(e) = validate_access_modes("restore.target.pvc.accessModes", &t.access_modes)
            .into_iter()
            .next()
    {
        return Err(e);
    }
    // `pvcConsumer` derives the workload from a *backup source* PVC; a restore has no such
    // source (it writes a target whose consumer may not exist yet), so it is backup-only.
    if let Some(m) = &spec.mover {
        forbid_pvc_consumer(
            m,
            "restore",
            "Fix: use inheritSecurityContextFrom.workloadSelector, or set \
             mover.securityContext explicitly",
        )?;
        validate_mover(m, "Restore mover")?;
    }
    Ok(())
}

/// Upper bound on `spec.selection.paths` entries — mirrors the CRD schema's
/// `maxItems` so the webhook and the API server agree.
pub const MAX_SELECTION_PATHS: usize = 64;
/// Upper bound on one `spec.selection.paths` entry, in bytes (Linux `PATH_MAX`);
/// mirrors the schema's `maxLength`.
pub const MAX_SELECTION_PATH_BYTES: usize = 4096;
/// Upper bound on one path segment, in bytes (Linux `NAME_MAX`).
const MAX_SELECTION_SEGMENT_BYTES: usize = 255;

/// A partial-restore selection is safe to join onto the target mount.
///
/// Each path must be relative, `/`-separated and normalized — no leading or
/// trailing `/`, no empty, `.` or `..` segment, no control character — so that
/// `<target>/<path>` can never resolve outside the target (`..`) and two
/// spellings of one path cannot both be selected. Duplicates and a path nested
/// inside another selected path are rejected too: selecting `docs` already
/// restores `docs/report.odt`, and restoring one tree twice is never what was
/// meant.
///
/// ```
/// use kopiur_api::restore::PathSelection;
/// use kopiur_api::validate::validate_path_selection;
///
/// let ok = PathSelection { paths: vec!["docs/report.odt".into(), "music/album".into()] };
/// assert!(validate_path_selection(&ok).is_ok());
///
/// let escape = PathSelection { paths: vec!["../etc/passwd".into()] };
/// assert!(validate_path_selection(&escape).is_err());
/// ```
pub fn validate_path_selection(sel: &PathSelection) -> ValidationResult {
    if sel.paths.is_empty() {
        return Err(ValidationError::MissingRequiredField {
            field: "restore.selection.paths (at least one path; omit spec.selection to \
                    restore the whole snapshot)"
                .to_string(),
        });
    }
    if sel.paths.len() > MAX_SELECTION_PATHS {
        return Err(ValidationError::InvalidFieldValue {
            field: "restore.selection.paths".to_string(),
            reason: format!(
                "{} paths selected, more than the {MAX_SELECTION_PATHS} allowed. Fix: select \
                 a common parent directory instead, or split into several Restores",
                sel.paths.len()
            ),
        });
    }
    for (index, path) in sel.paths.iter().enumerate() {
        if let Some(reason) = selection_path_problem(path) {
            return Err(ValidationError::RestoreSelectionPathInvalid {
                index,
                path: path.clone(),
                reason,
            });
        }
    }
    for (index, path) in sel.paths.iter().enumerate() {
        for (other_index, other) in sel.paths.iter().enumerate() {
            if index == other_index {
                continue;
            }
            if path == other && other_index < index {
                return Err(ValidationError::RestoreSelectionPathInvalid {
                    index,
                    path: path.clone(),
                    reason: format!(
                        "duplicates spec.selection.paths[{other_index}]. Fix: list each path once"
                    ),
                });
            }
            if path.len() > other.len()
                && path.starts_with(other.as_str())
                && path.as_bytes()[other.len()] == b'/'
            {
                return Err(ValidationError::RestoreSelectionPathInvalid {
                    index,
                    path: path.clone(),
                    reason: format!(
                        "is inside spec.selection.paths[{other_index}] = {other:?}, which \
                         already restores it. Fix: drop the nested path"
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Why one selection path is unusable, or `None` when it is fine. Pure so the
/// per-shape rules are unit-testable in isolation.
fn selection_path_problem(path: &str) -> Option<String> {
    if path.is_empty() {
        return Some("is empty. Fix: name a file or directory inside the snapshot".to_string());
    }
    if path.len() > MAX_SELECTION_PATH_BYTES {
        return Some(format!(
            "is {} bytes, longer than the {MAX_SELECTION_PATH_BYTES}-byte limit",
            path.len()
        ));
    }
    if let Some(c) = path.chars().find(|c| c.is_control()) {
        return Some(format!("contains the control character {c:?}"));
    }
    if path.starts_with('/') {
        return Some(format!(
            "is absolute. Fix: make it relative to the snapshot root, e.g. {:?}",
            path.trim_start_matches('/')
        ));
    }
    if path.ends_with('/') {
        return Some(format!(
            "ends with '/'. Fix: drop the trailing slash, e.g. {:?}",
            path.trim_end_matches('/')
        ));
    }
    for segment in path.split('/') {
        let problem = match segment {
            "" => Some("has an empty segment ('//'). Fix: use a single '/' between names"),
            "." => Some("has a '.' segment. Fix: remove it, paths must be normalized"),
            ".." => Some(
                "has a '..' segment, which could escape the restore target. Fix: name the path \
                 from the snapshot root without '..'",
            ),
            _ => None,
        };
        if let Some(p) = problem {
            return Some(p.to_string());
        }
        if segment.len() > MAX_SELECTION_SEGMENT_BYTES {
            return Some(format!(
                "has a {}-byte name, longer than the {MAX_SELECTION_SEGMENT_BYTES}-byte \
                 file-name limit",
                segment.len()
            ));
        }
    }
    None
}

/// An `asOf` point-in-time selector must be a valid RFC3339 timestamp — the
/// reconciler parses it with `chrono::DateTime::parse_from_rfc3339`, so the
/// webhook rejects anything that parser would choke on, with a fix in the message.
fn validate_as_of(field: &str, as_of: Option<&str>) -> ValidationResult {
    if let Some(s) = as_of
        && chrono::DateTime::parse_from_rfc3339(s).is_err()
    {
        return Err(ValidationError::InvalidFieldValue {
            field: field.to_string(),
            reason: format!(
                "{s:?} is not an RFC3339 timestamp. Fix: use e.g. 2026-05-01T00:00:00Z"
            ),
        });
    }
    Ok(())
}

/// Validate a `Restore` spec, accumulating all problems (wraps the fail-fast
/// [`validate_restore`] for caller symmetry).
pub fn validate_restore_spec(spec: &RestoreSpec) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Some(r) = &spec.repository
        && let Err(e) = validate_repository_ref(r)
    {
        errs.push(e);
    }
    if let Err(e) = validate_restore(spec) {
        errs.push(e);
    }
    if let Some(m) = &spec.mover
        && let Err(e) = validate_mover(m, "Restore mover")
    {
        errs.push(e);
    }
    if let Some(fp) = &spec.failure_policy
        && let Err(e) = validate_failure_policy(fp, "Restore")
    {
        errs.push(e);
    }
    if let Some(o) = &spec.options
        && let Some(p) = o.parallel
        && let Some(e) = require_min(
            "Restore spec.options.parallel",
            p.into(),
            NumericBound::Count,
        )
    {
        errs.push(e);
    }
    errs
}
