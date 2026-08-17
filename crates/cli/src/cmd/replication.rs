//! `kubectl kopiur replication run` — trigger an out-of-band replication run
//! by stamping the `run-requested` annotation; the operator routes it through
//! the SAME mover/gate/single-flight path as the cron slots and answers in
//! `status.manualRun`.
//!
//! The command covers BOTH replication kinds (`RepositoryReplication`,
//! `SnapshotReplication`) behind one verb, because "run my replication now" is
//! one intent — the kind is a detail of which object holds the schedule. It is
//! auto-detected from the name when unambiguous, and `--kind` settles the case
//! where a namespace has one of each under the same name.

use chrono::{DateTime, SecondsFormat, Utc};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kopiur_api::common::{ReplicationManualRunPhase, ReplicationManualRunStatus};
use kopiur_api::consts::RUN_REQUESTED_ANNOTATION;
use kopiur_api::{RepositoryReplication, SnapshotReplication};
use kube::api::{Api, Patch, PatchParams};
use serde::de::DeserializeOwned;

use crate::CmdOutput;
use crate::cli::{ReplicationKindArg, ReplicationRunArgs};
use crate::context::KubeCtx;
use crate::error::{CliError, classify_kube};
use crate::wait::{DEFAULT_WAIT_TIMEOUT, wait_for};

/// The two kinds `replication run` can target, behind one interface: naming for
/// messages plus the `status.manualRun`/`status.conditions` reads the command
/// needs. Implemented for exactly the two replication CRs, so a third kind
/// cannot be targeted without stating its naming here.
pub trait ReplicationTarget:
    kube::Resource<Scope = kube::core::NamespaceResourceScope, DynamicType = ()>
    + Clone
    + std::fmt::Debug
    + DeserializeOwned
    + Send
    + 'static
{
    /// CamelCase kind, for messages.
    const KIND: &'static str;
    /// Lowercase singular, for `<singular>.<group>/<name>` output.
    const SINGULAR: &'static str;
    /// Lowercase plural, for RBAC hints and `kubectl get` remediation.
    const PLURAL: &'static str;

    /// `status.manualRun`, absent until a run has ever been requested.
    fn manual_run(&self) -> Option<&ReplicationManualRunStatus>;
    /// `status.conditions`, where the failure detail lives.
    fn conditions(&self) -> &[Condition];
}

impl ReplicationTarget for RepositoryReplication {
    const KIND: &'static str = "RepositoryReplication";
    const SINGULAR: &'static str = "repositoryreplication";
    const PLURAL: &'static str = "repositoryreplications";

    fn manual_run(&self) -> Option<&ReplicationManualRunStatus> {
        self.status.as_ref()?.manual_run.as_ref()
    }
    fn conditions(&self) -> &[Condition] {
        self.status
            .as_ref()
            .map(|s| s.conditions.as_slice())
            .unwrap_or_default()
    }
}

impl ReplicationTarget for SnapshotReplication {
    const KIND: &'static str = "SnapshotReplication";
    const SINGULAR: &'static str = "snapshotreplication";
    const PLURAL: &'static str = "snapshotreplications";

    fn manual_run(&self) -> Option<&ReplicationManualRunStatus> {
        self.status.as_ref()?.manual_run.as_ref()
    }
    fn conditions(&self) -> &[Condition] {
        self.status
            .as_ref()
            .map(|s| s.conditions.as_slice())
            .unwrap_or_default()
    }
}

/// The annotation merge-patch for one run request. Pure.
///
/// Deliberately ONE annotation: unlike `maintenance run` there is no companion
/// `run-mode`, because a replication has exactly one kind of run — so this must
/// never write (or clear) that key on a replication object.
pub fn run_patch(requested_at: &str) -> serde_json::Value {
    serde_json::json!({
        "metadata": { "annotations": { RUN_REQUESTED_ANNOTATION: requested_at } }
    })
}

/// Did `status.manualRun` answer THIS request (matching `requestedAt`)?
/// `Ok` = succeeded, `Err` = failed, `None` = keep waiting.
/// Exhaustive over [`ReplicationManualRunPhase`].
pub fn answered<K: ReplicationTarget>(
    obj: &K,
    requested_at: &str,
) -> Option<Result<Box<K>, Box<K>>> {
    let manual = obj.manual_run()?;
    if manual.requested_at.as_deref() != Some(requested_at) {
        return None;
    }
    match manual.phase.as_ref()? {
        // Recorded but blocked (the replication is suspended) — the run is
        // still owed, so keep waiting; the caller's timeout bounds it and its
        // hint names the suspension.
        ReplicationManualRunPhase::Pending => None,
        ReplicationManualRunPhase::Running => None,
        ReplicationManualRunPhase::Succeeded => Some(Ok(Box::new(obj.clone()))),
        ReplicationManualRunPhase::Failed => Some(Err(Box::new(obj.clone()))),
        // Not an answer this build can read: keep waiting rather than reporting
        // a success or a failure we cannot substantiate.
        ReplicationManualRunPhase::Unknown(_) => None,
    }
}

/// Failure detail from the conditions (the manual-run status itself has no
/// failure block; the reconciler writes the reason into conditions). Pure.
pub fn failure_detail<K: ReplicationTarget>(obj: &K, name: &str, requested_at: &str) -> String {
    let condition_msg = obj
        .conditions()
        .iter()
        .find(|c| c.status == "False")
        .map(|c| format!(" ({}): {}", c.reason, c.message))
        .unwrap_or_default();
    format!(
        "{} {name} requested run ({requested_at}) failed{condition_msg}\n\
         the mover Job's logs are at `kubectl get jobs -l {}={name}`\n",
        K::KIND,
        instance_label::<K>(),
    )
}

/// The Job label selecting this kind's mover Jobs, for the logs hint.
fn instance_label<K: ReplicationTarget>() -> &'static str {
    // Not a `match` on a value — the two kinds are distinguished by their
    // associated const, which is already exhaustive by construction.
    if K::KIND == RepositoryReplication::KIND {
        "kopiur.home-operations.com/replication"
    } else {
        kopiur_api::consts::SNAPSHOT_REPLICATION_LABEL
    }
}

/// Run `replication run`: resolve the target kind, stamp the annotation, and
/// optionally wait for `status.manualRun` to answer.
pub async fn run(
    ctx: &KubeCtx,
    args: &ReplicationRunArgs,
    now: DateTime<Utc>,
) -> Result<CmdOutput, CliError> {
    if matches!(ctx.scope, crate::context::Scope::All) {
        return Err(CliError::AllNamespacesNotApplicable {
            command: "replication run",
        });
    }
    // Exhaustive over the flag: an explicit kind targets it directly, an
    // omitted one is auto-detected from what actually exists.
    match args.kind {
        Some(ReplicationKindArg::Repository) => {
            run_kind::<RepositoryReplication>(ctx, args, now).await
        }
        Some(ReplicationKindArg::Snapshot) => run_kind::<SnapshotReplication>(ctx, args, now).await,
        None => match detect_kind(ctx, &args.name).await? {
            ReplicationKindArg::Repository => {
                run_kind::<RepositoryReplication>(ctx, args, now).await
            }
            ReplicationKindArg::Snapshot => run_kind::<SnapshotReplication>(ctx, args, now).await,
        },
    }
}

/// Which kind `name` refers to when `--kind` was omitted: exactly one of the
/// two must exist under that name in this namespace. Zero is a not-found (the
/// message names both plurals); two is ambiguous and asks for `--kind` rather
/// than guessing which replication the user meant to fire.
async fn detect_kind(ctx: &KubeCtx, name: &str) -> Result<ReplicationKindArg, CliError> {
    let ns = ctx.namespace.as_str();
    let repo: Api<RepositoryReplication> = Api::namespaced(ctx.client.clone(), ns);
    let snap: Api<SnapshotReplication> = Api::namespaced(ctx.client.clone(), ns);
    let repo_hit = repo.get_opt(name).await.map_err(|e| {
        classify_kube(
            "get",
            RepositoryReplication::KIND,
            RepositoryReplication::PLURAL,
            Some(ns),
            Some(name),
            e,
        )
    })?;
    let snap_hit = snap.get_opt(name).await.map_err(|e| {
        classify_kube(
            "get",
            SnapshotReplication::KIND,
            SnapshotReplication::PLURAL,
            Some(ns),
            Some(name),
            e,
        )
    })?;
    match (repo_hit.is_some(), snap_hit.is_some()) {
        (true, false) => Ok(ReplicationKindArg::Repository),
        (false, true) => Ok(ReplicationKindArg::Snapshot),
        (true, true) => Err(CliError::AmbiguousTarget {
            what: format!(
                "both a RepositoryReplication and a SnapshotReplication are named {name} in \
                 namespace {ns}; pass --kind repository or --kind snapshot"
            ),
            candidates: format!(
                "{}/{name}, {}/{name}",
                RepositoryReplication::SINGULAR,
                SnapshotReplication::SINGULAR
            ),
        }),
        (false, false) => Err(CliError::NotFound {
            kind: "RepositoryReplication/SnapshotReplication",
            plural: "repositoryreplications,snapshotreplications",
            name: name.to_string(),
            scope: crate::error::scope_suffix(Some(ns)),
            scope_flag: format!(" -n {ns}"),
        }),
    }
}

/// The kind-generic body: patch the annotation, then optionally wait.
async fn run_kind<K: ReplicationTarget>(
    ctx: &KubeCtx,
    args: &ReplicationRunArgs,
    now: DateTime<Utc>,
) -> Result<CmdOutput, CliError> {
    let ns = ctx.namespace.as_str();
    let name = args.name.as_str();
    let api: Api<K> = Api::namespaced(ctx.client.clone(), ns);
    let requested_at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let params = PatchParams {
        field_manager: Some(crate::consts::FIELD_MANAGER.to_string()),
        ..Default::default()
    };
    api.patch(name, &params, &Patch::Merge(run_patch(&requested_at)))
        .await
        .map_err(|e| classify_kube("patch", K::KIND, K::PLURAL, Some(ns), Some(name), e))?;

    let requested_line = format!(
        "{}.{}/{name} run requested ({requested_at})\n",
        K::SINGULAR,
        kopiur_api::GROUP
    );
    if !args.wait {
        return Ok(CmdOutput::ok(requested_line));
    }

    eprint!("{requested_line}");
    let timeout = args.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT);
    let requested_for_check = requested_at.clone();
    let verdict = wait_for(
        &api,
        name,
        format!("{} {name} requested run", K::KIND),
        format!(
            "watch it with `kubectl get {} {name} -n {ns} -o jsonpath='{{.status.manualRun}}'`, \
             or raise --timeout. A phase of Pending means the replication is suspended — \
             `kubectl kopiur resume` it and the run starts",
            K::PLURAL
        ),
        timeout,
        move |o: &K| answered(o, &requested_for_check),
    )
    .await;

    match verdict? {
        Ok(done) => {
            let completed = done
                .manual_run()
                .and_then(|m| m.completed_at.clone())
                .unwrap_or_default();
            Ok(CmdOutput::ok(format!(
                "{} {name} run completed at {completed}\n",
                K::KIND
            )))
        }
        Err(failed) => {
            eprint!("{}", failure_detail(failed.as_ref(), name, &requested_at));
            Ok(CmdOutput {
                text: String::new(),
                exit: 1,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repl(manual: Option<serde_json::Value>) -> RepositoryReplication {
        let mut status = serde_json::json!({});
        if let Some(m) = manual {
            status["manualRun"] = m;
        }
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "RepositoryReplication",
            "metadata": { "name": "offsite", "namespace": "media" },
            "spec": {
                "sourceRef": { "kind": "Repository", "name": "nas" },
                "destination": { "s3": { "bucket": "mirror" } },
                "schedule": { "cron": "0 5 * * *" }
            },
            "status": status,
        }))
        .expect("a RepositoryReplication")
    }

    const RAW: &str = "2026-06-11T12:00:00Z";

    #[test]
    fn run_patch_sets_only_the_run_requested_annotation() {
        // A replication has no run-mode; writing one would be meaningless at
        // best and, on an object that also carries a Maintenance-style value,
        // confusing at worst.
        let p = run_patch(RAW);
        let annotations = p["metadata"]["annotations"]
            .as_object()
            .expect("annotations object");
        assert_eq!(annotations.len(), 1, "{annotations:?}");
        assert_eq!(annotations[RUN_REQUESTED_ANNOTATION], RAW);
    }

    #[test]
    fn answered_matches_only_this_request_and_is_exhaustive() {
        // Nothing recorded yet.
        assert!(answered(&repl(None), RAW).is_none());
        assert!(
            answered(&repl(Some(serde_json::json!({ "requestedAt": RAW }))), RAW).is_none(),
            "a requestedAt with no phase is not an answer"
        );
        // A DIFFERENT request's outcome answers nothing.
        assert!(
            answered(
                &repl(Some(
                    serde_json::json!({ "requestedAt": "2026-06-11T13:00:00Z", "phase": "Succeeded" })
                )),
                RAW
            )
            .is_none()
        );
        // Not-yet-terminal phases keep the wait alive — including `Pending`
        // (suspended) and a phase from a NEWER operator.
        for phase in ["Pending", "Running", "Verifying"] {
            assert!(
                answered(
                    &repl(Some(
                        serde_json::json!({ "requestedAt": RAW, "phase": phase })
                    )),
                    RAW
                )
                .is_none(),
                "{phase} must not resolve the wait"
            );
        }
        // Terminal outcomes resolve it, each on its own side.
        assert!(
            answered(
                &repl(Some(
                    serde_json::json!({ "requestedAt": RAW, "phase": "Succeeded" })
                )),
                RAW
            )
            .is_some_and(|r| r.is_ok())
        );
        assert!(
            answered(
                &repl(Some(
                    serde_json::json!({ "requestedAt": RAW, "phase": "Failed" })
                )),
                RAW
            )
            .is_some_and(|r| r.is_err())
        );
    }

    #[test]
    fn failure_detail_names_the_condition_and_how_to_reach_the_logs() {
        let mut r = repl(Some(
            serde_json::json!({ "requestedAt": RAW, "phase": "Failed" }),
        ));
        r.status.as_mut().unwrap().conditions = vec![Condition {
            type_: "Ready".into(),
            status: "False".into(),
            reason: "ReplicationFailed".into(),
            message: "requested replication Job failed; see the Job/pod logs".into(),
            last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                "2026-06-11T12:00:00Z".parse().unwrap(),
            ),
            observed_generation: None,
        }];
        let out = failure_detail(&r, "offsite", RAW);
        assert!(out.contains("RepositoryReplication offsite"), "{out}");
        assert!(out.contains(RAW), "{out}");
        assert!(out.contains("ReplicationFailed"), "{out}");
        assert!(
            out.contains("kopiur.home-operations.com/replication=offsite"),
            "{out}"
        );
        // With no False condition it still reports the failure, just without detail.
        let bare = repl(Some(
            serde_json::json!({ "requestedAt": RAW, "phase": "Failed" }),
        ));
        assert!(failure_detail(&bare, "offsite", RAW).contains("failed"));
    }

    #[test]
    fn each_kind_points_at_its_own_job_label() {
        assert_eq!(
            instance_label::<RepositoryReplication>(),
            "kopiur.home-operations.com/replication"
        );
        assert_eq!(
            instance_label::<SnapshotReplication>(),
            kopiur_api::consts::SNAPSHOT_REPLICATION_LABEL
        );
        assert_ne!(
            instance_label::<RepositoryReplication>(),
            instance_label::<SnapshotReplication>()
        );
    }
}
