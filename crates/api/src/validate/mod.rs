//! Cross-field validation the type system can't express (ADR §2.2 principle 8).
//!
//! These are the rules a single struct's types can't enforce: "field X is
//! forbidden only when sibling Y has a particular variant," "this string must
//! parse as a cron," "a discovered backup may only Retain." They live here as pure
//! functions so the **webhook calls them at admission and the controller calls them
//! defensively** — one validator, two callers (SKILL hard-rule 4). No `kube::Client`,
//! no `tokio`.
//!
//! ## Fail-fast vs. accumulate (see [`crate::error`])
//!
//! Single-rule helpers return [`ValidationResult`] (fail-fast — first problem).
//! The per-CRD aggregate validators (`validate_backup_config`, …) return
//! `Vec<ValidationError>` so a user sees every independent problem in one apply.
//! An empty vec means valid.

use crate::backend::NfsVolume;
use crate::common::{FailurePolicy, MoverSpec, RepositoryMode};
use crate::error::{ValidationError, ValidationResult};
use crate::server::{ServerAuth, ServerSpec};
use crate::snapshot_policy::Source;
use k8s_openapi::api::core::v1::ResourceRequirements;
use kube_quantity::ParsedQuantity;

mod backend;
mod identity;
mod repository;
mod restore;
mod snapshot;

pub use backend::*;
pub use identity::*;
pub use repository::*;
pub use restore::*;
pub use snapshot::*;

/// A single backup `Source` is well-formed: **exactly one** of `pvc`,
/// `pvcSelector`, or `nfs` is set (ADR §3.3 — modeled as sibling Options because
/// the forms share `sourcePath*` keys, so it's a webhook check, not an enum). When
/// the source is `nfs`, its server/path are also validated.
pub fn validate_source(source: &Source) -> ValidationResult {
    let set: Vec<&str> = [
        ("pvc", source.pvc.is_some()),
        ("pvcSelector", source.pvc_selector.is_some()),
        ("nfs", source.nfs.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect();

    match set.as_slice() {
        [] => Err(ValidationError::MissingRequiredField {
            field: "source.pvc, source.pvcSelector, or source.nfs".to_string(),
        }),
        [first, second, ..] => Err(ValidationError::MutuallyExclusive {
            a: (*first).to_string(),
            b: (*second).to_string(),
            context: "snapshot source".to_string(),
        }),
        [_only] => match &source.nfs {
            Some(nfs) => validate_nfs_volume(nfs, "snapshot source"),
            None => Ok(()),
        },
    }
}

/// An inline [`NfsVolume`] is well-formed: a non-empty server and an absolute
/// export path. The structural schema can't express either, so the webhook does.
/// `context` names where it appears (e.g. `"snapshot source"`, `"filesystem repo"`)
/// for an actionable message.
pub fn validate_nfs_volume(nfs: &NfsVolume, context: &str) -> ValidationResult {
    if nfs.server.trim().is_empty() {
        return Err(ValidationError::MissingRequiredField {
            field: format!("{context} nfs.server"),
        });
    }
    if !nfs.path.starts_with('/') {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{context} nfs.path"),
            reason: format!(
                "must be an absolute export path beginning with '/' (got {:?})",
                nfs.path
            ),
        });
    }
    Ok(())
}

/// Validate a `MoverSpec`. `inheritSecurityContextFrom` copies **both** the workload
/// pod's container and pod security contexts, so it is **mutually exclusive** with
/// **both** explicit `securityContext` and `podSecurityContext`: the mover's effective
/// contexts must have a single, unambiguous source so the privileged-mover gate runs on
/// exactly one. `context` names the owning resource for the message (e.g. `"Restore
/// mover"`).
pub fn validate_mover(mover: &MoverSpec, context: &str) -> ValidationResult {
    if mover.inherit_security_context_from.is_some() {
        if mover.security_context.is_some() {
            return Err(ValidationError::MutuallyExclusive {
                a: "mover.securityContext".to_string(),
                b: "mover.inheritSecurityContextFrom".to_string(),
                context: context.to_string(),
            });
        }
        if mover.pod_security_context.is_some() {
            return Err(ValidationError::MutuallyExclusive {
                a: "mover.podSecurityContext".to_string(),
                b: "mover.inheritSecurityContextFrom".to_string(),
                context: context.to_string(),
            });
        }
    }
    if let Some(resources) = &mover.resources {
        validate_resources(resources, context)?;
    }
    Ok(())
}

/// The first resource key whose `requests` value exceeds its `limits` value (both present
/// and parseable), as `(key, request, limit)`. Quantity comparison uses `kube_quantity`'s
/// `ParsedQuantity` (the same `k8s-openapi` `Quantity` type the cluster uses), so
/// `"1Gi" > "512Mi"` is compared correctly across binary/SI/milli suffixes. **Best-effort:**
/// a key whose quantity fails to parse is skipped, never a false rejection.
fn requests_exceeding_limits(resources: &ResourceRequirements) -> Option<(String, String, String)> {
    let (Some(requests), Some(limits)) = (resources.requests.as_ref(), resources.limits.as_ref())
    else {
        return None;
    };
    for (key, req) in requests {
        let Some(lim) = limits.get(key) else { continue };
        let (Ok(req_p), Ok(lim_p)) = (ParsedQuantity::try_from(req), ParsedQuantity::try_from(lim))
        else {
            continue;
        };
        if req_p > lim_p {
            return Some((key.clone(), req.0.clone(), lim.0.clone()));
        }
    }
    None
}

/// Validate that a `ResourceRequirements` has no `requests > limits` for any key. A pod with
/// `requests > limits` is **rejected by the API server**, so the mover Job never creates a
/// pod and the run hangs — the same silent-wedge class as an impossible securityContext.
/// `context` names the owner (e.g. `"SnapshotPolicy mover"`).
pub fn validate_resources(resources: &ResourceRequirements, context: &str) -> ValidationResult {
    if let Some((key, req, lim)) = requests_exceeding_limits(resources) {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{context} resources.requests.{key}"),
            reason: format!(
                "request `{req}` exceeds limit `{lim}`; the API server rejects a pod whose \
                 requests exceed its limits, so the mover Job would never create a pod (it hangs \
                 instead of failing). Lower the request or raise the limit."
            ),
        });
    }
    Ok(())
}

/// Validate a [`FailurePolicy`]'s numeric fields are sane: `activeDeadlineSeconds` and
/// `podStartupDeadlineSeconds` must be positive (the kubelet rejects a non-positive Job
/// deadline, and a non-positive grace would fail every pod on its first reconcile);
/// `backoffLimit` must be non-negative. `context` names the owner (e.g. `"Snapshot"`).
pub fn validate_failure_policy(fp: &FailurePolicy, context: &str) -> ValidationResult {
    if let Some(d) = fp.active_deadline_seconds
        && d <= 0
    {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{context} failurePolicy.activeDeadlineSeconds"),
            reason: format!("must be a positive number of seconds (got {d})"),
        });
    }
    if let Some(g) = fp.pod_startup_deadline_seconds
        && g <= 0
    {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{context} failurePolicy.podStartupDeadlineSeconds"),
            reason: format!(
                "must be a positive number of seconds (got {g}); it bounds how long a \
                 non-starting mover pod is tolerated before the run fails"
            ),
        });
    }
    if let Some(b) = fp.backoff_limit
        && b < 0
    {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{context} failurePolicy.backoffLimit"),
            reason: format!("must be >= 0 (got {b})"),
        });
    }
    Ok(())
}

/// A cron expression parses with the same parser the controller uses at runtime, so
/// bad expressions are rejected at apply time, not at first reconcile (ADR §4.1).
///
/// `croner` 2.x does not implement Jenkins-style `H`. Since kopiur resolves `H`
/// deterministically in [`crate::jitter::substitute_h`] (not in the parser), we
/// substitute every `H` field with the fixed placeholder `0` purely to validate the
/// expression's *shape* here. The real `H` spread is produced at scheduling time.
///
/// ```
/// use kopiur_api::validate::validate_cron;
/// use kopiur_api::ValidationError;
///
/// // Valid 5-field crons pass — including Jenkins-style `H` (resolved later).
/// assert!(validate_cron("0 2 * * *").is_ok());
/// assert!(validate_cron("H 2 * * *").is_ok());
///
/// // Garbage is rejected at apply time, not at first reconcile (ADR §4.1).
/// assert!(matches!(
///     validate_cron("not a cron"),
///     Err(ValidationError::InvalidCron { .. }),
/// ));
/// ```
pub fn validate_cron(expr: &str) -> ValidationResult {
    let probe = expr
        .split_whitespace()
        .map(|f| if f == "H" { "0" } else { f })
        .collect::<Vec<_>>()
        .join(" ");
    match croner::Cron::new(&probe).parse() {
        Ok(_) => Ok(()),
        Err(e) => Err(ValidationError::InvalidCron {
            expr: expr.to_string(),
            reason: e.to_string(),
        }),
    }
}

/// The shared `spec.server` rules the type system can't express (server addendum):
///   * `auth.insecure` requires `acknowledgeInsecure: true` — a no-auth server exposes
///     full read/read of the repository, so it must be explicit.
///   * `service.port` must be non-zero.
///   * `readOnly: false` is contradictory on a `mode: ReadOnly` repository — a ReadOnly
///     repo can never serve a writable UI, so the explicit denial is rejected (omitting
///     the field is fine; the mode forces read-only).
///
/// `mode` is the parent repository's [`RepositoryMode`] (both callers have it). Accumulates
/// so a user sees every server problem at once. The PVC `ReadWriteMany` requirement for
/// filesystem-backend servers is **not** here — it needs a live PVC read and is enforced
/// at reconcile, not admission.
pub fn validate_server(server: &ServerSpec, mode: RepositoryMode) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    if let Some(ServerAuth::Insecure(ack)) = &server.auth
        && !ack.acknowledge_insecure
    {
        errs.push(ValidationError::InsecureServerNotAcknowledged);
    }
    if let Some(service) = &server.service
        && service.port == Some(0)
    {
        errs.push(ValidationError::InvalidServerPort { port: 0 });
    }
    if server.read_only == Some(false) && !mode.allows_writes() {
        errs.push(ValidationError::InvalidFieldValue {
            field: "server.readOnly".to_string(),
            reason: "a Repository with spec.mode: ReadOnly cannot serve a read-write UI; remove \
                     server.readOnly (the ReadOnly mode forces the UI read-only) or set the \
                     repository's spec.mode: ReadWrite"
                .to_string(),
        });
    }
    if let Some(res) = &server.resources
        && let Err(e) = validate_resources(res, "server")
    {
        errs.push(e);
    }
    errs
}

/// `inheritSecurityContextFrom.pvcConsumer` derives the mover identity from a **backup
/// source** PVC's consumer; a kind that has no backup source (Restore, Maintenance) must
/// reject it at admission rather than fail at runtime. `field_prefix` names the owning kind
/// (e.g. `"restore"`/`"maintenance"`), `instead` is the kind-appropriate remedy.
pub(crate) fn forbid_pvc_consumer(
    mover: &MoverSpec,
    field_prefix: &str,
    instead: &str,
) -> ValidationResult {
    if matches!(
        mover.inherit_security_context_from,
        Some(crate::common::InheritSecurityContextFrom::PvcConsumer(_))
    ) {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{field_prefix}.mover.inheritSecurityContextFrom.pvcConsumer"),
            reason: format!(
                "is only valid for a backup source — there is no source PVC here to derive a \
                 workload from. {instead}"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
