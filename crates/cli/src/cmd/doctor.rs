//! `kubectl kopiur doctor` — diagnose an installation: CRDs, operator and
//! webhook health (including a live admission probe), repository readiness,
//! credential Secrets, blocked/stuck work, recent failures, and recent
//! warnings. Every check lands in a closed enum and renders Pass / Warn /
//! Fail{what, why, fix}; the exit code is 1 iff anything failed. RBAC the user
//! lacks degrades a check to Warn (with the missing grant named), never a
//! crash.
//!
//! ## Blocked ≠ old (issue #359)
//!
//! The stuck check reads **conditions**, not just phases. A structural gate
//! ([`kopiur_api::gates::STRUCTURAL_GATES`] — a missing namespace opt-in, a
//! missing credential Secret, a held mass-deletion breaker, a schedule wedged
//! on an unreadable run) parks an object at an unremarkable phase and never
//! self-heals, so an age threshold is exactly the wrong instrument: doctor
//! reported all-green for the first hour of a permanent outage. A gate hit is
//! therefore **age-independent**, and its severity comes from the shared
//! registry rather than from anything restated here — a gate the operator
//! grows is one the CLI reports with no code change on this side.

use chrono::{DateTime, Utc};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kopiur_api::common::{PhaseLabel, RepositoryKind};
use kopiur_api::creds::mover_creds_secret_refs;
use kopiur_api::gates::{GateScope, GateSeverity, STRUCTURAL_GATES, StructuralGate};
use kopiur_api::{
    ClusterRepository, Repository, Restore, RestorePhase, Snapshot, SnapshotPhase, SnapshotPolicy,
    SnapshotSchedule,
};
use kube::ResourceExt;
use kube::api::{Api, ListParams, PostParams};
use kube::core::CustomResourceExt as KubeCustomResourceExt;
use serde::Serialize;

use crate::cli::DoctorArgs;
use crate::context::{KubeCtx, Scope};
use crate::error::CliError;
use crate::output::OutputFormat;

/// Every check doctor performs. Closed enum: adding a check forces the runner
/// and the renderer to handle it. Nine checks, run in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DoctorCheck {
    /// All 8 kopiur CRDs are installed and serve `v1alpha1`.
    CrdsInstalled,
    /// The controller Deployment exists and has ready replicas.
    ControllerRunning,
    /// The webhook Deployment exists and has ready replicas (when installed).
    WebhookRunning,
    /// A live dry-run admission probe: an invalid SnapshotPolicy must be denied.
    WebhookAdmits,
    /// Every Repository/ClusterRepository is phase Ready and carries no
    /// repository-scoped structural gate (a held mass-deletion breaker).
    RepositoriesReady,
    /// Every repository's credential Secret(s) resolve.
    CredentialsPresent,
    /// No Snapshot/Restore/SnapshotSchedule is parked on a structural gate, and
    /// no Snapshot/Restore has been non-terminal longer than the threshold.
    NoStuckWork,
    /// No Snapshot/Restore failed within `--failure-lookback` (older retained
    /// failures warn instead).
    RecentFailures,
    /// No recent Warning events on kopiur objects.
    RecentWarnings,
}

impl DoctorCheck {
    /// Human title for the report line.
    pub fn title(self) -> &'static str {
        match self {
            Self::CrdsInstalled => "CRDs installed",
            Self::ControllerRunning => "controller running",
            Self::WebhookRunning => "webhook running",
            Self::WebhookAdmits => "webhook admission (live dry-run probe)",
            Self::RepositoriesReady => "repositories ready",
            Self::CredentialsPresent => "credential secrets present",
            Self::NoStuckWork => "no blocked or stuck work",
            Self::RecentFailures => "no recent failed snapshots/restores",
            Self::RecentWarnings => "recent warning events",
        }
    }
}

/// Outcome of one check.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// All good.
    Pass,
    /// Could not fully verify, or a non-fatal observation; message says why.
    Warn(String),
    /// Something is wrong; what/why/fix.
    Fail {
        /// What is broken.
        what: String,
        /// Why it matters / why it happens.
        why: String,
        /// How to fix it.
        fix: String,
    },
}

/// One line of the doctor report.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    /// Which check.
    pub check: DoctorCheck,
    /// What happened.
    pub outcome: Outcome,
}

/// The full report (`-o json|yaml` emits this).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    /// All check results, in run order.
    pub checks: Vec<CheckResult>,
}

impl DoctorReport {
    /// Exit code: 1 iff any check failed (warnings don't fail the run).
    pub fn exit_code(&self) -> u8 {
        let failed = self
            .checks
            .iter()
            .any(|c| matches!(c.outcome, Outcome::Fail { .. }));
        u8::from(failed)
    }
}

/// Render the human report. Pure.
pub fn render(report: &DoctorReport) -> String {
    let mut out = String::new();
    for result in &report.checks {
        let line = match &result.outcome {
            Outcome::Pass => format!("  ok    {}\n", result.check.title()),
            Outcome::Warn(msg) => format!("  warn  {}: {}\n", result.check.title(), msg),
            Outcome::Fail { what, why, fix } => format!(
                "  FAIL  {}: {}\n        why: {}\n        fix: {}\n",
                result.check.title(),
                what,
                why,
                fix
            ),
        };
        out.push_str(&line);
    }
    let failed = report
        .checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Fail { .. }))
        .count();
    let warned = report
        .checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Warn(_)))
        .count();
    out.push_str(&format!(
        "\n{} check(s): {} failed, {} warning(s)\n",
        report.checks.len(),
        failed,
        warned
    ));
    out
}

/// The fix line for every "this plugin is older than the server" verdict, so
/// the three skew paths (an undecodable list, an unknown phase, an unregistered
/// gate reason) tell the user the same thing.
const UPGRADE_PLUGIN_FIX: &str = "upgrade the plugin to the operator's version (kubectl krew upgrade kopiur, or reinstall \
     kubectl-kopiur from the operator's release) and re-run doctor";

/// Map a kube error on a check into an outcome.
///
/// Access problems (RBAC) and transport failures **degrade to Warn** naming the
/// missing grant: doctor must never crash on a restricted kubeconfig.
///
/// A **decode** failure does not: it means the API server is serving a shape
/// this plugin cannot read, so every object of that kind silently vanished from
/// the checks that follow. Reporting "could not verify" for a whole kind and
/// still exiting 0 is the #359 failure mode one level up, so it is a Fail.
fn warn_for(verb: &str, resource: &str, e: &kube::Error) -> Outcome {
    match e {
        kube::Error::SerdeError(se) => Outcome::Fail {
            what: format!("cannot decode the {resource} this cluster serves: {se}"),
            why: "the server is writing a shape this plugin does not understand (an operator \
                  newer than the plugin, or a partially-completed upgrade), so NONE of the \
                  objects of that kind could be examined — this check saw an empty cluster"
                .into(),
            fix: UPGRADE_PLUGIN_FIX.into(),
        },
        kube::Error::Api(ae) if ae.code == 403 => Outcome::Warn(format!(
            "cannot {verb} {resource} (RBAC); grant `{verb}` on `{resource}` or run \
             with a more privileged kubeconfig to enable this check"
        )),
        other => Outcome::Warn(format!("cannot {verb} {resource}: {other}")),
    }
}

/// The 8 CRD names doctor expects, from the same types the plugin is built
/// against (so "installed" means "this plugin's schema vintage exists").
fn expected_crds() -> Vec<(String, CustomResourceDefinition)> {
    fn entry<K: KubeCustomResourceExt>() -> (String, CustomResourceDefinition) {
        let crd = K::crd();
        (crd.metadata.name.clone().unwrap_or_default(), crd)
    }
    vec![
        entry::<Repository>(),
        entry::<ClusterRepository>(),
        entry::<SnapshotPolicy>(),
        entry::<Snapshot>(),
        entry::<kopiur_api::SnapshotSchedule>(),
        entry::<Restore>(),
        entry::<kopiur_api::Maintenance>(),
        entry::<kopiur_api::RepositoryReplication>(),
    ]
}

async fn check_crds(ctx: &KubeCtx) -> Outcome {
    let api: Api<CustomResourceDefinition> = Api::all(ctx.client.clone());
    let mut missing = Vec::new();
    for (name, _) in expected_crds() {
        match api.get_opt(&name).await {
            Ok(Some(crd)) => {
                let serves_v1alpha1 = crd
                    .spec
                    .versions
                    .iter()
                    .any(|v| v.name == kopiur_api::VERSION && v.served);
                if !serves_v1alpha1 {
                    return Outcome::Fail {
                        what: format!("CRD {name} does not serve {}", kopiur_api::VERSION),
                        why: "this plugin (and the operator) speak v1alpha1 only".into(),
                        fix: "upgrade/reinstall the kopiur CRDs (helm upgrade, or apply deploy/crds/)"
                            .into(),
                    };
                }
            }
            Ok(None) => missing.push(name),
            Err(e) => return warn_for("get", "customresourcedefinitions", &e),
        }
    }
    if missing.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail {
            what: format!("missing CRD(s): {}", missing.join(", ")),
            why: "without the CRDs the API server rejects every kopiur object".into(),
            fix:
                "install kopiur (helm install kopiur oci://ghcr.io/home-operations/charts/kopiur) \
                  or apply deploy/crds/"
                    .into(),
        }
    }
}

/// Find kopiur Deployments by the chart's labels, across namespaces. Returns
/// the outcome plus whether the Deployment EXISTS at all (the admission probe
/// gates on existence — a present-but-unready webhook should be probed, since
/// that is exactly when "failed calling webhook" surfaces).
async fn check_deployment(ctx: &KubeCtx, component: &str, required: bool) -> (Outcome, bool) {
    let api: Api<Deployment> = Api::all(ctx.client.clone());
    let selector = format!("app.kubernetes.io/name=kopiur,app.kubernetes.io/component={component}");
    let listed = match api.list(&ListParams::default().labels(&selector)).await {
        Ok(l) => l,
        Err(e) => return (warn_for("list", "deployments", &e), true),
    };
    let Some(deploy) = listed.items.first() else {
        if !required {
            return (
                Outcome::Warn(format!(
                    "no {component} Deployment found (label {selector}); skipped if not installed"
                )),
                false,
            );
        }
        return (
            Outcome::Fail {
                what: format!("no {component} Deployment found (label {selector})"),
                why: "without the controller nothing reconciles — backups will not run".into(),
                fix: "install kopiur (helm install …) or check the release's namespace".into(),
            },
            false,
        );
    };
    let ready = deploy
        .status
        .as_ref()
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);
    let outcome = if ready >= 1 {
        Outcome::Pass
    } else {
        Outcome::Fail {
            what: format!(
                "{component} Deployment {}/{} has 0 ready replicas",
                deploy.metadata.namespace.clone().unwrap_or_default(),
                deploy.name_any()
            ),
            why: "the pods are not Ready (crash loop, image pull, scheduling, …)".into(),
            fix: format!(
                "kubectl -n {} describe deploy/{} and check the pod events/logs",
                deploy.metadata.namespace.clone().unwrap_or_default(),
                deploy.name_any()
            ),
        }
    };
    (outcome, true)
}

/// Live admission probe: a dry-run create of a deliberately-invalid
/// SnapshotPolicy. Denied = the webhook intercepts (healthy); admitted = it is
/// not intercepting; transport error = broken wiring. Zero cluster mutation
/// (server-side dryRun).
async fn check_webhook_admission(ctx: &KubeCtx, webhook_installed: bool) -> Outcome {
    if !webhook_installed {
        return Outcome::Warn(
            "webhook not installed; admission-time validation is off (the controller still \
             validates defensively)"
                .into(),
        );
    }
    let ns = ctx.namespace.as_str();
    let api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), ns);
    // Invalid on purpose: a ClusterRepository ref must not carry a namespace
    // (api::validate refuses it; shared by webhook and controller).
    let invalid: SnapshotPolicy = serde_json::from_value(serde_json::json!({
        "apiVersion": kopiur_api::consts::API_VERSION,
        "kind": "SnapshotPolicy",
        "metadata": { "name": "kopiur-doctor-probe", "namespace": ns },
        "spec": {
            "repository": { "kind": "ClusterRepository", "name": "x", "namespace": "not-allowed" },
            "sources": [ { "pvc": { "name": "x" } } ]
        }
    }))
    .expect("probe fixture");
    let params = PostParams {
        dry_run: true,
        field_manager: Some(crate::consts::FIELD_MANAGER.to_string()),
    };
    match api.create(&params, &invalid).await {
        // Denied BY KOPIUR's webhook: reachable and validating. Healthy. (The
        // apiserver names the denying webhook; a Kyverno/OPA denial must not
        // mask a broken kopiur webhook behind failurePolicy: Ignore.)
        Err(kube::Error::Api(ae))
            if ae.message.contains("denied the request") && ae.message.contains("kopiur") =>
        {
            Outcome::Pass
        }
        Err(kube::Error::Api(ae)) if ae.message.contains("denied the request") => {
            Outcome::Warn(format!(
                "the probe was denied by a NON-kopiur webhook, so kopiur's own validation \
                 could not be confirmed: {}",
                ae.message
            ))
        }
        // Admitted: the webhook did NOT intercept an invalid object.
        Ok(_) => Outcome::Fail {
            what: "an invalid SnapshotPolicy passed admission (dry-run)".into(),
            why: "the validating webhook is not intercepting kopiur objects — bad specs will \
                  land and fail later at reconcile time"
                .into(),
            fix: "check the ValidatingWebhookConfiguration, the webhook Service endpoints, and \
                  the webhook pod logs"
                .into(),
        },
        // Webhook wired but unreachable: the failurePolicy surfaces as an error.
        Err(kube::Error::Api(ae)) if ae.message.contains("failed calling webhook") => {
            Outcome::Fail {
                what: "the API server cannot call the kopiur webhook".into(),
                why: format!(
                    "admission requests error instead of validating: {}",
                    ae.message
                ),
                fix: "check the webhook Service/EndpointSlices, the CA bundle, and the webhook \
                      pod (kubectl -n <ns> logs deploy/<release>-webhook)"
                    .into(),
            }
        }
        Err(kube::Error::Api(ae)) if ae.code == 403 => Outcome::Warn(
            "cannot dry-run create snapshotpolicies (RBAC); grant `create` (dryRun) to enable \
             the admission probe"
                .into(),
        ),
        Err(e) => Outcome::Warn(format!("admission probe inconclusive: {e}")),
    }
}

struct RepoSummary {
    kind: RepositoryKind,
    name: String,
    namespace: Option<String>,
    phase: Option<String>,
    ready_message: Option<String>,
    /// The repository's full condition list, so the repository-scoped rows of
    /// the shared gate registry (the mass-deletion breaker) are checked here
    /// rather than only where the *phase* happens to notice something.
    conditions: Vec<Condition>,
    backend: kopiur_api::Backend,
    encryption: kopiur_api::common::Encryption,
}

impl RepoSummary {
    /// `Repository/nas` — the identity used in every message about this repo.
    fn label(&self) -> String {
        format!("{:?}/{}", self.kind, self.name)
    }
}

fn ready_message(conditions: &[Condition]) -> Option<String> {
    conditions
        .iter()
        .find(|c| c.type_ == kopiur_api::consts::READY_CONDITION)
        .map(|c| c.message.clone())
}

/// Summarize a namespaced `Repository`. Pure (so the gate arm of
/// [`check_repos_ready`] is unit-testable from a JSON fixture).
fn summarize_repository(r: &Repository) -> RepoSummary {
    let conditions = r
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    RepoSummary {
        kind: RepositoryKind::Repository,
        name: r.name_any(),
        namespace: r.metadata.namespace.clone(),
        phase: r
            .status
            .as_ref()
            .and_then(|s| s.phase.as_ref())
            .map(|p| p.label().to_string()),
        ready_message: ready_message(&conditions),
        conditions,
        backend: r.spec.backend.clone(),
        encryption: r.spec.encryption.clone(),
    }
}

/// Summarize a cluster-scoped `ClusterRepository`. Pure.
fn summarize_cluster_repository(r: &ClusterRepository) -> RepoSummary {
    let conditions = r
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    RepoSummary {
        kind: RepositoryKind::ClusterRepository,
        name: r.name_any(),
        namespace: None,
        phase: r
            .status
            .as_ref()
            .and_then(|s| s.phase.as_ref())
            .map(|p| p.label().to_string()),
        ready_message: ready_message(&conditions),
        conditions,
        backend: r.spec.backend.clone(),
        encryption: r.spec.encryption.clone(),
    }
}

async fn list_repos(ctx: &KubeCtx) -> Result<Vec<RepoSummary>, Outcome> {
    let api: Api<Repository> = match &ctx.scope {
        Scope::All => Api::all(ctx.client.clone()),
        Scope::Namespace(ns) => Api::namespaced(ctx.client.clone(), ns),
    };
    let mut repos: Vec<RepoSummary> = match api.list(&ListParams::default()).await {
        Ok(listed) => listed.items.iter().map(summarize_repository).collect(),
        Err(e) => return Err(warn_for("list", "repositories", &e)),
    };
    let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
    match api.list(&ListParams::default()).await {
        Ok(listed) => repos.extend(listed.items.iter().map(summarize_cluster_repository)),
        Err(e) => return Err(warn_for("list", "clusterrepositories", &e)),
    }
    Ok(repos)
}

/// Repositories that are not `Ready`, **plus** repositories carrying a
/// repository-scoped structural gate. Pure.
///
/// The gate arm matters independently of the phase: the mass-deletion breaker
/// holds every pending Snapshot deletion for a repository that is otherwise
/// perfectly `Ready`, so a phase-only check reports green while a whole
/// deletion wave is frozen awaiting an acknowledgement.
fn check_repos_ready(repos: &[RepoSummary]) -> Outcome {
    let mut fails: Vec<String> = Vec::new();
    let mut warns: Vec<String> = Vec::new();
    for r in repos {
        if r.phase.as_deref() != Some("Ready") {
            fails.push(format!(
                "{} ({}{})",
                r.label(),
                r.phase.as_deref().unwrap_or("no status"),
                r.ready_message
                    .as_deref()
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            ));
        }
        match first_gate(&r.conditions, GateScope::covers_repository) {
            Some(GateHit::Known(gate, cond)) => {
                let line = format!("{}: {}", r.label(), describe_gate(gate, cond));
                match gate.severity {
                    GateSeverity::Fail => fails.push(line),
                    GateSeverity::Warn => warns.push(line),
                }
            }
            // The hint is inlined rather than left to a `fix:` line: this is the
            // only unregistered-gate report whose finding carries no per-class
            // fix (the Warn outcome is just these lines, and when a Fail is also
            // present they are folded into ITS fix, which is about condition
            // messages). Without it the reader is told about the skew and not
            // what to do about it. The stuck-work path says the same thing in
            // `StuckKind::fix`, so it is not repeated in the shared describer.
            Some(GateHit::Unregistered(cond)) => warns.push(format!(
                "{}: {} — {UPGRADE_PLUGIN_FIX}",
                r.label(),
                describe_unregistered_gate(cond)
            )),
            None => {}
        }
    }
    if !fails.is_empty() {
        // The warn-level findings are still listed — a Fail elsewhere must not
        // swallow the ReadOnly/unknown-reason lines a reader needs to see.
        fails.extend(warns);
        return Outcome::Fail {
            what: format!("repositories not Ready or blocked: {}", fails.join("; ")),
            why: "backups/restores against an unready repository cannot run, and a repository \
                  holding a structural gate (a tripped mass-deletion breaker) blocks the work \
                  waiting on it until a human acts"
                .into(),
            fix: "the condition message above is the operator's diagnosis and carries the exact \
                  command where one applies; `kubectl describe` the repository for events"
                .into(),
        };
    }
    if warns.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Warn(warns.join("; "))
    }
}

async fn check_credentials(ctx: &KubeCtx, repos: &[RepoSummary]) -> Outcome {
    let mut missing = Vec::new();
    let mut unverifiable = Vec::new();
    for repo in repos {
        let default_ns = repo.namespace.as_deref();
        for cred in mover_creds_secret_refs(&repo.backend, &repo.encryption, default_ns) {
            let Some(ns) = cred.namespace.clone().or(default_ns.map(str::to_string)) else {
                missing.push(format!(
                    "{:?}/{}: secret {:?} has no resolvable namespace (a ClusterRepository \
                     reference must pin one)",
                    repo.kind, repo.name, cred.name
                ));
                continue;
            };
            let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
            match api.get_opt(&cred.name).await {
                Ok(Some(_)) => {}
                Ok(None) => missing.push(format!(
                    "{:?}/{}: secret {}/{} not found",
                    repo.kind, repo.name, ns, cred.name
                )),
                // Don't let one unreadable Secret discard confirmed misses.
                Err(e) => unverifiable.push(format!("{}/{}: {e}", ns, cred.name)),
            }
        }
    }
    if missing.is_empty() && !unverifiable.is_empty() {
        return Outcome::Warn(format!(
            "could not verify {} secret(s): {}",
            unverifiable.len(),
            unverifiable.join("; ")
        ));
    }
    if missing.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail {
            what: format!("missing credential Secret(s): {}", missing.join("; ")),
            why: "movers load credentials via namespace-local envFrom; a missing Secret \
                  fails every run against that repository"
                .into(),
            fix: "create the Secret in the named namespace (or enable credentialProjection \
                  where supported)"
                .into(),
        }
    }
}

// --- the shared structural-gate core (pure) ---------------------------------

/// A live condition weighed against the shared registry.
#[derive(Debug)]
enum GateHit<'c> {
    /// The condition IS a registered gate: `type`, `status` AND `reason` match
    /// a row, so its severity and meaning are known.
    Known(&'static StructuralGate, &'c Condition),
    /// The condition's `type`+`status` is a gate polarity this build knows, but
    /// its `reason` matches no row — a gate a NEWER operator grew. Reported
    /// (naming the raw reason) rather than dropped: an unrecognized block is
    /// still a block, and the silence is the bug #359 was about.
    Unregistered(&'c Condition),
}

/// The most severe structural gate among `conditions` that applies to a kind,
/// per `covers` (one of the `GateScope::covers_*` classifiers).
///
/// Precedence, strictly: a registered `Fail` row, then a registered `Warn` row,
/// then an unregistered trip. Severity outranks position because
/// `status.conditions` is an arbitrarily-ordered array: without this, a
/// `RepositoryWritable=False` (Warn) listed before a `MoverPermitted=False`
/// (Fail) would silently downgrade the object's verdict — the same inversion
/// the unregistered-last ordering already guards against.
fn first_gate(conditions: &[Condition], covers: fn(GateScope) -> bool) -> Option<GateHit<'_>> {
    let mut warned: Option<GateHit<'_>> = None;
    let mut unregistered: Option<&Condition> = None;
    for cond in conditions {
        let scoped = || STRUCTURAL_GATES.iter().filter(|g| covers(g.applies_to));
        if let Some(gate) = scoped().find(|g| g.matches(&cond.type_, &cond.status, &cond.reason)) {
            match gate.severity {
                GateSeverity::Fail => return Some(GateHit::Known(gate, cond)),
                GateSeverity::Warn => warned.get_or_insert(GateHit::Known(gate, cond)),
            };
            continue;
        }
        if unregistered.is_none() && scoped().any(|g| g.trips(&cond.type_, &cond.status)) {
            unregistered = Some(cond);
        }
    }
    warned.or_else(|| unregistered.map(GateHit::Unregistered))
}

/// One line about a registered gate. The operator's condition message already
/// contains the exact fix command (`kubectl annotate namespace …`), so it is
/// quoted verbatim rather than paraphrased.
fn describe_gate(gate: &StructuralGate, cond: &Condition) -> String {
    format!(
        "blocked on {}={} ({}): {}",
        gate.condition, gate.blocked_status, gate.reason, cond.message
    )
}

/// One line about a gate whose `reason` this build has no row for.
fn describe_unregistered_gate(cond: &Condition) -> String {
    format!(
        "blocked on {}={} with reason `{}`, which this plugin does not know (the operator is \
         newer than the plugin): {}",
        cond.type_, cond.status, cond.reason, cond.message
    )
}

/// The phase facts the classifier needs, extracted per kind so one classifier
/// serves `Snapshot`, `Restore` and the phase-less `SnapshotSchedule`.
struct PhaseView<'a> {
    /// The phase string, for messages. `None` when the kind has no phase, or
    /// no status has been written yet.
    label: Option<&'a str>,
    /// Whether the operator considers the object finished
    /// (`SnapshotPhase::is_terminal` / `RestorePhase::is_terminal` — the single
    /// shared definition, so the CLI and the controller cannot disagree).
    terminal: bool,
    /// Whether the phase string is one this build cannot interpret.
    unknown: bool,
}

impl PhaseView<'_> {
    /// The view for a kind with no phase at all (`SnapshotSchedule`): never
    /// finished, never unreadable — only its gates matter.
    fn phaseless() -> Self {
        Self {
            label: None,
            terminal: false,
            unknown: false,
        }
    }
}

/// Why an object is worth surfacing. Ordered by how it is reported: gates
/// first (they never self-heal), then age, then version skew.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StuckKind {
    /// Parked on a registered structural gate. **Age-independent**: a gate is a
    /// permanent state, so waiting for `--stuck-threshold` before reporting it
    /// only delays the diagnosis (issue #359).
    Blocked {
        /// The registry row that identified it (its severity is the verdict).
        gate: &'static StructuralGate,
        /// The rendered [`describe_gate`] line — one renderer, so the CLI
        /// cannot describe the same gate two ways in two checks.
        detail: String,
    },
    /// Non-terminal for longer than `--stuck-threshold`.
    Overdue,
    /// A phase string this plugin cannot interpret — the operator is newer.
    UnknownPhase {
        /// The raw phase string, verbatim.
        phase: String,
    },
    /// A gated condition tripped with a `reason` no registry row covers.
    UnregisteredGate {
        /// The full description (condition, raw reason, message).
        detail: String,
    },
}

impl StuckKind {
    /// How loudly to report it. A registered gate defers to its row's severity;
    /// an overdue object or an unreadable phase is a Fail; an unknown reason
    /// warns, because this plugin cannot know that it blocks anything.
    fn severity(&self) -> GateSeverity {
        match self {
            Self::Blocked { gate, .. } => gate.severity,
            Self::Overdue | Self::UnknownPhase { .. } => GateSeverity::Fail,
            Self::UnregisteredGate { .. } => GateSeverity::Warn,
        }
    }

    /// Report order: blocked, then overdue, then version-skew.
    fn rank(&self) -> u8 {
        match self {
            Self::Blocked { .. } => 0,
            Self::Overdue => 1,
            Self::UnknownPhase { .. } => 2,
            Self::UnregisteredGate { .. } => 3,
        }
    }

    /// The clause appended after the object's name.
    fn detail(&self, threshold_label: &str) -> String {
        match self {
            Self::Blocked { detail, .. } => detail.clone(),
            Self::Overdue => format!("non-terminal for longer than {threshold_label}"),
            Self::UnknownPhase { phase } => {
                format!("phase `{phase}` is not one this plugin understands")
            }
            Self::UnregisteredGate { detail } => detail.clone(),
        }
    }

    /// Why this class of finding matters (one sentence per class, deduped).
    fn why(&self) -> &'static str {
        match self {
            Self::Blocked { .. } => {
                "a structural gate never self-heals — the operator has parked the object until a \
                 human makes an out-of-band change, so it will wait forever however new it is"
            }
            Self::Overdue => {
                "a Snapshot/Restore should reach a terminal phase; a long Pending/Running usually \
                 means an unschedulable mover pod, a missing PVC, or an unreachable backend"
            }
            Self::UnknownPhase { .. } | Self::UnregisteredGate { .. } => {
                "the operator wrote a phase/reason this plugin does not know, so this plugin \
                 cannot tell whether that work is progressing"
            }
        }
    }

    /// How to fix this class of finding.
    fn fix(&self) -> &'static str {
        match self {
            Self::Blocked { .. } => {
                "the condition message above is the operator's own diagnosis and carries the \
                 exact command to run; apply it and the object proceeds on its own"
            }
            Self::Overdue => {
                "kubectl kopiur logs snapshot|restore <name> and `kubectl describe` the object \
                 for its conditions/events; if this is a legitimately long run (e.g. a large \
                 initial backup), raise --stuck-threshold"
            }
            Self::UnknownPhase { .. } | Self::UnregisteredGate { .. } => UPGRADE_PLUGIN_FIX,
        }
    }
}

/// Classify one object. **Pure** — every IO-bound caller reduces to this.
///
/// Order is deliberate: an unreadable phase first (nothing else can be trusted
/// about the object), then a registered gate (age-independent), then the age
/// threshold, then an unregistered gate. Putting the unregistered gate LAST
/// keeps it from downgrading an object that is also overdue from Fail to Warn.
fn classify_stuck(
    phase: &PhaseView<'_>,
    conditions: &[Condition],
    covers: fn(GateScope) -> bool,
    age: Option<chrono::Duration>,
    threshold: chrono::Duration,
) -> Option<StuckKind> {
    if phase.unknown {
        return Some(StuckKind::UnknownPhase {
            phase: phase.label.unwrap_or("<unset>").to_string(),
        });
    }
    // Terminal work is finished: it is `RecentFailures`' business, not this
    // check's. A stale gate condition left on a completed object must not
    // resurrect it as "blocked".
    if phase.terminal {
        return None;
    }
    let gate = first_gate(conditions, covers);
    if let Some(GateHit::Known(gate, cond)) = gate {
        return Some(StuckKind::Blocked {
            gate,
            detail: describe_gate(gate, cond),
        });
    }
    if age.is_some_and(|a| a > threshold) {
        return Some(StuckKind::Overdue);
    }
    match gate {
        Some(GateHit::Unregistered(cond)) => Some(StuckKind::UnregisteredGate {
            detail: describe_unregistered_gate(cond),
        }),
        Some(GateHit::Known(..)) | None => None,
    }
}

/// How long an in-flight object has been waiting.
///
/// A `deletionTimestamp`, when present, is the anchor — NOT `creationTimestamp`.
/// A `Deleting` Snapshot is a finalizer reclaiming a kopia snapshot, and the
/// snapshots being reclaimed are the OLD ones: measuring a routine retention
/// prune of a 90-day-old Snapshot from its creation reports every prune wave as
/// stuck the instant it starts. Measured from the deletion request, only a
/// genuinely wedged finalizer trips the threshold.
fn stuck_age(meta: &kube::core::ObjectMeta, now: DateTime<Utc>) -> Option<chrono::Duration> {
    let t = meta
        .deletion_timestamp
        .as_ref()
        .or(meta.creation_timestamp.as_ref())?;
    let anchor = DateTime::from_timestamp(t.0.as_second(), 0)?;
    Some(now - anchor)
}

fn conditions_of(conditions: Option<&Vec<Condition>>) -> &[Condition] {
    conditions.map(Vec::as_slice).unwrap_or_default()
}

/// `classify_stuck` for a `Snapshot`. Pure.
fn snapshot_stuck(
    s: &Snapshot,
    now: DateTime<Utc>,
    threshold: chrono::Duration,
) -> Option<StuckKind> {
    let phase = s.status.as_ref().and_then(|st| st.phase.as_ref());
    let view = PhaseView {
        label: phase.map(PhaseLabel::label),
        terminal: phase.is_some_and(SnapshotPhase::is_terminal),
        unknown: phase.is_some_and(SnapshotPhase::is_unknown),
    };
    classify_stuck(
        &view,
        conditions_of(s.status.as_ref().map(|st| &st.conditions)),
        GateScope::covers_snapshot,
        stuck_age(&s.metadata, now),
        threshold,
    )
}

/// `classify_stuck` for a `Restore`. Pure.
fn restore_stuck(
    r: &Restore,
    now: DateTime<Utc>,
    threshold: chrono::Duration,
) -> Option<StuckKind> {
    let phase = r.status.as_ref().and_then(|st| st.phase.as_ref());
    let view = PhaseView {
        label: phase.map(PhaseLabel::label),
        terminal: phase.is_some_and(RestorePhase::is_terminal),
        unknown: phase.is_some_and(RestorePhase::is_unknown),
    };
    classify_stuck(
        &view,
        conditions_of(r.status.as_ref().map(|st| &st.conditions)),
        GateScope::covers_restore,
        stuck_age(&r.metadata, now),
        threshold,
    )
}

/// `classify_stuck` for a `SnapshotSchedule` — gates only. Pure.
///
/// A schedule has no phase and no age to be overdue against; what it can have
/// is `ScheduleRunnable=False`, which under the default
/// `concurrencyPolicy: Forbid` means NO further backups will run while every
/// object involved still looks healthy.
fn schedule_stuck(s: &SnapshotSchedule) -> Option<StuckKind> {
    classify_stuck(
        &PhaseView::phaseless(),
        conditions_of(s.status.as_ref().map(|st| &st.conditions)),
        GateScope::covers_snapshot_schedule,
        None,
        chrono::Duration::zero(),
    )
}

/// One classified object, ready to render.
struct StuckEntry {
    /// `snapshot media/nightly-1`.
    object: String,
    kind: StuckKind,
}

/// Render the classified objects into a check outcome. Pure.
fn stuck_outcome(entries: &mut [StuckEntry], threshold_label: &str) -> Outcome {
    entries.sort_by_key(|e| e.kind.rank());
    let (failing, warning): (Vec<&StuckEntry>, Vec<&StuckEntry>) = entries
        .iter()
        .partition(|e| e.kind.severity() == GateSeverity::Fail);
    let line = |e: &&StuckEntry| format!("{}: {}", e.object, e.kind.detail(threshold_label));
    if !failing.is_empty() {
        // Only the failing findings drive why/fix; a warn-level finding cannot
        // add a clause to a report it did not cause.
        let clauses = |f: fn(&StuckKind) -> &'static str| {
            let mut out: Vec<&'static str> = failing.iter().map(|e| f(&e.kind)).collect();
            out.dedup();
            out.join(" — also: ")
        };
        let mut what: Vec<String> = failing.iter().map(line).collect();
        what.extend(warning.iter().map(line));
        return Outcome::Fail {
            what: what.join("; "),
            why: clauses(StuckKind::why),
            fix: clauses(StuckKind::fix),
        };
    }
    if warning.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Warn(warning.iter().map(line).collect::<Vec<_>>().join("; "))
    }
}

/// The work objects both work-scoped checks read, listed once.
struct Work {
    snapshots: Vec<Snapshot>,
    restores: Vec<Restore>,
    schedules: Vec<SnapshotSchedule>,
    /// Kinds that could not be listed, as the outcome their error mapped to.
    ///
    /// Degradation is **per kind**, never all-or-nothing. A kubeconfig scoped
    /// to Snapshots and Restores but not SnapshotSchedules is ordinary, and
    /// discarding two successful listings because a third 403'd would report
    /// exit 0 with a blocked Snapshot sitting right there — the very failure
    /// mode #359 is about. What listed is still examined; what did not is named
    /// alongside the verdict.
    degraded: Vec<Outcome>,
}

async fn list_work(ctx: &KubeCtx) -> Work {
    async fn listed<K>(ctx: &KubeCtx, resource: &str, degraded: &mut Vec<Outcome>) -> Vec<K>
    where
        K: kube::Resource<Scope = kube::core::NamespaceResourceScope>
            + Clone
            + std::fmt::Debug
            + serde::de::DeserializeOwned,
        K::DynamicType: Default,
    {
        let api: Api<K> = match &ctx.scope {
            Scope::All => Api::all(ctx.client.clone()),
            Scope::Namespace(ns) => Api::namespaced(ctx.client.clone(), ns),
        };
        match api.list(&ListParams::default()).await {
            Ok(l) => l.items,
            Err(e) => {
                degraded.push(warn_for("list", resource, &e));
                Vec::new()
            }
        }
    }
    let mut degraded = Vec::new();
    Work {
        snapshots: listed::<Snapshot>(ctx, "snapshots", &mut degraded).await,
        restores: listed::<Restore>(ctx, "restores", &mut degraded).await,
        schedules: listed::<SnapshotSchedule>(ctx, "snapshotschedules", &mut degraded).await,
        degraded,
    }
}

/// The one-line summary of a degradation, whatever severity it carried.
fn degradation_note(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Warn(msg) => msg.clone(),
        Outcome::Fail { what, .. } => what.clone(),
        Outcome::Pass => String::new(),
    }
}

fn with_notes(head: &str, notes: &[String]) -> String {
    if head.is_empty() {
        return notes.join("; ");
    }
    format!("{head}; {}", notes.join("; "))
}

/// Fold per-kind listing degradations into a check's verdict. Pure.
///
/// A finding always survives: degradation is appended to a `Fail`'s `what`
/// rather than replacing it, so "a Snapshot is blocked AND schedules were
/// unreadable" reports both. A degradation that is itself fatal (a decode/skew
/// failure) escalates a Pass/Warn to `Fail`, because a kind nobody could read
/// is not a kind anybody verified.
fn merge_degradation(base: Outcome, degraded: &[Outcome]) -> Outcome {
    if degraded.is_empty() {
        return base;
    }
    let notes: Vec<String> = degraded.iter().map(degradation_note).collect();
    let fatal = degraded.iter().find_map(|o| match o {
        Outcome::Fail { why, fix, .. } => Some((why.clone(), fix.clone())),
        Outcome::Pass | Outcome::Warn(_) => None,
    });
    match (base, fatal) {
        (Outcome::Fail { what, why, fix }, _) => Outcome::Fail {
            what: with_notes(&what, &notes),
            why,
            fix,
        },
        (other, Some((why, fix))) => Outcome::Fail {
            what: with_notes(&degradation_note(&other), &notes),
            why,
            fix,
        },
        (other, None) => Outcome::Warn(with_notes(&degradation_note(&other), &notes)),
    }
}

/// `ns/name`, with the namespace omitted for cluster-scoped objects.
fn object_label(kind: &str, meta: &kube::core::ObjectMeta) -> String {
    format!(
        "{kind} {}/{}",
        meta.namespace.clone().unwrap_or_default(),
        meta.name.clone().unwrap_or_default()
    )
}

/// Blocked or stuck work across `Snapshot`, `Restore` and `SnapshotSchedule`.
fn check_stuck(work: &Work, threshold: std::time::Duration, now: DateTime<Utc>) -> Outcome {
    let threshold_label = format!("{}s", threshold.as_secs());
    let threshold = chrono::Duration::from_std(threshold).unwrap_or(chrono::Duration::hours(1));
    let mut entries = Vec::new();
    for s in &work.snapshots {
        if let Some(kind) = snapshot_stuck(s, now, threshold) {
            entries.push(StuckEntry {
                object: object_label("snapshot", &s.metadata),
                kind,
            });
        }
    }
    for r in &work.restores {
        if let Some(kind) = restore_stuck(r, now, threshold) {
            entries.push(StuckEntry {
                object: object_label("restore", &r.metadata),
                kind,
            });
        }
    }
    for s in &work.schedules {
        if let Some(kind) = schedule_stuck(s) {
            entries.push(StuckEntry {
                object: object_label("snapshotschedule", &s.metadata),
                kind,
            });
        }
    }
    stuck_outcome(&mut entries, &threshold_label)
}

// --- recent failures --------------------------------------------------------

/// One terminally-`Failed` object.
struct FailureEntry {
    /// `snapshot media/nightly-1`.
    object: String,
    /// When the failure was recorded; `None` when nothing dates it (treated as
    /// old, so an undatable failure can never flip doctor red forever).
    at: Option<DateTime<Utc>>,
    /// The operator's diagnosis: the registered gate that explains the failure
    /// when there is one, else the kstatus condition message.
    message: Option<String>,
    /// Set when a `Warn`-severity registry row explains this failure.
    ///
    /// The registry's severity is authoritative on BOTH paths, not just the
    /// in-flight one. `RepositoryWritable=False` is the case that matters: its
    /// writer sets `phase: Failed` in the same status patch, so a repository
    /// deliberately flipped to `mode: ReadOnly` for a migration produces one
    /// fresh `Failed` Snapshot per schedule slot. Windowing those as current
    /// problems would exit 1 for as long as the migration lasts — exactly the
    /// "a green/red verdict must not hinge on it" the row's own comment
    /// forbids. Explained failures are reported, never counted as red.
    explained_by_warn_gate: bool,
}

/// When a terminal object recorded its failure: the newest transition time
/// among the kstatus failure markers (`Stalled=True`, `Ready=False` — what
/// `io::set_ready` writes at the terminal transition), falling back to
/// `creationTimestamp`. There is no `completionTime` on these kinds.
fn failed_at(conditions: &[Condition], meta: &kube::core::ObjectMeta) -> Option<DateTime<Utc>> {
    let from_conditions = conditions
        .iter()
        .filter(|c| {
            (c.type_ == kopiur_api::consts::STALLED_CONDITION && c.status == "True")
                || (c.type_ == kopiur_api::consts::READY_CONDITION && c.status == "False")
        })
        .filter_map(|c| DateTime::from_timestamp(c.last_transition_time.0.as_second(), 0))
        .max();
    from_conditions.or_else(|| {
        meta.creation_timestamp
            .as_ref()
            .and_then(|t| DateTime::from_timestamp(t.0.as_second(), 0))
    })
}

/// The operator's failure diagnosis: the `Ready` condition message (which every
/// terminal write stamps), else the `Stalled` one.
fn failure_message(conditions: &[Condition]) -> Option<String> {
    let pick = |type_: &str| {
        conditions
            .iter()
            .find(|c| c.type_ == type_)
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    pick(kopiur_api::consts::READY_CONDITION)
        .or_else(|| pick(kopiur_api::consts::STALLED_CONDITION))
}

/// Join at most `cap` items, summarizing the rest.
fn join_capped(items: &[String], cap: usize) -> String {
    if items.len() <= cap {
        return items.join("; ");
    }
    format!("{}; (+{} more)", items[..cap].join("; "), items.len() - cap)
}

/// Terminal failures split by the `--failure-lookback` window. Pure.
///
/// The window exists because `failedJobsHistoryLimit` KEEPS Failed CRs by
/// design: an unbounded "any Failed object fails doctor" contract would leave
/// a healthy install permanently red because one backup failed last month.
/// Recent = a current problem (Fail); older = retained history (Warn).
fn failures_outcome(
    entries: &[FailureEntry],
    now: DateTime<Utc>,
    lookback: std::time::Duration,
) -> Outcome {
    let lookback_label = format!("{}s", lookback.as_secs());
    let window =
        chrono::Duration::from_std(lookback).unwrap_or_else(|_| chrono::Duration::hours(24));
    let cutoff = now - window;
    // A failure a Warn-severity gate explains never enters the window at all.
    let (explained, unexplained): (Vec<&FailureEntry>, Vec<&FailureEntry>) =
        entries.iter().partition(|e| e.explained_by_warn_gate);
    let (recent, older): (Vec<&&FailureEntry>, Vec<&&FailureEntry>) = unexplained
        .iter()
        .partition(|e| e.at.is_some_and(|t| t > cutoff));
    let line = |e: &FailureEntry| {
        format!(
            "{}{}{}",
            e.object,
            e.at.map(|t| format!(" at {}", t.to_rfc3339()))
                .unwrap_or_default(),
            e.message
                .as_deref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        )
    };
    let explained_lines: Vec<String> = explained.iter().map(|e| line(e)).collect();
    if !recent.is_empty() {
        let mut lines: Vec<String> = recent.iter().map(|e| line(e)).collect();
        let older_note = if older.is_empty() {
            String::new()
        } else {
            format!(
                " (plus {} older failure(s) retained as history)",
                older.len()
            )
        };
        let recent_count = recent.len();
        // Explained failures are listed alongside, never counted as red.
        lines.extend(explained_lines);
        return Outcome::Fail {
            what: format!(
                "{recent_count} failed in the last {lookback_label}: {}{older_note}",
                join_capped(&lines, 10)
            ),
            why: "a Failed Snapshot/Restore means a backup or restore did NOT happen; a failure \
                  inside the lookback window is a current problem, not retained history"
                .into(),
            fix: "kubectl kopiur logs snapshot|restore <name> for the mover output, and \
                  `kubectl describe` the object for its failure conditions; widen or narrow the \
                  window with --failure-lookback"
                .into(),
        };
    }
    if older.is_empty() && explained_lines.is_empty() {
        return Outcome::Pass;
    }
    let mut note = Vec::new();
    if !explained_lines.is_empty() {
        note.push(format!(
            "{} failure(s) explained by a deliberate configuration: {}",
            explained_lines.len(),
            join_capped(&explained_lines, 5)
        ));
    }
    if !older.is_empty() {
        let lines: Vec<String> = older.iter().map(|e| line(e)).collect();
        note.push(format!(
            "{} failed object(s) retained as history, none in the last {lookback_label}: {}",
            older.len(),
            join_capped(&lines, 5)
        ));
    }
    Outcome::Warn(note.join("; "))
}

/// Build one [`FailureEntry`], letting a `Warn`-severity registry row explain
/// (and de-escalate) the failure. Pure.
fn failure_entry(
    object: String,
    conditions: &[Condition],
    meta: &kube::core::ObjectMeta,
    covers: fn(GateScope) -> bool,
) -> FailureEntry {
    let warn_gate = match first_gate(conditions, covers) {
        Some(GateHit::Known(gate, cond)) if gate.severity == GateSeverity::Warn => {
            Some(describe_gate(gate, cond))
        }
        // A Fail-severity gate adds nothing here: the object is already red and
        // the window is the right instrument for it.
        Some(GateHit::Known(..) | GateHit::Unregistered(_)) | None => None,
    };
    FailureEntry {
        object,
        at: failed_at(conditions, meta),
        explained_by_warn_gate: warn_gate.is_some(),
        message: warn_gate.or_else(|| failure_message(conditions)),
    }
}

/// Terminally-`Failed` Snapshots/Restores, windowed by `--failure-lookback`.
fn check_recent_failures(
    work: &Work,
    lookback: std::time::Duration,
    now: DateTime<Utc>,
) -> Outcome {
    let mut entries = Vec::new();
    for s in &work.snapshots {
        if s.status.as_ref().and_then(|st| st.phase.as_ref()) == Some(&SnapshotPhase::Failed) {
            entries.push(failure_entry(
                object_label("snapshot", &s.metadata),
                conditions_of(s.status.as_ref().map(|st| &st.conditions)),
                &s.metadata,
                GateScope::covers_snapshot,
            ));
        }
    }
    for r in &work.restores {
        if r.status.as_ref().and_then(|st| st.phase.as_ref()) == Some(&RestorePhase::Failed) {
            entries.push(failure_entry(
                object_label("restore", &r.metadata),
                conditions_of(r.status.as_ref().map(|st| &st.conditions)),
                &r.metadata,
                GateScope::covers_restore,
            ));
        }
    }
    failures_outcome(&entries, now, lookback)
}

async fn check_warnings(ctx: &KubeCtx, now: DateTime<Utc>) -> Outcome {
    let api: Api<Event> = match &ctx.scope {
        Scope::All => Api::all(ctx.client.clone()),
        Scope::Namespace(ns) => Api::namespaced(ctx.client.clone(), ns),
    };
    let listed = match api
        .list(&ListParams::default().fields("type=Warning"))
        .await
    {
        Ok(l) => l,
        Err(e) => return warn_for("list", "events.events.k8s.io", &e),
    };
    let cutoff = now - chrono::Duration::hours(1);
    let mut recent: Vec<String> = listed
        .items
        .iter()
        .filter(|e| {
            e.regarding
                .as_ref()
                .and_then(|r| r.api_version.as_deref())
                .map(|v| v.starts_with(kopiur_api::GROUP))
                .unwrap_or(false)
        })
        .filter(|e| {
            let at = e
                .series
                .as_ref()
                .map(|s| s.last_observed_time.0)
                .or_else(|| e.event_time.as_ref().map(|t| t.0))
                // core/v1-emitted aggregated events: lastTimestamp tracks
                // recurrence; creationTimestamp is only the FIRST occurrence.
                .or_else(|| e.deprecated_last_timestamp.as_ref().map(|t| t.0))
                .or_else(|| e.metadata.creation_timestamp.as_ref().map(|t| t.0));
            at.and_then(|t| DateTime::from_timestamp(t.as_second(), 0))
                .is_some_and(|t| t > cutoff)
        })
        .map(|e| {
            format!(
                "{} {}/{}: {}",
                e.reason.clone().unwrap_or_default(),
                e.regarding
                    .as_ref()
                    .and_then(|r| r.namespace.clone())
                    .unwrap_or_default(),
                e.regarding
                    .as_ref()
                    .and_then(|r| r.name.clone())
                    .unwrap_or_default(),
                e.note.clone().unwrap_or_default()
            )
        })
        .collect();
    recent.sort();
    recent.dedup();
    if recent.is_empty() {
        Outcome::Pass
    } else {
        // Warnings are informational here — the specific checks above turn the
        // actionable ones into Fails; this is the catch-all surface.
        Outcome::Warn(format!(
            "{} warning(s) on kopiur objects in the last hour: {}",
            recent.len(),
            recent.join(" | ")
        ))
    }
}

/// Run all checks in order.
pub async fn run(
    ctx: &KubeCtx,
    args: &DoctorArgs,
    output: OutputFormat,
    now: DateTime<Utc>,
) -> Result<crate::CmdOutput, CliError> {
    let mut checks = Vec::new();
    checks.push(CheckResult {
        check: DoctorCheck::CrdsInstalled,
        outcome: check_crds(ctx).await,
    });
    let (controller, _) = check_deployment(ctx, "controller", true).await;
    checks.push(CheckResult {
        check: DoctorCheck::ControllerRunning,
        outcome: controller,
    });
    let (webhook, webhook_installed) = check_deployment(ctx, "webhook", false).await;
    checks.push(CheckResult {
        check: DoctorCheck::WebhookRunning,
        outcome: webhook,
    });
    checks.push(CheckResult {
        check: DoctorCheck::WebhookAdmits,
        outcome: check_webhook_admission(ctx, webhook_installed).await,
    });
    match list_repos(ctx).await {
        Ok(repos) => {
            checks.push(CheckResult {
                check: DoctorCheck::RepositoriesReady,
                outcome: check_repos_ready(&repos),
            });
            checks.push(CheckResult {
                check: DoctorCheck::CredentialsPresent,
                outcome: check_credentials(ctx, &repos).await,
            });
        }
        Err(warn) => {
            checks.push(CheckResult {
                check: DoctorCheck::RepositoriesReady,
                outcome: warn,
            });
            checks.push(CheckResult {
                check: DoctorCheck::CredentialsPresent,
                outcome: Outcome::Warn("skipped (repositories not listable)".into()),
            });
        }
    }
    // One listing pass feeds both work-scoped checks. A kind that could not be
    // listed degrades ONLY itself: whatever listed is still examined, and the
    // unreadable kind is named next to the verdict.
    let work = list_work(ctx).await;
    checks.push(CheckResult {
        check: DoctorCheck::NoStuckWork,
        outcome: merge_degradation(
            check_stuck(&work, args.stuck_threshold, now),
            &work.degraded,
        ),
    });
    checks.push(CheckResult {
        check: DoctorCheck::RecentFailures,
        outcome: merge_degradation(
            check_recent_failures(&work, args.failure_lookback, now),
            &work.degraded,
        ),
    });
    checks.push(CheckResult {
        check: DoctorCheck::RecentWarnings,
        outcome: check_warnings(ctx, now).await,
    });

    let report = DoctorReport { checks };
    let exit = report.exit_code();
    let text = match output {
        OutputFormat::Table | OutputFormat::Wide => render(&report),
        OutputFormat::Yaml => {
            let value = serde_json::to_value(&report).map_err(|e| CliError::Serialization {
                what: "doctor report",
                source: e.into(),
            })?;
            serde_yaml::to_string(&value).map_err(|e| CliError::Serialization {
                what: "doctor report",
                source: e.into(),
            })?
        }
        OutputFormat::Json => {
            let mut s =
                serde_json::to_string_pretty(&report).map_err(|e| CliError::Serialization {
                    what: "doctor report",
                    source: e.into(),
                })?;
            s.push('\n');
            s
        }
        OutputFormat::Name => {
            return Err(CliError::Serialization {
                what: "doctor report as -o name (doctor is a report, not a resource; use -o json)",
                source: Box::new(std::io::Error::other("unsupported output format")),
            });
        }
    };
    Ok(crate::CmdOutput { text, exit })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(outcomes: Vec<Outcome>) -> DoctorReport {
        DoctorReport {
            checks: outcomes
                .into_iter()
                .map(|outcome| CheckResult {
                    check: DoctorCheck::CrdsInstalled,
                    outcome,
                })
                .collect(),
        }
    }

    #[test]
    fn exit_code_is_one_iff_any_fail() {
        assert_eq!(report(vec![Outcome::Pass]).exit_code(), 0);
        assert_eq!(
            report(vec![Outcome::Pass, Outcome::Warn("x".into())]).exit_code(),
            0,
            "warnings must not fail the run"
        );
        assert_eq!(
            report(vec![
                Outcome::Pass,
                Outcome::Fail {
                    what: "w".into(),
                    why: "y".into(),
                    fix: "f".into()
                }
            ])
            .exit_code(),
            1
        );
    }

    #[test]
    fn render_carries_what_why_fix_for_failures() {
        let text = render(&report(vec![
            Outcome::Pass,
            Outcome::Warn("cannot list deployments (RBAC)".into()),
            Outcome::Fail {
                what: "missing CRD(s): snapshots.kopiur.home-operations.com".into(),
                why: "without the CRDs the API server rejects every kopiur object".into(),
                fix: "install kopiur".into(),
            },
        ]));
        assert!(text.contains("ok    CRDs installed"), "{text}");
        assert!(
            text.contains("warn  CRDs installed: cannot list deployments"),
            "{text}"
        );
        assert!(
            text.contains("FAIL  CRDs installed: missing CRD(s)"),
            "{text}"
        );
        assert!(text.contains("why: without the CRDs"), "{text}");
        assert!(text.contains("fix: install kopiur"), "{text}");
        assert!(
            text.contains("3 check(s): 1 failed, 1 warning(s)"),
            "{text}"
        );
    }

    #[test]
    fn rbac_misses_degrade_to_warn_with_the_grant_named() {
        let e = kube::Error::Api(
            kube::core::Status::failure("forbidden", "Forbidden")
                .with_code(403)
                .boxed(),
        );
        let Outcome::Warn(msg) = warn_for("list", "secrets", &e) else {
            panic!("403 must degrade to Warn");
        };
        assert!(msg.contains("grant `list` on `secrets`"), "{msg}");
    }

    #[test]
    fn expected_crds_covers_all_eight_kinds() {
        let names: Vec<String> = expected_crds().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names.len(), 8);
        for n in &names {
            assert!(n.ends_with(".kopiur.home-operations.com"), "{n}");
        }
    }

    #[test]
    fn an_undecodable_list_fails_instead_of_degrading_to_warn() {
        // Version skew: the whole kind vanished from every check that follows,
        // so "could not verify" + exit 0 would be a green report about a
        // cluster this plugin cannot read.
        let e = kube::Error::SerdeError(
            serde_json::from_str::<SnapshotPhase>("not-json").expect_err("decode error"),
        );
        let Outcome::Fail { what, fix, .. } = warn_for("list", "snapshots", &e) else {
            panic!("a decode failure must be a Fail");
        };
        assert!(what.contains("cannot decode the snapshots"), "{what}");
        assert!(fix.contains("upgrade the plugin"), "{fix}");
    }

    // --- fixtures ------------------------------------------------------------

    const NOW: &str = "2026-06-11T12:00:00Z";

    fn now() -> DateTime<Utc> {
        NOW.parse().expect("fixed clock")
    }

    fn ago(minutes: i64) -> String {
        (now() - chrono::Duration::minutes(minutes)).to_rfc3339()
    }

    fn hour() -> chrono::Duration {
        chrono::Duration::hours(1)
    }

    /// A condition as the operator writes it (JSON, so the decode path is the
    /// cluster's).
    fn condition(type_: &str, status: &str, reason: &str, message: &str) -> serde_json::Value {
        serde_json::json!({
            "type": type_,
            "status": status,
            "reason": reason,
            "message": message,
            "lastTransitionTime": ago(5),
        })
    }

    fn snapshot(meta: serde_json::Value, status: serde_json::Value) -> Snapshot {
        serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Snapshot",
            "metadata": meta,
            "spec": {},
            "status": status,
        }))
        .expect("snapshot fixture")
    }

    /// A Snapshot created `age_minutes` ago, at `phase`, with `conditions`.
    fn snap_at(phase: &str, age_minutes: i64, conditions: Vec<serde_json::Value>) -> Snapshot {
        snapshot(
            serde_json::json!({
                "name": "nightly-1",
                "namespace": "media",
                "creationTimestamp": ago(age_minutes),
            }),
            serde_json::json!({ "phase": phase, "conditions": conditions }),
        )
    }

    fn restore_at(phase: &str, age_minutes: i64, conditions: Vec<serde_json::Value>) -> Restore {
        serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Restore",
            "metadata": {
                "name": "r1",
                "namespace": "media",
                "creationTimestamp": ago(age_minutes),
            },
            "spec": {
                "source": { "snapshotRef": { "name": "nightly-1" } },
                "target": { "pvcRef": { "name": "data" } }
            },
            "status": { "phase": phase, "conditions": conditions },
        }))
        .expect("restore fixture")
    }

    fn schedule_with(conditions: Vec<serde_json::Value>) -> SnapshotSchedule {
        serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "SnapshotSchedule",
            "metadata": { "name": "nightly", "namespace": "media" },
            "spec": { "policyRef": { "name": "nightly" }, "schedule": { "cron": "0 2 * * *" } },
            "status": { "conditions": conditions },
        }))
        .expect("schedule fixture")
    }

    fn repo_with(phase: &str, conditions: Vec<serde_json::Value>) -> RepoSummary {
        let repo: Repository = serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Repository",
            "metadata": { "name": "nas", "namespace": "media" },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "creds", "key": "KOPIA_PASSWORD" } }
            },
            "status": { "phase": phase, "conditions": conditions },
        }))
        .expect("repository fixture");
        summarize_repository(&repo)
    }

    /// The privileged-mover refusal, verbatim in shape: the operator's message
    /// carries the fix command, which doctor must quote.
    fn mover_blocked_condition() -> serde_json::Value {
        condition(
            kopiur_api::consts::MOVER_PERMITTED_CONDITION,
            "False",
            kopiur_api::consts::PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
            "the mover needs elevated privileges; run: kubectl annotate namespace media \
             kopiur.home-operations.com/privileged-movers=allow",
        )
    }

    fn work(snapshots: Vec<Snapshot>, restores: Vec<Restore>) -> Work {
        Work {
            snapshots,
            restores,
            schedules: Vec::new(),
            degraded: Vec::new(),
        }
    }

    fn fail_parts(outcome: &Outcome) -> (String, String, String) {
        match outcome {
            Outcome::Fail { what, why, fix } => (what.clone(), why.clone(), fix.clone()),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    // --- the #359 regressions -----------------------------------------------

    #[test]
    fn blocked_snapshot_fails_immediately_under_threshold() {
        // THE issue: a Snapshot parked on `MoverPermitted=False` one minute ago.
        // The pre-fix check looked only at phase + a 1h age, so this reported
        // all-green for an hour — of an outage that never self-heals.
        let w = work(
            vec![snap_at("Pending", 1, vec![mover_blocked_condition()])],
            vec![],
        );
        let outcome = check_stuck(&w, std::time::Duration::from_secs(3600), now());
        let (what, why, fix) = fail_parts(&outcome);
        assert!(what.contains("snapshot media/nightly-1"), "{what}");
        assert!(what.contains("MoverPermitted=False"), "{what}");
        assert!(
            what.contains("PrivilegedMoverNotPermitted"),
            "the registry reason must be named: {what}"
        );
        assert!(
            what.contains("kubectl annotate namespace media"),
            "the operator's fix command must be quoted verbatim: {what}"
        );
        assert!(why.contains("never self-heals"), "{why}");
        assert!(fix.contains("operator's own diagnosis"), "{fix}");
    }

    #[test]
    fn blocked_restore_mirrors_the_snapshot_gate() {
        let w = work(
            vec![],
            vec![restore_at("Pending", 1, vec![mover_blocked_condition()])],
        );
        let (what, ..) = fail_parts(&check_stuck(
            &w,
            std::time::Duration::from_secs(3600),
            now(),
        ));
        assert!(what.contains("restore media/r1"), "{what}");
        assert!(what.contains("MoverPermitted=False"), "{what}");
    }

    #[test]
    fn deleting_past_threshold_measured_from_deletion_timestamp() {
        // A wedged finalizer: deleted 3h ago, still Deleting, no gate condition.
        // The clock runs from the DELETION request.
        let s = snapshot(
            serde_json::json!({
                "name": "nightly-1",
                "namespace": "media",
                "creationTimestamp": ago(60 * 24 * 90),
                "deletionTimestamp": ago(180),
            }),
            serde_json::json!({ "phase": "Deleting" }),
        );
        let (what, ..) = fail_parts(&check_stuck(
            &work(vec![s], vec![]),
            std::time::Duration::from_secs(3600),
            now(),
        ));
        assert!(what.contains("non-terminal for longer than"), "{what}");
    }

    #[test]
    fn routine_prune_of_old_snapshot_is_not_stuck() {
        // The false-positive the deletionTimestamp anchor exists to prevent: a
        // 90-day-old Snapshot deleted 60 seconds ago by a retention prune. Its
        // CREATION age is enormous; its deletion is seconds old.
        let s = snapshot(
            serde_json::json!({
                "name": "nightly-1",
                "namespace": "media",
                "creationTimestamp": ago(60 * 24 * 90),
                "deletionTimestamp": ago(1),
            }),
            serde_json::json!({ "phase": "Deleting" }),
        );
        assert!(matches!(
            check_stuck(
                &work(vec![s], vec![]),
                std::time::Duration::from_secs(3600),
                now()
            ),
            Outcome::Pass
        ));
    }

    #[test]
    fn deletion_held_snapshot_is_blocked_not_overdue() {
        // The mass-deletion breaker holds the finalizer: age-independent, and
        // reported as the GATE (which names the ack command), not as "old".
        let s = snapshot(
            serde_json::json!({
                "name": "nightly-1",
                "namespace": "media",
                "creationTimestamp": ago(60 * 24 * 90),
                "deletionTimestamp": ago(1),
            }),
            serde_json::json!({
                "phase": "Deleting",
                "conditions": [condition(
                    kopiur_api::consts::DELETION_HELD_CONDITION,
                    "True",
                    kopiur_api::consts::MASS_DELETION_BREAKER_REASON,
                    "held pending acknowledgement; run: kubectl annotate repository nas …",
                )],
            }),
        );
        let (what, ..) = fail_parts(&check_stuck(
            &work(vec![s], vec![]),
            std::time::Duration::from_secs(3600),
            now(),
        ));
        assert!(what.contains("DeletionHeld=True"), "{what}");
        assert!(
            !what.contains("non-terminal for longer than"),
            "a held deletion is blocked, not overdue: {what}"
        );
    }

    #[test]
    fn terminal_phases_are_never_stuck() {
        // Every terminal phase, even ancient, even carrying a stale gate
        // condition: finished work is `RecentFailures`' business.
        for phase in SnapshotPhase::ALL.iter().filter(|p| p.is_terminal()) {
            let s = snap_at(phase.label(), 60 * 24 * 30, vec![mover_blocked_condition()]);
            assert_eq!(
                snapshot_stuck(&s, now(), hour()),
                None,
                "terminal SnapshotPhase::{phase:?} must never be stuck"
            );
        }
        for phase in RestorePhase::ALL.iter().filter(|p| p.is_terminal()) {
            let r = restore_at(phase.label(), 60 * 24 * 30, vec![mover_blocked_condition()]);
            assert_eq!(
                restore_stuck(&r, now(), hour()),
                None,
                "terminal RestorePhase::{phase:?} must never be stuck"
            );
        }
        // …and every non-terminal one IS reported once it is old enough, so
        // this test cannot pass by classifying everything as finished.
        for phase in SnapshotPhase::ALL.iter().filter(|p| !p.is_terminal()) {
            let s = snap_at(phase.label(), 180, vec![]);
            assert_eq!(
                snapshot_stuck(&s, now(), hour()),
                Some(StuckKind::Overdue),
                "in-flight SnapshotPhase::{phase:?} past the threshold must be Overdue"
            );
        }
    }

    #[test]
    fn unknown_phase_fails_with_upgrade_hint() {
        // A phase written by a NEWER operator. It decodes (M2's `Unknown`
        // fallback) instead of poisoning the list, and doctor says so.
        let s = snap_at("Quiescing", 1, vec![]);
        let (what, _, fix) = fail_parts(&check_stuck(
            &work(vec![s], vec![]),
            std::time::Duration::from_secs(3600),
            now(),
        ));
        assert!(what.contains("`Quiescing`"), "{what}");
        assert!(fix.contains("upgrade the plugin"), "{fix}");
    }

    #[test]
    fn fresh_pending_work_is_not_stuck() {
        let w = work(
            vec![snap_at("Pending", 1, vec![])],
            vec![restore_at("Restoring", 1, vec![])],
        );
        assert!(matches!(
            check_stuck(&w, std::time::Duration::from_secs(3600), now()),
            Outcome::Pass
        ));
    }

    #[test]
    fn pending_past_threshold_is_overdue() {
        let w = work(vec![snap_at("Pending", 180, vec![])], vec![]);
        let (what, why, fix) = fail_parts(&check_stuck(
            &w,
            std::time::Duration::from_secs(3600),
            now(),
        ));
        assert!(
            what.contains("non-terminal for longer than 3600s"),
            "{what}"
        );
        assert!(why.contains("unschedulable mover pod"), "{why}");
        assert!(fix.contains("--stuck-threshold"), "{fix}");
    }

    #[test]
    fn unrelated_false_conditions_do_not_block() {
        // Only registry conditions gate. A `Ready=False` mid-reconcile, or a
        // health condition, must not make a healthy young object "blocked".
        let s = snap_at(
            "Running",
            1,
            vec![
                condition("Ready", "False", "Running", "the mover Job is running"),
                condition(
                    kopiur_api::consts::INDEX_BLOB_HEALTH_CONDITION,
                    "False",
                    "TooManyIndexBlobs",
                    "index blob count is high",
                ),
                // A gate row that belongs to a DIFFERENT kind: the scope filter
                // must reject it on a Snapshot.
                condition(
                    kopiur_api::consts::SCHEDULE_RUNNABLE_CONDITION,
                    "False",
                    kopiur_api::consts::BLOCKED_ON_UNREADABLE_RUN_REASON,
                    "not a Snapshot-scoped gate",
                ),
            ],
        );
        assert_eq!(snapshot_stuck(&s, now(), hour()), None);
    }

    #[test]
    fn readonly_repository_gate_warns_and_does_not_fail_the_run() {
        // A `Warn` registry row must not turn doctor red: a ReadOnly repository
        // is a legitimate deliberate configuration.
        let s = snap_at(
            "Pending",
            1,
            vec![condition(
                kopiur_api::consts::REPOSITORY_WRITABLE_CONDITION,
                "False",
                kopiur_api::consts::REPOSITORY_READ_ONLY_REASON,
                "repository nas is ReadOnly; backups are refused",
            )],
        );
        let outcome = check_stuck(
            &work(vec![s], vec![]),
            std::time::Duration::from_secs(3600),
            now(),
        );
        let Outcome::Warn(msg) = &outcome else {
            panic!("a Warn-severity gate must warn, not fail: {outcome:?}");
        };
        assert!(msg.contains("RepositoryWritable=False"), "{msg}");
        assert_eq!(report(vec![outcome]).exit_code(), 0);
    }

    #[test]
    fn an_unregistered_gate_reason_warns_naming_the_raw_reason() {
        // A gated condition tripped with a reason no row covers: a gate a newer
        // operator grew. Surfaced (the coarse `trips` filter) rather than
        // silently dropped, but only as a Warn — this build cannot know it.
        let s = snap_at(
            "Pending",
            1,
            vec![condition(
                kopiur_api::consts::MOVER_PERMITTED_CONDITION,
                "False",
                "SomeFutureRefusal",
                "refused for a reason from the future",
            )],
        );
        let Outcome::Warn(msg) = check_stuck(
            &work(vec![s], vec![]),
            std::time::Duration::from_secs(3600),
            now(),
        ) else {
            panic!("an unregistered reason must warn");
        };
        assert!(msg.contains("SomeFutureRefusal"), "{msg}");
        assert!(msg.contains("newer than the plugin"), "{msg}");
    }

    #[test]
    fn an_unregistered_gate_never_downgrades_an_overdue_object() {
        // Ordering guard: the Warn-level skew signal must not mask the Fail an
        // old object earns on its own.
        let s = snap_at(
            "Pending",
            180,
            vec![condition(
                kopiur_api::consts::MOVER_PERMITTED_CONDITION,
                "False",
                "SomeFutureRefusal",
                "refused for a reason from the future",
            )],
        );
        assert_eq!(snapshot_stuck(&s, now(), hour()), Some(StuckKind::Overdue));
    }

    #[test]
    fn a_blocked_schedule_fails_and_names_the_blocking_snapshot() {
        // #359 one kind removed: under `concurrencyPolicy: Forbid` no further
        // backups run, while every object involved looks healthy.
        let w = Work {
            snapshots: vec![],
            restores: vec![],
            degraded: vec![],
            schedules: vec![schedule_with(vec![condition(
                kopiur_api::consts::SCHEDULE_RUNNABLE_CONDITION,
                "False",
                kopiur_api::consts::BLOCKED_ON_UNREADABLE_RUN_REASON,
                "Snapshot `nightly-7` holds this schedule's concurrency gate at phase `Quiescing`",
            )])],
        };
        let (what, ..) = fail_parts(&check_stuck(
            &w,
            std::time::Duration::from_secs(3600),
            now(),
        ));
        assert!(what.contains("snapshotschedule media/nightly"), "{what}");
        assert!(what.contains("ScheduleRunnable=False"), "{what}");
        assert!(what.contains("nightly-7"), "{what}");
    }

    #[test]
    fn a_healthy_schedule_is_not_reported() {
        assert_eq!(
            schedule_stuck(&schedule_with(vec![condition(
                kopiur_api::consts::SCHEDULE_RUNNABLE_CONDITION,
                "True",
                "Runnable",
                "the schedule's concurrency gate is clear",
            )])),
            None
        );
    }

    // --- repositories --------------------------------------------------------

    #[test]
    fn a_held_mass_deletion_breaker_fails_the_repository_check() {
        // Phase-green but blocked: the repository is Ready while a whole
        // deletion wave is frozen awaiting acknowledgement.
        let repo = repo_with(
            "Ready",
            vec![condition(
                kopiur_api::consts::MASS_DELETION_HELD_CONDITION,
                "True",
                kopiur_api::consts::MASS_DELETION_THRESHOLD_EXCEEDED_REASON,
                "12 pending deletions are at/above the threshold of 10; run: kubectl annotate …",
            )],
        );
        let (what, ..) = fail_parts(&check_repos_ready(&[repo]));
        assert!(what.contains("Repository/nas"), "{what}");
        assert!(what.contains("MassDeletionHeld=True"), "{what}");
        assert!(what.contains("kubectl annotate"), "{what}");
    }

    #[test]
    fn a_ready_repository_with_no_gates_passes() {
        assert!(matches!(
            check_repos_ready(&[repo_with(
                "Ready",
                vec![condition("Ready", "True", "Connected", "connected")]
            )]),
            Outcome::Pass
        ));
    }

    #[test]
    fn an_unregistered_repository_gate_reason_warns() {
        let repo = repo_with(
            "Ready",
            vec![condition(
                kopiur_api::consts::MASS_DELETION_HELD_CONDITION,
                "True",
                "SomeFutureHold",
                "held for a reason from the future",
            )],
        );
        let Outcome::Warn(msg) = check_repos_ready(&[repo]) else {
            panic!("an unregistered repository gate must warn");
        };
        assert!(msg.contains("SomeFutureHold"), "{msg}");
        // Naming the skew without naming the remedy is half a report: this Warn
        // carries no `fix:` line of its own, so the upgrade hint has to be in it.
        assert!(msg.contains(UPGRADE_PLUGIN_FIX), "{msg}");
    }

    #[test]
    fn an_unready_repository_still_fails_with_its_ready_message() {
        let repo = repo_with(
            "Degraded",
            vec![condition(
                "Ready",
                "False",
                "ConnectFailed",
                "credentials rejected",
            )],
        );
        let (what, ..) = fail_parts(&check_repos_ready(&[repo]));
        assert!(what.contains("Repository/nas (Degraded"), "{what}");
        assert!(what.contains("credentials rejected"), "{what}");
    }

    // --- recent failures -----------------------------------------------------

    /// A `Failed` object whose kstatus failure markers transitioned `n` minutes
    /// ago (what `io::set_ready` writes at the terminal transition).
    fn failed_snapshot(name: &str, transition_minutes_ago: i64) -> Snapshot {
        let at = (now() - chrono::Duration::minutes(transition_minutes_ago)).to_rfc3339();
        snapshot(
            serde_json::json!({
                "name": name,
                "namespace": "media",
                "creationTimestamp": ago(transition_minutes_ago + 5),
            }),
            serde_json::json!({
                "phase": "Failed",
                "conditions": [
                    { "type": "Ready", "status": "False", "reason": "MoverJobFailed",
                      "message": "the mover Job failed: kopia could not connect",
                      "lastTransitionTime": at },
                    { "type": "Stalled", "status": "True", "reason": "MoverJobFailed",
                      "message": "the mover Job failed: kopia could not connect",
                      "lastTransitionTime": at },
                ],
            }),
        )
    }

    #[test]
    fn recent_failed_snapshot_fails_doctor() {
        let w = work(vec![failed_snapshot("nightly-1", 30)], vec![]);
        let (what, why, fix) = fail_parts(&check_recent_failures(
            &w,
            std::time::Duration::from_secs(24 * 3600),
            now(),
        ));
        assert!(what.contains("snapshot media/nightly-1"), "{what}");
        assert!(what.contains("kopia could not connect"), "{what}");
        assert!(why.contains("did NOT happen"), "{why}");
        assert!(fix.contains("kubectl kopiur logs"), "{fix}");
    }

    #[test]
    fn old_failed_snapshot_warns_not_fails() {
        // Retained history (`failedJobsHistoryLimit` keeps these by design)
        // must not leave a healthy install permanently red.
        let w = work(vec![failed_snapshot("nightly-1", 60 * 24 * 7)], vec![]);
        let outcome = check_recent_failures(&w, std::time::Duration::from_secs(24 * 3600), now());
        let Outcome::Warn(msg) = &outcome else {
            panic!("an old failure must warn: {outcome:?}");
        };
        assert!(msg.contains("retained as history"), "{msg}");
        assert_eq!(report(vec![outcome]).exit_code(), 0);
    }

    #[test]
    fn a_recent_failure_reports_the_older_ones_alongside_it() {
        let w = work(
            vec![
                failed_snapshot("nightly-1", 30),
                failed_snapshot("nightly-0", 60 * 24 * 7),
            ],
            vec![],
        );
        let (what, ..) = fail_parts(&check_recent_failures(
            &w,
            std::time::Duration::from_secs(24 * 3600),
            now(),
        ));
        assert!(what.contains("nightly-1"), "{what}");
        assert!(what.contains("plus 1 older failure(s)"), "{what}");
    }

    #[test]
    fn succeeded_work_is_not_a_failure() {
        let w = work(vec![snap_at("Succeeded", 30, vec![])], vec![]);
        assert!(matches!(
            check_recent_failures(&w, std::time::Duration::from_secs(24 * 3600), now()),
            Outcome::Pass
        ));
    }

    #[test]
    fn a_failure_with_no_dating_information_warns_rather_than_failing_forever() {
        let s = serde_json::from_value::<Snapshot>(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Snapshot",
            "metadata": { "name": "orphan", "namespace": "media" },
            "spec": {},
            "status": { "phase": "Failed" },
        }))
        .expect("snapshot fixture");
        assert!(matches!(
            check_recent_failures(
                &work(vec![s], vec![]),
                std::time::Duration::from_secs(24 * 3600),
                now()
            ),
            Outcome::Warn(_)
        ));
    }

    #[test]
    fn a_readonly_repository_failure_warns_instead_of_failing_doctor() {
        // The registry's severity is authoritative on the TERMINAL path too.
        // The ReadOnly writer sets `phase: Failed` in the same status patch, so
        // a repository flipped to `mode: ReadOnly` for a migration produces a
        // fresh Failed Snapshot every schedule slot. Windowing those as current
        // problems would exit 1 for the whole migration — which the row's own
        // "a green/red verdict must not hinge on it" forbids.
        let at = (now() - chrono::Duration::minutes(5)).to_rfc3339();
        let s = snapshot(
            serde_json::json!({
                "name": "nightly-1",
                "namespace": "media",
                "creationTimestamp": ago(10),
            }),
            serde_json::json!({
                "phase": "Failed",
                "conditions": [
                    { "type": "Ready", "status": "False", "reason": "RepositoryReadOnly",
                      "message": "refused", "lastTransitionTime": at },
                    { "type": kopiur_api::consts::REPOSITORY_WRITABLE_CONDITION,
                      "status": "False",
                      "reason": kopiur_api::consts::REPOSITORY_READ_ONLY_REASON,
                      "message": "repository nas is ReadOnly; backups are refused",
                      "lastTransitionTime": at },
                ],
            }),
        );
        let outcome = check_recent_failures(
            &work(vec![s], vec![]),
            std::time::Duration::from_secs(24 * 3600),
            now(),
        );
        let Outcome::Warn(msg) = &outcome else {
            panic!("a Warn-severity gate must explain, not fail: {outcome:?}");
        };
        assert!(
            msg.contains("explained by a deliberate configuration"),
            "{msg}"
        );
        assert!(msg.contains("RepositoryWritable=False"), "{msg}");
        assert!(msg.contains("repository nas is ReadOnly"), "{msg}");
        assert_eq!(report(vec![outcome]).exit_code(), 0);
    }

    #[test]
    fn a_gate_explained_failure_is_listed_but_never_counted_as_recent() {
        // A genuine recent failure still fails — and the explained one is
        // listed alongside it, not swallowed and not added to the count.
        let at = (now() - chrono::Duration::minutes(5)).to_rfc3339();
        let readonly = snapshot(
            serde_json::json!({ "name": "ro-1", "namespace": "media", "creationTimestamp": ago(10) }),
            serde_json::json!({
                "phase": "Failed",
                "conditions": [
                    { "type": kopiur_api::consts::REPOSITORY_WRITABLE_CONDITION,
                      "status": "False",
                      "reason": kopiur_api::consts::REPOSITORY_READ_ONLY_REASON,
                      "message": "repository nas is ReadOnly", "lastTransitionTime": at },
                ],
            }),
        );
        let w = work(vec![readonly, failed_snapshot("nightly-1", 30)], vec![]);
        let (what, ..) = fail_parts(&check_recent_failures(
            &w,
            std::time::Duration::from_secs(24 * 3600),
            now(),
        ));
        assert!(what.starts_with("1 failed in the last"), "{what}");
        assert!(what.contains("nightly-1"), "{what}");
        assert!(what.contains("ro-1"), "{what}");
    }

    // --- per-kind list degradation ------------------------------------------

    fn forbidden(resource: &str) -> Outcome {
        warn_for(
            "list",
            resource,
            &kube::Error::Api(
                kube::core::Status::failure("forbidden", "Forbidden")
                    .with_code(403)
                    .boxed(),
            ),
        )
    }

    #[test]
    fn one_unlistable_kind_does_not_hide_a_blocked_object_in_another() {
        // A kubeconfig without `list snapshotschedules` is ordinary. Discarding
        // the Snapshots that DID list would exit 0 with a wedged Snapshot right
        // there — the failure mode under repair, one kind over.
        let mut w = work(
            vec![snap_at("Pending", 1, vec![mover_blocked_condition()])],
            vec![],
        );
        w.degraded.push(forbidden("snapshotschedules"));
        let (what, ..) = fail_parts(&merge_degradation(
            check_stuck(&w, std::time::Duration::from_secs(3600), now()),
            &w.degraded,
        ));
        assert!(what.contains("snapshot media/nightly-1"), "{what}");
        assert!(what.contains("MoverPermitted=False"), "{what}");
        assert!(
            what.contains("grant `list` on `snapshotschedules`"),
            "the degradation must be named alongside the finding: {what}"
        );
    }

    #[test]
    fn a_degraded_listing_with_no_findings_warns() {
        let mut w = work(vec![snap_at("Pending", 1, vec![])], vec![]);
        w.degraded.push(forbidden("snapshotschedules"));
        let Outcome::Warn(msg) = merge_degradation(
            check_stuck(&w, std::time::Duration::from_secs(3600), now()),
            &w.degraded,
        ) else {
            panic!("an RBAC gap with nothing else found must warn");
        };
        assert!(msg.contains("snapshotschedules"), "{msg}");
    }

    #[test]
    fn an_undecodable_kind_escalates_a_clean_check_to_fail() {
        // A kind nobody could decode is not a kind anybody verified.
        let mut w = work(vec![], vec![]);
        w.degraded.push(warn_for(
            "list",
            "snapshots",
            &kube::Error::SerdeError(
                serde_json::from_str::<SnapshotPhase>("not-json").expect_err("decode error"),
            ),
        ));
        let (what, _, fix) = fail_parts(&merge_degradation(
            check_stuck(&w, std::time::Duration::from_secs(3600), now()),
            &w.degraded,
        ));
        assert!(what.contains("cannot decode the snapshots"), "{what}");
        assert!(fix.contains("upgrade the plugin"), "{fix}");
    }

    #[test]
    fn a_fail_severity_gate_outranks_a_warn_one_whatever_the_condition_order() {
        // `status.conditions` is an arbitrarily-ordered array: position must
        // not decide the verdict.
        let readonly = condition(
            kopiur_api::consts::REPOSITORY_WRITABLE_CONDITION,
            "False",
            kopiur_api::consts::REPOSITORY_READ_ONLY_REASON,
            "repository nas is ReadOnly",
        );
        for conditions in [
            vec![readonly.clone(), mover_blocked_condition()],
            vec![mover_blocked_condition(), readonly],
        ] {
            let s = snap_at("Pending", 1, conditions);
            let kind = snapshot_stuck(&s, now(), hour()).expect("blocked");
            assert_eq!(kind.severity(), GateSeverity::Fail, "{kind:?}");
            assert!(
                kind.detail("1h").contains("MoverPermitted=False"),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_failing_repository_still_lists_its_warn_level_findings() {
        let unready = repo_with(
            "Degraded",
            vec![condition(
                "Ready",
                "False",
                "ConnectFailed",
                "credentials rejected",
            )],
        );
        let mut skewed = repo_with(
            "Ready",
            vec![condition(
                kopiur_api::consts::MASS_DELETION_HELD_CONDITION,
                "True",
                "SomeFutureHold",
                "held for a reason from the future",
            )],
        );
        skewed.name = "archive".into();
        let (what, ..) = fail_parts(&check_repos_ready(&[unready, skewed]));
        assert!(what.contains("credentials rejected"), "{what}");
        assert!(
            what.contains("SomeFutureHold"),
            "a Fail must not swallow the warn-level lines: {what}"
        );
    }

    #[test]
    fn every_doctor_check_has_a_distinct_title() {
        let checks = [
            DoctorCheck::CrdsInstalled,
            DoctorCheck::ControllerRunning,
            DoctorCheck::WebhookRunning,
            DoctorCheck::WebhookAdmits,
            DoctorCheck::RepositoriesReady,
            DoctorCheck::CredentialsPresent,
            DoctorCheck::NoStuckWork,
            DoctorCheck::RecentFailures,
            DoctorCheck::RecentWarnings,
        ];
        let titles: std::collections::BTreeSet<&str> = checks.iter().map(|c| c.title()).collect();
        assert_eq!(titles.len(), checks.len(), "doctor runs 9 distinct checks");
    }
}
