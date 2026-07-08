use super::*;
use crate::error::{ValidationError, ValidationResult};
use crate::restore::{RestoreSource, RestoreSpec, RestoreTarget};

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
                    "{wt:?} is not a valid Go-style duration; use a positive number with an \
                     s/m/h suffix (e.g. 90s, 5m, 1h) — how long the restore waits for the \
                     source snapshot to appear before applying onMissingSnapshot"
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
                     ({deadline}s): the wait for the source snapshot is polled inside the \
                     restore Job, so a waitTimeout at or beyond the deadline would let the \
                     Job be killed before onMissingSnapshot applies. Lower waitTimeout or \
                     raise activeDeadlineSeconds.",
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
            if let Some(m) = &spec.mover
                && m.inherit_security_context_from.is_some()
            {
                return Err(ValidationError::InvalidFieldValue {
                    field: "restore.mover.inheritSecurityContextFrom".to_string(),
                    reason: "is not allowed with target.populator: no workload pod exists at \
                             provision time to inherit a security context from; set \
                             mover.securityContext explicitly or rely on the repository's \
                             moverDefaults instead"
                        .to_string(),
                });
            }
        }
        RestoreTarget::Pvc(_) | RestoreTarget::PvcRef(_) => {}
    }
    // `pvcConsumer` derives the workload from a *backup source* PVC; a restore has no such
    // source (it writes a target whose consumer may not exist yet), so it is backup-only.
    if let Some(m) = &spec.mover {
        forbid_pvc_consumer(
            m,
            "restore",
            "Use inheritSecurityContextFrom.workloadSelector (the pod that will read the restored \
             data), or an explicit mover.securityContext, instead.",
        )?;
        validate_mover(m, "Restore mover")?;
    }
    Ok(())
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
                "{s:?} is not an RFC3339 timestamp; use e.g. 2026-05-01T00:00:00Z \
                 (the newest snapshot at or before this instant is restored)"
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
        && let Some(e) = require_min("Restore spec.options.parallel", p.into(), 1)
    {
        errs.push(e);
    }
    errs
}
