use crate::common::IdentityDefaults;
use crate::error::{ValidationError, ValidationResult};
use crate::snapshot_policy::{SnapshotPolicySpec, Source};
use std::collections::BTreeMap;

/// An already-admitted `SnapshotPolicy`'s identity, keyed for collision detection
/// (ADR-0005 §6). `repo_key` is a normalized repository identity (e.g.
/// `"ClusterRepository/shared"` or `"Repository/backups/nas"`) so two policies are
/// "the same repository" only when their keys match; `name` is the policy's
/// `namespace/name` for the actionable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingIdentity {
    /// The other policy's resolved `username@hostname[:path]` identity string.
    pub identity: String,
    /// The other policy's normalized repository key.
    pub repo_key: String,
    /// `namespace/name` of the other policy (for the conflict message).
    pub name: String,
}

/// Detect whether a `SnapshotPolicy`'s resolved identity collides with an
/// already-admitted policy's identity **in the same repository** (ADR-0005 §6).
/// Pure so the decision is unit-tested; the webhook does the IO (list policies,
/// resolve each identity) and calls this. Returns the conflicting `namespace/name`
/// or `None`.
///
/// - `self_name` is the candidate's own `namespace/name`, skipped so a re-apply of
///   the same object never collides with itself.
/// - A collision requires BOTH the same `repo_key` AND the same `identity` string.
///
/// ```
/// use kopiur_api::validate::{detect_identity_collision, ExistingIdentity};
///
/// let existing = vec![ExistingIdentity {
///     identity: "pg@billing:/pvc/data".into(),
///     repo_key: "ClusterRepository/shared".into(),
///     name: "billing/pg-a".into(),
/// }];
/// // Same identity + same repo, different policy → collision.
/// assert_eq!(
///     detect_identity_collision("pg@billing:/pvc/data", "ClusterRepository/shared", "billing/pg-b", &existing),
///     Some("billing/pg-a".to_string()),
/// );
/// // Same identity but a DIFFERENT repository → no collision (separate snapshot history).
/// assert_eq!(
///     detect_identity_collision("pg@billing:/pvc/data", "Repository/billing/nas", "billing/pg-b", &existing),
///     None,
/// );
/// // Self (same name) is skipped.
/// assert_eq!(
///     detect_identity_collision("pg@billing:/pvc/data", "ClusterRepository/shared", "billing/pg-a", &existing),
///     None,
/// );
/// ```
pub fn detect_identity_collision(
    self_identity: &str,
    self_repo_key: &str,
    self_name: &str,
    existing: &[ExistingIdentity],
) -> Option<String> {
    existing
        .iter()
        .find(|e| e.name != self_name && e.repo_key == self_repo_key && e.identity == self_identity)
        .map(|e| e.name.clone())
}

// --- Identity shape validation (kopia username@hostname:path contract) -------

/// Generous byte cap for a single identity component. kopia imposes none; this only
/// bounds adversarial input (a hostname mirrors DNS's 253 here).
pub(crate) const IDENTITY_MAX_LEN: usize = 253;

/// The shape problem (if any) with a kopia identity `username`/`hostname` component.
/// kopia (`snapshot.ParseSourceInfo`) splits a source on the **first** `@` and
/// **first** `:` with no escaping, so an embedded delimiter silently reparses the
/// identity into a *different* one; whitespace and ASCII control characters survive
/// verbatim but make the identity un-typeable/un-findable on a later
/// `snapshot list --source`. This is the minimal shape rule — NOT a character class;
/// dots, dashes, slashes and unicode letters all pass.
fn identity_char_problem(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some("must not be empty".to_string());
    }
    if value.len() > IDENTITY_MAX_LEN {
        return Some(format!(
            "is {} bytes; the maximum is {IDENTITY_MAX_LEN}",
            value.len()
        ));
    }
    if value.contains('@') {
        return Some("must not contain '@' (kopia's username/hostname delimiter)".to_string());
    }
    if value.contains(':') {
        return Some("must not contain ':' (kopia's hostname/path delimiter)".to_string());
    }
    if let Some(c) = value.chars().find(|c| c.is_ascii_whitespace()) {
        return Some(format!("must not contain whitespace (found {c:?})"));
    }
    if let Some(c) = value.chars().find(|c| c.is_ascii_control()) {
        return Some(format!("must not contain control characters (found {c:?})"));
    }
    None
}

/// Validate a resolved kopia identity component (`username`/`hostname`). Shape-only
/// (see [`identity_char_problem`]); `field` names the surface for the message. Called
/// both from the static admission validator (on explicit overrides) and from
/// [`crate::resolve_identity`] (on the fully-resolved value, covering CEL results and
/// defaults), so a bad identity can never be pinned.
pub fn validate_identity_component(field: &str, value: &str) -> ValidationResult {
    match identity_char_problem(value) {
        None => Ok(()),
        Some(reason) => Err(ValidationError::IdentityComponentInvalid {
            field: field.to_string(),
            value: value.to_string(),
            reason,
        }),
    }
}

/// Maximum length of `Repository`/`ClusterRepository` `identityDefaults.cluster`.
/// A cluster identity is a short, human-chosen suffix appended onto a namespace
/// name — not free text — so this is generous headroom well under DNS's 253-byte
/// label ceiling, not a real constraint in practice.
pub const CLUSTER_NAME_MAX_LEN: usize = 32;

/// Validate a `Repository`/`ClusterRepository` `identityDefaults.cluster`: an RFC 1123 label
/// (`^[a-z0-9]([a-z0-9-]*[a-z0-9])?$`), 1..=[`CLUSTER_NAME_MAX_LEN`] characters,
/// with dots called out explicitly as forbidden even though a well-formed RFC
/// 1123 label never contains one anyway — the message needs to explain *why* to
/// whoever hits it: `cluster` is concatenated onto a namespace as
/// `<namespace>.<cluster>` for the default hostname (see
/// [`crate::identity::resolve_identity`]), and [`crate::identity::classify_hostname`]
/// splits that hostname back apart at the FIRST `.`, so a dot anywhere in
/// `cluster` would make that split ambiguous.
pub fn validate_cluster_name(value: &str) -> ValidationResult {
    if value.is_empty() {
        return Err(ValidationError::ClusterNameInvalid {
            value: value.to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    if value.len() > CLUSTER_NAME_MAX_LEN {
        return Err(ValidationError::ClusterNameInvalid {
            value: value.to_string(),
            reason: format!(
                "is {} characters; the maximum is {CLUSTER_NAME_MAX_LEN}",
                value.len()
            ),
        });
    }
    if value.contains('.') {
        return Err(ValidationError::ClusterNameInvalid {
            value: value.to_string(),
            reason: "must not contain '.' — the first '.' in a hostname is the \
                     namespace/cluster delimiter"
                .to_string(),
        });
    }
    let is_rfc1123_label = value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if !is_rfc1123_label {
        return Err(ValidationError::ClusterNameInvalid {
            value: value.to_string(),
            reason: "must be a lowercase RFC 1123 label: lowercase alphanumeric \
                     characters or '-', starting and ending with an alphanumeric \
                     character"
                .to_string(),
        });
    }
    Ok(())
}

/// Validate a kopia identity `sourcePath` (the part after the first `:`). Lenient:
/// spaces and `:` are allowed (only the first `:` is kopia's delimiter, and the rest
/// is the path verbatim), but the path must be non-empty and free of newlines / ASCII
/// control characters.
pub fn validate_source_path(field: &str, value: &str) -> ValidationResult {
    let reason = if value.is_empty() {
        Some("must not be empty when set".to_string())
    } else {
        value
            .chars()
            .find(|c| c.is_ascii_control())
            .map(|c| format!("must not contain control characters (found {c:?})"))
    };
    match reason {
        None => Ok(()),
        Some(reason) => Err(ValidationError::IdentitySourcePathInvalid {
            field: field.to_string(),
            value: value.to_string(),
            reason,
        }),
    }
}

// --- Fork-on-edit guard (re-identifying a policy with history orphans snapshots) ---

/// Pure decision for the fork-on-edit guard on a `username@hostname` change. Returns
/// `Some(IdentityWouldFork)` iff the policy has snapshot history, the change was not
/// acknowledged, and the resolved identity actually differs. The webhook does the IO
/// (read the old object's pinned identity + history, resolve the new identity) and
/// calls this.
///
/// ```
/// use kopiur_api::validate::detect_identity_fork;
///
/// // History + a real change + no ack → fork.
/// assert!(detect_identity_fork("pg@billing", "pg@payments", true, false).is_some());
/// // No history yet (e.g. typo fixed before the first backup) → allowed.
/// assert!(detect_identity_fork("pg@billing", "pg@payments", false, false).is_none());
/// // Acknowledged → allowed.
/// assert!(detect_identity_fork("pg@billing", "pg@payments", true, true).is_none());
/// // No actual change → allowed.
/// assert!(detect_identity_fork("pg@billing", "pg@billing", true, false).is_none());
/// ```
pub fn detect_identity_fork(
    old_identity: &str,
    new_identity: &str,
    has_history: bool,
    acknowledged: bool,
) -> Option<ValidationError> {
    (has_history && !acknowledged && old_identity != new_identity).then(|| {
        ValidationError::IdentityWouldFork {
            old: old_identity.to_string(),
            new: new_identity.to_string(),
        }
    })
}

/// The `(pvcName, effectivePath)` kopia would record for a PVC-addressed source: an
/// explicit `sourcePathOverride`, else the `/pvc/<name>` default (mirrors
/// [`crate::resolve_identity`]). `None` for non-PVC sources (selector/NFS), which the
/// path-fork guard does not reason about — their data identity is the selection/export
/// itself, not an editable per-source path.
fn pvc_source_effective_path(source: &Source) -> Option<(String, String)> {
    let name = source.pvc.as_ref()?.name.clone();
    let path = source
        .source_path_override
        .clone()
        .unwrap_or_else(|| format!("/pvc/{name}"));
    Some((name, path))
}

/// The `sourcePathStrategy` a **selector** source resolves to, keyed by a stable
/// identifier for that source.
///
/// Selector sources have no PVC name to key on and their matched set changes
/// over time, so they are keyed by position. That is exactly right for this
/// guard: the question is "did source #N's path SHAPE change", not "which PVCs
/// does it match today".
///
/// This exists because flipping `PvcName` → `PvcNamespacedName` rewrites every
/// member's kopia path (`/pvc/x` → `/pvc/ns/x`), which re-identifies the source
/// and orphans every manifest it has taken — precisely what this guard is for,
/// and precisely what it did not cover while the selector was unimplemented.
fn selector_source_strategies(spec: &SnapshotPolicySpec) -> BTreeMap<usize, &'static str> {
    spec.sources
        .iter()
        .enumerate()
        .filter(|(_, s)| s.pvc_selector.is_some())
        .map(|(i, s)| {
            let label = match s
                .source_path_strategy
                .unwrap_or(crate::snapshot_policy::SourcePathStrategy::PvcName)
            {
                crate::snapshot_policy::SourcePathStrategy::PvcName => "/pvc/<name>",
                crate::snapshot_policy::SourcePathStrategy::PvcNamespacedName => {
                    "/pvc/<namespace>/<name>"
                }
            };
            (i, label)
        })
        .collect()
}

/// Pure decision for the fork-on-edit guard on a per-source path change. A PVC's kopia
/// source path is part of its identity, so changing `sourcePathOverride` on a PVC that
/// already has history orphans that PVC's snapshots exactly as a username/hostname
/// change would. Sources are matched across the edit by PVC name (paths are never
/// CEL-driven, so an old-vs-new spec diff is complete); selector/NFS sources are out of
/// scope. Returns the first offending change.
pub fn detect_source_path_fork(
    old: &SnapshotPolicySpec,
    new: &SnapshotPolicySpec,
    has_history: bool,
    acknowledged: bool,
) -> Option<ValidationError> {
    if !has_history || acknowledged {
        return None;
    }
    let old_paths: BTreeMap<String, String> = old
        .sources
        .iter()
        .filter_map(pvc_source_effective_path)
        .collect();
    for source in &new.sources {
        if let Some((name, new_path)) = pvc_source_effective_path(source)
            && let Some(old_path) = old_paths.get(&name)
            && *old_path != new_path
        {
            return Some(ValidationError::IdentityWouldFork {
                old: old_path.clone(),
                new: new_path,
            });
        }
    }
    // Same guard for selector sources: a `sourcePathStrategy` flip rewrites
    // every matched PVC's kopia path at once, so it forks harder than any
    // single `sourcePathOverride` edit could.
    let old_strategies = selector_source_strategies(old);
    for (index, new_shape) in selector_source_strategies(new) {
        if let Some(old_shape) = old_strategies.get(&index)
            && *old_shape != new_shape
        {
            return Some(ValidationError::IdentityWouldFork {
                old: (*old_shape).to_string(),
                new: new_shape.to_string(),
            });
        }
    }
    None
}

// --- Repository identityDefaults edit guard (fleet-wide silent re-identification) ---

/// Pure decision for the repository `identityDefaults`-edit guard. An edit to a
/// `Repository`/`ClusterRepository`'s `identityDefaults` (`cluster`,
/// `hostnameExpr`, or `usernameExpr`) changes what every consumer
/// `SnapshotPolicy` relying on those defaults resolves to — silently, with no
/// per-policy edit to acknowledge it (unlike [`detect_identity_fork`], which
/// guards a policy's own edit). Returns
/// `Some(`[`ValidationError::RepositoryIdentityWouldFork`]`)` iff
/// `identityDefaults` actually changed, at least one consumer has snapshot
/// history, and the change is not acknowledged. The webhook does the IO (list
/// consumer `SnapshotPolicy`s, read the ack annotation) and calls this.
///
/// ```
/// use kopiur_api::common::IdentityDefaults;
/// use kopiur_api::validate::detect_repository_identity_change;
///
/// let old = IdentityDefaults { cluster: Some("east".into()), ..Default::default() };
/// let new = IdentityDefaults { cluster: Some("west".into()), ..Default::default() };
/// let consumers = ["billing/pg".to_string()];
///
/// // Change + a consumer with history + no ack → rejected, naming the consumer.
/// assert!(detect_repository_identity_change(Some(&old), Some(&new), false, &consumers).is_some());
/// // No consumers with history → nothing to orphan → allowed.
/// assert!(detect_repository_identity_change(Some(&old), Some(&new), false, &[]).is_none());
/// // Acknowledged → allowed.
/// assert!(detect_repository_identity_change(Some(&old), Some(&new), true, &consumers).is_none());
/// // No actual change → allowed.
/// assert!(detect_repository_identity_change(Some(&old), Some(&old), false, &consumers).is_none());
/// ```
pub fn detect_repository_identity_change(
    old: Option<&IdentityDefaults>,
    new: Option<&IdentityDefaults>,
    acknowledged: bool,
    consumers_with_history: &[String],
) -> Option<ValidationError> {
    if acknowledged || consumers_with_history.is_empty() || old == new {
        return None;
    }
    Some(ValidationError::RepositoryIdentityWouldFork {
        consumers: consumers_with_history.to_vec(),
    })
}
