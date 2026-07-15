use super::*;

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{
    ConfigMap, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, PodSecurityContext,
    SecurityContext, ServiceAccount, VolumeResourceRequirements,
};
use k8s_openapi::api::rbac::v1::{RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::core::ObjectMeta;
use kube::{Api, ResourceExt};

use kopiur_api::common::{InheritSecurityContextFrom, MoverSpec, PodSelector};
use kopiur_api::secctx_compat::{is_managed_by_kopiur, pod_mounts_claim};

use crate::consts::PRIVILEGED_MOVERS_ANNOTATION;
use crate::error::{Error, Result};

/// Apply a mover run's objects (server-side): the `Job` (which carries the
/// work spec inline in its pod env) and, for bootstrap/probe runs only, the
/// result `ConfigMap` the mover PATCHes its outcome into. Everything carries
/// the owner reference so GC reaps it with the CR (§4.10); the Job's
/// `ttlSecondsAfterFinished` handles the interim.
pub async fn apply_mover_objects(
    client: &kube::Client,
    namespace: &str,
    name: &str,
    result_config_map: Option<&ConfigMap>,
    job: &Job,
) -> Result<()> {
    if let Some(config_map) = result_config_map {
        let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
        apply(&cm_api, name, config_map).await?;
    }
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    apply(&job_api, name, job).await?;
    Ok(())
}

/// Delete the same-named `ConfigMap` of a mover run, tolerating a 404 (owner
/// GC or an earlier pass may have won). Today that ConfigMap exists only for
/// bootstrap/probe runs (the result channel); for every other run kind this is
/// a no-op 404 in the steady state, and cleans up the LEGACY per-run work-spec
/// ConfigMap left by operator versions that mounted the spec instead of
/// embedding it in the Job env.
pub async fn delete_work_spec_cm(
    client: &kube::Client,
    namespace: &str,
    job_name: &str,
) -> Result<()> {
    let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    match cm_api.delete(job_name, &DeleteParams::default()).await {
        Ok(_) => {}
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => return Err(Error::Kube(e)),
    }
    Ok(())
}

/// Delete a mover run being consumed: the Job (background propagation, so its
/// pods are reaped too) AND its same-named `ConfigMap` (the bootstrap result
/// channel, or a legacy work-spec leftover), both tolerating a 404 (the kube
/// TTL controller may have reaped the Job first). Only for runs observed
/// terminal or being force-failed.
///
/// Propagates non-404 errors: the bootstrap/probe consumers rely on the delete
/// for their consume-exactly-once semantics, so a failure there must requeue.
pub async fn delete_mover_run(
    client: &kube::Client,
    namespace: &str,
    job_name: &str,
) -> Result<()> {
    let job_api: Api<Job> = Api::namespaced(client.clone(), namespace);
    match job_api.delete(job_name, &DeleteParams::background()).await {
        Ok(_) => {}
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => return Err(Error::Kube(e)),
    }
    delete_work_spec_cm(client, namespace, job_name).await
}

/// Labels marking the per-namespace mover RBAC objects as kopiur-managed.
fn mover_managed_labels() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "kopiur".to_string(),
        ),
        (
            "app.kubernetes.io/component".to_string(),
            "mover".to_string(),
        ),
    ])
}

/// Build the least-privilege mover `ServiceAccount` for namespace `ns`. Pure (no
/// IO) so the shape is unit-testable. The mover Job runs as this SA; it is minted
/// per workload namespace because a mover Job runs there, not in the operator's
/// namespace where the operator SA lives.
pub fn build_mover_service_account(ns: &str, name: &str) -> ServiceAccount {
    ServiceAccount {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(mover_managed_labels()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Build the `RoleBinding` that grants the mover SA the mover role within `ns`.
/// `role_kind` is `ClusterRole` (cluster install: one shared role bound per
/// namespace) or `Role` (namespaced install: a role in the operator namespace).
/// Pure (no IO) so the subject/roleRef wiring is unit-testable.
pub fn build_mover_rolebinding(
    ns: &str,
    sa_name: &str,
    role_kind: &str,
    role_name: &str,
) -> RoleBinding {
    RoleBinding {
        metadata: ObjectMeta {
            name: Some(sa_name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(mover_managed_labels()),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: role_kind.to_string(),
            name: role_name.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: sa_name.to_string(),
            namespace: Some(ns.to_string()),
            api_group: None,
        }]),
    }
}

/// Ensure the mover `ServiceAccount` + its `RoleBinding` exist in `ns` (the mover
/// Job's namespace). Idempotent server-side apply — reconcilers call this before
/// every mover Job so the SA is present in the workload namespace (else the Job
/// `FailedCreate`s with `serviceaccount ... not found` and never schedules a pod).
/// The objects are kopiur-managed and shared across all mover Jobs in the
/// namespace (no owner reference, so deleting one Snapshot does not revoke them).
pub async fn ensure_mover_rbac(
    client: &kube::Client,
    ns: &str,
    sa_name: &str,
    role_kind: &str,
    role_name: &str,
) -> Result<()> {
    let sa = build_mover_service_account(ns, sa_name);
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), ns);
    apply(&sa_api, sa_name, &sa).await?;

    let rb = build_mover_rolebinding(ns, sa_name, role_kind, role_name);
    let rb_api: Api<RoleBinding> = Api::namespaced(client.clone(), ns);
    apply(&rb_api, sa_name, &rb).await?;
    Ok(())
}

/// Build the `RoleBinding` that grants a user-supplied **workload-identity**
/// ServiceAccount the mover role within `ns`. Named `kopiur-mover-wi-<sa>` —
/// distinct from the minted-SA binding (named after the mover SA) so the two
/// can never clobber each other, and truncated-with-hash when a long SA name
/// would overflow the 253-char object-name limit. Pure (no IO) so the
/// subject/roleRef wiring is unit-testable.
pub fn build_wi_rolebinding(
    ns: &str,
    wi_sa: &str,
    role_kind: &str,
    role_name: &str,
) -> RoleBinding {
    let mut rb = build_mover_rolebinding(ns, wi_sa, role_kind, role_name);
    rb.metadata.name = Some(wi_rolebinding_name(wi_sa));
    rb
}

/// Deterministic, ≤253-char name for the workload-identity RoleBinding:
/// `kopiur-mover-wi-<sa>`, truncating the SA component and appending a stable
/// hash when the full name would overflow.
pub fn wi_rolebinding_name(wi_sa: &str) -> String {
    const PREFIX: &str = "kopiur-mover-wi-";
    const MAX: usize = 253;
    let budget = MAX - PREFIX.len();
    if wi_sa.len() <= budget {
        format!("{PREFIX}{wi_sa}")
    } else {
        let hash = short_hash(wi_sa); // 8 hex chars
        let keep = budget.saturating_sub(hash.len() + 1); // room for "-<hash>"
        let trunc: String = wi_sa.chars().take(keep).collect();
        format!("{PREFIX}{trunc}-{hash}")
    }
}

/// 8-hex-char content hash for name truncation (same idiom as the
/// maintenance/verification job names, which use `crate::naming::short_hash`).
///
/// Deliberately NOT consolidated onto `crate::naming::short_hash` (FNV-1a):
/// this one predates it and uses `DefaultHasher` (SipHash), whose output is
/// not guaranteed stable across Rust releases. Switching algorithms would
/// rename any existing over-budget `kopiur-mover-wi-*` RoleBinding on upgrade
/// and orphan the old one. Tracked as a follow-up (needs a migration story).
fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// The actionable message for a workload-identity ServiceAccount that does not
/// exist in the mover namespace (what / why / how-to-fix). Pure so the exact
/// text is unit-asserted. `cloud` selects the annotation hint the user needs.
pub fn missing_workload_identity_sa_message(
    sa: &str,
    ns: &str,
    cloud: kopiur_api::creds::WorkloadIdentityCloud,
) -> String {
    use kopiur_api::creds::WorkloadIdentityCloud;
    let annotation = match cloud {
        WorkloadIdentityCloud::S3 => {
            "eks.amazonaws.com/role-arn (IRSA) or an EKS Pod Identity \
                                      association"
        }
        WorkloadIdentityCloud::Azure => "azure.workload.identity/client-id",
        WorkloadIdentityCloud::Gcs => "iam.gke.io/gcp-service-account",
    };
    format!(
        "backend auth.workloadIdentity names ServiceAccount `{sa}`, but it does not exist in \
         namespace `{ns}` where the mover Job runs. Kopiur never creates this ServiceAccount — \
         its cloud-federation annotations are your contract with the cloud's identity webhook. \
         Fix: create ServiceAccount `{sa}` in `{ns}` with the federation binding ({annotation})."
    )
}

/// The identity a mover Job runs as, resolved from the repository backend(s):
/// either the user's workload-identity ServiceAccount or the operator-minted
/// mover SA. `azure_workload_identity` flags that the pod must carry the
/// azure-workload-identity opt-in label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoverRunIdentity {
    /// The ServiceAccount the mover Job runs as (`None` only when the operator
    /// is configured without a mover SA and no workload identity is in play).
    pub service_account: Option<String>,
    /// Whether any workload-identity backend federates with Azure, so the pod
    /// needs the `azure.workload.identity/use: "true"` label for the azure
    /// webhook to inject the credential env.
    pub azure_workload_identity: bool,
}

impl MoverRunIdentity {
    /// Stamp the pod-reaching labels this identity requires onto a mover Job's
    /// label set (today: the azure-workload-identity opt-in).
    pub fn decorate_labels(&self, labels: &mut BTreeMap<String, String>) {
        if self.azure_workload_identity {
            labels.insert(
                kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL.to_string(),
                kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL_VALUE.to_string(),
            );
        }
    }
}

/// Resolve the identity a mover Job runs as and ensure its RBAC, in the Job's
/// namespace `ns`. The single launch-site helper (every reconciler calls this
/// instead of `ensure_mover_rbac` directly):
///
/// * A backend with `auth.workloadIdentity` ⇒ the Job runs as the **user's**
///   ServiceAccount. The SA is preflighted with a `get` (a Job naming a missing
///   SA `FailedCreate`s with no pod and hangs) and **never applied** — its
///   cloud annotations are user-owned; SSA would contend with them. Absent ⇒
///   `Error::MissingDependency` with the what/why/fix message. Present ⇒ the
///   mover role is bound to it (the mover PATCHes `*/status` and its result
///   ConfigMap at runtime regardless of which SA it runs as).
/// * Otherwise ⇒ today's behavior: mint the operator's mover SA + RoleBinding.
///
/// `backends` carries every backend the one mover pod touches — one for every
/// reconciler except replication, which passes source **and** destination. The
/// first workload-identity backend names the SA (admission guarantees a
/// both-workload-identity pair agrees), while the Azure label is OR'd across
/// all of them (an S3-WI → Azure-WI replication still needs the label).
pub async fn ensure_mover_identity(
    client: &kube::Client,
    ns: &str,
    backends: &[&kopiur_api::backend::Backend],
    ctx_sa: Option<&str>,
    role_kind: &str,
    role_name: &str,
) -> Result<MoverRunIdentity> {
    use kopiur_api::creds::{WorkloadIdentityCloud, backend_workload_identity};
    let wi: Vec<_> = backends
        .iter()
        .filter_map(|b| backend_workload_identity(b))
        .collect();
    let Some((first, _)) = wi.first() else {
        if let Some(sa) = ctx_sa {
            ensure_mover_rbac(client, ns, sa, role_kind, role_name).await?;
        }
        return Ok(MoverRunIdentity {
            service_account: ctx_sa.map(str::to_string),
            azure_workload_identity: false,
        });
    };
    let sa_name = first.service_account_name.clone();
    let azure = wi
        .iter()
        .any(|(_, cloud)| *cloud == WorkloadIdentityCloud::Azure);
    // Preflight: the SA must already exist (user-created, cloud-annotated).
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), ns);
    if sa_api
        .get_opt(&sa_name)
        .await
        .map_err(Error::Kube)?
        .is_none()
    {
        let cloud = wi[0].1;
        return Err(Error::MissingDependency(
            missing_workload_identity_sa_message(&sa_name, ns, cloud),
        ));
    }
    let rb = build_wi_rolebinding(ns, &sa_name, role_kind, role_name);
    let rb_name = rb.metadata.name.clone().unwrap_or_default();
    let rb_api: Api<RoleBinding> = Api::namespaced(client.clone(), ns);
    apply(&rb_api, &rb_name, &rb).await?;
    Ok(MoverRunIdentity {
        service_account: Some(sa_name),
        azure_workload_identity: azure,
    })
}

/// Whether namespace `ns` has opted in to elevated (root/privileged) movers via the
/// [`PRIVILEGED_MOVERS_ANNOTATION`]. If the namespace cannot be read because the
/// operator lacks `namespaces get` (a namespaced-scope install, where the operator
/// is already confined to admin-chosen namespaces), the check fails **open** with a
/// warning rather than blocking every privileged mover.
pub async fn namespace_allows_privileged_movers(client: &kube::Client, ns: &str) -> Result<bool> {
    use k8s_openapi::api::core::v1::Namespace;
    let api: Api<Namespace> = Api::all(client.clone());
    match api.get(ns).await {
        Ok(namespace) => Ok(namespace
            .annotations()
            .get(PRIVILEGED_MOVERS_ANNOTATION)
            .is_some_and(|v| v == "true")),
        // Forbidden (no cluster-scoped namespaces:get, e.g. a namespaced install):
        // can't determine the opt-in, so don't block — the operator is already
        // scoped to admin-selected namespaces in that mode.
        Err(kube::Error::Api(e)) if e.code == 403 => {
            tracing::warn!(
                namespace = ns,
                "cannot read namespace to check the privileged-movers opt-in (operator lacks \
                 namespaces:get); allowing the privileged mover"
            );
            Ok(true)
        }
        Err(e) => Err(Error::Kube(e)),
    }
}

/// The actionable message for a privileged mover refused in a namespace that has
/// not opted in (what / why / how-to-fix). Pure so the exact text is unit-asserted.
/// `kind` is the owning resource's kind (e.g. `SnapshotPolicy`, `Restore`) and `name`
/// its name, so the message names the right object to fix.
pub fn privileged_mover_message(kind: &str, name: &str, ns: &str, mover_sa: &str) -> String {
    format!(
        "{kind} `{name}` requests a privileged mover (e.g. `runAsUser: 0`, \
         `privileged: true`, added capabilities, or `privilegedMode`), but namespace `{ns}` has \
         not opted in. A tenant with access to `{ns}` could reuse the minted `{mover_sa}` \
         ServiceAccount to run pods at that privilege, so an elevated mover requires an explicit \
         per-namespace opt-in. Fix: a cluster admin annotates the namespace — `kubectl annotate \
         namespace {ns} {PRIVILEGED_MOVERS_ANNOTATION}=true` — or remove the elevated \
         securityContext/privilegedMode from the {kind} `spec.mover`."
    )
}

/// Render a k8s [`LabelSelector`] as a kube list-query string
/// (`k1=v1,k2=v2,key in (a,b),!key`). kube 3.1 has no built-in `LabelSelector` →
/// query conversion, so this fills the gap for [`resolve_inherited_security_context`].
/// Pure + unit-tested. An empty selector renders to `""` (matches everything — the
/// caller treats a `matchNothing` selector as a config error before calling).
pub fn label_selector_to_string(sel: &LabelSelector) -> String {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;
    let mut terms: Vec<String> = Vec::new();
    if let Some(labels) = &sel.match_labels {
        for (k, v) in labels {
            terms.push(format!("{k}={v}"));
        }
    }
    if let Some(exprs) = &sel.match_expressions {
        for LabelSelectorRequirement {
            key,
            operator,
            values,
        } in exprs
        {
            let vals = values.clone().unwrap_or_default().join(",");
            match operator.as_str() {
                "In" => terms.push(format!("{key} in ({vals})")),
                "NotIn" => terms.push(format!("{key} notin ({vals})")),
                "Exists" => terms.push(key.clone()),
                "DoesNotExist" => terms.push(format!("!{key}")),
                // Unknown operator: skip (the webhook/schema constrain the set).
                _ => {}
            }
        }
    }
    terms.join(",")
}

/// The container- and pod-level security contexts inherited from a workload pod.
/// At least one is `Some` (a fully context-less workload is an error to inherit from).
pub type InheritedContexts = (Option<SecurityContext>, Option<PodSecurityContext>);

/// One successful inherit: the copied contexts plus **which** pod/container they came from.
/// The provenance is not decoration — it is what lets the reconciler report the mover's
/// identity honestly (naming the pod in a condition/Event) instead of asserting a match from
/// the fact that the inherit code path ran, which is the bug this type exists to prevent.
#[derive(Debug, Clone, PartialEq)]
pub struct InheritSource {
    /// The workload's container + pod security contexts, copied verbatim.
    pub contexts: InheritedContexts,
    /// The pod the contexts were read from.
    pub pod: String,
    /// The container within that pod whose `securityContext` was copied.
    pub container: String,
}

impl InheritSource {
    /// The **effective** UID these inherited contexts pin, following kubelet precedence
    /// (`container.runAsUser ?? pod.runAsUser`). `None` when the workload pins no UID at
    /// either level — its identity then comes from its image's `USER`, which is unreadable
    /// from the spec, and inheriting contributes no UID at all.
    pub fn uid(&self) -> Option<i64> {
        kopiur_api::common::effective_run_as_user(
            self.contexts.0.as_ref(),
            self.contexts.1.as_ref(),
        )
    }

    /// Whether the inherited contexts pin **any** identity the mover can act on: an effective
    /// UID, or any group (`runAsGroup` / `fsGroup` / `supplementalGroups`).
    ///
    /// Groups are not a footnote here. A workload that pins only `runAsGroup: 1000`, taking
    /// its UID from its image, still lets the mover read `0640` data through the group bit —
    /// so "no UID" alone does not mean inheriting achieved nothing. When BOTH are absent,
    /// inheriting is a provable no-op: the workload's identity lives in its image, which the
    /// pod spec cannot show, and the mover falls back to its own image's UID.
    pub fn pins_identity(&self) -> bool {
        if self.uid().is_some() {
            return true;
        }
        let sc = self.contexts.0.clone().unwrap_or_default();
        !kopiur_api::secctx_compat::mover_identity(&sc, self.contexts.1.as_ref())
            .groups
            .is_empty()
    }
}

/// What `inheritSecurityContextFrom` produced for this run. Exhaustively matched by the
/// reconcilers so a new variant cannot be silently ignored (§5.5).
#[derive(Debug, Clone, PartialEq)]
pub enum InheritOutcome {
    /// The recipe requested no inheritance — the explicit contexts are the only source.
    NotRequested,
    /// Inherited from a live workload pod.
    Inherited {
        /// The pod the contexts were read from.
        pod: String,
        /// The container whose `securityContext` was copied.
        container: String,
        /// The effective UID the **inherited layer alone** pins, before the recipe's explicit
        /// context is overlaid (see [`InheritSource::uid`]). Compared against the resolved UID
        /// to detect an inherit that an explicit `runAsUser` silently displaced.
        uid: Option<i64>,
        /// Whether the inherited layer pinned any identity at all — see
        /// [`InheritSource::pins_identity`]. `false` means inheriting was a provable no-op.
        pins_identity: bool,
    },
    /// Inheritance could not resolve a workload pod, and the recipe's explicit context — which
    /// pins a real identity — stands in for it. Never silent: the reconciler reports this.
    Fallback {
        /// The actionable reason inheritance failed, for the condition/Event message.
        reason: String,
    },
}

/// The mover's recipe-layer security contexts plus how they were arrived at.
///
/// `contexts` feeds [`kopiur_api::common::resolve_mover`] as `recipe_sc`/`recipe_psc`;
/// `outcome` lets the caller report provenance instead of guessing.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMoverSecurity {
    /// The recipe layer's container + pod contexts.
    pub contexts: InheritedContexts,
    /// How `contexts` was arrived at.
    pub outcome: InheritOutcome,
    /// Pods from an **unfiltered** namespace LIST, reusable by the compat assessment so it
    /// need not LIST again.
    ///
    /// `Some` ONLY on the `pvcConsumer` path, which lists the whole namespace. This is a
    /// correctness invariant, not an optimization toggle:
    ///
    /// - `workloadSelector` lists **label-filtered** ([`resolve_inherited_security_context`]),
    ///   so its result is a subset of the claim's consumers. Feeding it to
    ///   `workload_identities` would narrow the writer set and could flip a
    ///   `LikelyIncompatible` verdict to `Compatible` — a false `SecurityContextCompatible=True`,
    ///   the very bug the honest assessment exists to kill.
    /// - `NotRequested` lists nothing, so an empty `Vec` would be indistinguishable from
    ///   "listed, found zero consumers" and would suppress `True` for every non-inherit run.
    ///
    /// Hence `None` for both: the assessment then does its own unfiltered LIST, as today.
    pub unfiltered_pods: Option<Vec<Pod>>,
}

/// Resolve `inheritSecurityContextFrom` to the workload's **container** AND **pod**
/// security contexts: find a pod in `ns` matching the selector, pick the named
/// container (or the pod's first), and return that container's `securityContext`
/// together with the pod's `spec.securityContext` (so the mover inherits the app's
/// `fsGroup` too, not just its UID). A `Running` pod is preferred so a
/// terminating/pending replica isn't read. Returns `Err(MissingDependency)` — a
/// transient, requeue-on-the-fast-cadence condition — when no pod matches, the named
/// container is absent, or the pod has neither context to inherit.
pub async fn resolve_inherited_security_context(
    client: &kube::Client,
    ns: &str,
    selector: &PodSelector,
) -> Result<InheritSource> {
    let query = label_selector_to_string(&selector.pod_selector);
    if query.is_empty() {
        return Err(Error::MissingDependency(format!(
            "mover.inheritSecurityContextFrom.podSelector is empty in namespace `{ns}` — set \
             matchLabels/matchExpressions identifying the workload pod whose securityContext the \
             mover should inherit (UID/GID match)"
        )));
    }
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let pods = api.list(&ListParams::default().labels(&query)).await?.items;
    inherited_security_context_from_pods(&pods, selector.container.as_deref(), ns, &query)
}

/// Pure core of [`resolve_inherited_security_context`]: from the pods matching the
/// selector, pick a workload pod (a `Running` one preferred, else the first), then
/// return its chosen container's `securityContext` and the pod-level
/// `spec.securityContext`. Returns an actionable `Err(MissingDependency)` when no pod
/// matches, the named container is absent, or the pod sets **neither** a container nor
/// a pod securityContext to inherit. Pure (the `list` IO is the caller's) so the
/// pick/extract logic is unit-tested directly.
pub fn inherited_security_context_from_pods(
    pods: &[Pod],
    container: Option<&str>,
    ns: &str,
    query: &str,
) -> Result<InheritSource> {
    if pods.is_empty() {
        return Err(Error::MissingDependency(format!(
            "no pod matches mover.inheritSecurityContextFrom (`{query}`) in namespace `{ns}` — the \
             workload whose securityContext the mover inherits must be running so its UID/GID can \
             be read; scale it up or fix the selector"
        )));
    }
    // Prefer a Running pod; otherwise take the first match.
    let pod = pods.iter().find(|p| pod_is_running(p)).unwrap_or(&pods[0]);
    // `None` claim: a label selector names a workload, not a volume, so there is no claim to
    // identify the app container by — an unnamed container falls back to the pod's first.
    extract_inherited_contexts(pod, container, None, ns)
}

/// Whether a pod's `status.phase` is `Running`.
fn pod_is_running(p: &Pod) -> bool {
    p.status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .map(|ph| ph == "Running")
        .unwrap_or(false)
}

/// Extract the inherited `(container, pod)` security contexts from one chosen workload
/// pod, plus the pod-level `spec.securityContext`. Shared by `workloadSelector` and
/// `pvcConsumer`. Errors when the named container is absent or the pod sets neither context.
///
/// Container choice, in order: the **named** container (a config error if absent); else, when
/// `claim` is known (`pvcConsumer`), the single container that **mounts the claim**; else the
/// pod's first. The claim-mounter step matters on sidecar-injected pods, where "first" is
/// whatever the injector prepended (`istio-proxy`, uid 1337) rather than the app that wrote
/// the data.
fn extract_inherited_contexts(
    pod: &Pod,
    container: Option<&str>,
    claim: Option<&str>,
    ns: &str,
) -> Result<InheritSource> {
    let containers = pod
        .spec
        .as_ref()
        .map(|s| s.containers.as_slice())
        .unwrap_or(&[]);
    let chosen = match container {
        Some(name) => Some(containers.iter().find(|c| c.name == name).ok_or_else(|| {
            Error::MissingDependency(format!(
                "pod `{}` (matched by mover.inheritSecurityContextFrom in `{ns}`) has no \
                 container `{name}` — fix `inheritSecurityContextFrom.container`",
                pod.name_any()
            ))
        })?),
        None => claim
            .and_then(|c| kopiur_api::secctx_compat::container_mounting_claim(pod, c))
            .or_else(|| containers.first()),
    };
    let container_sc = chosen.and_then(|c| c.security_context.clone());
    let pod_sc = pod.spec.as_ref().and_then(|s| s.security_context.clone());
    if container_sc.is_none() && pod_sc.is_none() {
        return Err(Error::MissingDependency(format!(
            "pod `{}` (mover.inheritSecurityContextFrom, `{ns}`) sets no securityContext — neither \
             a container nor a pod-level one — to inherit; set one on the workload, or use an \
             explicit mover.securityContext / mover.podSecurityContext instead",
            pod.name_any()
        )));
    }
    Ok(InheritSource {
        contexts: (container_sc, pod_sc),
        pod: pod.name_any(),
        container: chosen.map(|c| c.name.clone()).unwrap_or_default(),
    })
}

/// Resolve `inheritSecurityContextFrom.pvcConsumer` (backup sources only): find the
/// workload pod(s) that mount the backup source PVC `claim` in `ns` and inherit one's
/// security context — so the mover's UID/GID matches the workload *by construction*, with
/// no hand-written selector. `Err(MissingDependency)` (transient, requeue) when there is no
/// source PVC, or no non-kopiur pod currently mounts it.
pub async fn resolve_pvc_consumer_security_context(
    client: &kube::Client,
    ns: &str,
    source_pvc: Option<&str>,
    container: Option<&str>,
) -> Result<(InheritSource, Vec<Pod>)> {
    let claim = source_pvc.ok_or_else(|| {
        Error::MissingDependency(
            "mover.inheritSecurityContextFrom.pvcConsumer is only valid for a backup whose source \
             is a single PVC — this run has no source PVC to derive the workload from; use \
             workloadSelector or an explicit mover.securityContext instead"
                .to_string(),
        )
    })?;
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    // Unfiltered on purpose: the compat assessment needs EVERY pod mounting the claim, so
    // this list is handed back for reuse rather than re-listed. See `ResolvedMoverSecurity`.
    let pods = api.list(&ListParams::default()).await?.items;
    let source = pvc_consumer_security_context_from_pods(&pods, claim, ns, container)?;
    Ok((source, pods))
}

/// Pure core of [`resolve_pvc_consumer_security_context`]: from all pods in the namespace,
/// pick the workload consuming `claim` and return its security contexts. **Excludes
/// kopiur-managed pods** (the mover itself mounts the source PVC — it must never inherit
/// from itself). Selection is deterministic: a `Running` consumer is preferred, ties broken
/// by `(namespace, name)` so the same pod is chosen across reconciles. Pure (the `list` IO
/// is the caller's) so the discovery/pick is unit-tested directly.
pub fn pvc_consumer_security_context_from_pods(
    pods: &[Pod],
    claim: &str,
    ns: &str,
    container: Option<&str>,
) -> Result<InheritSource> {
    let mut consumers: Vec<&Pod> = pods
        .iter()
        .filter(|p| pod_mounts_claim(p, claim))
        .filter(|p| !is_managed_by_kopiur(p))
        .collect();
    // Deterministic order: Running first, then lexicographic (namespace, name).
    consumers.sort_by(|a, b| {
        pod_is_running(b)
            .cmp(&pod_is_running(a))
            .then_with(|| pod_key(a).cmp(&pod_key(b)))
    });
    let pod = consumers.first().ok_or_else(|| {
        Error::MissingDependency(format!(
            "no running workload pod mounts the backup source PVC `{claim}` in namespace `{ns}` — \
             mover.inheritSecurityContextFrom.pvcConsumer derives the mover's UID/GID from the pod \
             that consumes this PVC, so that pod must be running; scale the workload up, or use an \
             explicit mover.securityContext / workloadSelector instead"
        ))
    })?;
    // Pass the claim: with no explicit `container`, prefer the one that actually MOUNTS the
    // source PVC over the pod's first, which on a sidecar-injected pod is the injected proxy.
    extract_inherited_contexts(pod, container, Some(claim), ns)
}

/// `(namespace, name)` key for deterministic pod ordering.
fn pod_key(p: &Pod) -> (String, String) {
    (
        p.metadata.namespace.clone().unwrap_or_default(),
        p.metadata.name.clone().unwrap_or_default(),
    )
}

/// Ensure a controller-owned **persistent** kopia cache PVC named `name` exists in
/// `ns` (a warm cache reused across this owner's runs, ADR §3.1). Idempotent:
/// returns the claim name if it already exists (the spec is immutable, so we never
/// re-apply over it), otherwise creates it `ReadWriteOnce` at `capacity` with the
/// optional `storage_class`, owner-referenced so it is GC'd with `owner`. Because it
/// is `ReadWriteOnce`, persistent cache assumes non-overlapping runs for the owner.
pub async fn ensure_cache_pvc(
    client: &kube::Client,
    ns: &str,
    name: &str,
    owner: OwnerReference,
    capacity: &str,
    storage_class: Option<&str>,
) -> Result<String> {
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    if api.get_opt(name).await?.is_some() {
        return Ok(name.to_string());
    }
    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![owner]),
            labels: Some(child_labels(&[(
                "kopiur.home-operations.com/component",
                "mover-cache",
            )])),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(std::collections::BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(capacity.to_string()),
                )])),
                limits: None,
            }),
            storage_class_name: storage_class.map(String::from),
            ..Default::default()
        }),
        ..Default::default()
    };
    match api.create(&PostParams::default(), &pvc).await {
        Ok(_) => Ok(name.to_string()),
        // A concurrent reconcile won the create race: the PVC is there, reuse it.
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(name.to_string()),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// The mover's **recipe-layer** container AND pod security contexts, plus how they were
/// arrived at. Each context is `None` when unset (the Job builder then applies the hardened
/// container default and no pod context). The result feeds BOTH the privileged-mover gate and
/// the mover `Job`, so an inherited root context — container or pod — is gated exactly like an
/// explicit one.
///
/// When `inheritSecurityContextFrom` is set, the workload's contexts are copied and the
/// recipe's **explicit** `securityContext`/`podSecurityContext` are then overlaid on top
/// (field-wise, explicit wins). The two are layers, not alternatives: what you wrote always
/// wins, and inheritance fills in whatever the workload pins that you left blank. Doing the
/// overlay here — rather than threading a fourth layer through
/// [`kopiur_api::common::resolve_mover`] — keeps that function and all seven of its call sites
/// untouched, and is field-wise identical because the merge is a per-field `over.or(base)` and
/// therefore associative: every field resolves to
/// `explicit.or(inherited).or(moverDefaults).or(hardened)`.
///
/// One exception to "explicit wins": an inherited `runAsUser: 0` under an explicit
/// `runAsNonRoot: true` is normalized by INV-1 ([`kopiur_api::invariants`]) into a *root*
/// mover, because the kubelet rejects that pair outright. Only `runAsUser` can de-escalate an
/// inherited root UID; the result stays privileged-gated either way.
///
/// The returned [`ResolvedMoverSecurity::outcome`] carries the *provenance* so callers can
/// report the mover's identity from what actually happened. Asserting compatibility from the
/// fact that a given branch ran is exactly the defect this signature exists to prevent.
///
/// `source_pvc` is the backup source claim name, used only by the
/// `inheritSecurityContextFrom.pvcConsumer` mode to discover the workload that mounts it;
/// pass `None` for restore/maintenance movers (which have no backup source — `pvcConsumer`
/// then fails with an actionable error, as it is backup-source-only).
pub async fn resolve_mover_security_contexts(
    client: &kube::Client,
    ns: &str,
    mover: Option<&MoverSpec>,
    source_pvc: Option<&str>,
) -> Result<ResolvedMoverSecurity> {
    let Some(m) = mover else {
        return Ok(ResolvedMoverSecurity {
            contexts: (None, None),
            outcome: InheritOutcome::NotRequested,
            unfiltered_pods: None,
        });
    };
    // `unfiltered_pods` is `Some` ONLY for `pvcConsumer` — see `ResolvedMoverSecurity`.
    let resolved = match &m.inherit_security_context_from {
        Some(InheritSecurityContextFrom::WorkloadSelector(sel)) => {
            resolve_inherited_security_context(client, ns, sel)
                .await
                .map(|source| (source, None))
        }
        Some(InheritSecurityContextFrom::PvcConsumer(pc)) => {
            resolve_pvc_consumer_security_context(client, ns, source_pvc, pc.container.as_deref())
                .await
                .map(|(source, pods)| (source, Some(pods)))
        }
        None => {
            return Ok(ResolvedMoverSecurity {
                contexts: (m.security_context.clone(), m.pod_security_context.clone()),
                outcome: InheritOutcome::NotRequested,
                unfiltered_pods: None,
            });
        }
    };
    let (source, unfiltered_pods) = match resolved {
        Ok(ok) => ok,
        // The workload isn't there to read (scaled to zero, mid-rollout, bad selector). If the
        // recipe wrote a context that pins a real identity, that context IS the deliberate
        // fallback — proceed on it rather than holding the run. The reconciler reports this.
        //
        // The predicate is `effective_run_as_user(..).is_some()`, NOT "a context field exists":
        // a context that pins no identity (say, seccomp only) cannot stand in for a workload's,
        // so falling back on it would just produce a wrong-UID run while claiming to have a
        // plan. Keying on a pinned identity also makes this mutually exclusive with the
        // pins-nothing warning by construction — the two can never fire for the same run.
        Err(Error::MissingDependency(reason))
            if kopiur_api::common::effective_run_as_user(
                m.security_context.as_ref(),
                m.pod_security_context.as_ref(),
            )
            .is_some() =>
        {
            return Ok(ResolvedMoverSecurity {
                contexts: (m.security_context.clone(), m.pod_security_context.clone()),
                outcome: InheritOutcome::Fallback { reason },
                unfiltered_pods: None,
            });
        }
        // Everything else propagates, including `Error::Kube` (e.g. a 403 listing pods).
        // A workload that isn't running is workload state; a broken API call is an operator
        // misconfiguration, and silently degrading it to a fallback would mint wrong-UID
        // backups namespace-wide until somebody noticed. Requeue instead.
        Err(e) => return Err(e),
    };
    let outcome = InheritOutcome::Inherited {
        pod: source.pod.clone(),
        container: source.container.clone(),
        uid: source.uid(),
        pins_identity: source.pins_identity(),
    };
    // `inherited ⊂ explicit`: the recipe's explicit context is the higher layer, so each field
    // it sets wins and the inherited value fills the rest. Reuses the exhaustive merge helpers
    // (a k8s-openapi field addition breaks their struct literals) rather than re-deriving one.
    let (inherited_sc, inherited_psc) = source.contexts;
    Ok(ResolvedMoverSecurity {
        contexts: (
            kopiur_api::common::merge_security_context_opt(
                inherited_sc.as_ref(),
                m.security_context.as_ref(),
            ),
            kopiur_api::common::merge_pod_security_context_opt(
                inherited_psc.as_ref(),
                m.pod_security_context.as_ref(),
            ),
        ),
        outcome,
        unfiltered_pods,
    })
}

/// Container `waiting.reason` values that mean the pod will never start without a spec
/// change — a *wedged* mover, not a slow one. A pod in one of these states stays `Pending`
/// forever: it never terminates, so `Job.backoffLimit` never decrements and only the (long)
/// `activeDeadlineSeconds` backstop would ever stop it. `CreateContainerConfigError` is the
/// exact reason an inherited-root securityContext produced before [`normalize_nonroot_invariant`].
const WEDGED_WAITING_REASONS: &[&str] = &[
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
    "ErrImagePull",
    "ImagePullBackOff",
    "InvalidImageName",
    "ErrImageNeverPull",
];

/// Verdict for the pods backing a non-terminal mover Job. See [`classify_wedged_pods`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WedgedVerdict {
    /// No pod is wedged (or none exist yet) — the Job is making normal progress.
    Progressing,
    /// A pod is wedged but has not yet exceeded the grace window — keep waiting.
    Within {
        /// The wedged pod's reason (e.g. `CreateContainerConfigError`, `Unschedulable`).
        reason: String,
    },
    /// A pod has been wedged past the grace window — fail the run fast.
    Wedged {
        /// The wedged pod's reason (e.g. `CreateContainerConfigError`, `Unschedulable`).
        reason: String,
        /// Human-readable detail (container name + kubelet/scheduler message) for the
        /// `Failed` condition surfaced to the user.
        message: String,
    },
}

/// If `pod` is in a non-starting / unschedulable state, return its `(reason, message)`.
/// Inspects init + regular container `waiting` reasons and the `PodScheduled=False /
/// Unschedulable` condition. Pure.
fn pod_wedge_reason(pod: &Pod) -> Option<(String, String)> {
    let status = pod.status.as_ref()?;
    let container_statuses = status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten());
    for cs in container_statuses {
        if let Some(w) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
            let reason = w.reason.as_deref().unwrap_or_default();
            if WEDGED_WAITING_REASONS.contains(&reason) {
                let detail = w.message.as_deref().unwrap_or_default();
                let message = if detail.is_empty() {
                    format!("container `{}`: {reason}", cs.name)
                } else {
                    format!("container `{}`: {reason} — {detail}", cs.name)
                };
                return Some((reason.to_string(), message));
            }
        }
    }
    for c in status.conditions.iter().flatten() {
        if c.type_ == "PodScheduled"
            && c.status == "False"
            && c.reason.as_deref() == Some("Unschedulable")
        {
            let message = c
                .message
                .clone()
                .unwrap_or_else(|| "pod is unschedulable".to_string());
            return Some(("Unschedulable".to_string(), message));
        }
    }
    None
}

/// Classify the pods of a non-terminal mover Job: is any pod stuck in a non-starting
/// (`CreateContainerConfigError`/`ImagePullBackOff`/…) or `Unschedulable` state, and for
/// how long? Pure + time-injected so it unit-tests without a clock or cluster.
///
/// `grace_seconds` bounds how long a wedged pod is tolerated; `now_unix` is the decision
/// clock in unix seconds (the async wrapper passes `Utc::now().timestamp()` — never
/// persisted to status, so no churn — per the status-churn/hot-loop guidance). A wedged
/// pod's age is measured from its `creationTimestamp`: config/image errors are immediate
/// and deterministic and won't self-heal, so age-since-creation is a sound, conservative
/// proxy for "how long wedged". Working in unix seconds avoids the k8s-openapi `Time`
/// (jiff) ↔ chrono mismatch.
pub fn classify_wedged_pods(pods: &[Pod], grace_seconds: i64, now_unix: i64) -> WedgedVerdict {
    let mut within: Option<String> = None;
    for pod in pods {
        let Some((reason, message)) = pod_wedge_reason(pod) else {
            continue;
        };
        let age_secs = pod
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| now_unix - t.0.as_second())
            .unwrap_or(0);
        if age_secs >= grace_seconds {
            return WedgedVerdict::Wedged { reason, message };
        }
        within.get_or_insert(reason);
    }
    match within {
        Some(reason) => WedgedVerdict::Within { reason },
        None => WedgedVerdict::Progressing,
    }
}

/// Fetch a mover Job's pods (by the `batch.kubernetes.io/job-name` label the Job controller
/// stamps) and classify whether one is wedged past `grace_seconds`. Thin IO over the pure
/// [`classify_wedged_pods`]; the reconciler fails the owning CR fast on [`WedgedVerdict::Wedged`].
pub async fn wedged_pod_verdict(
    client: &kube::Client,
    ns: &str,
    job_name: &str,
    grace_seconds: i64,
) -> Result<WedgedVerdict> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(&format!("batch.kubernetes.io/job-name={job_name}"));
    let pods = api.list(&lp).await?.items;
    Ok(classify_wedged_pods(
        &pods,
        grace_seconds,
        chrono::Utc::now().timestamp(),
    ))
}

#[cfg(test)]
mod wedged_tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateWaiting, ContainerStatus, PodCondition, PodStatus,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    /// A pod created `age_secs` ago whose `mover` container is waiting on `reason`.
    fn waiting_pod(reason: &str, created_unix: i64) -> Pod {
        Pod {
            metadata: ObjectMeta {
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_second(created_unix).unwrap(),
                )),
                ..Default::default()
            },
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: "mover".to_string(),
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some(reason.to_string()),
                            message: Some("a kubelet detail".to_string()),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn unschedulable_pod(created_unix: i64) -> Pod {
        Pod {
            metadata: ObjectMeta {
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_second(created_unix).unwrap(),
                )),
                ..Default::default()
            },
            status: Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "PodScheduled".to_string(),
                    status: "False".to_string(),
                    reason: Some("Unschedulable".to_string()),
                    message: Some("0/6 nodes are available".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn create_container_config_error_past_grace_is_wedged() {
        // The exact production failure: an impossible securityContext parks the container
        // in CreateContainerConfigError. Past the grace window it must fail fast.
        let pods = [waiting_pod("CreateContainerConfigError", 0)];
        match classify_wedged_pods(&pods, 300, 600) {
            WedgedVerdict::Wedged { reason, message } => {
                assert_eq!(reason, "CreateContainerConfigError");
                assert!(message.contains("mover"), "message names the container");
            }
            other => panic!("expected Wedged, got {other:?}"),
        }
    }

    #[test]
    fn within_grace_is_not_yet_failed() {
        // Same wedge, but only 60s old with a 300s grace → keep waiting, don't fail.
        let pods = [waiting_pod("CreateContainerConfigError", 540)];
        assert_eq!(
            classify_wedged_pods(&pods, 300, 600),
            WedgedVerdict::Within {
                reason: "CreateContainerConfigError".to_string()
            }
        );
    }

    #[test]
    fn image_pull_backoff_and_unschedulable_are_wedge_reasons() {
        assert!(matches!(
            classify_wedged_pods(&[waiting_pod("ImagePullBackOff", 0)], 300, 600),
            WedgedVerdict::Wedged { .. }
        ));
        assert!(matches!(
            classify_wedged_pods(&[unschedulable_pod(0)], 300, 600),
            WedgedVerdict::Wedged { reason, .. } if reason == "Unschedulable"
        ));
    }

    #[test]
    fn a_normal_starting_pod_is_progressing() {
        // No wedged reason (e.g. ContainerCreating is transient and not in the set) →
        // Progressing, so a legitimately slow/long mover is never failed by this path.
        let pods = [waiting_pod("ContainerCreating", 0)];
        assert_eq!(
            classify_wedged_pods(&pods, 300, 600),
            WedgedVerdict::Progressing
        );
        // And no pods at all (Job just created) is Progressing too.
        assert_eq!(
            classify_wedged_pods(&[], 300, 600),
            WedgedVerdict::Progressing
        );
    }
}
