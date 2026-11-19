//! Resolves a `Snapshot` to the repository, credentials, and image a browse
//! session needs: which kopia snapshot to read, which repository holds it, and
//! where (and with which Secrets) a session pod may run. The decisions are pure
//! functions over the fetched CRs; the IO fns just fetch and delegate.

use kopiur_api::backend::Backend;
use kopiur_api::common::{Encryption, MoverDefaults, RepositoryKind};
use kopiur_api::consts::DEFAULT_CA_BUNDLE_KEY;
use kopiur_api::creds::mover_creds_secret_refs;
use kopiur_api::{ClusterRepository, Repository, Snapshot};
use kube::Api;
use kube::ResourceExt;

use crate::ctx::OpsCtx;
use crate::error::{OpsError, ProjectionBlock, classify_kube};

/// Everything the browse data-plane needs about one repository: identity (for
/// the session Job's name/labels/owner), backend + encryption (for the work
/// spec and credentials), and which namespace the repository itself lives in
/// (`None` for the cluster-scoped kind).
pub struct RepoHandle {
    /// Which repository CRD kind this is.
    pub kind: RepositoryKind,
    /// The repository object's name.
    pub name: String,
    /// Its UID (the session Job's ownerReference, so cluster GC reaps sessions
    /// with the repository).
    pub uid: String,
    /// The repository's own namespace (`None` for `ClusterRepository`).
    pub namespace: Option<String>,
    /// Where the repository's movers run (`ClusterRepository.spec.execution`).
    /// Always `InNamespace` for a namespaced `Repository`, which has no such
    /// field.
    pub execution: kopiur_api::ExecutionMode,
    /// The storage backend.
    pub backend: Backend,
    /// The encryption (password Secret) settings.
    pub encryption: Encryption,
    /// The repository's `moverDefaults`. A browse session deliberately applies
    /// none of the placement/resource/security layers (it is an interactive
    /// read-only pod on the hardened baseline), and it takes only
    /// **`podAnnotations`** from this — a mesh sidecar that never exits would
    /// keep the session Job alive past its TTL.
    ///
    /// `podLabels` are deliberately NOT applied, and the asymmetry is the
    /// point: the canonical `podLabels` value is a Kueue
    /// `kueue.x-k8s.io/queue-name`, which would park a human's interactive
    /// browse session behind the nightly backup queue — the same reason
    /// `kopiur_controller::pool` keeps `BrowseSession` out of the repository
    /// concurrency pool.
    pub mover_defaults: Option<MoverDefaults>,
}

/// A fully-resolved browse target: the kopia snapshot id plus its repository.
pub struct BrowseTarget {
    /// The Snapshot CR name (for messages).
    pub snapshot: String,
    /// The namespace the Snapshot lives in.
    pub namespace: String,
    /// The namespace the session pod runs in ([`session_namespace_for`]): the
    /// Snapshot's own, or the operator namespace for a `ClusterRepository`
    /// with `execution.mode: Central`.
    pub session_namespace: String,
    /// The kopia snapshot manifest id pinned in the Snapshot's status.
    pub kopia_snapshot_id: String,
    /// The repository holding the snapshot.
    pub repo: RepoHandle,
    /// The RESOLVED PEM content of the backend's `tls.caBundleRef` ConfigMap
    /// (`None` when the backend declares none). Resolved once at target
    /// resolution ([`resolve_ca_bundle`]) and carried here so both transports'
    /// work-spec builders stay pure.
    pub ca_bundle_pem: Option<String>,
    /// Whether the operator may copy a credential Secret from another
    /// namespace for the session ([`session_creds`]).
    pub projection: SessionProjection,
}

/// Extract the kopia snapshot id a Snapshot pins in status, or say exactly why
/// there is none. Pure.
pub fn kopia_id_of(snap: &Snapshot) -> Result<String, OpsError> {
    let name = snap.name_any();
    match snap.status.as_ref().and_then(|s| s.snapshot.as_ref()) {
        Some(info) if !info.kopia_snapshot_id.is_empty() => Ok(info.kopia_snapshot_id.clone()),
        _ => {
            let phase = snap
                .status
                .as_ref()
                .and_then(|s| s.phase.as_ref())
                .map(|p| format!("phase is {}", serde_json::json!(p).as_str().unwrap_or("?")))
                .unwrap_or_else(|| "it has no status yet".to_string());
            Err(OpsError::SnapshotNotBrowsable {
                name,
                reason: format!("{phase} and no kopia snapshot id is recorded"),
            })
        }
    }
}

/// Whether the operator may copy a credential Secret from another namespace into
/// a browse session's: the two gates a backup's projection passes, read off the
/// objects the session resolved. The operator re-checks both before it copies
/// anything; these only decide what the session Job asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionProjection {
    /// The `ClusterRepository`'s `spec.credentialProjection.allowed`. Always
    /// false for a namespaced `Repository`.
    pub allowed: bool,
    /// The browsed `Snapshot`'s pinned `status.resolved.credentialProjection`:
    /// whether the run that took it opted in. Absent reads as no.
    pub opted_in: bool,
}

/// One credential Secret a session pod loads via `envFrom`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCreds {
    /// The Secret is already in the session's namespace.
    Direct(String),
    /// The Secret lives elsewhere; the operator copies it to `copy_name` in the
    /// session's namespace for as long as the session Job exists.
    Projected {
        /// Where the original lives.
        source_namespace: String,
        /// The original's name.
        source_name: String,
        /// The copy the pod loads.
        copy_name: String,
    },
}

impl SessionCreds {
    /// The name the pod's `envFrom` loads.
    pub fn env_from_name(&self) -> &str {
        match self {
            SessionCreds::Direct(name) => name,
            SessionCreds::Projected { copy_name, .. } => copy_name,
        }
    }
}

/// The credential Secrets the session Job `job_name` loads via `envFrom`. One in
/// the session's namespace loads directly. One elsewhere is a copy the operator
/// writes, when the repository is a `ClusterRepository` that allows it and the
/// snapshot's run opted in, the same gates a backup passes; otherwise it is
/// refused, naming the gate. Pure.
pub fn session_creds(target: &BrowseTarget, job_name: &str) -> Result<Vec<SessionCreds>, OpsError> {
    let repo = &target.repo;
    let session_namespace = target.session_namespace.as_str();
    let refs = mover_creds_secret_refs(&repo.backend, &repo.encryption, repo.namespace.as_deref());
    let mut creds = Vec::with_capacity(refs.len());
    // Indexed by the ref's position, as the operator's copies are, so a
    // same-namespace password beside a cross-namespace auth Secret still
    // names the copy `-creds-1`.
    for (idx, r) in refs.into_iter().enumerate() {
        let source_namespace = match r.namespace {
            Some(ns) if ns == session_namespace => {
                creds.push(SessionCreds::Direct(r.name));
                continue;
            }
            Some(ns) => ns,
            // Only reachable for a ClusterRepository reference that pins no
            // namespace (the webhook normally refuses this).
            None => {
                return Err(OpsError::ClusterRepoSecretNamespaceMissing {
                    secret: r.name,
                    repository: repo.name.clone(),
                });
            }
        };
        if let Some(blocked_by) = projection_block(target) {
            return Err(OpsError::CredsOutsideSessionNamespace {
                secret: r.name,
                secret_namespace: source_namespace,
                session_namespace: session_namespace.to_string(),
                blocked_by,
            });
        }
        creds.push(SessionCreds::Projected {
            source_namespace,
            source_name: r.name,
            copy_name: kopiur_api::creds::browse_session_copy_name(job_name, idx),
        });
    }
    Ok(creds)
}

/// Whether the run that took `snap` opted into credential projection, from the
/// pin it wrote (`status.resolved.credentialProjection`). A snapshot with no pin
/// (it predates the field, or never ran) reads as no. Pure.
pub fn snapshot_opted_in(snap: &Snapshot) -> bool {
    snap.status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.credential_projection.as_ref())
        .is_some_and(|p| p.enabled)
}

/// The first gate that keeps the operator from copying a Secret for this
/// session, or `None` when it may.
fn projection_block(target: &BrowseTarget) -> Option<ProjectionBlock> {
    match target.repo.kind {
        RepositoryKind::Repository => Some(ProjectionBlock::NotAClusterRepository),
        // A Central repository keeps its credentials in the operator
        // namespace. The session already runs there, so a Secret that is
        // somewhere else is a misplaced Secret, never one to copy.
        RepositoryKind::ClusterRepository
            if target.repo.execution == kopiur_api::ExecutionMode::Central =>
        {
            Some(ProjectionBlock::CentralExecution {
                repository: target.repo.name.clone(),
            })
        }
        RepositoryKind::ClusterRepository if !target.projection.allowed => {
            Some(ProjectionBlock::OwnerNotAllowed {
                repository: target.repo.name.clone(),
            })
        }
        RepositoryKind::ClusterRepository if !target.projection.opted_in => {
            Some(ProjectionBlock::ConsumerNotOptedIn {
                snapshot: target.snapshot.clone(),
            })
        }
        RepositoryKind::ClusterRepository => None,
    }
}

/// Resolve `snapshot_name` in `namespace` into a [`BrowseTarget`], discovering
/// the operator's namespace from the cluster if a `ClusterRepository`'s CA
/// bundle turns out to need it.
///
/// The `kubectl kopiur` entry point: a CLI has no pod of its own to read
/// `KOPIUR_NAMESPACE` from, so it always pays for the fail-closed
/// controller-Deployment lookup. A server that already knows the operator's
/// namespace calls [`resolve_with`] instead.
pub async fn resolve(
    ctx: &OpsCtx,
    namespace: &str,
    snapshot_name: &str,
    transport: SessionTransport,
) -> Result<BrowseTarget, OpsError> {
    resolve_with(
        ctx,
        namespace,
        snapshot_name,
        &OperatorNamespace::DiscoverFromControllerDeployment,
        transport,
    )
    .await
}

/// How the resolved snapshot will be read. It decides whether the session's
/// namespace is worth finding out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTransport {
    /// Through a session pod in the cluster.
    Pod,
    /// By a kopia process on the caller's machine (`--local`). No session pod
    /// exists, so nothing is looked up to place one: a Central repository's
    /// session namespace would otherwise cost a cluster-wide Deployment LIST
    /// that a `--local` caller may not be allowed to make.
    Local,
}

/// Resolve `snapshot_name` in `namespace` into a [`BrowseTarget`]: fetch the
/// Snapshot, derive its repository via the shared
/// [`kopiur_api::snapshot::repository_ref_for`] rule, then fetch the
/// Repository/ClusterRepository for backend + credentials.
///
/// `operator_namespace` is threaded straight through to [`resolve_ca_bundle`],
/// the only step that consults it (and only for a `ClusterRepository` whose
/// backend declares a `tls.caBundleRef`). It is a parameter rather than a
/// constant because the two front ends know different things: `kopiur-ui` runs
/// inside the operator's install and is told its own namespace by the chart, so
/// it passes [`OperatorNamespace::Known`] and skips a cluster-wide Deployment
/// LIST that the human it impersonates may well not be allowed to perform.
pub async fn resolve_with(
    ctx: &OpsCtx,
    namespace: &str,
    snapshot_name: &str,
    operator_namespace: &OperatorNamespace,
    transport: SessionTransport,
) -> Result<BrowseTarget, OpsError> {
    let ns = namespace.to_string();
    let snaps: Api<Snapshot> = Api::namespaced(ctx.client.clone(), &ns);
    let snap = snaps.get(snapshot_name).await.map_err(|e| {
        classify_kube(
            "get",
            "Snapshot",
            "snapshots",
            Some(&ns),
            Some(snapshot_name),
            e,
        )
    })?;
    let kopia_snapshot_id = kopia_id_of(&snap)?;
    let rref = kopiur_api::snapshot::repository_ref_for(&snap).ok_or_else(|| {
        OpsError::RepositoryUnderivable {
            snapshot: snapshot_name.to_string(),
        }
    })?;

    // Exhaustive over the repository kinds (ADR §5.5): a new kind must decide
    // its fetch + namespace story here before it compiles.
    let (repo, allowed) = match rref.kind {
        RepositoryKind::Repository => {
            // An absent ref namespace means "the Snapshot's own namespace".
            let repo_ns = rref.namespace.clone().unwrap_or_else(|| ns.clone());
            // The session Job lives in the SNAPSHOT's namespace and is owned
            // by the Repository — a cross-namespace ownerReference is invalid
            // (Kubernetes GC treats the owner as missing and reaps the Job
            // mid-read). Refuse up front with the --local escape hatch.
            if repo_ns != ns {
                return Err(OpsError::RepoOutsideSessionNamespace {
                    repo: format!("Repository/{}", rref.name),
                    repo_namespace: repo_ns,
                    session_namespace: ns,
                });
            }
            let api: Api<Repository> = Api::namespaced(ctx.client.clone(), &repo_ns);
            let r = api.get(&rref.name).await.map_err(|e| {
                classify_kube(
                    "get",
                    "Repository",
                    "repositories",
                    Some(&repo_ns),
                    Some(&rref.name),
                    e,
                )
            })?;
            let handle = RepoHandle {
                kind: RepositoryKind::Repository,
                name: r.name_any(),
                uid: r.uid().unwrap_or_default(),
                namespace: Some(repo_ns),
                execution: kopiur_api::ExecutionMode::InNamespace,
                backend: r.spec.backend.clone(),
                encryption: r.spec.encryption.clone(),
                mover_defaults: r.spec.mover_defaults.clone(),
            };
            // A namespaced Repository never projects: its Secrets sit beside it.
            (handle, false)
        }
        RepositoryKind::ClusterRepository => {
            let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
            let r = api.get(&rref.name).await.map_err(|e| {
                classify_kube(
                    "get",
                    "ClusterRepository",
                    "clusterrepositories",
                    None,
                    Some(&rref.name),
                    e,
                )
            })?;
            let allowed = r
                .spec
                .credential_projection
                .as_ref()
                .is_some_and(|p| p.allowed);
            let handle = RepoHandle {
                kind: RepositoryKind::ClusterRepository,
                name: r.name_any(),
                uid: r.uid().unwrap_or_default(),
                namespace: None,
                execution: r
                    .spec
                    .execution
                    .as_ref()
                    .map(|e| e.mode)
                    .unwrap_or_default(),
                backend: r.spec.backend.clone(),
                encryption: r.spec.encryption.clone(),
                mover_defaults: r.spec.mover_defaults.clone(),
            };
            (handle, allowed)
        }
    };

    let ca_bundle_pem = resolve_ca_bundle(ctx, &repo, operator_namespace).await?;
    let session_namespace = match transport {
        SessionTransport::Pod => session_namespace_for(ctx, &repo, &ns, operator_namespace).await?,
        // Never used: there is no session.
        SessionTransport::Local => ns.clone(),
    };
    Ok(BrowseTarget {
        snapshot: snapshot_name.to_string(),
        namespace: ns,
        session_namespace,
        kopia_snapshot_id,
        repo,
        ca_bundle_pem,
        projection: SessionProjection {
            allowed,
            opted_in: snapshot_opted_in(&snap),
        },
    })
}

/// The namespace a browse session of `repo` runs in.
///
/// A session pod reads only the repository, so it does not have to run beside
/// the Snapshot. For a `ClusterRepository` with `execution.mode: Central` it
/// runs in the operator namespace, where that repository keeps its
/// credentials: no Secret is then copied into the Snapshot's namespace. The
/// caller creates the session Job and execs into its pod as themselves, so
/// browsing such a repository needs their own access to the operator
/// namespace. Every other repository's session runs in `snapshot_namespace`.
pub async fn session_namespace_for(
    ctx: &OpsCtx,
    repo: &RepoHandle,
    snapshot_namespace: &str,
    operator_namespace: &OperatorNamespace,
) -> Result<String, OpsError> {
    match central_session(repo) {
        false => Ok(snapshot_namespace.to_string()),
        true => match known_operator_namespace(operator_namespace) {
            Some(ns) => Ok(ns.to_string()),
            None => discover_operator_namespace(ctx, &repo.name, "its browse sessions").await,
        },
    }
}

/// **Pure.** Whether `repo`'s browse sessions run in the operator namespace.
/// Exhaustive over the repository kind and the execution mode.
pub fn central_session(repo: &RepoHandle) -> bool {
    match (repo.kind, repo.execution) {
        (RepositoryKind::ClusterRepository, kopiur_api::ExecutionMode::Central) => true,
        (RepositoryKind::ClusterRepository, kopiur_api::ExecutionMode::InNamespace)
        | (RepositoryKind::Repository, _) => false,
    }
}

/// Where a `ClusterRepository`'s `tls.caBundleRef` ConfigMap lives — the
/// operator's own namespace. Exhaustive: a caller either knows it (a server
/// reads `KOPIUR_NAMESPACE` from its own pod) or asks for the CLI's discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorNamespace {
    /// The namespace, already known to the caller.
    Known(String),
    /// Discover it from the single controller Deployment cluster-wide.
    DiscoverFromControllerDeployment,
}

/// **Pure.** The operator namespace when the caller already knows it, or `None`
/// when the cluster must be asked. Split out of [`resolve_ca_bundle`] so the
/// "does this bypass the cluster lookup?" decision is exhaustive over
/// [`OperatorNamespace`] and directly testable.
pub fn known_operator_namespace(source: &OperatorNamespace) -> Option<&str> {
    match source {
        OperatorNamespace::Known(ns) => Some(ns.as_str()),
        OperatorNamespace::DiscoverFromControllerDeployment => None,
    }
}

/// Resolve the PEM content of the repository backend's `tls.caBundleRef`
/// ConfigMap, or `Ok(None)` when the backend declares none — the client-side
/// twin of the controller's `resolve_backend_ca`, with the same namespace rule:
/// a namespaced `Repository`'s bundle lives in the repository's own namespace,
/// a `ClusterRepository`'s in the operator's. A caller that does not know the
/// operator's namespace passes
/// [`OperatorNamespace::DiscoverFromControllerDeployment`], which finds it the
/// same way the mover-image lookup finds the operator: exactly one controller
/// Deployment cluster-wide (fail closed on zero or several — see
/// [`super::session::CONTROLLER_DEPLOYMENT_SELECTOR`]).
pub async fn resolve_ca_bundle(
    ctx: &OpsCtx,
    repo: &RepoHandle,
    operator_ns: &OperatorNamespace,
) -> Result<Option<String>, OpsError> {
    use k8s_openapi::api::core::v1::ConfigMap;
    let Some(key_ref) = kopiur_mover::repo_meta::backend_tls_ca_keyref(&repo.backend) else {
        return Ok(None);
    };
    let Some(cm_name) = key_ref.config_map_name.as_deref() else {
        // Webhook-rejected normally; surface it rather than silently skipping
        // TLS trust the repository asked for.
        return Err(OpsError::CaBundleUnresolvable {
            repository: repo.name.clone(),
            detail: "tls.caBundleRef is set but names no ConfigMap".to_string(),
            fix: "set tls.caBundleRef.configMapName, or remove caBundleRef".to_string(),
        });
    };
    let ns = match &repo.namespace {
        Some(ns) => ns.clone(),
        None => match known_operator_namespace(operator_ns) {
            Some(ns) => ns.to_string(),
            None => {
                discover_operator_namespace(
                    ctx,
                    &repo.name,
                    &format!("its tls.caBundleRef ConfigMap {cm_name:?}"),
                )
                .await?
            }
        },
    };
    let key = key_ref.key.as_deref().unwrap_or(DEFAULT_CA_BUNDLE_KEY);
    let api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), &ns);
    let cm = api
        .get_opt(cm_name)
        .await
        .map_err(|e| {
            classify_kube(
                "get",
                "ConfigMap",
                "configmaps",
                Some(&ns),
                Some(cm_name),
                e,
            )
        })?
        .ok_or_else(|| OpsError::CaBundleUnresolvable {
            repository: repo.name.clone(),
            detail: format!("ConfigMap {ns}/{cm_name} not found"),
            fix: format!("create it with the PEM CA bundle under key {key:?}"),
        })?;
    // Text under `data`, bytes under `binaryData` — accept both, like the
    // controller, so `kubectl create configmap --from-file` works either way.
    let content = cm
        .data
        .as_ref()
        .and_then(|d| d.get(key))
        .cloned()
        .or_else(|| {
            cm.binary_data
                .as_ref()
                .and_then(|d| d.get(key))
                .and_then(|b| String::from_utf8(b.0.clone()).ok())
        });
    let Some(pem) = content else {
        let available: Vec<_> = cm
            .data
            .iter()
            .flat_map(|d| d.keys())
            .chain(cm.binary_data.iter().flat_map(|d| d.keys()))
            .cloned()
            .collect();
        return Err(OpsError::CaBundleUnresolvable {
            repository: repo.name.clone(),
            detail: format!("ConfigMap {ns}/{cm_name} has no key {key:?} (found: {available:?})"),
            fix: "add the PEM CA bundle under that key, or set tls.caBundleRef.key to an \
                  existing key"
                .to_string(),
        });
    };
    if !pem.contains("BEGIN CERTIFICATE") {
        return Err(OpsError::CaBundleUnresolvable {
            repository: repo.name.clone(),
            detail: format!(
                "ConfigMap {ns}/{cm_name} key {key:?} is not a PEM certificate bundle \
                 (no \"BEGIN CERTIFICATE\" block)"
            ),
            fix: "store the CA chain PEM-encoded".to_string(),
        });
    }
    Ok(Some(pem))
}

/// Discover the operator's namespace (where a `ClusterRepository`'s
/// `tls.caBundleRef` ConfigMap lives, and where a Central repository's browse
/// sessions run) from the single controller Deployment — the same
/// exactly-one-cluster-wide convention `resolve_mover_image` uses, so a
/// spoofed kopiur-labeled Deployment cannot silently redirect the lookup.
/// `needed_for` says, in the error, what the repository needs it for.
async fn discover_operator_namespace(
    ctx: &OpsCtx,
    repository: &str,
    needed_for: &str,
) -> Result<String, OpsError> {
    use k8s_openapi::api::apps::v1::Deployment;
    let selector = super::session::CONTROLLER_DEPLOYMENT_SELECTOR;
    let api: Api<Deployment> = Api::all(ctx.client.clone());
    let list = api
        .list(&kube::api::ListParams::default().labels(selector))
        .await
        .map_err(|e| classify_kube("list", "Deployment", "deployments", None, None, e))?;
    match list.items.as_slice() {
        [] => Err(OpsError::OperatorNamespaceUnresolvable {
            repository: repository.to_string(),
            needed_for: needed_for.to_string(),
            why: format!("no Deployment matches {selector} in any namespace"),
            fix: "check that the kopiur operator is installed".to_string(),
        }),
        [only] => {
            only.metadata
                .namespace
                .clone()
                .ok_or_else(|| OpsError::OperatorNamespaceUnresolvable {
                    repository: repository.to_string(),
                    needed_for: needed_for.to_string(),
                    why: "the controller Deployment carries no namespace".to_string(),
                    fix: "check the install; a namespaced Deployment always has one".to_string(),
                })
        }
        many => Err(OpsError::OperatorNamespaceUnresolvable {
            repository: repository.to_string(),
            needed_for: needed_for.to_string(),
            why: format!(
                "{} Deployments match {selector}: {}, so the operator namespace is ambiguous",
                many.len(),
                many.iter()
                    .map(|d| format!(
                        "{}/{}",
                        d.metadata.namespace.clone().unwrap_or_default(),
                        d.metadata.name.clone().unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            fix: "remove the extra or stale controller Deployment (there should be only one)"
                .to_string(),
        }),
    }
}

/// Fetch just the [`RepoHandle`] for an explicitly-named repository (the
/// `session end --repository` path). A namespaced `Repository` is looked up in
/// the context's namespace.
pub async fn resolve_repo(
    ctx: &OpsCtx,
    kind: RepositoryKind,
    name: &str,
) -> Result<RepoHandle, OpsError> {
    match kind {
        RepositoryKind::Repository => {
            let ns = ctx.namespace.clone();
            let api: Api<Repository> = Api::namespaced(ctx.client.clone(), &ns);
            let r = api.get(name).await.map_err(|e| {
                classify_kube(
                    "get",
                    "Repository",
                    "repositories",
                    Some(&ns),
                    Some(name),
                    e,
                )
            })?;
            Ok(RepoHandle {
                kind,
                name: r.name_any(),
                uid: r.uid().unwrap_or_default(),
                namespace: Some(ns),
                execution: kopiur_api::ExecutionMode::InNamespace,
                backend: r.spec.backend.clone(),
                encryption: r.spec.encryption.clone(),
                mover_defaults: r.spec.mover_defaults.clone(),
            })
        }
        RepositoryKind::ClusterRepository => {
            let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
            let r = api.get(name).await.map_err(|e| {
                classify_kube(
                    "get",
                    "ClusterRepository",
                    "clusterrepositories",
                    None,
                    Some(name),
                    e,
                )
            })?;
            Ok(RepoHandle {
                kind,
                name: r.name_any(),
                uid: r.uid().unwrap_or_default(),
                namespace: None,
                execution: r
                    .spec
                    .execution
                    .as_ref()
                    .map(|e| e.mode)
                    .unwrap_or_default(),
                backend: r.spec.backend.clone(),
                encryption: r.spec.encryption.clone(),
                mover_defaults: r.spec.mover_defaults.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(v: serde_json::Value) -> Snapshot {
        serde_json::from_value(v).expect("snapshot fixture")
    }

    #[test]
    fn kopia_id_extracted_from_a_succeeded_snapshot() {
        let s = snap(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": "db-1", "namespace": "media" },
            "spec": {},
            "status": {
                "phase": "Succeeded",
                "snapshot": {
                    "kopiaSnapshotID": "kdeadbeef",
                    "identity": { "username": "db", "hostname": "media", "sourcePath": "/data" }
                }
            }
        }));
        assert_eq!(kopia_id_of(&s).unwrap(), "kdeadbeef");
    }

    #[test]
    fn running_or_statusless_snapshot_is_not_browsable_with_the_phase_named() {
        let running = snap(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": "db-1", "namespace": "media" },
            "spec": {},
            "status": { "phase": "Running" }
        }));
        let msg = kopia_id_of(&running).unwrap_err().to_string();
        assert!(msg.contains("phase is Running"), "{msg}");

        let bare = snap(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": "db-2", "namespace": "media" },
            "spec": {}
        }));
        let msg = kopia_id_of(&bare).unwrap_err().to_string();
        assert!(msg.contains("no status yet"), "{msg}");
    }

    fn repo_handle(kind: RepositoryKind, ns: Option<&str>, secret_ns: Option<&str>) -> RepoHandle {
        use kopiur_api::backend::S3Backend;
        use kopiur_api::common::SecretKeyRef;
        RepoHandle {
            kind,
            name: "nas".into(),
            uid: "u1".into(),
            namespace: ns.map(str::to_string),
            execution: kopiur_api::ExecutionMode::InNamespace,
            backend: Backend::S3(S3Backend {
                bucket: "b".into(),
                prefix: None,
                endpoint: None,
                region: None,
                auth: None,
                tls: None,
            }),
            encryption: Encryption {
                password_secret_ref: SecretKeyRef {
                    name: "creds".into(),
                    key: Some("KOPIA_PASSWORD".into()),
                    namespace: secret_ns.map(str::to_string),
                },
            },
            mover_defaults: None,
        }
    }

    fn target(repo: RepoHandle, projection: SessionProjection) -> BrowseTarget {
        BrowseTarget {
            snapshot: "db-1".into(),
            namespace: "media".into(),
            session_namespace: "media".into(),
            kopia_snapshot_id: "kdead".into(),
            repo,
            ca_bundle_pem: None,
            projection,
        }
    }

    const JOB: &str = "kopiur-browse-nas-0123abcd";

    /// A Central ClusterRepository's session runs in the operator namespace,
    /// where its Secrets are: they load directly, and one that is anywhere
    /// else is refused, never copied. Copying it is exactly what
    /// `execution.mode: Central` promises not to do.
    #[test]
    fn a_central_repositorys_session_never_asks_for_a_copied_secret() {
        let central = |secret_ns: &str| {
            let mut repo = repo_handle(RepositoryKind::ClusterRepository, None, Some(secret_ns));
            repo.execution = kopiur_api::ExecutionMode::Central;
            assert!(central_session(&repo));
            let mut t = target(
                repo,
                // Both projection gates open: they must not matter.
                SessionProjection {
                    allowed: true,
                    opted_in: true,
                },
            );
            t.session_namespace = "kopiur-system".into();
            t
        };
        // The Secret is where the session runs.
        assert_eq!(
            session_creds(&central("kopiur-system"), JOB).unwrap(),
            vec![SessionCreds::Direct("creds".into())]
        );
        // The Secret is in the Snapshot's namespace, or anywhere else.
        for elsewhere in ["media", "secrets"] {
            match session_creds(&central(elsewhere), JOB) {
                Err(OpsError::CredsOutsideSessionNamespace {
                    session_namespace,
                    blocked_by: ProjectionBlock::CentralExecution { repository },
                    ..
                }) => {
                    assert_eq!(session_namespace, "kopiur-system");
                    assert_eq!(repository, "nas");
                }
                other => panic!("expected a CentralExecution refusal, got {other:?}"),
            }
        }

        // Every other repository's session runs beside the Snapshot.
        assert!(!central_session(&repo_handle(
            RepositoryKind::ClusterRepository,
            None,
            Some("kopiur-system")
        )));
        let mut namespaced = repo_handle(RepositoryKind::Repository, Some("media"), None);
        // A namespaced Repository has no `execution`; a stray value moves nothing.
        namespaced.execution = kopiur_api::ExecutionMode::Central;
        assert!(!central_session(&namespaced));
    }
    const BOTH: SessionProjection = SessionProjection {
        allowed: true,
        opted_in: true,
    };

    #[test]
    fn same_namespace_secret_loads_directly() {
        let t = target(
            repo_handle(RepositoryKind::Repository, Some("media"), None),
            SessionProjection::default(),
        );
        assert_eq!(
            session_creds(&t, JOB).unwrap(),
            vec![SessionCreds::Direct("creds".into())]
        );
    }

    #[test]
    fn a_namespaced_repository_elsewhere_is_refused_without_offering_projection() {
        // Repository in `backups`, session (snapshot) in `media`: the secret
        // falls back to the repository's namespace, which the session pod
        // cannot envFrom, and only a ClusterRepository may project.
        let t = target(
            repo_handle(RepositoryKind::Repository, Some("backups"), None),
            BOTH,
        );
        let err = session_creds(&t, JOB).unwrap_err();
        assert!(matches!(
            err,
            OpsError::CredsOutsideSessionNamespace {
                blocked_by: ProjectionBlock::NotAClusterRepository,
                ..
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("namespace backups"), "{msg}");
        assert!(msg.contains("--local"), "{msg}");
    }

    #[test]
    fn a_cluster_repository_secret_elsewhere_is_projected_when_both_gates_hold() {
        let t = target(
            repo_handle(RepositoryKind::ClusterRepository, None, Some("kopiur")),
            BOTH,
        );
        assert_eq!(
            session_creds(&t, JOB).unwrap(),
            vec![SessionCreds::Projected {
                source_namespace: "kopiur".into(),
                source_name: "creds".into(),
                copy_name: format!("{JOB}-creds-0"),
            }]
        );
    }

    #[test]
    fn each_closed_gate_is_named_in_the_refusal() {
        let repo = || repo_handle(RepositoryKind::ClusterRepository, None, Some("kopiur"));
        let owner_closed = session_creds(
            &target(
                repo(),
                SessionProjection {
                    allowed: false,
                    opted_in: true,
                },
            ),
            JOB,
        )
        .unwrap_err();
        assert!(matches!(
            &owner_closed,
            OpsError::CredsOutsideSessionNamespace {
                blocked_by: ProjectionBlock::OwnerNotAllowed { repository },
                ..
            } if repository == "nas"
        ));
        assert!(
            owner_closed
                .to_string()
                .contains("spec.credentialProjection.allowed"),
            "{owner_closed}"
        );

        let consumer_closed = session_creds(
            &target(
                repo(),
                SessionProjection {
                    allowed: true,
                    opted_in: false,
                },
            ),
            JOB,
        )
        .unwrap_err();
        assert!(matches!(
            &consumer_closed,
            OpsError::CredsOutsideSessionNamespace {
                blocked_by: ProjectionBlock::ConsumerNotOptedIn { snapshot },
                ..
            } if snapshot == "db-1"
        ));
        assert!(
            consumer_closed
                .to_string()
                .contains("spec.credentialProjection.enabled"),
            "{consumer_closed}"
        );
    }

    #[test]
    fn a_cluster_repository_secret_pinned_to_the_session_namespace_loads_directly() {
        let t = target(
            repo_handle(RepositoryKind::ClusterRepository, None, Some("media")),
            SessionProjection::default(),
        );
        assert_eq!(
            session_creds(&t, JOB).unwrap(),
            vec![SessionCreds::Direct("creds".into())]
        );
        // No namespace at all on a cluster repo ref: names the CRD field.
        let t = target(
            repo_handle(RepositoryKind::ClusterRepository, None, None),
            BOTH,
        );
        let msg = session_creds(&t, JOB).unwrap_err().to_string();
        assert!(msg.contains("secretRef.namespace"), "{msg}");
    }

    #[test]
    fn a_snapshot_opted_in_only_when_its_run_pinned_projection_on() {
        let with = |resolved: serde_json::Value| {
            snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "db-1", "namespace": "media" },
                "spec": {},
                "status": { "resolved": resolved }
            }))
        };
        assert!(snapshot_opted_in(&with(
            serde_json::json!({ "credentialProjection": { "enabled": true } })
        )));
        assert!(!snapshot_opted_in(&with(
            serde_json::json!({ "credentialProjection": { "enabled": false } })
        )));
        assert!(
            !snapshot_opted_in(&with(serde_json::json!({}))),
            "no pin is no"
        );
    }

    #[test]
    fn the_copy_name_is_the_one_the_operator_writes() {
        // The session Job and the operator agree on the name through one
        // shared function; this pins that the session uses it, per source.
        assert_eq!(
            kopiur_api::creds::browse_session_copy_name(JOB, 1),
            format!("{JOB}-creds-1")
        );
    }

    #[test]
    fn a_known_operator_namespace_bypasses_the_cluster_lookup() {
        assert_eq!(
            known_operator_namespace(&OperatorNamespace::Known("kopiur-system".into())),
            Some("kopiur-system")
        );
        // Discovery must NOT resolve to a default here: returning None is what
        // sends `resolve_ca_bundle` to the fail-closed cluster lookup.
        assert_eq!(
            known_operator_namespace(&OperatorNamespace::DiscoverFromControllerDeployment),
            None
        );
    }
}
