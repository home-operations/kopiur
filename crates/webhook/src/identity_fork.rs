//! Fork-on-edit guard at admission.
//!
//! Kopia records every snapshot under `username@hostname:sourcePath`. If a
//! `SnapshotPolicy` is *edited* so its resolved identity changes — renamed, a
//! namespace/label change feeding an `identityDefaults` CEL expression, a hand-edited
//! `spec.identity`, or a per-source `sourcePathOverride` change — new snapshots land
//! under the **new** kopia source while the old history stays under the old one:
//! restore/verify/`fromPolicy` resolve the new identity (old history is reachable only
//! via `Restore.spec.source.identity`), and old- and new-lineage `Snapshot` CRs keep
//! competing in the policy's one GFS retention timeline. That is a silent
//! re-identification, so the webhook **rejects** such an edit unless the operator
//! acknowledges it with the [`ALLOW_IDENTITY_CHANGE_ANNOTATION`].
//!
//! ## Pure core + thin IO (mirrors [`crate::identity_collision`])
//!
//! - The decision is the pure, unit-tested [`api::validate::detect_identity_fork`] (on
//!   the `username@hostname`) and [`api::validate::detect_source_path_fork`] (on each
//!   PVC source's path — paths are never CEL-driven, so an old-vs-new spec diff is
//!   complete).
//! - [`check_identity_fork`] is the thin IO caller: it reads the old object's pinned
//!   identity + history from `oldObject.status`, resolves the *new* identity (fetching
//!   the referenced repository's — `Repository` or `ClusterRepository` —
//!   `identityDefaults` for CEL), and calls the pure decisions.
//! - It only fires on UPDATE, only when the policy has real history
//!   (`status.lastSuccessfulSnapshot`), and **degrades to allow** when it cannot make
//!   a confident decision (no client, or no pinned identity yet) — the same
//!   fail-open posture as the collision guard.

use std::collections::BTreeMap;

use kopiur_api as api;

use api::consts::ALLOW_IDENTITY_CHANGE_ANNOTATION;
use api::error::ValidationError;
use api::snapshot_policy::{SnapshotPolicySpec, SnapshotPolicyStatus};
use kube::Client;

/// Whether the incoming object acknowledges an intentional re-identification via the
/// [`ALLOW_IDENTITY_CHANGE_ANNOTATION`] (any non-empty value; presence-only).
fn acknowledged(annotations: Option<&BTreeMap<String, String>>) -> bool {
    annotations
        .and_then(|a| a.get(ALLOW_IDENTITY_CHANGE_ANNOTATION))
        .is_some_and(|v| !v.trim().is_empty())
}

/// Check whether an UPDATE to a `SnapshotPolicy` would fork its kopia identity while it
/// already has snapshot history. Returns the [`ValidationError::IdentityWouldFork`] to
/// reject with, or `None` to allow.
///
/// `old_spec`/`old_status` come from the admission request's `oldObject`. The guard:
/// - allows when the change is acknowledged, or the policy has no successful snapshot
///   yet (`status.lastSuccessfulSnapshot` unset → no history to orphan);
/// - checks per-source path forks purely (no client);
/// - checks the `username@hostname` fork by resolving the new identity, which needs the
///   referenced repository's `identityDefaults` for CEL — if there is no client, or the
///   old identity was never pinned, it degrades to allow.
#[allow(clippy::too_many_arguments)]
pub async fn check_identity_fork(
    client: Option<&Client>,
    name: &str,
    namespace: &str,
    new_spec: &SnapshotPolicySpec,
    new_labels: Option<&BTreeMap<String, String>>,
    new_annotations: Option<&BTreeMap<String, String>>,
    old_spec: &SnapshotPolicySpec,
    old_status: &SnapshotPolicyStatus,
) -> Option<ValidationError> {
    let acked = acknowledged(new_annotations);
    let has_history = old_status.last_successful_snapshot.is_some();
    if !has_history || acked {
        return None;
    }

    // Per-source path fork: pure spec diff, no client required.
    if let Some(e) = api::validate::detect_source_path_fork(old_spec, new_spec, has_history, acked)
    {
        return Some(e);
    }

    // username@hostname fork: compare the previously-pinned identity against the
    // freshly-resolved one. The old identity must have been pinned (else there is no
    // baseline — degrade to allow), and resolving the new identity needs the
    // referenced repository's identityDefaults (CEL), so without a client we degrade
    // to allow.
    let old_id = old_status.resolved.as_ref()?.identity.as_ref()?;
    let old_uh = format!("{}@{}", old_id.username, old_id.hostname);

    let client = client?;
    // The multi-repo fork guard (a hard branch over every per-repo identity)
    // lands in M9. Until then the admission feature gate refuses
    // spec.repositories before this guard can run, so this arm only fires on
    // a garbage stored shape — deny with the same validator error rather than
    // silently admitting a re-identifying edit.
    let repo_ref = match api::single_repository_ref(new_spec) {
        Ok(r) => r,
        Err(e) => return Some(e),
    };
    let defaults =
        crate::identity_collision::cluster_repo_defaults_for(client, repo_ref, namespace).await;
    let new_id = crate::identity_collision::resolve_policy_identity(
        name,
        namespace,
        new_spec,
        new_labels,
        new_annotations,
        defaults.as_ref(),
    )?;
    let new_uh = format!("{}@{}", new_id.username, new_id.hostname);

    api::validate::detect_identity_fork(&old_uh, &new_uh, has_history, acked)
}
