//! Destination-side identity gathering for the `SnapshotReplication`
//! identity-overlap admission check (issue #368).
//!
//! The **decision** — which of these identities the replication's
//! `spec.selection` would also select — is the pure
//! [`kopiur_api::validate::replication_identity_overlap`]. This module is the
//! thin IO in front of it, reusing the identity-collision guard's machinery
//! (`repo_key`, `resolve_policy_identity`, the repository `identityDefaults`
//! lookup) so the two admission features cannot resolve identities
//! differently:
//!
//! 1. list every `SnapshotPolicy` cluster-wide (like
//!    [`crate::identity_collision::check_identity_collision`] — a
//!    `RepositoryRef` is a documented cross-namespace reference, so the
//!    destination's consumers are not confined to any one namespace);
//! 2. keep the policies whose `spec.repository` resolves to the replication's
//!    DESTINATION (`repo_key` equality);
//! 3. resolve each kept policy's kopia identity — `status.resolved.identity`
//!    when the controller has pinned one (fine at admission), else the same
//!    live resolution the collision guard performs — expanded per resolved
//!    source path, so a multi-source (pvcSelector) policy contributes one
//!    identity triple per path.
//!
//! **Best-effort, fail-open**: a failed LIST, an unresolvable identity, or a
//! missing repository degrades to "no identities found" — the overlap check is
//! a footgun guard, not a security boundary, and the runtime condition is the
//! backstop. This mirrors the collision guard's posture exactly.

use kopiur_api as api;

use api::SnapshotPolicy;
use api::common::{IdentityDefaults, RepositoryRef, ResolvedIdentity};
use kube::{Api, Client, ResourceExt};

use crate::identity_collision::{cluster_repo_defaults_for, repo_key, resolve_policy_identity};

/// The resolved identities every `SnapshotPolicy` targeting `dest` writes
/// directly (one entry per resolved source path). `cr_namespace` is the
/// admitted `SnapshotReplication`'s own namespace (the ref's effective
/// namespace when it names none). Empty on any IO failure (fail-open — see
/// module doc).
pub async fn dest_policy_identities(
    client: &Client,
    dest: &RepositoryRef,
    cr_namespace: &str,
) -> Vec<ResolvedIdentity> {
    let policies_api: Api<SnapshotPolicy> = Api::all(client.clone());
    let Ok(policies) = policies_api.list(&Default::default()).await else {
        return Vec::new();
    };
    let dest_key = repo_key(dest, cr_namespace);
    // The destination repository's identityDefaults, fetched at most once: every
    // kept policy targets the SAME repository, so one lookup serves them all
    // (and none is needed when every policy carries a pinned resolved identity).
    let mut defaults: Option<Option<IdentityDefaults>> = None;
    let mut out = Vec::new();
    for p in policies {
        let Some(ns) = p.namespace() else { continue };
        // Any-of over every ref the policy names (tolerant iterator): a
        // multi-repo policy writes the destination directly when ANY of its
        // refs resolves to it. Single-repo behavior is unchanged.
        if !api::repository_refs(&p.spec).any(|r| repo_key(r, &ns) == dest_key) {
            continue;
        }
        let base = match pinned_identity(&p) {
            Some(id) => Some(id),
            None => {
                let d = match &defaults {
                    Some(d) => d.clone(),
                    None => {
                        let fetched = cluster_repo_defaults_for(client, dest, cr_namespace).await;
                        defaults = Some(fetched.clone());
                        fetched
                    }
                };
                resolve_policy_identity(
                    &p.name_any(),
                    &ns,
                    &p.spec,
                    p.metadata.labels.as_ref(),
                    p.metadata.annotations.as_ref(),
                    d.as_ref(),
                )
            }
        };
        if let Some(base) = base {
            out.extend(expand_per_source(&p, base));
        }
    }
    out
}

/// The identity the controller pinned in `status.resolved.identity`, if any.
fn pinned_identity(p: &SnapshotPolicy) -> Option<ResolvedIdentity> {
    p.status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.identity.clone())
}

/// Expand one policy's base identity across its resolved source paths: a
/// multi-source (pvcSelector-expanded) policy snapshots one path per source
/// under the same `username@hostname`, so the overlap check must see every
/// `(username, hostname, path)` triple, not just the first. Falls back to the
/// base identity's own path when nothing is resolved. Pure.
fn expand_per_source(p: &SnapshotPolicy, base: ResolvedIdentity) -> Vec<ResolvedIdentity> {
    let resolved_paths: Vec<Option<String>> = p
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .map(|r| r.sources.iter().map(|s| s.source_path.clone()).collect())
        .unwrap_or_default();
    if resolved_paths.is_empty() {
        return vec![base];
    }
    resolved_paths
        .into_iter()
        .map(|path| ResolvedIdentity {
            username: base.username.clone(),
            hostname: base.hostname.clone(),
            source_path: path.or_else(|| base.source_path.clone()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(v: serde_json::Value) -> SnapshotPolicy {
        serde_json::from_value(v).expect("policy fixture decodes")
    }

    #[test]
    fn expand_per_source_covers_every_resolved_path() {
        let p = policy(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": "pg", "namespace": "billing" },
            "spec": {
                "repository": { "kind": "Repository", "name": "dst" },
                "sources": [ { "pvcSelector": { "labelSelector": { "matchLabels": { "app": "pg" } } } } ]
            },
            "status": { "resolved": {
                "identity": { "username": "pg", "hostname": "billing" },
                "sources": [
                    { "pvc": "billing/data-0", "sourcePath": "/pvc/data-0" },
                    { "pvc": "billing/data-1", "sourcePath": "/pvc/data-1" },
                ]
            } }
        }));
        let base = pinned_identity(&p).expect("pinned identity");
        let ids = expand_per_source(&p, base);
        let paths: Vec<Option<&str>> = ids.iter().map(|i| i.source_path.as_deref()).collect();
        assert_eq!(paths, vec![Some("/pvc/data-0"), Some("/pvc/data-1")]);
        assert!(
            ids.iter()
                .all(|i| i.username == "pg" && i.hostname == "billing")
        );
    }

    #[test]
    fn expand_per_source_falls_back_to_the_base_identity() {
        let p = policy(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": "pg", "namespace": "billing" },
            "spec": {
                "repository": { "kind": "Repository", "name": "dst" },
                "sources": [ { "pvc": { "name": "data" } } ]
            }
        }));
        let base = ResolvedIdentity {
            username: "pg".into(),
            hostname: "billing".into(),
            source_path: Some("/pvc/data".into()),
        };
        let ids = expand_per_source(&p, base.clone());
        assert_eq!(ids, vec![base]);
    }
}
