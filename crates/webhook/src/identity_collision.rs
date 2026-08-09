//! Identity-collision detection at admission (ADR-0005 §6).
//!
//! Kopia records every snapshot under `username@hostname:sourcePath`. Two
//! `SnapshotPolicy`s that resolve to the *same* identity in the *same* repository
//! would interleave snapshots into one kopia identity — corrupting the snapshot
//! history. The webhook pins identity at admission (ADR-0003 §4.2); this extends it
//! to **reject** a `SnapshotPolicy` whose resolved identity collides with an
//! already-admitted one, naming the conflict.
//!
//! ## Pure core + thin IO (mirrors the tenancy module)
//!
//! - [`repo_key`] and [`policy_identity_string`] are **pure** (no cluster) and
//!   unit-tested. They turn a policy + its repository ref into the comparable
//!   `(identity, repo_key)` pair.
//! - The api-crate [`api::validate::detect_identity_collision`] is the pure
//!   decision over a candidate pair + a list of existing pairs.
//! - [`check_identity_collision`] is the **thin IO caller**: it lists every
//!   `SnapshotPolicy` cluster-wide, fetches each referenced repository's (`Repository`
//!   or `ClusterRepository`) `identityDefaults` (cached), resolves identities, and
//!   calls the pure decision.
//!   It **fails open on IO errors** (a transient list/get failure must not wedge
//!   unrelated applies) — the collision guard is a best-effort admission check, not
//!   a security boundary, and the controller would still pin distinct status.

use std::collections::BTreeMap;

use kopiur_api as api;

use api::common::{IdentityDefaults, RepositoryKind, RepositoryRef};
use api::validate::ExistingIdentity;
use api::{ClusterRepository, IdentityInputs, Repository, SnapshotPolicy, SnapshotPolicySpec};
use kube::{Api, Client, ResourceExt};

/// The normalized repository key — hoisted into `kopiur_api::common` (the
/// shared validator's duplicate-repo check normalizes with the SAME function),
/// re-exported here so the webhook's existing callers keep their import path.
pub use api::common::repo_key;

/// Resolve a `SnapshotPolicy`'s kopia identity string (`username@hostname[:path]`),
/// reusing the api-crate kernel ([`api::resolve_identity`] + [`api::identity_string`]).
/// `defaults` is the referenced repository's `identityDefaults` (CEL `*Expr`) —
/// `Repository` or `ClusterRepository`, whichever the policy targets; `None` if it
/// has none set (or the lookup failed). Returns `None` if an expression fails to
/// resolve (the per-field validators already reject those; here we just skip the
/// collision check for an unresolvable identity rather than panic).
pub fn resolve_policy_identity(
    name: &str,
    namespace: &str,
    spec: &SnapshotPolicySpec,
    labels: Option<&BTreeMap<String, String>>,
    annotations: Option<&BTreeMap<String, String>>,
    defaults: Option<&IdentityDefaults>,
) -> Option<api::common::ResolvedIdentity> {
    let first = spec.sources.first();
    let pvc_name = first.and_then(|s| s.pvc.as_ref().map(|p| p.name.clone()));
    let nfs_source_path = first.and_then(|s| s.nfs.as_ref().map(|n| n.path.clone()));
    let source_path_override = first.and_then(|s| s.source_path_override.clone());
    let inputs = IdentityInputs {
        object_name: name,
        namespace,
        overrides: spec.identity.as_ref(),
        defaults,
        labels,
        annotations,
        pvc_name: pvc_name.as_deref(),
        default_source_path: nfs_source_path.as_deref(),
        source_path_override: source_path_override.as_deref(),
    };
    api::resolve_identity(&inputs).ok()
}

/// As [`resolve_policy_identity`], formatted as kopia's `username@hostname[:path]`
/// string for collision comparison.
pub fn policy_identity_string(
    name: &str,
    namespace: &str,
    spec: &SnapshotPolicySpec,
    labels: Option<&BTreeMap<String, String>>,
    annotations: Option<&BTreeMap<String, String>>,
    defaults: Option<&IdentityDefaults>,
) -> Option<String> {
    resolve_policy_identity(name, namespace, spec, labels, annotations, defaults)
        .map(|r| api::identity_string(&r))
}

/// Look up the `identityDefaults` of the `Repository`/`ClusterRepository` a policy
/// references, if any (`None` when the lookup fails). Cached by [`repo_key`] (kind +
/// effective namespace + name) so listing N policies that share a repository does
/// one get — a plain repo-name key would collide across namespaces for a namespaced
/// `Repository`. Fetch the `identityDefaults` of the repository a single policy
/// references (no cache — for callers resolving one policy, e.g. the fork guard).
pub(crate) async fn cluster_repo_defaults_for(
    client: &Client,
    repo: &RepositoryRef,
    owner_namespace: &str,
) -> Option<IdentityDefaults> {
    let mut cache = BTreeMap::new();
    cluster_repo_defaults(client, repo, owner_namespace, &mut cache).await
}

async fn cluster_repo_defaults(
    client: &Client,
    repo: &RepositoryRef,
    owner_namespace: &str,
    cache: &mut BTreeMap<String, Option<IdentityDefaults>>,
) -> Option<IdentityDefaults> {
    let key = repo_key(repo, owner_namespace);
    if let Some(cached) = cache.get(&key) {
        return cached.clone();
    }
    let defaults = match repo.kind {
        RepositoryKind::ClusterRepository => {
            let api: Api<ClusterRepository> = Api::all(client.clone());
            api.get_opt(&repo.name)
                .await
                .ok()
                .flatten()
                .and_then(|c| c.spec.identity_defaults)
        }
        RepositoryKind::Repository => {
            let ns = repo.namespace.as_deref().unwrap_or(owner_namespace);
            let api: Api<Repository> = Api::namespaced(client.clone(), ns);
            api.get_opt(&repo.name)
                .await
                .ok()
                .flatten()
                .and_then(|r| r.spec.identity_defaults)
        }
    };
    cache.insert(key, defaults.clone());
    defaults
}

/// Resolve a policy's `(identity, repo_key)` pairs for collision comparison —
/// **one pair per repository the policy names** (plan B5: the unit of identity
/// is the pair, so a multi-repo policy contributes N pairs, each identity
/// resolved under THAT repository's `identityDefaults`; the repo_key-keyed
/// `cache` amortizes the defaults lookups). Single-repo yields exactly the one
/// pair it always did. Uses the tolerant [`api::repository_refs`] iterator so
/// a malformed stored shape still contributes whatever pairs it can. A member
/// whose identity can't be resolved is skipped (the per-field validators
/// handle malformed expressions).
pub(crate) async fn policy_pairs(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &SnapshotPolicySpec,
    labels: Option<&BTreeMap<String, String>>,
    annotations: Option<&BTreeMap<String, String>>,
    cache: &mut BTreeMap<String, Option<IdentityDefaults>>,
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for repo in api::repository_refs(spec) {
        let defaults = cluster_repo_defaults(client, repo, namespace, cache).await;
        if let Some(identity) = policy_identity_string(
            name,
            namespace,
            spec,
            labels,
            annotations,
            defaults.as_ref(),
        ) {
            pairs.push((identity, repo_key(repo, namespace)));
        }
    }
    pairs
}

/// A detected identity collision: the conflicting policy's `namespace/name`, the
/// resolved identity string that collided, and the repository it collided in
/// (so the rejection message is exact — with a multi-repo policy only ONE of
/// the N pairs may be the problem).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    /// `namespace/name` of the already-admitted policy with the same identity.
    pub conflict: String,
    /// The resolved `username@hostname[:path]` identity that collided.
    pub identity: String,
    /// Normalized key of the repository the collision happened in.
    pub repo_key: String,
}

/// Check the incoming `SnapshotPolicy` for an identity collision with an
/// already-admitted policy in the same repository (ADR-0005 §6). Returns the
/// [`Collision`] when found, else `None`.
///
/// Thin IO: lists every `SnapshotPolicy` cluster-wide, resolves each one's
/// `(identity, repo_key)` pair (using the exact referenced repository's identity
/// defaults), and calls the pure [`api::validate::detect_identity_collision`]. Self (same
/// `namespace/name`) is skipped. **Fails open** if the client is absent or the list
/// fails — a transient IO error must not wedge applies, and this is a best-effort
/// guard (the controller still pins distinct status).
pub async fn check_identity_collision(
    client: Option<&Client>,
    self_name: &str,
    self_namespace: &str,
    self_spec: &SnapshotPolicySpec,
    self_labels: Option<&BTreeMap<String, String>>,
    self_annotations: Option<&BTreeMap<String, String>>,
) -> Option<Collision> {
    let client = client?;
    let mut cache: BTreeMap<String, Option<IdentityDefaults>> = BTreeMap::new();

    let self_pairs = policy_pairs(
        client,
        self_name,
        self_namespace,
        self_spec,
        self_labels,
        self_annotations,
        &mut cache,
    )
    .await;
    if self_pairs.is_empty() {
        return None; // nothing resolvable to compare (same skip as before)
    }

    let api: Api<SnapshotPolicy> = Api::all(client.clone());
    let policies = api.list(&Default::default()).await.ok()?;

    let self_full = format!("{self_namespace}/{self_name}");
    let mut existing: Vec<ExistingIdentity> = Vec::new();
    for p in policies {
        let Some(ns) = p.namespace() else { continue };
        let name = p.name_any();
        let full = format!("{ns}/{name}");
        if full == self_full {
            continue; // self
        }
        // A stored multi-repo policy contributes one ExistingIdentity entry
        // per member — same expansion as the candidate side.
        for (identity, key) in policy_pairs(
            client,
            &name,
            &ns,
            &p.spec,
            p.metadata.labels.as_ref(),
            p.metadata.annotations.as_ref(),
            &mut cache,
        )
        .await
        {
            existing.push(ExistingIdentity {
                identity,
                repo_key: key,
                name: full.clone(),
            });
        }
    }

    api::validate::detect_identity_collision_multi(&self_pairs, &self_full, &existing).map(|hit| {
        Collision {
            conflict: hit.conflict,
            identity: hit.identity,
            repo_key: hit.repo_key,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::common::{Identity, RepositoryRef};
    use api::snapshot_policy::{PvcSource, Source};
    use serde_json::{Value, json};

    fn spec_with(repo: RepositoryRef, identity: Option<Identity>, pvc: &str) -> SnapshotPolicySpec {
        SnapshotPolicySpec {
            repository: Some(repo),
            repositories: vec![],
            identity,
            sources: vec![Source {
                pvc: Some(PvcSource { name: pvc.into() }),
                pvc_selector: None,
                nfs: None,
                source_path_override: None,
                source_path_strategy: None,
                ..Default::default()
            }],
            copy_method: Default::default(),
            volume_snapshot_class_name: None,
            staging: None,
            group_by: None,
            retention: None,
            default_deletion_policy: None,
            compression: None,
            files: None,
            extra_args: vec![],
            error_handling: None,
            upload: None,
            verification: None,
            preflight: None,
            suspend: false,
            hooks: None,
            mover: None,
            credential_projection: None,
            deletion: None,
            adoption: None,
        }
    }

    #[test]
    fn repo_key_namespaced_uses_effective_namespace() {
        let r = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: None,
        };
        assert_eq!(repo_key(&r, "billing"), "Repository/billing/nas");
        let explicit = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: Some("backups".into()),
        };
        assert_eq!(repo_key(&explicit, "billing"), "Repository/backups/nas");
    }

    #[test]
    fn repo_key_cluster_is_namespace_free() {
        let r = RepositoryRef {
            kind: RepositoryKind::ClusterRepository,
            name: "shared".into(),
            namespace: None,
        };
        assert_eq!(repo_key(&r, "billing"), "ClusterRepository/shared");
    }

    #[test]
    fn policy_identity_uses_name_namespace_and_pvc_path() {
        let spec = spec_with(
            RepositoryRef {
                kind: RepositoryKind::Repository,
                name: "nas".into(),
                namespace: None,
            },
            None,
            "data",
        );
        let id = policy_identity_string("pg", "billing", &spec, None, None, None).unwrap();
        assert_eq!(id, "pg@billing:/pvc/data");
    }

    #[test]
    fn policy_identity_honors_explicit_override() {
        let spec = spec_with(
            RepositoryRef {
                kind: RepositoryKind::Repository,
                name: "nas".into(),
                namespace: None,
            },
            Some(Identity {
                username: Some("custom".into()),
                hostname: Some("host".into()),
            }),
            "data",
        );
        let id = policy_identity_string("pg", "billing", &spec, None, None, None).unwrap();
        assert_eq!(id, "custom@host:/pvc/data");
    }

    /// A `Client` whose every request returns the same canned JSON body — a
    /// hermetic, no-cluster stand-in for `Api::get_opt`, mirroring
    /// `handlers::tests::mock_list_client`.
    fn mock_get_client(body: Value) -> Client {
        let svc = tower::service_fn(move |_req: http::Request<kube::client::Body>| {
            let body = body.clone();
            async move {
                let resp = http::Response::builder()
                    .status(http::StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        Client::new(svc, "test-ns")
    }

    /// A `Client` that routes by URI-path substring (first matching fragment
    /// wins; unmatched paths get an empty list body). Mirrors
    /// `handlers::tests::mock_path_client`.
    fn mock_path_client(routes: Vec<(&'static str, Value)>) -> Client {
        let svc = tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let path = req.uri().path().to_string();
            let body = routes
                .iter()
                .find(|(fragment, _)| path.contains(fragment))
                .map(|(_, b)| b.clone())
                .unwrap_or_else(|| json!({ "items": [] }));
            async move {
                let resp = http::Response::builder()
                    .status(http::StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        Client::new(svc, "test-ns")
    }

    /// Routes serving both referenced repositories (no `identityDefaults`) and
    /// a stored single-repo policy `billing/pg-a` targeting
    /// `ClusterRepository/shared` with a pinned explicit identity `pg@host`.
    fn multi_collision_routes() -> Vec<(&'static str, Value)> {
        vec![
            // NOTE: listed before any bare "repositories/" fragment — the
            // cluster-scoped path also contains that substring.
            (
                "clusterrepositories/shared",
                json!({
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "ClusterRepository",
                    "metadata": { "name": "shared" },
                    "spec": {
                        "backend": { "filesystem": { "path": "/r" } },
                        "encryption": { "passwordSecretRef": { "name": "s", "namespace": "kopiur-system" } },
                        "allowedNamespaces": { "all": true },
                    }
                }),
            ),
            (
                "repositories/nas",
                json!({
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "Repository",
                    "metadata": { "name": "nas", "namespace": "billing" },
                    "spec": {
                        "backend": { "filesystem": { "path": "/r" } },
                        "encryption": { "passwordSecretRef": { "name": "s" } },
                    }
                }),
            ),
            (
                "snapshotpolicies",
                json!({ "items": [{
                    "metadata": { "name": "pg-a", "namespace": "billing" },
                    "spec": {
                        "repository": { "kind": "ClusterRepository", "name": "shared" },
                        "identity": { "username": "pg", "hostname": "host" },
                        "sources": [ { "pvc": { "name": "data" } } ],
                    }
                }] }),
            ),
        ]
    }

    fn multi_spec(identity: Option<Identity>) -> SnapshotPolicySpec {
        let mut v = json!({
            "repositories": [
                { "kind": "Repository", "name": "nas" },
                { "kind": "ClusterRepository", "name": "shared" },
            ],
            "sources": [ { "pvc": { "name": "data" } } ],
        });
        if let Some(id) = identity {
            v["identity"] = serde_json::to_value(id).unwrap();
        }
        serde_json::from_value(v).expect("spec fixture decodes")
    }

    #[tokio::test]
    async fn multi_repo_overlap_in_one_repo_collides_naming_that_repo() {
        // The candidate resolves TWO (identity, repo_key) pairs; only the
        // `ClusterRepository/shared` pair overlaps the stored policy — the hit
        // names that member, not the harmless `Repository/billing/nas` one.
        let client = mock_path_client(multi_collision_routes());
        let spec = multi_spec(Some(Identity {
            username: Some("pg".into()),
            hostname: Some("host".into()),
        }));
        let collision =
            check_identity_collision(Some(&client), "pg-b", "billing", &spec, None, None)
                .await
                .expect("must collide");
        assert_eq!(collision.conflict, "billing/pg-a");
        assert_eq!(collision.repo_key, "ClusterRepository/shared");
        assert_eq!(collision.identity, "pg@host:/pvc/data");
    }

    #[tokio::test]
    async fn same_identity_in_different_repositories_does_not_collide() {
        // Identical identity, but the candidate only targets
        // `Repository/billing/nas` while the stored policy writes
        // `ClusterRepository/shared` — separate snapshot histories, no
        // collision.
        let client = mock_path_client(multi_collision_routes());
        let spec = spec_with(
            RepositoryRef {
                kind: RepositoryKind::Repository,
                name: "nas".into(),
                namespace: None,
            },
            Some(Identity {
                username: Some("pg".into()),
                hostname: Some("host".into()),
            }),
            "data",
        );
        assert_eq!(
            check_identity_collision(Some(&client), "pg-b", "billing", &spec, None, None).await,
            None
        );
    }

    #[tokio::test]
    async fn single_repo_collision_behaves_exactly_as_before() {
        // N = 1 ≡ the old behavior: a single-repo candidate colliding with the
        // stored single-repo policy in the same repository.
        let client = mock_path_client(multi_collision_routes());
        let spec = spec_with(
            RepositoryRef {
                kind: RepositoryKind::ClusterRepository,
                name: "shared".into(),
                namespace: None,
            },
            Some(Identity {
                username: Some("pg".into()),
                hostname: Some("host".into()),
            }),
            "data",
        );
        let collision =
            check_identity_collision(Some(&client), "pg-b", "billing", &spec, None, None)
                .await
                .expect("must collide");
        assert_eq!(collision.conflict, "billing/pg-a");
        assert_eq!(collision.repo_key, "ClusterRepository/shared");
    }

    #[tokio::test]
    async fn cluster_repo_defaults_for_resolves_a_namespaced_repository() {
        // M5: `RepositorySpec` now carries `identityDefaults` too, so the lookup
        // must do a NAMESPACED get (not `Api::all`, which only makes sense for the
        // cluster-scoped kind) for `kind: Repository`.
        let body = json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Repository",
            "metadata": { "name": "nas", "namespace": "billing" },
            "spec": {
                "backend": { "filesystem": { "path": "/r" } },
                "encryption": { "passwordSecretRef": { "name": "s" } },
                "identityDefaults": { "cluster": "east" },
            }
        });
        let client = mock_get_client(body);
        let repo_ref = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: None,
        };
        // No explicit ref namespace: falls back to the policy's own (owner) namespace.
        let defaults = cluster_repo_defaults_for(&client, &repo_ref, "billing").await;
        assert_eq!(
            defaults.expect("identityDefaults").cluster.as_deref(),
            Some("east")
        );
    }
}
