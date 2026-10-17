//! The in-cluster browse session pod and its read-only exec transport.
//!
//! A mover `Job` (built with the same pure builders the operator uses) connects
//! to the repository **read-only**, signals readiness, and idles for its TTL;
//! callers exec the closed [`SessionCmd`](kopiur_kopia::SessionCmd) surface into
//! it. One warm session per repository — repeated `ls`/`cat` reuse it instantly.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt;
use kube::api::{Api, AttachParams, DeleteParams, ListParams, LogParams, PostParams};
use tokio::io::AsyncReadExt;

use kopiur_api::common::{MoverDefaults, RepositoryKind};
use kopiur_api::consts::{
    SESSION_BROWSE, SESSION_CREDS_LABEL, SESSION_CREDS_PROJECTED, SESSION_CREDS_REFUSED_ANNOTATION,
    SESSION_LABEL, SESSION_REPO_LABEL, SESSION_SNAPSHOT_ANNOTATION,
};
use kopiur_kopia::SessionCmd;
use kopiur_mover::jobs::{self, JobLimits, MoverJobInputs, VolumeMountSpec};
use kopiur_mover::repo_meta::{
    backend_to_repository_connect, filesystem_repo_mount_source, filesystem_repo_path,
};
use kopiur_mover::workspec::{
    BrowseSessionOp, MoverWorkSpec, Operation, ResolvedIdentity, TargetRef,
};

use super::resolve::{BrowseTarget, RepoHandle, SessionCreds, session_creds};
use crate::ctx::OpsCtx;
use crate::error::{OpsError, classify_kube};

/// The kopia binary inside the mover image (distroless: fixed path, no PATH).
pub const SESSION_KOPIA_BIN: &str = "/usr/local/bin/kopia";
/// The mover binary inside the image, re-invoked by the readinessProbe.
pub const SESSION_MOVER_BIN: &str = "/usr/local/bin/kopiur-mover";
/// The upstream label the Job controller stamps on its pods.
const JOB_NAME_LABEL: &str = "batch.kubernetes.io/job-name";
/// How long `ensure` waits for the session pod to become Ready. Covers image
/// pull + scheduling + a cold object-store connect; the pod's own probe budget
/// (2s × 60) sits inside this.
const READY_WAIT: Duration = Duration::from_secs(300);
/// The env var on the controller Deployment naming the mover image (mirrors
/// `kopiur_controller::config::MOVER_IMAGE_ENV` without a controller dep).
const MOVER_IMAGE_ENV: &str = "KOPIUR_MOVER_IMAGE";
/// The env var on the controller Deployment listing the mover pods'
/// `imagePullSecrets` names, comma-separated (mirrors
/// `kopiur_controller::config::MOVER_IMAGE_PULL_SECRETS_ENV`, #579).
const MOVER_IMAGE_PULL_SECRETS_ENV: &str = "KOPIUR_MOVER_IMAGE_PULL_SECRETS";
/// The chart labels identifying THE controller Deployment. Shared by the
/// mover-image lookup ([`resolve_mover_image`]) and the operator-namespace
/// discovery ([`super::resolve::resolve_ca_bundle`]); both demand exactly one
/// match cluster-wide and fail closed otherwise.
pub const CONTROLLER_DEPLOYMENT_SELECTOR: &str =
    "app.kubernetes.io/name=kopiur,app.kubernetes.io/component=controller";

/// Which mover image a session pod runs. Exhaustive: a caller that already
/// knows the image (a server told its own image by the chart) pins it; one that
/// does not (the CLI) asks for the fail-closed discovery below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoverImageSource {
    /// A pinned image (and its pull secrets), used verbatim.
    Fixed(MoverImage),
    /// Read both from the single controller Deployment's `KOPIUR_MOVER_IMAGE`
    /// and `KOPIUR_MOVER_IMAGE_PULL_SECRETS`.
    DiscoverFromControllerDeployment,
}

/// The mover image a session pod runs, plus the registry Secret NAMES set as
/// its `imagePullSecrets` (the same list the operator stamps on its own
/// movers). The names resolve in the session pod's namespace — nothing is
/// copied — so each Secret must exist there. Empty → no `imagePullSecrets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoverImage {
    /// The image reference.
    pub image: String,
    /// `imagePullSecrets` names, in order, deduplicated.
    pub pull_secrets: Vec<String>,
}

impl From<&str> for MoverImage {
    /// An image with no pull secrets (a public registry).
    fn from(image: &str) -> Self {
        MoverImage {
            image: image.to_string(),
            pull_secrets: Vec::new(),
        }
    }
}

/// **Pure.** The image when the caller already knows it, or `None` when the
/// cluster must be asked. Split out of the async resolver so the "does this
/// bypass the cluster lookup?" decision is exhaustive over
/// [`MoverImageSource`] and directly testable — a new variant that silently
/// fell through to discovery would be a fail-open change to a fail-closed path.
pub fn pinned_mover_image(source: &MoverImageSource) -> Option<&MoverImage> {
    match source {
        MoverImageSource::Fixed(pinned) => Some(pinned),
        MoverImageSource::DiscoverFromControllerDeployment => None,
    }
}

/// Parse a comma-separated `KOPIUR_MOVER_IMAGE_PULL_SECRETS` value into
/// [`MoverImage::pull_secrets`] names — re-exported so a caller that pins the
/// image (the console) parses exactly like the operator does.
pub use kopiur_mover::jobs::parse_image_pull_secret_names;

/// **Pure.** The session pod's `imagePullSecrets` references for `mover`.
pub fn session_pull_secrets(
    mover: &MoverImage,
) -> Vec<k8s_openapi::api::core::v1::LocalObjectReference> {
    jobs::image_pull_secret_refs(&mover.pull_secrets)
}

/// Progress notifications while a session is being established. A session is
/// interactive: creating one can take a minute (image pull, cold connect), so
/// the caller decides how that is surfaced — the CLI prints to stderr, a server
/// logs or streams it.
///
/// The three named events are the session's lifecycle decisions; the `waiting_*`
/// hooks report the readiness poll and default to silence, so an implementation
/// that only cares about lifecycle need not write them.
pub trait SessionProgress: Send + Sync {
    /// A warm session already existed under the deterministic name; reusing it.
    fn reusing(&self, job: &str);
    /// No usable session existed; this Job is being created.
    fn starting(&self, job: &str);
    /// A concurrent caller won the create race; joining the session it made.
    fn joining(&self, job: &str);
    /// The readiness wait for the session pod has begun. Default: silent.
    fn waiting(&self, job: &str) {
        let _ = job;
    }
    /// One readiness poll elapsed without the pod being Ready. Default: silent.
    fn waiting_tick(&self) {}
    /// The readiness wait ended (ready, failed, or timed out). Default: silent.
    fn waiting_done(&self) {}
}

/// Reports session progress through `tracing` — the default for a server, which
/// has no terminal to draw on.
pub struct TracingProgress;

impl SessionProgress for TracingProgress {
    fn reusing(&self, job: &str) {
        tracing::info!(job, "reusing warm browse session");
    }
    fn starting(&self, job: &str) {
        tracing::info!(job, "starting browse session");
    }
    fn joining(&self, job: &str) {
        tracing::info!(
            job,
            "a concurrent command already started this session; joining it"
        );
    }
    fn waiting(&self, job: &str) {
        tracing::debug!(job, "waiting for the session pod to connect (read-only)");
    }
}

/// FNV-1a 64-bit — a tiny, dependency-free stable hash for deterministic
/// resource names. NOT a security boundary, just name disambiguation.
fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Deterministic session Job name for a repository:
/// `kopiur-browse-<name≤24>-<hash8 of kind/ns/name>`. Deterministic so a second
/// invocation finds (and reuses) the warm session; hashed so two repositories
/// whose truncated names collide still get distinct Jobs. Always well under the
/// 63-char limit pod names derive from.
pub fn session_job_name(kind: RepositoryKind, namespace: Option<&str>, name: &str) -> String {
    let key = format!("{kind:?}/{}/{name}", namespace.unwrap_or_default());
    let hash8 = format!("{:016x}", fnv1a64(&key));
    let short: String = name.chars().take(24).collect();
    format!("kopiur-browse-{short}-{}", &hash8[..8])
}

/// The labels stamped on a session Job (and selected on to find it):
/// `session=browse` + `session-repo=<Kind>-<name>`.
pub fn session_labels(kind: RepositoryKind, name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (SESSION_LABEL.to_string(), SESSION_BROWSE.to_string()),
        (
            SESSION_REPO_LABEL.to_string(),
            session_repo_label_value(kind, name),
        ),
    ])
}

/// Which of a repository's `moverDefaults` pod-metadata fields a browse SESSION
/// pod takes, as `(pod_labels, pod_annotations)`.
///
/// **ASYMMETRIC ON PURPOSE: annotations yes, labels no.**
///
/// `podAnnotations` are applied because the canonical use is a mesh sidecar
/// opt-out (`sidecar.istio.io/inject: "false"`), and that matters MORE for a
/// session than for a batch mover: an injected sidecar that never exits would
/// outlive the session's TTL and leave a pod running after the user's command
/// returned.
///
/// `podLabels` are NOT applied, because the canonical `podLabels` value is a
/// Kueue `kueue.x-k8s.io/queue-name` — which would park a human's interactive
/// `kubectl kopiur browse` behind the nightly backup queue. That is the same
/// hazard `kopiur_controller::pool` avoids by excluding `BrowseSession` from
/// the repository concurrency pool: a person waiting at a terminal is not batch
/// work, and the field alone cannot tell admission which it is.
///
/// A pure fn (not an inline expression at the Job-build site) so the asymmetry
/// is assertable without a cluster.
pub fn session_pod_metadata(defaults: Option<&MoverDefaults>) -> SessionPodMetadata {
    SessionPodMetadata {
        pod_labels: None,
        pod_annotations: defaults.and_then(|d| d.pod_annotations.clone()),
    }
}

/// What [`session_pod_metadata`] decides a session pod carries. `pod_labels` is
/// a field rather than an omission so the deliberate `None` is visible at the
/// Job-build site and cannot be "fixed" by someone who assumes it was forgotten.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionPodMetadata {
    /// Always `None` — see [`session_pod_metadata`].
    pub pod_labels: Option<BTreeMap<String, String>>,
    /// `moverDefaults.podAnnotations`, passed through.
    pub pod_annotations: Option<BTreeMap<String, String>>,
}

/// The `session-repo` label value, `<Kind>-<name>`, clamped to the 63-char
/// label-value limit (the hashed Job name keeps clamped repositories distinct).
pub fn session_repo_label_value(kind: RepositoryKind, name: &str) -> String {
    let kind_str = match kind {
        RepositoryKind::Repository => "Repository",
        RepositoryKind::ClusterRepository => "ClusterRepository",
    };
    let mut v = format!("{kind_str}-{name}");
    v.truncate(63);
    v
}

/// Pull the mover image out of the controller Deployment's
/// `KOPIUR_MOVER_IMAGE` env (any container), so session pods run the exact
/// image the operator stamps into its own movers — plus that same container's
/// `KOPIUR_MOVER_IMAGE_PULL_SECRETS` (absent → none), so a private-registry
/// image can actually be pulled. Pure.
pub fn mover_image_from_deployments(deployments: &[Deployment]) -> Option<MoverImage> {
    deployments.iter().find_map(|d| {
        d.spec
            .as_ref()?
            .template
            .spec
            .as_ref()?
            .containers
            .iter()
            .find_map(|c| {
                let env = c.env.as_ref()?;
                let value = |name: &str| {
                    env.iter()
                        .find_map(|e| (e.name == name).then(|| e.value.clone()).flatten())
                };
                Some(MoverImage {
                    image: value(MOVER_IMAGE_ENV)?,
                    pull_secrets: jobs::parse_image_pull_secret_names(
                        &value(MOVER_IMAGE_PULL_SECRETS_ENV).unwrap_or_default(),
                    ),
                })
            })
    })
}

/// Whether a session Job is already finished/failed/being-deleted (so a new
/// one must replace it rather than be "reused"). Pure.
pub fn job_is_terminal(job: &Job) -> bool {
    if job.metadata.deletion_timestamp.is_some() {
        return true;
    }
    job.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|conds| {
            conds
                .iter()
                .any(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
        })
}

/// Build the session's mover work spec. Browse ignores the identity/targetRef
/// operationally, but they are filled honestly: the identity names the client as
/// the actor, the targetRef names the repository the session holds open. Pure.
pub fn session_work_spec(target: &BrowseTarget, ttl: Duration) -> MoverWorkSpec {
    MoverWorkSpec {
        version: 1,
        operation: Operation::BrowseSession(BrowseSessionOp {
            ttl_seconds: ttl.as_secs(),
        }),
        identity: ResolvedIdentity {
            username: "kubectl-kopiur-browse".into(),
            hostname: target.namespace.clone(),
            source_path: String::new(),
        },
        repository: backend_to_repository_connect(
            &target.repo.backend,
            target.ca_bundle_pem.clone(),
        ),
        target_ref: TargetRef {
            api_version: kopiur_api::consts::API_VERSION.into(),
            kind: match target.repo.kind {
                RepositoryKind::Repository => "Repository".into(),
                RepositoryKind::ClusterRepository => "ClusterRepository".into(),
            },
            name: target.repo.name.clone(),
            namespace: target.namespace.clone(),
            claim_key: None,
        },
        hook_plan: Default::default(),
        options: Default::default(),
        cache: Default::default(),
        throttle: Default::default(),
    }
}

/// A live, Ready session pod the closed [`SessionCmd`] surface can be exec'd
/// into.
pub struct ExecSession {
    client: kube::Client,
    /// The session Job's (and ConfigMap's) namespace.
    pub namespace: String,
    /// The session Job name.
    pub job_name: String,
    /// The Ready pod execed into.
    pub pod: String,
}

impl ExecSession {
    /// Find-or-create the session Job for the target's repository, then wait
    /// for its pod to become Ready (the read-only connect succeeded).
    pub async fn ensure(
        ctx: &OpsCtx,
        target: &BrowseTarget,
        ttl: Duration,
        image: &MoverImageSource,
        progress: &dyn SessionProgress,
    ) -> Result<ExecSession, OpsError> {
        let ns = target.namespace.clone();
        let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
        let labels = session_labels(target.repo.kind, &target.repo.name);
        let selector = labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");

        let expected_name = session_job_name(
            target.repo.kind,
            target.repo.namespace.as_deref(),
            &target.repo.name,
        );
        // Sessions are shared per repository, so this snapshot may join one
        // another snapshot started. Its own credential gates still apply: a
        // snapshot whose run never opted into projection is refused here, not
        // let in through a session whose copy someone else's snapshot earned.
        session_creds(target, &expected_name)?;
        let existing = jobs_api
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|e| classify_kube("list", "Job", "jobs", Some(&ns), None, e))?
            .items
            .into_iter()
            // The session-repo LABEL clamps at 63 chars, so two long repo
            // names can share it — the deterministic NAME disambiguates.
            .find(|j| j.metadata.name.as_deref() == Some(expected_name.as_str()));

        // `created`: whether this call made the Job, so anything the operator
        // later records on it is about this session (see `stuck_verdict`).
        let (job_name, created) = match existing {
            Some(job) if !job_is_terminal(&job) => {
                progress.reusing(&job.name_any());
                (job.name_any(), false)
            }
            Some(job) => {
                // A finished/expired session under the deterministic name: replace it.
                let name = job.name_any();
                let _ = jobs_api.delete(&name, &DeleteParams::background()).await;
                wait_job_gone(&jobs_api, &name).await?;
                let name = create_session_job(ctx, target, ttl, &labels, image, progress).await?;
                (name, true)
            }
            None => {
                let name = create_session_job(ctx, target, ttl, &labels, image, progress).await?;
                (name, true)
            }
        };

        let pod = wait_pod_ready(&ctx.client, &ns, &job_name, created, progress).await?;
        Ok(ExecSession {
            client: ctx.client.clone(),
            namespace: ns,
            job_name,
            pod,
        })
    }

    /// Exec one session command and capture its full stdout (manifest/list
    /// JSON). Non-zero exits surface kopia's stderr.
    pub async fn exec_capture(&self, cmd: SessionCmd) -> Result<Vec<u8>, OpsError> {
        let mut buf = Vec::new();
        self.exec_stream(cmd, &mut buf).await?;
        Ok(buf)
    }

    /// Exec one session command, streaming stdout **byte-for-byte** into
    /// `sink` (the file-content path; binary-safe, no line splitting). Returns
    /// the byte count.
    pub async fn exec_stream(
        &self,
        cmd: SessionCmd,
        sink: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> Result<u64, OpsError> {
        let argv = cmd.argv(SESSION_KOPIA_BIN);
        let what = argv[1..].join(" ");
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut attached = pods
            .exec(
                &self.pod,
                argv.clone(),
                &AttachParams::default()
                    .stdin(false)
                    .stdout(true)
                    .stderr(true),
            )
            .await
            .map_err(|e| {
                classify_kube(
                    "create",
                    "Pod",
                    "pods/exec",
                    Some(&self.namespace),
                    Some(&self.pod),
                    e,
                )
            })?;

        let mut stdout = attached.stdout().ok_or_else(|| OpsError::SessionExec {
            what: what.clone(),
            stderr: "no stdout stream attached".into(),
        })?;
        let mut stderr = attached.stderr().ok_or_else(|| OpsError::SessionExec {
            what: what.clone(),
            stderr: "no stderr stream attached".into(),
        })?;
        let status_fut = attached
            .take_status()
            .ok_or_else(|| OpsError::SessionExec {
                what: what.clone(),
                stderr: "no status stream attached".into(),
            })?;

        let copy = tokio::io::copy(&mut stdout, sink);
        let read_err = async {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf).await;
            buf
        };
        let (copied, err_text, status) = tokio::join!(copy, read_err, status_fut);
        let n = copied.map_err(|source| OpsError::StreamIo {
            what: format!("streaming `{what}` output"),
            source,
        })?;

        // The apiserver reports the exec result as a V1Status on the error
        // channel; anything but an explicit Success is a failed read.
        let ok = status
            .as_ref()
            .is_some_and(|s| s.status.as_deref() == Some("Success"));
        if ok {
            Ok(n)
        } else {
            let detail = status
                .as_ref()
                .and_then(|s| s.message.clone())
                .unwrap_or_default();
            let stderr_text = if err_text.trim().is_empty() {
                detail
            } else {
                err_text.trim_end().to_string()
            };
            Err(OpsError::SessionExec {
                what,
                stderr: stderr_text,
            })
        }
    }
}

/// Create the session Job (the work spec rides its pod env — the session is
/// one self-cleaning object), returning the Job name.
async fn create_session_job(
    ctx: &OpsCtx,
    target: &BrowseTarget,
    ttl: Duration,
    labels: &BTreeMap<String, String>,
    image: &MoverImageSource,
    progress: &dyn SessionProgress,
) -> Result<String, OpsError> {
    let ns = &target.namespace;
    let name = session_job_name(
        target.repo.kind,
        target.repo.namespace.as_deref(),
        &target.repo.name,
    );
    // The browse session talks to a single repository; its Secret(s) load
    // verbatim, from the operator's copies when they live elsewhere.
    let creds = session_creds(target, &name)?;
    let creds_secrets: Vec<jobs::CredsEnvFrom> = creds
        .iter()
        .map(|c| jobs::CredsEnvFrom::plain(c.env_from_name()))
        .collect();
    let projection = session_projection_metadata(&creds, &target.snapshot);
    let work_spec = session_work_spec(target, ttl);
    let mover = resolve_mover_image(ctx, image).await?;
    let pull_secrets = session_pull_secrets(&mover);

    // The repository volume (filesystem backends) is mounted READ-ONLY — the
    // kopia connect is already read-only; this makes the mount match.
    let repo_volume =
        filesystem_repo_mount_source(&target.repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: filesystem_repo_path(&target.repo.backend)
                .expect("a backend with a repo volume has a repo path"),
            read_only: true,
        });

    // Workload identity: the session pod must run as the backend's federated
    // ServiceAccount (and, for Azure, carry the webhook opt-in label) — there
    // is no credential Secret to envFrom. Static-Secret repos keep no SA (the
    // session pod never talks to the kube API; the namespace default suffices).
    let workload_identity = kopiur_api::creds::backend_workload_identity(&target.repo.backend);
    let session_pod_meta = session_pod_metadata(target.repo.mover_defaults.as_ref());
    let mut labels = labels.clone();
    labels.extend(projection.labels);
    if matches!(
        workload_identity,
        Some((_, kopiur_api::creds::WorkloadIdentityCloud::Azure))
    ) {
        labels.insert(
            kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL.to_string(),
            kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL_VALUE.to_string(),
        );
    }

    let inputs = MoverJobInputs {
        name: &name,
        namespace: ns,
        owner: owner_ref_for_repo(&target.repo),
        work_spec: &work_spec,
        image: &mover.image,
        image_pull_policy: None,
        image_pull_secrets: &pull_secrets,
        limits: JobLimits {
            backoff_limit: 0,
            // The TTL bounds the session; the deadline backstops a pod that
            // never finishes connecting.
            active_deadline_seconds: Some(ttl.as_secs() as i64 + 120),
            ttl_seconds_after_finished: Some(60),
        },
        resources: None,
        // The hardened baseline every mover gets when no moverDefaults/recipe
        // overlays apply — a read-only session never needs elevation.
        security_context: kopiur_api::common::hardened_security_context(),
        pod_security_context: None,
        node_selector: None,
        tolerations: None,
        affinity: None,
        // Annotations yes, labels no — see `session_pod_metadata` for why.
        pod_labels: session_pod_meta.pod_labels,
        pod_annotations: session_pod_meta.pod_annotations,
        labels,
        source_volume: None,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        // The session pod never talks to the kube API: no ServiceAccount for a
        // static-Secret repo (the namespace default, no extra rights). A
        // workload-identity repo runs as its federated SA — that's the
        // credential.
        service_account: workload_identity.map(|(wi, _)| wi.service_account_name.as_str()),
        passthrough_env: Vec::new(),
        extra_env: Vec::new(),
        annotations: projection.annotations,
        cache_volume: Default::default(),
        scratch_volume: None,
        readiness_exec: Some(vec![SESSION_MOVER_BIN.to_string(), "ready".to_string()]),
    };

    // The session is ONE object: the Job carries the work spec inline in its
    // pod env, so there is no sidecar ConfigMap to create (or leak — #224).
    let job = jobs::build_job(&inputs).map_err(|source| OpsError::Serialization {
        what: "browse session work spec",
        source: Box::new(source),
    })?;
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
    progress.starting(&name);
    let created = match jobs_api.create(&PostParams::default(), &job).await {
        Ok(created) => created,
        // A concurrent caller won the race: the session exists — reuse it.
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            progress.joining(&name);
            return Ok(name);
        }
        Err(e) => {
            return Err(classify_kube(
                "create",
                "Job",
                "jobs",
                Some(ns),
                Some(&name),
                e,
            ));
        }
    };
    Ok(created.name_any())
}

/// What a session Job whose credentials the operator copies carries, so the
/// operator finds it and can check it: the `session-creds=projected` label it
/// watches, and the `Snapshot` it was started for, whose pinned opt-in the
/// operator re-reads before copying anything.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionProjectionMetadata {
    /// Labels added to the Job.
    pub labels: BTreeMap<String, String>,
    /// Annotations added to the Job.
    pub annotations: BTreeMap<String, String>,
}

/// [`SessionProjectionMetadata`] for a session loading `creds`: empty unless one
/// of them is a copy. Pure.
pub fn session_projection_metadata(
    creds: &[SessionCreds],
    snapshot: &str,
) -> SessionProjectionMetadata {
    if !creds
        .iter()
        .any(|c| matches!(c, SessionCreds::Projected { .. }))
    {
        return SessionProjectionMetadata::default();
    }
    SessionProjectionMetadata {
        labels: BTreeMap::from([(
            SESSION_CREDS_LABEL.to_string(),
            SESSION_CREDS_PROJECTED.to_string(),
        )]),
        annotations: BTreeMap::from([(
            SESSION_SNAPSHOT_ANNOTATION.to_string(),
            snapshot.to_string(),
        )]),
    }
}

/// The session Job's ownerReference: the Repository/ClusterRepository it holds
/// open, so deleting the repository reaps any session.
fn owner_ref_for_repo(repo: &RepoHandle) -> OwnerReference {
    let kind = match repo.kind {
        RepositoryKind::Repository => "Repository",
        RepositoryKind::ClusterRepository => "ClusterRepository",
    };
    jobs::owner_ref(kind, &repo.name, &repo.uid)
}

/// Resolve the mover image a session pod runs. A [`MoverImageSource::Fixed`]
/// image is used verbatim; [`MoverImageSource::DiscoverFromControllerDeployment`]
/// reads it from the controller Deployment (label
/// `app.kubernetes.io/name=kopiur,app.kubernetes.io/component=controller`,
/// env `KOPIUR_MOVER_IMAGE`). The discovery FAILS rather than degrades:
/// - a pinned stale-default fallback would be a mover that cannot parse a
///   BrowseSession work spec — a guaranteed-broken session is worse than an
///   actionable error;
/// - it demands EXACTLY ONE matching Deployment cluster-wide, so a spoofed
///   kopiur-labeled Deployment in another namespace cannot silently redirect
///   session pods (and their repository credentials) to an attacker-chosen
///   image — two matches fail closed and name both.
///
/// Deliberately resolve-only for the CLI: there is no `--image` flag, so a
/// session always runs what the operator runs.
async fn resolve_mover_image(
    ctx: &OpsCtx,
    image: &MoverImageSource,
) -> Result<MoverImage, OpsError> {
    if let Some(pinned) = pinned_mover_image(image) {
        return Ok(pinned.clone());
    }
    let selector = CONTROLLER_DEPLOYMENT_SELECTOR;
    let api: Api<Deployment> = Api::all(ctx.client.clone());
    let list = api
        .list(&ListParams::default().labels(selector))
        .await
        .map_err(|e| classify_kube("list", "Deployment", "deployments", None, None, e))?;
    match list.items.as_slice() {
        [] => Err(OpsError::MoverImageUnresolvable {
            why: format!("no Deployment matches {selector} in any namespace"),
            fix: "check that the kopiur operator is installed (browsing uses its mover image)"
                .into(),
        }),
        [only] => mover_image_from_deployments(std::slice::from_ref(only)).ok_or_else(|| {
            OpsError::MoverImageUnresolvable {
                why: format!(
                    "the controller Deployment {}/{} has no {MOVER_IMAGE_ENV} env var",
                    only.metadata.namespace.clone().unwrap_or_default(),
                    only.metadata.name.clone().unwrap_or_default()
                ),
                fix: "check the chart/deployment; the operator must know its mover image".into(),
            }
        }),
        many => Err(OpsError::MoverImageUnresolvable {
            why: format!(
                "{} Deployments match {selector}: {}, so the mover image is ambiguous",
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
                .into(),
        }),
    }
}

/// Poll until the session Job's pod reports Ready (the mover wrote its
/// readiness marker after the read-only connect), reporting each poll through
/// `progress`. On pod failure, surfaces the pod's log tail.
async fn wait_pod_ready(
    client: &kube::Client,
    namespace: &str,
    job_name: &str,
    created: bool,
    progress: &dyn SessionProgress,
) -> Result<String, OpsError> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let selector = format!("{JOB_NAME_LABEL}={job_name}");
    let started = std::time::Instant::now();
    let deadline = started + READY_WAIT;
    // The Secret the pod was last seen waiting on, so a deadline reached while
    // waiting on the operator's copy says so rather than "timed out".
    let mut stuck_on: Option<String> = None;
    progress.waiting(job_name);
    let result = loop {
        if std::time::Instant::now() > deadline {
            break Err(match stuck_on.as_deref() {
                Some(secret) => stuck_on_secret(job_name, namespace, secret),
                None => OpsError::SessionNotReady {
                    job: job_name.to_string(),
                    after: format!("{}s", READY_WAIT.as_secs()),
                },
            });
        }
        // The Job itself, each round: its uid picks out this session's pods,
        // and the operator records a refusal on it. A failed read is neither:
        // no pod matches, and the deadline still ends the wait.
        let job = jobs.get_opt(job_name).await.ok().flatten();
        let uid = job.as_ref().and_then(|j| j.uid()).unwrap_or_default();
        let refusal = job.as_ref().and_then(|j| {
            j.annotations()
                .get(SESSION_CREDS_REFUSED_ANNOTATION)
                .cloned()
        });
        let list = pods
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|e| classify_kube("list", "Pod", "pods", Some(namespace), None, e));
        let list: Vec<Pod> = match list {
            Ok(l) => l
                .items
                .into_iter()
                .filter(|p| is_session_pod(p, &uid))
                .collect(),
            Err(e) => break Err(e),
        };
        if let Some(pod) = list.iter().find(|p| pod_is_ready(p)) {
            break Ok(pod.name_any());
        }
        if let Some(secret) = list.iter().find_map(missing_secret) {
            if let Some(stop) = stuck_verdict(
                job_name,
                namespace,
                &secret,
                refusal.as_deref(),
                created,
                started.elapsed(),
            ) {
                break Err(stop);
            }
            stuck_on = Some(secret);
        }
        if let Some(failed) = list.iter().find(|p| pod_phase(p) == Some("Failed")) {
            let tail = pod_log_tail(&pods, &failed.name_any()).await;
            break Err(OpsError::SessionPodFailed {
                job: job_name.to_string(),
                namespace: namespace.to_string(),
                detail: tail,
            });
        }
        progress.waiting_tick();
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    progress.waiting_done();
    result
}

/// How long a session pod may wait on a missing Secret of its own namespace
/// before the wait gives up on it: nothing will create that one but a person.
/// A missing *copy* is waited out for the whole readiness budget instead, since
/// the operator may be a retry away from writing it, unless it has said it
/// will not ([`stuck_verdict`]).
const MISSING_SECRET_GRACE: Duration = Duration::from_secs(30);

/// The credential Secret a pod is waiting for, when the kubelet cannot start it
/// because that Secret does not exist (`CreateContainerConfigError`,
/// `secret "<name>" not found`). Pure.
fn missing_secret(pod: &Pod) -> Option<String> {
    pod.status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .into_iter()
        .flatten()
        .filter_map(|c| c.state.as_ref()?.waiting.as_ref())
        .filter(|w| w.reason.as_deref() == Some("CreateContainerConfigError"))
        .find_map(|w| {
            let message = w.message.as_deref()?;
            let rest = message.strip_prefix("secret \"")?;
            let (name, tail) = rest.split_once('"')?;
            tail.trim_start()
                .starts_with("not found")
                .then(|| name.to_string())
        })
}

/// How long a refusal found on a reused session may be stale: the operator's
/// re-check interval, plus a little for the write to land.
const STALE_REFUSAL: Duration =
    Duration::from_secs(kopiur_api::consts::SESSION_CREDS_RECHECK_SECS + 5);

/// **Pure.** Whether `pod` belongs to this session: owned by the Job with
/// `job_uid`, and not on its way out. A session Job's name is reused, so the
/// previous session's pod can still be terminating under the same `job-name`
/// label, holding credentials this session was never given.
fn is_session_pod(pod: &Pod, job_uid: &str) -> bool {
    pod.metadata.deletion_timestamp.is_none()
        && pod
            .owner_references()
            .iter()
            .any(|o| o.kind == "Job" && o.uid == job_uid)
}

/// **Pure.** Whether a session pod stuck on `secret` after `waited` should end
/// the wait, and with what. A missing copy (`{job}-creds-*`) ends it only when
/// the operator recorded a refusal on the Job; otherwise the operator may still
/// write it, and the wait goes on. A missing Secret of the namespace itself ends
/// it after [`MISSING_SECRET_GRACE`].
///
/// A refusal on a session this call `created` is about it. One on a reused
/// session may predate a gate someone just fixed, so it is trusted only once
/// the operator has had its re-check ([`STALE_REFUSAL`]) and still says so.
fn stuck_verdict(
    job: &str,
    namespace: &str,
    secret: &str,
    refusal: Option<&str>,
    created: bool,
    waited: Duration,
) -> Option<OpsError> {
    let projected = secret.starts_with(&format!("{job}-creds-"));
    let refusal = refusal.filter(|_| created || waited > STALE_REFUSAL);
    match (projected, refusal) {
        (true, Some(reason)) => Some(OpsError::SessionCredsRefused {
            job: job.to_string(),
            reason: reason.to_string(),
        }),
        (true, None) => None,
        (false, _) => {
            (waited > MISSING_SECRET_GRACE).then(|| stuck_on_secret(job, namespace, secret))
        }
    }
}

/// The error for a session pod stuck on `secret`: the operator's missing copy,
/// or a Secret of the session's own namespace that is not there. Pure.
fn stuck_on_secret(job: &str, namespace: &str, secret: &str) -> OpsError {
    OpsError::SessionCredsMissing {
        job: job.to_string(),
        namespace: namespace.to_string(),
        secret: secret.to_string(),
        projected: secret.starts_with(&format!("{job}-creds-")),
    }
}

/// Whether a pod's `Ready` condition is `True`. Pure.
fn pod_is_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|conds| {
            conds
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
}

/// A pod's phase string, if reported.
fn pod_phase(pod: &Pod) -> Option<&str> {
    pod.status.as_ref().and_then(|s| s.phase.as_deref())
}

/// Best-effort tail of a failed session pod's logs for the error message.
async fn pod_log_tail(pods: &Api<Pod>, pod: &str) -> String {
    match pods
        .logs(
            pod,
            &LogParams {
                tail_lines: Some(15),
                ..Default::default()
            },
        )
        .await
    {
        Ok(text) if !text.trim().is_empty() => text.trim_end().to_string(),
        _ => "(no pod logs available)".to_string(),
    }
}

/// Poll until a Job is fully gone (its replacement can then be created under
/// the same deterministic name).
async fn wait_job_gone(jobs_api: &Api<Job>, name: &str) -> Result<(), OpsError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match jobs_api.get_opt(name).await {
            Ok(None) => return Ok(()),
            Ok(Some(_)) if std::time::Instant::now() > deadline => {
                return Err(OpsError::SessionNotReady {
                    job: name.to_string(),
                    after: "60s (deleting the previous, expired session)".to_string(),
                });
            }
            Ok(Some(_)) => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => {
                return Err(classify_kube("get", "Job", "jobs", None, Some(name), e));
            }
        }
    }
}

/// Find the session Job for a repository (the `session end` lookup). Returns
/// `None` when no session exists.
pub async fn find_session_job(
    ctx: &OpsCtx,
    namespace: &str,
    kind: RepositoryKind,
    repo_namespace: Option<&str>,
    repo_name: &str,
) -> Result<Option<Job>, OpsError> {
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let labels = session_labels(kind, repo_name);
    let selector = labels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    let expected = session_job_name(kind, repo_namespace, repo_name);
    let list = jobs_api
        .list(&ListParams::default().labels(&selector))
        .await
        .map_err(|e| classify_kube("list", "Job", "jobs", Some(namespace), None, e))?;
    // Same clamped-label caveat as `ensure`: confirm by deterministic name.
    Ok(list
        .items
        .into_iter()
        .find(|j| j.metadata.name.as_deref() == Some(expected.as_str())))
}

/// Delete a session Job (background propagation reaps its pod). The session's
/// whole spec rides the Job env, so the Job is the only object. The trailing
/// best-effort ConfigMap delete cleans up a LEGACY session's work-spec
/// ConfigMap (client versions that mounted the spec) — a 404 no-op otherwise.
pub async fn delete_session(ctx: &OpsCtx, namespace: &str, job_name: &str) -> Result<(), OpsError> {
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    jobs_api
        .delete(job_name, &DeleteParams::background())
        .await
        .map_err(|e| classify_kube("delete", "Job", "jobs", Some(namespace), Some(job_name), e))?;
    let cms: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);
    let _ = cms.delete(job_name, &DeleteParams::default()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_job_name_is_deterministic_distinct_and_short() {
        let a = session_job_name(RepositoryKind::Repository, Some("media"), "nas");
        let b = session_job_name(RepositoryKind::Repository, Some("media"), "nas");
        assert_eq!(a, b, "deterministic — a rerun finds the warm session");
        assert!(a.starts_with("kopiur-browse-nas-"), "{a}");

        // Same name, different kind/namespace → different Job.
        let c = session_job_name(RepositoryKind::ClusterRepository, None, "nas");
        let d = session_job_name(RepositoryKind::Repository, Some("other"), "nas");
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_ne!(c, d);

        // A maximal CRD name stays comfortably under the pod-name budget.
        let long = "x".repeat(253);
        let e = session_job_name(RepositoryKind::Repository, Some("media"), &long);
        assert!(e.len() <= 52, "{} chars: {e}", e.len());
    }

    #[test]
    fn a_browse_session_takes_pod_annotations_but_never_pod_labels() {
        // The asymmetry is deliberate and load-bearing: a Kueue queue-name on an
        // interactive session pod would park a human's `kubectl kopiur browse`
        // behind the nightly backup queue.
        let defaults: kopiur_api::common::MoverDefaults =
            serde_json::from_value(serde_json::json!({
                "podLabels": { "kueue.x-k8s.io/queue-name": "backups" },
                "podAnnotations": { "sidecar.istio.io/inject": "false" },
            }))
            .expect("valid moverDefaults");

        let meta = session_pod_metadata(Some(&defaults));
        assert_eq!(
            meta.pod_labels, None,
            "a Kueue queue-name must never reach an interactive session pod",
        );
        assert_eq!(
            meta.pod_annotations
                .as_ref()
                .and_then(|a| a.get("sidecar.istio.io/inject"))
                .map(String::as_str),
            Some("false"),
            "a mesh opt-out MUST reach it — an injected sidecar outlives the TTL",
        );

        // No moverDefaults at all leaves both unset.
        let bare = session_pod_metadata(None);
        assert_eq!(bare.pod_labels, None);
        assert_eq!(bare.pod_annotations, None);
    }

    #[test]
    fn session_labels_carry_the_wire_contract_keys() {
        let labels = session_labels(RepositoryKind::Repository, "nas");
        assert_eq!(labels[SESSION_LABEL], "browse");
        assert_eq!(labels[SESSION_REPO_LABEL], "Repository-nas");
        let labels = session_labels(RepositoryKind::ClusterRepository, "shared");
        assert_eq!(labels[SESSION_REPO_LABEL], "ClusterRepository-shared");
        // Label values are clamped to the k8s 63-char limit.
        let long = session_repo_label_value(RepositoryKind::Repository, &"y".repeat(100));
        assert_eq!(long.len(), 63);
    }

    #[test]
    fn mover_image_resolves_from_the_controller_env_or_none() {
        let dep: Deployment = serde_json::from_value(serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": "kopiur" },
            "spec": {
                "selector": { "matchLabels": { "app": "kopiur" } },
                "template": { "spec": { "containers": [{
                    "name": "controller",
                    "image": "ghcr.io/home-operations/kopiur-controller:v9",
                    "env": [
                        { "name": "OTHER", "value": "x" },
                        { "name": "KOPIUR_MOVER_IMAGE", "value": "ghcr.io/home-operations/kopiur-mover:v9" }
                    ]
                }] } }
            }
        }))
        .unwrap();
        assert_eq!(
            mover_image_from_deployments(&[dep]),
            Some(MoverImage {
                image: "ghcr.io/home-operations/kopiur-mover:v9".into(),
                // No pull-secrets env on the controller → none on the session.
                pull_secrets: Vec::new(),
            })
        );
        assert_eq!(mover_image_from_deployments(&[]), None);
    }

    #[test]
    fn mover_image_pull_secrets_resolve_from_the_controller_env() {
        // #579: a private-registry mover image needs the chart's pull secrets
        // on browse-session pods too, discovered the same way as the image.
        let dep: Deployment = serde_json::from_value(serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": "kopiur" },
            "spec": {
                "selector": { "matchLabels": { "app": "kopiur" } },
                "template": { "spec": { "containers": [{
                    "name": "controller",
                    "image": "registry.example/kopiur-controller:v9",
                    "env": [
                        { "name": "KOPIUR_MOVER_IMAGE", "value": "registry.example/kopiur-mover:v9" },
                        { "name": "KOPIUR_MOVER_IMAGE_PULL_SECRETS", "value": "global-cred, regcred,global-cred" }
                    ]
                }] } }
            }
        }))
        .unwrap();
        let mover = mover_image_from_deployments(&[dep]).expect("image discovered");
        assert_eq!(mover.image, "registry.example/kopiur-mover:v9");
        assert_eq!(mover.pull_secrets, vec!["global-cred", "regcred"]);
    }

    #[test]
    fn session_job_carries_the_resolved_pull_secrets() {
        let mover = MoverImage {
            image: "registry.example/kopiur-mover:v9".into(),
            pull_secrets: vec!["regcred".into()],
        };
        let refs = session_pull_secrets(&mover);
        assert_eq!(
            refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["regcred"]
        );
        assert!(session_pull_secrets(&MoverImage::from("m")).is_empty());
    }

    #[test]
    fn terminal_job_detection_covers_complete_failed_and_deleting() {
        let job = |conds: serde_json::Value, deleting: bool| -> Job {
            let mut v = serde_json::json!({
                "apiVersion": "batch/v1",
                "kind": "Job",
                "metadata": { "name": "j" },
                "status": { "conditions": conds }
            });
            if deleting {
                v["metadata"]["deletionTimestamp"] = serde_json::json!("2026-06-11T00:00:00Z");
            }
            serde_json::from_value(v).unwrap()
        };
        assert!(!job_is_terminal(&job(serde_json::json!([]), false)));
        assert!(job_is_terminal(&job(
            serde_json::json!([{ "type": "Complete", "status": "True" }]),
            false
        )));
        assert!(job_is_terminal(&job(
            serde_json::json!([{ "type": "Failed", "status": "True" }]),
            false
        )));
        assert!(job_is_terminal(&job(serde_json::json!([]), true)));
    }

    fn waiting_pod(reason: &str, message: &str) -> Pod {
        serde_json::from_value(serde_json::json!({
            "metadata": { "name": "kopiur-browse-nas-0123abcd-x7k2p" },
            "status": {
                "phase": "Pending",
                "containerStatuses": [{
                    "name": "mover",
                    "image": "mover",
                    "imageID": "",
                    "ready": false,
                    "restartCount": 0,
                    "state": { "waiting": { "reason": reason, "message": message } }
                }]
            }
        }))
        .expect("pod fixture")
    }

    #[test]
    fn a_pod_waiting_on_a_missing_secret_names_it() {
        let pod = waiting_pod(
            "CreateContainerConfigError",
            "secret \"kopiur-browse-nas-0123abcd-creds-0\" not found",
        );
        assert_eq!(
            missing_secret(&pod).as_deref(),
            Some("kopiur-browse-nas-0123abcd-creds-0")
        );
        assert_eq!(
            missing_secret(&waiting_pod("ContainerCreating", "")),
            None,
            "an ordinary start is not stuck"
        );
        assert_eq!(
            missing_secret(&waiting_pod(
                "CreateContainerConfigError",
                "configmap \"x\" not found"
            )),
            None
        );
    }

    #[test]
    fn a_missing_copy_waits_out_the_operators_retry_unless_it_refused() {
        let job = "kopiur-browse-nas-0123abcd";
        let copy = format!("{job}-creds-0");
        let long = Duration::from_secs(120);
        assert!(
            stuck_verdict(job, "vaultwarden", &copy, None, true, long).is_none(),
            "a copy not written yet may be a retry away: keep waiting"
        );
        let refused = stuck_verdict(
            job,
            "vaultwarden",
            &copy,
            Some("namespace vaultwarden is not in allowedNamespaces. Fix: add it there"),
            true,
            Duration::from_secs(2),
        )
        .expect("a refusal ends the wait at once");
        assert!(
            matches!(&refused, OpsError::SessionCredsRefused { .. }),
            "{refused}"
        );
        assert!(
            refused.to_string().contains("Fix: add it there"),
            "{refused}"
        );
    }

    #[test]
    fn only_a_live_pod_of_this_very_job_is_the_session() {
        // A session Job's name is reused, so the previous session's pod may
        // still be terminating under the same job-name label, with credentials
        // this session was never given. It is not this session.
        let pod = |owner_uid: &str, deleting: bool| -> Pod {
            let mut v = serde_json::json!({
                "metadata": {
                    "name": "kopiur-browse-nas-0123abcd-x7k2p",
                    "ownerReferences": [{
                        "apiVersion": "batch/v1", "kind": "Job",
                        "name": "kopiur-browse-nas-0123abcd", "uid": owner_uid,
                        "controller": true
                    }]
                }
            });
            if deleting {
                v["metadata"]["deletionTimestamp"] = "2026-10-11T12:00:00Z".into();
            }
            serde_json::from_value(v).expect("pod fixture")
        };
        assert!(is_session_pod(&pod("this-uid", false), "this-uid"));
        assert!(!is_session_pod(&pod("previous-uid", false), "this-uid"));
        assert!(!is_session_pod(&pod("this-uid", true), "this-uid"));
    }

    #[test]
    fn a_reused_session_trusts_a_refusal_only_once_the_operator_could_recheck_it() {
        let job = "kopiur-browse-nas-0123abcd";
        let copy = format!("{job}-creds-0");
        let refusal = Some("not allowed. Fix: allow it");
        // A session this call created: any refusal on it is about it.
        assert!(stuck_verdict(job, "ns", &copy, refusal, true, Duration::from_secs(1)).is_some());
        // A reused one may carry a refusal from before the gate was fixed.
        assert!(stuck_verdict(job, "ns", &copy, refusal, false, Duration::from_secs(1)).is_none());
        assert!(
            stuck_verdict(job, "ns", &copy, refusal, false, Duration::from_secs(25)).is_some(),
            "past the operator's re-check, a refusal still there is current"
        );
    }

    #[test]
    fn a_missing_secret_of_the_namespace_itself_ends_the_wait_after_the_grace() {
        let job = "kopiur-browse-nas-0123abcd";
        assert!(stuck_verdict(job, "media", "creds", None, true, Duration::from_secs(5)).is_none());
        assert!(matches!(
            stuck_verdict(job, "media", "creds", None, true, Duration::from_secs(31)),
            Some(OpsError::SessionCredsMissing {
                projected: false,
                ..
            })
        ));
    }

    #[test]
    fn a_missing_copy_points_at_the_operator_and_a_missing_secret_at_the_namespace() {
        let job = "kopiur-browse-nas-0123abcd";
        let copy = stuck_on_secret(job, "vaultwarden", &format!("{job}-creds-0")).to_string();
        assert!(
            copy.contains("features.credentialProjection.enabled"),
            "{copy}"
        );
        let own = stuck_on_secret(job, "vaultwarden", "creds").to_string();
        assert!(own.contains("create it in vaultwarden"), "{own}");
    }

    #[tokio::test]
    async fn joining_a_session_checks_this_snapshots_gates_too() {
        // Sessions are shared per repository, so a second snapshot may find one
        // already running. Its own gates must still be checked, before any
        // apiserver call: this client points at nothing, so reaching it would
        // be a connection error, not the refusal.
        use kopiur_api::backend::{Backend, S3Backend};
        use kopiur_api::common::{Encryption, SecretKeyRef};
        let config = kube::Config::new("http://127.0.0.1:1".parse().unwrap());
        let ctx = OpsCtx {
            client: kube::Client::try_from(config).unwrap(),
            namespace: "vaultwarden".into(),
            scope: crate::ctx::Scope::Namespace("vaultwarden".into()),
            field_manager: "test".into(),
        };
        let target = BrowseTarget {
            snapshot: "never-opted-in".into(),
            namespace: "vaultwarden".into(),
            kopia_snapshot_id: "kdead".into(),
            repo: RepoHandle {
                kind: RepositoryKind::ClusterRepository,
                name: "rustfs".into(),
                uid: "u1".into(),
                namespace: None,
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
                        key: None,
                        namespace: Some("kopiur".into()),
                    },
                },
                mover_defaults: None,
            },
            ca_bundle_pem: None,
            projection: crate::browse::resolve::SessionProjection {
                allowed: true,
                opted_in: false,
            },
        };
        let err = ExecSession::ensure(
            &ctx,
            &target,
            Duration::from_secs(60),
            &MoverImageSource::Fixed("mover".into()),
            &TracingProgress,
        )
        .await
        .err()
        .expect("a snapshot that never opted in must be refused");
        assert!(
            matches!(err, OpsError::CredsOutsideSessionNamespace { .. }),
            "refused before touching the apiserver, got: {err}"
        );
    }

    #[test]
    fn a_session_loading_a_copy_is_marked_for_the_operator() {
        let projected = [
            SessionCreds::Direct("local".into()),
            SessionCreds::Projected {
                source_namespace: "kopiur".into(),
                source_name: "creds".into(),
                copy_name: "kopiur-browse-nas-0123abcd-creds-1".into(),
            },
        ];
        let meta = session_projection_metadata(&projected, "db-1");
        assert_eq!(
            meta.labels.get(SESSION_CREDS_LABEL).map(String::as_str),
            Some(SESSION_CREDS_PROJECTED)
        );
        assert_eq!(
            meta.annotations
                .get(SESSION_SNAPSHOT_ANNOTATION)
                .map(String::as_str),
            Some("db-1")
        );
        assert_eq!(
            session_projection_metadata(&[SessionCreds::Direct("local".into())], "db-1"),
            SessionProjectionMetadata::default(),
            "a session that loads only its own namespace's Secrets asks for nothing"
        );
    }

    #[test]
    fn work_spec_pins_the_ttl_and_read_only_browse_operation() {
        use kopiur_api::backend::{Backend, FilesystemBackend};
        use kopiur_api::common::{Encryption, SecretKeyRef};
        let target = BrowseTarget {
            snapshot: "db-1".into(),
            namespace: "media".into(),
            kopia_snapshot_id: "kdead".into(),
            repo: RepoHandle {
                kind: RepositoryKind::Repository,
                name: "nas".into(),
                uid: "u1".into(),
                namespace: Some("media".into()),
                backend: Backend::Filesystem(FilesystemBackend {
                    path: "/repo".into(),
                    volume: None,
                }),
                encryption: Encryption {
                    password_secret_ref: SecretKeyRef {
                        name: "creds".into(),
                        namespace: None,
                        key: None,
                    },
                },
                mover_defaults: None,
            },
            ca_bundle_pem: None,
            projection: Default::default(),
        };
        let ws = session_work_spec(&target, Duration::from_secs(1800));
        match &ws.operation {
            Operation::BrowseSession(op) => assert_eq!(op.ttl_seconds, 1800),
            other => panic!("expected BrowseSession, got {other:?}"),
        }
        assert_eq!(ws.target_ref.kind, "Repository");
        assert_eq!(ws.target_ref.name, "nas");
        assert_eq!(ws.target_ref.namespace, "media");
        // The repository connect mirrors the backend.
        let v = serde_json::to_value(&ws).unwrap();
        assert_eq!(v["repository"]["filesystem"]["path"], "/repo");
    }

    #[test]
    fn a_pinned_mover_image_bypasses_the_cluster_lookup() {
        let pinned = MoverImage {
            image: "ghcr.io/x/mover:1.2.3".into(),
            pull_secrets: vec!["regcred".into()],
        };
        assert_eq!(
            pinned_mover_image(&MoverImageSource::Fixed(pinned.clone())),
            Some(&pinned)
        );
        // Discovery must NOT resolve to a default here: returning None is what
        // sends `resolve_mover_image` to the fail-closed cluster lookup.
        assert_eq!(
            pinned_mover_image(&MoverImageSource::DiscoverFromControllerDeployment),
            None
        );
    }
}
