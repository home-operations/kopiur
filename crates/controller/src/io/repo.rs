use chrono::{DateTime, Utc};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::Api;
use kube::api::{Patch, PatchParams};
use kube::runtime::reflector::Store;

use kopiur_api::backend::Backend;
use kopiur_api::common::{
    CatalogBounds, DeletionProtectionSpec, Encryption, IdentityDefaults, MoverDefaults,
    NamespaceDeletePolicy, PhaseLabel, RepositoryKind, RepositoryMode, RepositoryRef,
    ScheduleDefaults,
};
use kopiur_api::preflight::{PreflightInputs, UNKNOWN_AGE};
use kopiur_api::repository::{RepositoryHealthStatus, RepositoryPhase, StorageStats};
use kopiur_api::{ClusterRepository, Maintenance, Repository};

use crate::error::{Error, Result};

/// Default key within the encryption password Secret when unset.
pub const DEFAULT_PASSWORD_KEY: &str = "KOPIA_PASSWORD";

/// The single repository a `SnapshotPolicy` recipe targets, on the
/// controller's defensive-validation error path
/// ([`Error::Validation`], the same class the reconcilers' spec re-validation
/// uses). The multi-repo shape errors here rather than silently picking
/// repository #1. Post-M8, its remaining callers are the DELIBERATELY
/// single-repo policy-LEVEL consumers — the verification scheduler (per-repo
/// verification lands M10) and the schedule's repository-timezone default —
/// both of which the multi-repo path bypasses; per-CHILD repository selection
/// goes through [`kopiur_api::snapshot::effective_repository_ref`] instead.
pub fn recipe_repo_ref(config: &kopiur_api::SnapshotPolicy) -> Result<&RepositoryRef> {
    kopiur_api::single_repository_ref(&config.spec).map_err(|e| Error::Validation(e.to_string()))
}

/// Default key within a `tls.caBundleRef` ConfigMap when unset — re-exported
/// from [`kopiur_api::consts`] (shared with the kubectl-plugin's client-side
/// resolver, which must agree on the default).
pub use kopiur_api::consts::DEFAULT_CA_BUNDLE_KEY;

/// Upper bound on a resolved CA bundle. The whole work spec must fit in a Job
/// env var ([`kopiur_mover::jobs`] caps it at 100 KiB against `MAX_ARG_STRLEN`),
/// so an oversized bundle is rejected HERE with a message naming the ConfigMap,
/// not later as an opaque work-spec-too-large build failure.
pub const MAX_CA_BUNDLE_BYTES: usize = 64 * 1024;

/// Resolve the mover work-spec throttle from a repository's `moverDefaults.throttle`
/// (ADR-0005 §13(e)). Thin wrapper over the single tested mapping so the field is
/// never inert; an absent throttle yields an empty spec (the mover skips the call).
pub fn throttle_spec(defaults: Option<&MoverDefaults>) -> kopiur_mover::workspec::ThrottleSpec {
    kopiur_mover::workspec::ThrottleSpec::from_mover_defaults(defaults)
}

/// The credentials a mover Job needs, sourced from a repository's
/// `encryption.passwordSecretRef`. The Secret is mounted as env (`envFrom`), so
/// here we only need its name; the key resolution is documented for callers that
/// read the password in-process.
#[derive(Debug, Clone)]
pub struct RepoCredentials {
    /// Secret name holding `KOPIA_PASSWORD` (+ optional backend creds).
    pub secret_name: String,
    /// The key within the Secret holding the repository password.
    pub password_key: String,
    /// Optional explicit namespace (cluster-scoped repos require it).
    pub namespace: Option<String>,
}

/// The repository surface a backup/restore/maintenance run needs, resolved from
/// either a namespaced [`Repository`] or a cluster-scoped [`ClusterRepository`].
///
/// Both CRDs expose the same `backend`/`encryption` shape; the reconcilers only
/// need those two fields to connect to kopia, so we normalize once at resolution
/// time. The discriminated [`RepositoryKind`] is `match`ed exhaustively in
/// [`resolve_repository_ref`] (ADR §5.5) — a future variant cannot compile until
/// it is handled here.
#[derive(Debug, Clone)]
pub struct ResolvedRepository {
    /// The repository's storage backend (normalized from either CRD).
    pub backend: Backend,
    /// The repository's encryption/password configuration.
    pub encryption: Encryption,
    /// The repository's own namespace (`Some` for a namespaced [`Repository`],
    /// `None` for a cluster-scoped [`ClusterRepository`]). Used as the *source*
    /// namespace fallback when a credential Secret reference omits one.
    pub repo_namespace: Option<String>,
    /// The repository's `moverDefaults` — the base for every mover it spawns
    /// (security context, pod security context, resources, cache, node placement,
    /// Job TTL), merged field-wise under each run's `mover` (ADR-0004 §1/§2).
    pub mover_defaults: Option<MoverDefaults>,
    /// The repository's `identityDefaults` (CEL `*Expr`), applied when a
    /// consumer doesn't override its identity (ADR-0004 §5; M5 gave the
    /// namespaced `Repository` the same field the cluster-scoped kind has had
    /// since M1).
    pub identity_defaults: Option<IdentityDefaults>,
    /// The repository's `scheduleDefaults` (e.g. `timezone`), inherited at
    /// reconcile time by consumers that don't set their own equivalent field —
    /// verification, replication, and maintenance schedules today (GitHub #174
    /// item 3). Resolved via [`kopiur_api::common::resolve_tz_with_default`], NOT
    /// admission-pinned.
    pub schedule_defaults: Option<ScheduleDefaults>,
    /// The repository's namespace-deletion cascade policy (ADR-0005 §5): `Orphan`
    /// (keep history) or `Delete` (cascade). Consulted by the `Snapshot` finalizer
    /// when the owning namespace is terminating.
    pub on_namespace_delete: NamespaceDeletePolicy,
    /// The `ClusterRepository`-owner credential-projection allow gate
    /// (`credentialProjection.allowed`, ADR-0005 §8). `false` for a namespaced
    /// `Repository` (which has no such gate — projection there is a same-namespace
    /// no-op, so the owner gate is irrelevant).
    pub credential_projection_allowed: bool,
    /// The repository's access mode (`ReadWrite`/`ReadOnly`, ADR-0005 §11). A
    /// `ReadOnly` repo refuses backup Jobs and maintenance (restores allowed); the
    /// `Snapshot` reconciler gates on [`RepositoryMode::allows_writes`].
    pub mode: RepositoryMode,
    /// An `OwnerReference` to the repository CR itself. The namespace-deletion
    /// cascade runs its `SnapshotDelete` Job *outside* the terminating namespace
    /// (ADR-0005 §5), where the deleting `Snapshot` cannot own it (cross-namespace
    /// owner references are invalid) — the repository, which outlives the
    /// namespace, owns the Job instead so GC still reaps it.
    pub owner_ref: OwnerReference,
    /// The repository's mass-deletion breaker config
    /// (`spec.deletionProtection`), cloned verbatim from either repo kind.
    /// `None` → the breaker's default threshold applies
    /// (`kopiur_api::consts::effective_mass_deletion_threshold`).
    pub deletion_protection: Option<DeletionProtectionSpec>,
    /// The RAW value of [`crate::consts::ALLOW_MASS_DELETION_ANNOTATION`] from
    /// the repository's `metadata.annotations`, if present. Deliberately
    /// unparsed here: parsing/clamping the RFC3339 ack (and rejecting a
    /// malformed value loudly rather than silently disarming the breaker) is
    /// the consumer's job (M4's breaker evaluation), not this resolver's.
    pub mass_deletion_ack: Option<String>,
    /// The repository's `spec.catalog` bounds (`CatalogBounds`), cloned verbatim
    /// from either repo kind. Carries `catalog.adoption`, the middle link of the
    /// policy → repo → constant adoption-policy chain the `SnapshotPolicy`
    /// reconciler resolves via
    /// [`kopiur_api::common::effective_adoption`](kopiur_api::common::effective_adoption)
    /// before running auto-adoption (M6). `None` → no catalog config, so
    /// adoption falls through to the policy's own `spec.adoption` (else the
    /// default).
    pub catalog: Option<CatalogBounds>,
    /// The RESOLVED content of the backend's `tls.caBundleRef` ConfigMap (a PEM
    /// CA bundle), read live at resolution time by [`resolve_backend_ca`];
    /// `None` when the backend declares none. Resolved HERE — not at each
    /// work-spec build site — so no mover-spawning path can forget it: the
    /// projection ([`kopiur_mover::repo_meta::backend_to_repository_connect`])
    /// requires the value as a parameter, and this field is the single source.
    pub ca_bundle_pem: Option<String>,
}

/// Which API a [`RepositoryRef`] resolves against, derived purely from `kind`.
///
/// Extracting this from the IO lets the namespaced-vs-cluster decision be
/// unit-tested without a cluster. It is the regression guard for the class of
/// bug where a `kind: ClusterRepository` ref silently fell through to a
/// namespaced `Repository` lookup and produced `missing dependency: Repository
/// <ns>/<name>` for cluster-backed `SnapshotPolicy`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoLookup {
    /// Namespaced `Repository` get in `namespace`.
    Namespaced {
        /// Namespace to perform the `Repository` get in.
        namespace: String,
        /// Name of the `Repository` to get.
        name: String,
    },
    /// Cluster-scoped `ClusterRepository` get (`Api::all`).
    Cluster {
        /// Name of the `ClusterRepository` to get.
        name: String,
    },
}

/// Pure mapping from a consumer's [`RepositoryRef`] (+ the default namespace to
/// use when the ref omits one) to the API lookup it implies. Exhaustive over
/// [`RepositoryKind`] (ADR §5.5): a new variant cannot compile until handled.
///
/// ```
/// use kopiur_controller::io::{repo_lookup, RepoLookup};
/// use kopiur_api::common::{RepositoryKind, RepositoryRef};
///
/// // A namespaced ref with no explicit namespace falls back to `default_ns`.
/// let r = RepositoryRef { kind: RepositoryKind::Repository, name: "nas".into(), namespace: None };
/// assert_eq!(
///     repo_lookup(&r, "billing"),
///     RepoLookup::Namespaced { namespace: "billing".into(), name: "nas".into() },
/// );
///
/// // A ClusterRepository ref ignores any namespace and resolves cluster-wide.
/// let c = RepositoryRef {
///     kind: RepositoryKind::ClusterRepository,
///     name: "shared".into(),
///     namespace: Some("ignored".into()),
/// };
/// assert_eq!(repo_lookup(&c, "billing"), RepoLookup::Cluster { name: "shared".into() });
/// ```
pub fn repo_lookup(repo_ref: &RepositoryRef, default_ns: &str) -> RepoLookup {
    match repo_ref.kind {
        RepositoryKind::Repository => RepoLookup::Namespaced {
            namespace: repo_ref
                .namespace
                .as_deref()
                .unwrap_or(default_ns)
                .to_string(),
            name: repo_ref.name.clone(),
        },
        // Cluster-scoped: `ref.namespace` is forbidden (webhook-enforced) and
        // deliberately ignored here.
        RepositoryKind::ClusterRepository => RepoLookup::Cluster {
            name: repo_ref.name.clone(),
        },
    }
}

/// Resolve a consumer CR's [`RepositoryRef`] to its backend + encryption,
/// honoring `kind` via [`repo_lookup`]:
///
/// - [`RepositoryKind::Repository`]: namespaced lookup, cross-namespace allowed
///   via `ref.namespace` (defaulting to `default_ns`, usually the consumer's
///   namespace).
/// - [`RepositoryKind::ClusterRepository`]: cluster-scoped lookup (`Api::all`).
///
/// `operator_ns` is the operator's own namespace (`ctx.operator_namespace`),
/// used only to resolve a `ClusterRepository`'s `tls.caBundleRef` ConfigMap
/// (see [`resolve_backend_ca`] for the namespace rule).
pub async fn resolve_repository_ref(
    client: &kube::Client,
    repo_ref: &RepositoryRef,
    default_ns: &str,
    operator_ns: Option<&str>,
) -> Result<ResolvedRepository> {
    match repo_lookup(repo_ref, default_ns) {
        RepoLookup::Namespaced { namespace, name } => {
            let api: Api<Repository> = Api::namespaced(client.clone(), &namespace);
            let repo = api.get_opt(&name).await?.ok_or_else(|| {
                Error::MissingDependency(format!("Repository {namespace}/{name}"))
            })?;
            let owner_ref = super::owner_ref_for(&repo, "Repository")?;
            let mass_deletion_ack = mass_deletion_ack(&repo);
            let ca_bundle_pem =
                resolve_backend_ca(client, &repo.spec.backend, Some(&namespace), operator_ns)
                    .await?;
            Ok(ResolvedRepository {
                repo_namespace: Some(namespace),
                backend: repo.spec.backend,
                encryption: repo.spec.encryption,
                mover_defaults: repo.spec.mover_defaults,
                identity_defaults: repo.spec.identity_defaults,
                schedule_defaults: repo.spec.schedule_defaults,
                on_namespace_delete: repo.spec.on_namespace_delete,
                // A namespaced Repository has no owner-side projection gate: its
                // credential Secret co-resides with the consumer, so projection is a
                // same-namespace no-op. Treat the gate as not-applicable (false).
                credential_projection_allowed: false,
                mode: repo.spec.mode,
                owner_ref,
                deletion_protection: repo.spec.deletion_protection,
                mass_deletion_ack,
                catalog: repo.spec.catalog,
                ca_bundle_pem,
            })
        }
        RepoLookup::Cluster { name } => {
            let api: Api<ClusterRepository> = Api::all(client.clone());
            let repo = api
                .get_opt(&name)
                .await?
                .ok_or_else(|| Error::MissingDependency(format!("ClusterRepository {name}")))?;
            let owner_ref = super::owner_ref_for(&repo, "ClusterRepository")?;
            let mass_deletion_ack = mass_deletion_ack(&repo);
            let ca_bundle_pem =
                resolve_backend_ca(client, &repo.spec.backend, None, operator_ns).await?;
            Ok(ResolvedRepository {
                repo_namespace: None,
                backend: repo.spec.backend,
                encryption: repo.spec.encryption,
                mover_defaults: repo.spec.mover_defaults,
                identity_defaults: repo.spec.identity_defaults,
                schedule_defaults: repo.spec.schedule_defaults,
                on_namespace_delete: repo.spec.on_namespace_delete,
                credential_projection_allowed: repo
                    .spec
                    .credential_projection
                    .map(|p| p.allowed)
                    .unwrap_or(false),
                mode: repo.spec.mode,
                owner_ref,
                deletion_protection: repo.spec.deletion_protection,
                mass_deletion_ack,
                catalog: repo.spec.catalog,
                ca_bundle_pem,
            })
        }
    }
}

/// Resolve the PEM content of a backend's `tls.caBundleRef` ConfigMap, or
/// `Ok(None)` when the backend declares none.
///
/// **Namespace rule** (mirrors the `SecretKeyRef` convention documented on
/// [`kopiur_api::common::SecretKeyRef`]): `ConfigMapKeyRef` carries no
/// namespace, so a namespaced `Repository`'s bundle lives in the repository's
/// own namespace (`referrer_ns`), and a `ClusterRepository`'s in the operator's
/// namespace (`operator_ns`, from `KOPIUR_NAMESPACE`). The content — never a
/// reference — is inlined into the mover work spec, so the bundle does NOT need
/// to exist in every namespace movers run in.
///
/// Every failure is typed and actionable (what failed / why / how to fix), and
/// fails the reconcile BEFORE a Job is stamped — the alternative (a ConfigMap
/// volume with `optional: false`) leaves the pod wedged in
/// `ContainerCreating` with no condition and no event.
pub async fn resolve_backend_ca(
    client: &kube::Client,
    backend: &Backend,
    referrer_ns: Option<&str>,
    operator_ns: Option<&str>,
) -> Result<Option<String>> {
    use k8s_openapi::api::core::v1::ConfigMap;
    let Some(key_ref) = kopiur_mover::repo_meta::backend_tls_ca_keyref(backend) else {
        return Ok(None);
    };
    // An empty `caBundleRef: {}` is webhook-rejected (`validate_backend`), but
    // this runs defensively on possibly-pre-webhook objects: surface it, don't
    // silently skip TLS trust the user asked for.
    let Some(cm_name) = key_ref.config_map_name.as_deref() else {
        return Err(Error::Validation(
            "tls.caBundleRef is set but names no ConfigMap: set \
             tls.caBundleRef.configMapName to the ConfigMap holding the PEM CA \
             bundle, or remove the caBundleRef block"
                .to_string(),
        ));
    };
    let ns = match referrer_ns.or(operator_ns) {
        Some(ns) => ns.to_string(),
        None => {
            return Err(Error::Invariant(format!(
                "cannot resolve tls.caBundleRef ConfigMap {cm_name:?}: a \
                 ClusterRepository's CA bundle lives in the operator's namespace, \
                 but KOPIUR_NAMESPACE is unset (running out-of-cluster?) — set \
                 KOPIUR_NAMESPACE or move the repository to a namespaced Repository"
            )));
        }
    };
    let key = key_ref.key.as_deref().unwrap_or(DEFAULT_CA_BUNDLE_KEY);
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    let cm = api.get_opt(cm_name).await?.ok_or_else(|| {
        Error::MissingCaBundle(format!(
            "tls.caBundleRef ConfigMap {ns}/{cm_name} not found: create it with the \
             PEM CA bundle under key {key:?} (for a ClusterRepository it must live \
             in the operator's namespace; for a Repository, in the repository's)"
        ))
    })?;
    // ConfigMaps carry text under `data` and bytes under `binaryData`; a PEM
    // bundle is text, but accept both so `kubectl create configmap --from-file`
    // works however the file was attached.
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
        return Err(Error::MissingCaBundle(format!(
            "tls.caBundleRef ConfigMap {ns}/{cm_name} has no key {key:?} (found: \
             {available:?}): add the PEM CA bundle under that key, or set \
             tls.caBundleRef.key to one of the existing keys"
        )));
    };
    if !pem.contains("BEGIN CERTIFICATE") {
        return Err(Error::Validation(format!(
            "tls.caBundleRef ConfigMap {ns}/{cm_name} key {key:?} is not a PEM \
             certificate bundle (no \"BEGIN CERTIFICATE\" block): store the CA \
             chain PEM-encoded — kopia passes it to the S3 TLS verifier verbatim"
        )));
    }
    if pem.len() > MAX_CA_BUNDLE_BYTES {
        return Err(Error::Validation(format!(
            "tls.caBundleRef ConfigMap {ns}/{cm_name} key {key:?} is {} bytes, \
             over the {MAX_CA_BUNDLE_BYTES}-byte limit: the bundle rides the mover \
             work spec (a Job env var), so trim it to the CA chain this endpoint \
             actually needs",
            pem.len()
        )));
    }
    Ok(Some(pem))
}

/// The RAW [`crate::consts::ALLOW_MASS_DELETION_ANNOTATION`] value from a
/// repository object's `metadata.annotations`, shared by both repo kinds so
/// the resolver's two arms don't duplicate the lookup.
fn mass_deletion_ack<K: kube::Resource>(repo: &K) -> Option<String> {
    repo.meta()
        .annotations
        .as_ref()?
        .get(crate::consts::ALLOW_MASS_DELETION_ANNOTATION)
        .cloned()
}

/// Whether the referenced repository is connected and healthy
/// (`status.phase == Ready`). Maintenance gates Job spawning on this: an
/// object-store repository must be bootstrapped (connected/created) before
/// `kopia maintenance` can reach it, so spawning a maintenance Job earlier just
/// produces a doomed pod (ADR §3.7, G7). Honors `kind` via [`repo_lookup`].
pub async fn repository_ready(
    client: &kube::Client,
    repo_ref: &RepositoryRef,
    default_ns: &str,
) -> Result<bool> {
    let ready = Some(kopiur_api::RepositoryPhase::Ready);
    match repo_lookup(repo_ref, default_ns) {
        RepoLookup::Namespaced { namespace, name } => {
            let api: Api<Repository> = Api::namespaced(client.clone(), &namespace);
            let repo = api.get_opt(&name).await?.ok_or_else(|| {
                Error::MissingDependency(format!("Repository {namespace}/{name}"))
            })?;
            Ok(repo.status.and_then(|s| s.phase) == ready)
        }
        RepoLookup::Cluster { name } => {
            let api: Api<ClusterRepository> = Api::all(client.clone());
            let repo = api
                .get_opt(&name)
                .await?
                .ok_or_else(|| Error::MissingDependency(format!("ClusterRepository {name}")))?;
            Ok(repo.status.and_then(|s| s.phase) == ready)
        }
    }
}

/// Seconds since an RFC3339 timestamp (`(known, age)`): clamped to ≥0, and
/// `(false, UNKNOWN_AGE)` when absent/unparseable so a freshness guard fails closed.
fn age_seconds(ts: Option<&str>, now: DateTime<Utc>) -> (bool, i64) {
    match ts.and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
        Some(t) => (true, (now - t.with_timezone(&Utc)).num_seconds().max(0)),
        None => (false, UNKNOWN_AGE),
    }
}

/// **Pure.** Map a repository status's common fields (shared by `Repository` and
/// `ClusterRepository`) into [`PreflightInputs`]. The `maintenance.*` fields are left
/// at their fail-closed defaults — the caller fills them via [`maintenance_recency`].
pub fn repo_status_to_inputs(
    phase: Option<&RepositoryPhase>,
    conditions: &[Condition],
    storage: Option<&StorageStats>,
    health: Option<&RepositoryHealthStatus>,
    last_reverify_at: Option<&str>,
    now: DateTime<Utc>,
) -> PreflightInputs {
    // `BackendReachable` absent ⇒ the health probe is off ⇒ no evidence the backend
    // is down, so don't block on it.
    let backend_reachable = conditions
        .iter()
        .find(|c| c.type_ == crate::consts::BACKEND_REACHABLE_CONDITION)
        .is_none_or(|c| c.status == "True");
    let (last_healthy_known, last_healthy_age_seconds) =
        age_seconds(health.and_then(|h| h.last_healthy_at.as_deref()), now);
    let (last_reverify_known, last_reverify_age_seconds) = age_seconds(last_reverify_at, now);
    // Counts/sizes fail OPEN for `> N` checks at the UNKNOWN_AGE sentinel, so a
    // `*Known` companion is exposed for the user to guard with. `split` derives both
    // from the SAME Option so the known flag can't desync from its value.
    let split = |o: Option<i64>| (o.is_some(), o.unwrap_or(UNKNOWN_AGE));
    let (snapshot_count_known, snapshot_count) = split(storage.and_then(|s| s.snapshot_count));
    let (index_blob_count_known, index_blob_count) =
        split(storage.and_then(|s| s.index_blob_count));
    let (size_bytes_known, size_bytes) = split(storage.and_then(|s| s.total_size_bytes));
    PreflightInputs {
        repository_phase: phase.map(|p| p.label().to_string()).unwrap_or_default(),
        repository_ready: phase == Some(&RepositoryPhase::Ready),
        backend_reachable,
        snapshot_count_known,
        snapshot_count,
        index_blob_count_known,
        index_blob_count,
        size_bytes_known,
        size_bytes,
        last_healthy_known,
        last_healthy_age_seconds,
        last_reverify_known,
        last_reverify_age_seconds,
        // Maintenance filled by the caller.
        ..PreflightInputs::default()
    }
}

/// **Pure over an iterator.** The "last successful maintenance" age for the repo
/// `(kind, name, match_namespace)`: the max of `{full,quick}.{lastRunAt,lastHandledAt}`
/// across every matching `Maintenance`. `lastRunAt` is mover-written on *every*
/// successful run — scheduled **and** manual run-now — and `lastHandledAt` is the
/// controller-observed terminal success (covering the reaped-Job case), so the max
/// reflects a manual run-now without touching the cron-slot dedup field. Returns
/// `(false, UNKNOWN_AGE)` when no successful run is recorded (fail-closed).
pub fn maintenance_recency(
    items: impl IntoIterator<Item = Maintenance>,
    kind: RepositoryKind,
    name: &str,
    match_namespace: Option<&str>,
    now: DateTime<Utc>,
) -> (bool, i64) {
    let mut newest: Option<DateTime<Utc>> = None;
    for m in items {
        let owner_ns = m.metadata.namespace.as_deref().unwrap_or_default();
        if !m
            .spec
            .repository
            .resolves_to(owner_ns, kind, name, match_namespace)
        {
            continue;
        }
        let Some(st) = m.status.as_ref() else {
            continue;
        };
        for run in [st.full.as_ref(), st.quick.as_ref()].into_iter().flatten() {
            for ts in [run.last_run_at.as_deref(), run.last_handled_at.as_deref()]
                .into_iter()
                .flatten()
            {
                if let Ok(t) = DateTime::parse_from_rfc3339(ts) {
                    let t = t.with_timezone(&Utc);
                    newest = Some(newest.map_or(t, |n| n.max(t)));
                }
            }
        }
    }
    match newest {
        Some(t) => (true, (now - t).num_seconds().max(0)),
        None => (false, UNKNOWN_AGE),
    }
}

/// Gather the live [`PreflightInputs`] for a repository plus its readiness — one
/// `Repository`/`ClusterRepository` GET for status, and the shared `Maintenance`
/// informer for recency. Returns `(inputs, ready)` so the caller derives the
/// readiness gate from the same fetch (no second lookup). `MissingDependency` if
/// the repository is absent (mirrors [`repository_ready`]).
pub async fn gather_preflight_inputs(
    client: &kube::Client,
    repo_ref: &RepositoryRef,
    default_ns: &str,
    maintenance_store: &Store<Maintenance>,
    now: DateTime<Utc>,
) -> Result<(PreflightInputs, bool)> {
    let (mut inputs, kind, name, match_ns) = match repo_lookup(repo_ref, default_ns) {
        RepoLookup::Namespaced { namespace, name } => {
            let api: Api<Repository> = Api::namespaced(client.clone(), &namespace);
            let repo = api.get_opt(&name).await?.ok_or_else(|| {
                Error::MissingDependency(format!("Repository {namespace}/{name}"))
            })?;
            let st = repo.status.as_ref();
            let inputs = repo_status_to_inputs(
                st.and_then(|s| s.phase.as_ref()),
                st.map(|s| s.conditions.as_slice()).unwrap_or(&[]),
                st.and_then(|s| s.storage_stats.as_ref()),
                st.and_then(|s| s.health.as_ref()),
                st.and_then(|s| s.last_reverify_at.as_deref()),
                now,
            );
            (inputs, RepositoryKind::Repository, name, Some(namespace))
        }
        RepoLookup::Cluster { name } => {
            let api: Api<ClusterRepository> = Api::all(client.clone());
            let repo = api
                .get_opt(&name)
                .await?
                .ok_or_else(|| Error::MissingDependency(format!("ClusterRepository {name}")))?;
            let st = repo.status.as_ref();
            let inputs = repo_status_to_inputs(
                st.and_then(|s| s.phase.as_ref()),
                st.map(|s| s.conditions.as_slice()).unwrap_or(&[]),
                st.and_then(|s| s.storage_stats.as_ref()),
                st.and_then(|s| s.health.as_ref()),
                st.and_then(|s| s.last_reverify_at.as_deref()),
                now,
            );
            (inputs, RepositoryKind::ClusterRepository, name, None)
        }
    };
    let ready = inputs.repository_ready;
    // Best-effort recency from the shared informer; a cold store ⇒ fail-closed
    // (has_run=false, UNKNOWN_AGE), bounded by the preflight timeout.
    let (has_run, age) = maintenance_recency(
        maintenance_store.state().iter().map(|m| (**m).clone()),
        kind,
        &name,
        match_ns.as_deref(),
        now,
    );
    inputs.maintenance_has_run = has_run;
    inputs.maintenance_last_success_age_seconds = age;
    Ok((inputs, ready))
}

/// Stamp the reverify annotation (`now`, RFC3339) to request an immediate
/// connectivity re-probe. Best-effort: skips a non-`Ready` repo (the gate already
/// holds) and a request within the rate-limit window; a missing repo is a no-op.
pub async fn request_repository_reverify(
    client: &kube::Client,
    repo_ref: &RepositoryRef,
    default_ns: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    /// At most one forced re-probe per repository per this window.
    const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
    let ready = Some(&kopiur_api::RepositoryPhase::Ready);
    let key = crate::consts::REVERIFY_REQUESTED_ANNOTATION;
    let body = |ts: &str| {
        Patch::Merge(serde_json::json!({
            "metadata": { "annotations": { key: ts } }
        }))
    };
    let pp = PatchParams::apply(super::apply::FIELD_MANAGER);
    let stamp = now.to_rfc3339();

    match repo_lookup(repo_ref, default_ns) {
        RepoLookup::Namespaced { namespace, name } => {
            let api: Api<Repository> = Api::namespaced(client.clone(), &namespace);
            let Some(repo) = api.get_opt(&name).await? else {
                return Ok(());
            };
            if repo.status.as_ref().and_then(|s| s.phase.as_ref()) != ready {
                return Ok(());
            }
            let existing = repo.metadata.annotations.as_ref().and_then(|a| a.get(key));
            if crate::catalog::should_request_reverify(
                existing.map(String::as_str),
                now,
                MIN_INTERVAL,
            ) {
                api.patch(&name, &pp, &body(&stamp)).await?;
            }
        }
        RepoLookup::Cluster { name } => {
            let api: Api<ClusterRepository> = Api::all(client.clone());
            let Some(repo) = api.get_opt(&name).await? else {
                return Ok(());
            };
            if repo.status.as_ref().and_then(|s| s.phase.as_ref()) != ready {
                return Ok(());
            }
            let existing = repo.metadata.annotations.as_ref().and_then(|a| a.get(key));
            if crate::catalog::should_request_reverify(
                existing.map(String::as_str),
                now,
                MIN_INTERVAL,
            ) {
                api.patch(&name, &pp, &body(&stamp)).await?;
            }
        }
    }
    Ok(())
}

/// Stamp the on-demand catalog-scan-request annotation
/// ([`crate::consts::CATALOG_SCAN_REQUESTED_ANNOTATION`]) with `token` (an opaque
/// RFC3339 timestamp) on the referenced `Repository`/`ClusterRepository`, so its
/// discovered snapshots re-materialize now instead of waiting for the next spec
/// change or periodic refresh (M6 auto-adoption). Mirrors the reverify-annotation
/// idiom ([`request_repository_reverify`]) but carries no rate limit of its own —
/// the policy reconciler already gates *when* it stamps (per-identity, once, or on
/// an actual adoption wave). 404-tolerant: a repository deleted out from under the
/// policy is a no-op, not an error.
pub async fn request_catalog_scan(
    client: &kube::Client,
    repo_ref: &RepositoryRef,
    default_ns: &str,
    token: &str,
) -> Result<()> {
    let key = crate::consts::CATALOG_SCAN_REQUESTED_ANNOTATION;
    let body = Patch::Merge(serde_json::json!({
        "metadata": { "annotations": { key: token } }
    }));
    let pp = PatchParams::apply(super::apply::FIELD_MANAGER);
    let outcome = match repo_lookup(repo_ref, default_ns) {
        RepoLookup::Namespaced { namespace, name } => {
            let api: Api<Repository> = Api::namespaced(client.clone(), &namespace);
            api.patch(&name, &pp, &body).await.map(|_| ())
        }
        RepoLookup::Cluster { name } => {
            let api: Api<ClusterRepository> = Api::all(client.clone());
            api.patch(&name, &pp, &body).await.map(|_| ())
        }
    };
    match outcome {
        Ok(()) => Ok(()),
        // The repository was deleted out from under the policy — a no-op.
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// Whether the named `Namespace` is being torn down — its `deletionTimestamp` is
/// set, or its `status.phase` is `Terminating`. Used by the `Snapshot` finalizer to
/// decide the namespace-deletion cascade (ADR-0005 §5). A missing Namespace (already
/// gone) counts as terminating — the namespace is clearly being removed. A transient
/// read error propagates so the caller can back off rather than guess — EXCEPT a
/// `403`: a namespaced-scope install has Role-only RBAC and can never read the
/// (cluster-scoped) Namespace, and blocking every Snapshot deletion on that would
/// wedge finalizers forever. There the answer is "not terminating" (normal deletion
/// semantics apply) — and if the namespace really is going away, the operator in it
/// is going away too.
pub async fn namespace_is_terminating(client: &kube::Client, namespace: &str) -> Result<bool> {
    use k8s_openapi::api::core::v1::Namespace;
    let api: Api<Namespace> = Api::all(client.clone());
    let got = match api.get_opt(namespace).await {
        Ok(got) => got,
        Err(kube::Error::Api(e)) if e.code == 403 => {
            tracing::debug!(
                namespace,
                "cannot read Namespace (namespaced-scope install); assuming not terminating"
            );
            return Ok(false);
        }
        Err(e) => return Err(Error::Kube(e)),
    };
    match got {
        None => Ok(true),
        Some(ns) => {
            let has_ts = ns.metadata.deletion_timestamp.is_some();
            let terminating = ns
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .map(|p| p == "Terminating")
                .unwrap_or(false);
            Ok(has_ts || terminating)
        }
    }
}

/// Resolve the credentials Secret reference from a repository's encryption block.
pub fn repo_credentials(enc: &Encryption) -> RepoCredentials {
    let r = &enc.password_secret_ref;
    RepoCredentials {
        secret_name: r.name.clone(),
        password_key: r
            .key
            .clone()
            .unwrap_or_else(|| DEFAULT_PASSWORD_KEY.to_string()),
        namespace: r.namespace.clone(),
    }
}

/// Read the repository password value from its Secret (used by the in-process
/// kopia path for filesystem repos — connect/create/status/snapshot list).
pub async fn read_repo_password(
    client: &kube::Client,
    namespace: &str,
    creds: &RepoCredentials,
) -> Result<String> {
    Ok(read_repo_credential(client, namespace, creds).await?.0)
}

/// Read the repository password value AND the password Secret's `resourceVersion`.
///
/// The `resourceVersion` lets the in-process filesystem reconciler detect a Secret
/// *content* change after a terminal failure: such an edit does not bump the CR's
/// `metadata.generation`, so the generation-keyed hard-stop ([`super::apply::terminal_gate_holds`])
/// also compares this version to reopen the gate when the credential is fixed.
/// (`resourceVersion` is always populated on a fetched object; defaults to `""`.)
pub async fn read_repo_credential(
    client: &kube::Client,
    namespace: &str,
    creds: &RepoCredentials,
) -> Result<(String, String)> {
    use k8s_openapi::api::core::v1::Secret;
    use kube::ResourceExt;
    let ns = creds.namespace.as_deref().unwrap_or(namespace);
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret = api.get(&creds.secret_name).await.map_err(|e| {
        Error::MissingDependency(format!(
            "credentials secret {ns}/{} not found: {e}",
            creds.secret_name
        ))
    })?;
    let version = secret.resource_version().unwrap_or_default();
    let data = secret.data.unwrap_or_default();
    let raw = data.get(&creds.password_key).ok_or_else(|| {
        Error::MissingDependency(format!(
            "secret {ns}/{} missing key {}",
            creds.secret_name, creds.password_key
        ))
    })?;
    let password = String::from_utf8(raw.0.clone())
        .map_err(|e| Error::Invariant(format!("password not valid utf-8: {e}")))?;
    Ok((password, version))
}

pub use kopiur_api::creds::backend_auth_secret_ref;

pub use kopiur_api::creds::{CredsSecretRef, mover_creds_secret_refs, mover_creds_secrets};

// The pure Backend → mover-shape projections (filesystem path/volume, TLS CA
// ConfigMap) moved to `kopiur_mover::repo_meta` (next to the work-spec contract)
// so the kubectl-plugin browse spawner can use them without a controller
// dependency. Re-exported so controller call sites keep their `io::*` paths.
pub use kopiur_mover::repo_meta::{
    backend_tls_ca_configmap, filesystem_repo_mount_source, filesystem_repo_path,
};

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::backend::BackendAuth;
    use kopiur_api::backend::{FilesystemBackend, S3Backend};
    use kopiur_api::common::{SecretKeyRef, SecretRef};

    fn enc(name: &str, ns: Option<&str>) -> Encryption {
        Encryption {
            password_secret_ref: SecretKeyRef {
                name: name.to_string(),
                namespace: ns.map(str::to_string),
                key: None,
            },
        }
    }

    #[test]
    fn refs_resolve_source_ns_from_repo_namespace_fallback() {
        // Namespaced Repository: password ref omits namespace → falls back to the
        // repository's own namespace.
        let backend = Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        });
        let refs = mover_creds_secret_refs(&backend, &enc("repo-pw", None), Some("team-a"));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "repo-pw");
        assert_eq!(refs[0].namespace.as_deref(), Some("team-a"));
    }

    #[test]
    fn refs_keep_explicit_ref_namespace_over_fallback() {
        // ClusterRepository: ref pins its own namespace; no repo namespace fallback.
        let refs = mover_creds_secret_refs(
            &Backend::Filesystem(FilesystemBackend {
                path: "/r".into(),
                volume: None,
            }),
            &enc("repo-pw", Some("kopiur-system")),
            None,
        );
        assert_eq!(refs[0].namespace.as_deref(), Some("kopiur-system"));
    }

    #[test]
    fn refs_include_distinct_backend_secret_password_first() {
        let backend = Backend::S3(S3Backend {
            bucket: "b".into(),
            prefix: None,
            endpoint: None,
            region: None,
            auth: Some(BackendAuth {
                secret_ref: Some(SecretRef {
                    name: "s3-keys".into(),
                    namespace: Some("kopiur-system".into()),
                }),
                workload_identity: None,
            }),
            tls: None,
        });
        let refs = mover_creds_secret_refs(&backend, &enc("repo-pw", Some("kopiur-system")), None);
        let names: Vec<_> = refs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["repo-pw", "s3-keys"]); // password first, order-stable
    }

    #[test]
    fn mover_creds_secrets_is_names_only_projection() {
        let backend = Backend::Filesystem(FilesystemBackend {
            path: "/r".into(),
            volume: None,
        });
        assert_eq!(
            mover_creds_secrets(&backend, &enc("repo-pw", Some("x"))),
            vec!["repo-pw"]
        );
    }
}
