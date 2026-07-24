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
use crate::common::{FailurePolicy, MoverSpec, PvcAccessMode, RepositoryMode};
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
            Some(nfs) => {
                // `readOnly: false` exists for exactly one purpose — letting the kubelet
                // apply `fsGroup` to the source — and the kubelet does not apply `fsGroup`
                // to in-tree NFS volumes at all. So on NFS it buys nothing and only makes
                // the export writable to the mover. Reject it rather than ship a knob that
                // silently does the opposite of what its user wants.
                if !crate::snapshot_policy::source_read_only(source) {
                    return Err(ValidationError::InvalidFieldValue {
                        field: "spec.sources[].readOnly".to_string(),
                        reason: "readOnly: false is not supported on an nfs source: the kubelet \
                                 does not apply fsGroup to in-tree NFS volumes, so a read-write \
                                 mount grants the mover no additional readability and only \
                                 exposes the export to writes. Remove readOnly (NFS is read \
                                 directly), and grant access with mover.podSecurityContext \
                                 supplementalGroups / mover.securityContext runAsUser matching \
                                 the export's ownership, or with a server-side ID remap"
                            .to_string(),
                    });
                }
                validate_nfs_volume(nfs, "snapshot source")
            }
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

/// Validate a PVC access-modes list wherever one appears (`spec.staging.accessModes`,
/// `restore.target.pvc.accessModes`). Three rules, one place, both callers (webhook
/// at admission, controller defensively):
///
///   * every entry must be canonical — an [`PvcAccessMode::Unknown`] value is either
///     a legacy stored string from before schema enforcement or a typo, and no PVC
///     could ever be provisioned from it;
///   * no duplicates;
///   * `ReadWriteOncePod` must be the **sole** mode — the apiserver rejects the
///     combination at PVC-create time, so catching it here fails at admission with
///     the reason instead of wedging the first run in a create-retry loop.
///
/// `field` names the exact path for the message. Accumulates so every bad entry is
/// reported in one apply.
pub fn validate_access_modes(field: &str, modes: &[PvcAccessMode]) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (i, mode) in modes.iter().enumerate() {
        if let PvcAccessMode::Unknown(value) = mode {
            errs.push(ValidationError::InvalidFieldValue {
                field: format!("{field}[{i}]"),
                reason: format!(
                    "{value:?} is not a Kubernetes access mode (valid: {}). No PVC can be \
                     provisioned from it — if this value was stored before kopiur enforced \
                     the schema, it was already broken then; edit the resource to one of the \
                     valid modes.",
                    PvcAccessMode::CANONICAL.join(", ")
                ),
            });
        }
        if !seen.insert(mode.mode_str().to_string()) {
            errs.push(ValidationError::InvalidFieldValue {
                field: format!("{field}[{i}]"),
                reason: format!(
                    "duplicate access mode {:?}; list each mode at most once",
                    mode.mode_str()
                ),
            });
        }
    }
    if modes.len() > 1
        && modes
            .iter()
            .any(|m| matches!(m, PvcAccessMode::ReadWriteOncePod))
    {
        errs.push(ValidationError::InvalidFieldValue {
            field: field.to_string(),
            reason: "ReadWriteOncePod may not be combined with other access modes — the \
                     apiserver rejects such a PVC at create time, so the run would wedge in \
                     a retry loop instead of failing here. Use ReadWriteOncePod alone, or \
                     drop it."
                .to_string(),
        });
    }
    errs
}

/// A numeric knob must be at least `min` — the shared one-liner behind every
/// `Option<u32>` count / `Option<i64>` bytes-per-second field (e.g.
/// `RepositoryReplication.spec.sync.parallel`), so the rule and its message
/// shape are written once instead of re-derived per field. `field` names the
/// exact path for the message (e.g. `"RepositoryReplication spec.sync.parallel"`).
/// Callers only invoke this for a `Some` value — an absent knob is always valid
/// and never reaches this helper.
pub fn require_min(field: &str, value: i64, min: i64) -> Option<ValidationError> {
    (value < min).then(|| ValidationError::InvalidFieldValue {
        field: field.to_string(),
        reason: format!("must be >= {min} (got {value})"),
    })
}

/// Validate a `MoverSpec`. `context` names the owning resource for the message (e.g.
/// `"Restore mover"`).
///
/// `inheritSecurityContextFrom` and the explicit `securityContext`/`podSecurityContext` are
/// **compatible**, not mutually exclusive: they are adjacent layers of the merge ladder
/// (`hardened ⊂ moverDefaults ⊂ inherited ⊂ explicit`), so the explicit context overrides the
/// inherited one field-wise and fills whatever the workload does not pin — and stands in alone
/// when inheritance cannot resolve a pod.
///
/// This pair used to be rejected here, on the rationale that "the mover's effective contexts
/// must have a single, unambiguous source so the privileged-mover gate runs on exactly one".
/// That rationale was never true: the gate has always evaluated the *merged* product of
/// hardened + `moverDefaults` + recipe (see the callers of
/// [`crate::common::requires_privilege_resolved`]), which
/// [`crate::invariants::enforce_security_context_invariants`] normalizes first — INV-1 exists
/// precisely to reconcile an inherited `runAsUser: 0` against the hardened `runAsNonRoot:
/// true`. Merging one more layer in cannot smuggle an elevated mover past it.
pub fn validate_mover(mover: &MoverSpec, context: &str) -> ValidationResult {
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
    match crate::jitter::cron_parser().parse(&probe) {
        Ok(_) => Ok(()),
        Err(e) => Err(ValidationError::InvalidCron {
            expr: expr.to_string(),
            reason: e.to_string(),
        }),
    }
}

/// A non-blocking admission WARNING (never a rejection) when a schedule's cron
/// fires more often than hourly (issue #249). Every fire creates one per-run
/// `Snapshot` CR per source, and they accumulate up to the `SnapshotPolicy`
/// retention window — each terminal one is then re-reconciled for that whole
/// window — so a sub-hourly cadence with a wide (or absent) retention can produce
/// thousands of CRs. Sub-hourly is legitimate for some workloads, so this is a
/// footgun heads-up, not a block.
///
/// Pure and cron-only: the schedule webhook is client-less and can't read the
/// referenced policy's retention, so the message states the cadence and the
/// CR-count relationship rather than an exact number. `None` for an hourly-or-slower
/// cadence, or a cron that doesn't parse (that error is surfaced by [`validate_cron`]).
pub fn schedule_cr_growth_warning(cron: &str) -> Option<String> {
    let fires = schedule_fires_per_hour(cron)?;
    if fires <= 1 {
        return None;
    }
    Some(format!(
        "schedule fires ~{fires}×/hour: each fire creates one Snapshot CR per source, and \
         they accumulate up to the SnapshotPolicy retention window (CR count ≈ fires × \
         retained snapshots). A sub-hourly schedule with a wide or absent retention can \
         produce thousands of Snapshot CRs, each re-reconciled for its whole retention \
         window. If unintended, use a coarser schedule, bound SnapshotPolicy.spec.retention, \
         or set the Snapshot deletionPolicy to Retain/Orphan. See docs/backups.md \
         ('How many Snapshot CRs will I have?')."
    ))
}

/// Count how many times `cron` fires within one representative active hour. Pure and
/// clock-free — anchored on the cron's FIRST fire from a fixed instant (not a fixed
/// calendar hour), so a day-of-week / day-of-month constrained cron is measured
/// during an hour it is actually active. `H` tokens are substituted to a fixed value
/// first (they pick a minute within the window, not the cadence, so the count is
/// H-independent). `None` if the cron doesn't parse.
fn schedule_fires_per_hour(cron: &str) -> Option<u32> {
    use chrono::{Duration, TimeZone, Utc};
    let probe = cron
        .split_whitespace()
        .map(|f| if f == "H" { "0" } else { f })
        .collect::<Vec<_>>()
        .join(" ");
    let parsed = crate::jitter::cron_parser().parse(&probe).ok()?;
    let anchor = Utc.with_ymd_and_hms(2001, 1, 1, 0, 0, 0).single()?;
    let first = parsed.find_next_occurrence(&anchor, true).ok()?;
    let horizon = first + Duration::hours(1);
    let mut cursor = first;
    let mut count = 0u32;
    // The cron grammar has no seconds field, so at most 60 fires/hour; cap defensively.
    for _ in 0..61 {
        let next = parsed.find_next_occurrence(&cursor, true).ok()?;
        if next >= horizon {
            break;
        }
        count += 1;
        cursor = next + Duration::seconds(1);
    }
    Some(count)
}

/// Validate an optional Go-style `jitter` duration (`30m`, `1h`, …) against the SAME
/// parser the controller uses at scheduling time, so a typo or an out-of-range value
/// is rejected at apply time rather than silently degrading to *no jitter* at the
/// next reconcile (`parse_go_duration` returns `None`, which the schedule treats as a
/// zero offset). `None` (no jitter) is always valid. `field` names the path for the
/// error message (e.g. `spec.schedule.jitter`).
pub fn validate_jitter(field: &str, jitter: Option<&str>) -> ValidationResult {
    if let Some(j) = jitter
        && crate::duration::parse_go_duration(j).is_none()
    {
        return Err(ValidationError::InvalidFieldValue {
            field: field.to_string(),
            reason: format!(
                "{j:?} is not a valid duration. Use a Go-style duration like 30s, 5m, or 1h"
            ),
        });
    }
    Ok(())
}

/// Validate an optional IANA timezone name against the same `chrono-tz` database the
/// controller uses at scheduling time, so a typo (e.g. `America/Chicgo`) is rejected at
/// apply time rather than silently resolving to UTC at the next reconcile. `None` (use
/// the controller default) is always valid.
///
/// ```
/// use kopiur_api::validate::validate_timezone;
///
/// assert!(validate_timezone(None).is_ok());
/// assert!(validate_timezone(Some("America/Chicago")).is_ok());
/// assert!(validate_timezone(Some("UTC")).is_ok());
/// assert!(validate_timezone(Some("America/Chicgo")).is_err());
/// ```
pub fn validate_timezone(name: Option<&str>) -> ValidationResult {
    match name {
        None => Ok(()),
        Some(tz) if tz.parse::<chrono_tz::Tz>().is_ok() => Ok(()),
        Some(tz) => Err(ValidationError::InvalidTimezone {
            name: tz.to_string(),
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

/// `inheritSecurityContextFrom.snapshot` reproduces the identity RECORDED on a backup
/// (`Snapshot.status.recorded`), so it only makes sense where a snapshot is being consumed —
/// a `Restore`. A backup's identity comes from the live workload and maintenance touches no
/// snapshot at all, so those kinds reject the variant at admission rather than fail (or,
/// worse, silently no-op) at runtime. `field_prefix` names the owning kind (e.g.
/// `"snapshotPolicy"`/`"maintenance"`), `reason` is the kind-appropriate what/why/fix.
pub(crate) fn forbid_snapshot_inherit(
    mover: &MoverSpec,
    field_prefix: &str,
    reason: &str,
) -> ValidationResult {
    if matches!(
        mover.inherit_security_context_from,
        Some(crate::common::InheritSecurityContextFrom::Snapshot(_))
    ) {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{field_prefix}.mover.inheritSecurityContextFrom.snapshot"),
            reason: reason.to_string(),
        });
    }
    Ok(())
}

/// Reject `inheritSecurityContextFrom` **entirely**, for a kind whose reconciler never resolves
/// it. Stronger than [`forbid_pvc_consumer`]: that rejects one variant on a kind that *does*
/// honor the other, this rejects the whole field on a kind that honors none of it.
///
/// Accepting a field and then ignoring it is the failure mode this repo exists to design out —
/// the manifest says the mover runs as the workload, the mover runs as something else, and
/// nothing says otherwise. If a kind cannot honor it, admission must say so.
pub(crate) fn forbid_inherit(
    mover: &MoverSpec,
    field_prefix: &str,
    reason: &str,
) -> ValidationResult {
    if mover.inherit_security_context_from.is_some() {
        return Err(ValidationError::InvalidFieldValue {
            field: format!("{field_prefix}.mover.inheritSecurityContextFrom"),
            reason: reason.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
