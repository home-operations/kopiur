//! The `RepositoryReplication` reconciler (ADR-0005 §13(d)).
//!
//! Mirrors the `Maintenance` scheduler (`crate::maintenance`): the controller is the
//! *scheduler*. Each reconcile it decides whether a replication is due (croner +
//! deterministic jitter via [`crate::snapshot_schedule::next_fire`], seeded by the
//! CR UID), gates on the source repository being Ready, then spawns at most one
//! per-slot owned mover Job (`kopia repository sync-to`) and tracks it to terminal.
//! The mover PATCHes `.status` (phase, `lastReplicated`).
//!
//! Hardening matches maintenance: per-slot deterministic Job names, single-flight via
//! a label selector, a repo-ready gate, a requeue cap, and transition-guarded status.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::api::ListParams;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};

use kopiur_api::{RepositoryReplication, validate};
use kopiur_mover::workspec::{
    MoverOptions, MoverWorkSpec, Operation, ReplicateOp, ResolvedIdentity, TargetRef,
};

use crate::consts::{
    API_VERSION, COMPONENT_LABEL, REPLICATION_COMPONENT, REPLICATION_INSTANCE_LABEL,
    REPLICATION_SLOT_ANNOTATION,
};
use crate::context::Context;
use crate::error::{Error, Result, error_policy_for};
use crate::io::{self, ResolvedRepository};
use crate::jobs::{self, JobLimits, MoverJobInputs, VolumeMountSpec};
use crate::naming::short_hash;
use crate::snapshot::{backend_to_repository_connect, job_terminal_state};
use crate::snapshot_schedule::{next_fire, parse_go_duration};

/// How long a finished replication Job lingers before TTL-reaping.
const REPLICATION_JOB_TTL_SECS: i64 = 3600;
/// Requeue while a replication Job is in flight.
const REQUEUE_RUNNING: Duration = Duration::from_secs(30);
/// Requeue while waiting for the source repository to become Ready.
const REQUEUE_NOT_READY: Duration = Duration::from_secs(60);
/// Requeue after a failed replication Job (re-check / bounded retry once TTL-reaped).
const REQUEUE_FAILED: Duration = Duration::from_secs(300);
/// Upper bound on any requeue so the schedule/readiness is re-evaluated.
const REQUEUE_CAP: Duration = Duration::from_secs(1800);

/// Reconcile a `RepositoryReplication`.
#[tracing::instrument(skip(repl, ctx), fields(kind = "RepositoryReplication", namespace = %repl.namespace().unwrap_or_default(), name = %repl.name_any()))]
pub async fn reconcile(
    repl: std::sync::Arc<RepositoryReplication>,
    ctx: std::sync::Arc<Context>,
) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(&repl, &ctx).await;
    ctx.metrics
        .record_reconcile("RepositoryReplication", start.elapsed().as_secs_f64());
    result
}

async fn reconcile_inner(repl: &RepositoryReplication, ctx: &Context) -> Result<Action> {
    // Defensive re-validation (one validator, two callers).
    let errs = validate::validate_repository_replication(&repl.spec);
    if let Some(first) = errs.into_iter().next() {
        return Err(Error::Validation(first.to_string()));
    }

    let namespace = repl
        .namespace()
        .ok_or_else(|| Error::Invariant("RepositoryReplication has no namespace".into()))?;
    let name = repl.name_any();
    let api: Api<RepositoryReplication> = Api::namespaced(ctx.client.clone(), &namespace);

    // §14(e): a suspended replication is skipped (surface phase + Ready=Reconciling).
    if repl.spec.suspend {
        patch_ready_if_changed(
            &api,
            &name,
            repl,
            io::ReadyOutcome::Reconciling,
            "Suspended",
            "replication is suspended (spec.suspend)",
            Some("Suspended"),
        )
        .await?;
        return Ok(Action::requeue(REQUEUE_CAP));
    }

    let source_ref = &repl.spec.source_ref;
    let repo = io::resolve_repository_ref(&ctx.client, source_ref, &namespace).await?;

    // Gate on the source repository being Ready (an object-store repo must be
    // bootstrapped before `sync-to` can reach it) — mirrors maintenance's G7.
    if !io::repository_ready(&ctx.client, source_ref, &namespace).await? {
        patch_ready_if_changed(
            &api,
            &name,
            repl,
            io::ReadyOutcome::Reconciling,
            "WaitingForRepository",
            "source repository is not Ready; deferring replication",
            None,
        )
        .await?;
        return Ok(Action::requeue(REQUEUE_NOT_READY));
    }

    let now = Utc::now();
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);
    // GitHub #174 item 3: `spec.schedule.timezone` wins; else the source
    // repository's `scheduleDefaults.timezone`; else UTC.
    let repo_tz = repo
        .schedule_defaults
        .as_ref()
        .and_then(|d| d.timezone.as_deref());

    let Some(slot) = due_slot(repl, now, repo_tz) else {
        // Mark Ready (idle, waiting for the next slot) and sleep.
        patch_ready_if_changed(
            &api,
            &name,
            repl,
            io::ReadyOutcome::Ready,
            "Idle",
            "replication is reconciled; waiting for the next scheduled slot",
            None,
        )
        .await?;
        return Ok(Action::requeue(cap(next_wakeup(repl, now, None, repo_tz))));
    };

    let job_name = replication_job_name(&name, slot);
    match job_api.get_opt(&job_name).await? {
        Some(job) => match job_terminal_state(&job) {
            // Success: the mover stamped status; sleep until the next slot. The
            // terminal Job (its env carries the whole run spec) self-reaps via
            // its TTL — nothing else to clean up.
            Some(true) => Ok(Action::requeue(cap(next_wakeup(
                repl,
                now,
                Some(slot),
                repo_tz,
            )))),
            // Failure: the failed Job lingers to its TTL as the bounded-retry
            // backoff (and keeps the pod logs).
            Some(false) => {
                patch_ready_if_changed(
                    &api,
                    &name,
                    repl,
                    io::ReadyOutcome::Stalled,
                    "ReplicationFailed",
                    "replication Job failed; see the Job/pod logs",
                    Some("Failed"),
                )
                .await?;
                Ok(Action::requeue(REQUEUE_FAILED))
            }
            None => Ok(Action::requeue(REQUEUE_RUNNING)),
        },
        None => {
            if has_active_replication_job(&job_api, &name).await? {
                return Ok(Action::requeue(REQUEUE_RUNNING));
            }
            spawn_replication_job(ctx, &namespace, &name, &job_name, repl, &repo, slot).await?;
            tracing::info!(replication = %name, slot = %slot.to_rfc3339(), "spawned replication Job");
            Ok(Action::requeue(REQUEUE_RUNNING))
        }
    }
}

/// Build + apply the per-slot replication mover Job.
#[allow(clippy::too_many_arguments)]
async fn spawn_replication_job(
    ctx: &Context,
    namespace: &str,
    cr_name: &str,
    job_name: &str,
    repl: &RepositoryReplication,
    repo: &ResolvedRepository,
    slot: DateTime<Utc>,
) -> Result<()> {
    let work_spec = build_replication_work_spec(repl, repo, namespace, cr_name);

    let mut labels = BTreeMap::new();
    labels.insert(
        COMPONENT_LABEL.to_string(),
        REPLICATION_COMPONENT.to_string(),
    );
    labels.insert(REPLICATION_INSTANCE_LABEL.to_string(), cr_name.to_string());
    let mut annotations = BTreeMap::new();
    annotations.insert(REPLICATION_SLOT_ANNOTATION.to_string(), slot.to_rfc3339());

    // Source filesystem repos need the repo volume mounted; object stores reach the
    // backend over the network.
    let repo_volume =
        io::filesystem_repo_mount_source(&repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repo.backend).unwrap_or_default(),
            read_only: false,
        });
    // A filesystem DESTINATION needs its volume mounted too — `kopia repository
    // sync-to` writes the mirror into it. Carried in the `source_volume` slot (the Job
    // builder just turns it into a pod volume/mount at the destination's path, which
    // the webhook guarantees differs from the source repo's path, so the two mounts
    // never collide). Object-store destinations reach the backend over the network.
    let dest_volume =
        io::filesystem_repo_mount_source(&repl.spec.destination).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repl.spec.destination).unwrap_or_default(),
            read_only: false,
        });
    let owner = io::owner_ref_for(repl, "RepositoryReplication")?;

    // Defensive re-checks of the admission rules (one validator, two callers):
    // (a) a same-kind static/workload-identity auth mix would leak the static side's
    // env into the workload-identity side's ambient credential chain; (b) the
    // destination's static credential Secret must co-reside with the Job (envFrom is
    // namespace-local and replication does not project credentials).
    if let Err(e) =
        kopiur_api::validate::validate_replication_auth(&repo.backend, &repl.spec.destination)
    {
        return Err(Error::Validation(e.to_string()));
    }
    if let Err(e) = kopiur_api::validate::validate_replication_destination_secret_namespace(
        &repl.spec.destination,
        namespace,
    ) {
        return Err(Error::Validation(e.to_string()));
    }
    // One replicate pod touches BOTH backends: a workload identity on either
    // names the SA the pod runs as (admission guarantees a both-WI pair agrees),
    // and an Azure-WI on either side requires the pod label.
    let mover_identity = io::ensure_mover_identity(
        &ctx.client,
        namespace,
        &[&repo.backend, &repl.spec.destination],
        ctx.mover_service_account.as_deref(),
        ctx.mover_role_kind.as_str(),
        &ctx.mover_clusterrole,
    )
    .await?;
    mover_identity.decorate_labels(&mut labels);

    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        &io::CredsPrefix::replication(cr_name),
        &owner,
        repo,
        // RepositoryReplication has no credentialProjection of its own; the source
        // repo's Secret must co-reside (the CR lives in the source's namespace).
        false,
        io::repo_kind_str(repl.spec.source_ref.kind),
        &repl.spec.source_ref.name,
    )
    .await?;
    if creds.projected > 0 {
        ctx.metrics
            .inc_secrets_projected(namespace, creds.projected);
    }
    // The DESTINATION backend's own credentials (issue #200): one replicate pod
    // touches TWO backends. Verify the destination Secret is present before launching
    // a Job that would otherwise hang on a missing-Secret `envFrom` (a workload-
    // identity or filesystem destination carries no such Secret and is skipped).
    let dest_secret = io::backend_auth_secret_ref(&repl.spec.destination);
    if let Some(secret) = dest_secret {
        let dest_names = [secret.name.clone()];
        let creds_ctx = io::CredsContext {
            secret_names: &dest_names,
            repo_kind: "RepositoryReplication destination",
            repo_name: cr_name,
            repo_secret_namespace: secret.namespace.as_deref(),
        };
        io::ensure_creds_present(&ctx.client, namespace, &creds_ctx).await?;
    }
    // Source Secrets load verbatim (kopia reads the plain names at connect and
    // persists them); the destination Secret rides under `KOPIUR_DEST_`.
    let creds_secrets = replication_creds_env_from(creds.names, dest_secret);

    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.mover_defaults.as_ref(),
        repl.spec
            .mover
            .as_ref()
            .and_then(|m| m.security_context.as_ref()),
        repl.spec
            .mover
            .as_ref()
            .and_then(|m| m.pod_security_context.as_ref()),
        repl.spec.mover.as_ref().and_then(|m| m.resources.as_ref()),
        repl.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
        repl.spec
            .mover
            .as_ref()
            .and_then(|m| m.ttl_seconds_after_finished),
    );
    let limits = JobLimits {
        ttl_seconds_after_finished: resolved_mover
            .ttl_seconds_after_finished
            .or(Some(REPLICATION_JOB_TTL_SECS)),
        ..JobLimits::default()
    };

    let inputs = MoverJobInputs {
        name: job_name,
        namespace,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        limits,
        resources: resolved_mover.resources.clone(),
        security_context: resolved_mover.security_context.clone(),
        pod_security_context: resolved_mover.pod_security_context.clone(),
        node_selector: resolved_mover.node_selector.clone(),
        tolerations: resolved_mover.tolerations.clone(),
        affinity: resolved_mover.affinity.clone(),
        labels,
        source_volume: dest_volume,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
        annotations,
        cache_volume: Default::default(),
        scratch_volume: None,
        readiness_exec: None,
    };
    let job = jobs::build_job(&inputs)?;
    io::apply_mover_objects(&ctx.client, namespace, job_name, None, &job).await?;
    Ok(())
}

/// Build the replication mover work spec. Pure (no IO) so the source→destination
/// mapping is unit-testable: connect to the source repository, sync-to the
/// destination backend.
pub fn build_replication_work_spec(
    repl: &RepositoryReplication,
    repo: &ResolvedRepository,
    namespace: &str,
    cr_name: &str,
) -> MoverWorkSpec {
    let sync = repl.spec.sync.unwrap_or_default();
    MoverWorkSpec {
        version: 1,
        operation: Operation::Replicate(ReplicateOp {
            destination: backend_to_repository_connect(&repl.spec.destination),
            // Additive sync by default (never prune the destination automatically).
            delete_extra: sync.delete_extra,
            parallel: sync.parallel,
            must_exist: sync.must_exist,
            times: sync.times,
            update: sync.update,
            max_download_speed_bytes_per_second: sync.max_download_speed_bytes_per_second,
            max_upload_speed_bytes_per_second: sync.max_upload_speed_bytes_per_second,
        }),
        // Replication does not snapshot; a stable sentinel identity (like maintenance).
        identity: ResolvedIdentity {
            username: "kopiur-replication".to_string(),
            hostname: namespace.to_string(),
            source_path: String::new(),
        },
        repository: backend_to_repository_connect(&repo.backend),
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "RepositoryReplication".to_string(),
            name: cr_name.to_string(),
            namespace: namespace.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: io::throttle_spec(repo.mover_defaults.as_ref()),
    }
}

/// The replication slot due now (cron + jitter strictly after the last run), or
/// `None` if not yet due. Pure given the CR, `now`, and the source repository's
/// `scheduleDefaults.timezone` (`repo_tz`, GitHub #174 item 3).
pub fn due_slot(
    repl: &RepositoryReplication,
    now: DateTime<Utc>,
    repo_tz: Option<&str>,
) -> Option<DateTime<Utc>> {
    let after = last_run_at(repl).unwrap_or_else(|| now - chrono::Duration::days(365));
    match slot_for(repl, after, repo_tz) {
        Ok(slot) if now >= slot => Some(slot),
        _ => None,
    }
}

/// Assemble the replication mover's `envFrom` credential set (issue #200): the
/// SOURCE repository's Secrets verbatim, plus the DESTINATION backend's Secret under
/// the [`DEST_ENV_PREFIX`](kopiur_api::creds::DEST_ENV_PREFIX) so its keys can't
/// collide with the source's identically named ones (the mover remaps them for the
/// `sync-to` subprocess only). The destination is appended WITHOUT deduping against
/// the source: a Secret referenced by both sides is loaded twice — once plain for the
/// source, once prefixed for the destination — because kopia reads the two copies
/// under different env-var names. A workload-identity or filesystem destination has
/// no auth Secret (`None`) and contributes nothing.
fn replication_creds_env_from(
    source_names: Vec<String>,
    dest_secret: Option<&kopiur_api::common::SecretRef>,
) -> Vec<jobs::CredsEnvFrom> {
    let mut creds = io::plain_creds(source_names);
    if let Some(secret) = dest_secret {
        creds.push(jobs::CredsEnvFrom::prefixed(
            secret.name.clone(),
            kopiur_api::creds::DEST_ENV_PREFIX,
        ));
    }
    creds
}

/// The next cron slot for this replication strictly after `after` (croner + jitter,
/// seeded by the CR UID). `spec.schedule.timezone` wins; else the source
/// repository's `scheduleDefaults.timezone`; else UTC.
fn slot_for(
    repl: &RepositoryReplication,
    after: DateTime<Utc>,
    repo_tz: Option<&str>,
) -> Result<DateTime<Utc>> {
    let seed = repl.uid().unwrap_or_else(|| repl.name_any());
    let jitter = repl
        .spec
        .schedule
        .jitter
        .as_deref()
        .and_then(parse_go_duration);
    let tz = kopiur_api::common::resolve_tz_with_default(
        repl.spec.schedule.timezone.as_deref(),
        repo_tz,
    );
    next_fire(&repl.spec.schedule.cron, jitter, &seed, after, tz)
}

/// Parse `status.lastReplicated` (RFC3339) into a `DateTime<Utc>`.
fn last_run_at(repl: &RepositoryReplication) -> Option<DateTime<Utc>> {
    repl.status
        .as_ref()
        .and_then(|s| s.last_replicated.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// How long until the next replication slot. When `handled` is set, that slot is the
/// search anchor (so a just-handled slot doesn't immediately re-fire). Floored at the
/// running cadence, capped by the caller.
fn next_wakeup(
    repl: &RepositoryReplication,
    now: DateTime<Utc>,
    handled: Option<DateTime<Utc>>,
    repo_tz: Option<&str>,
) -> Duration {
    let after = handled.unwrap_or_else(|| last_run_at(repl).unwrap_or(now));
    match slot_for(repl, after, repo_tz) {
        Ok(slot) if slot > now => (slot - now)
            .to_std()
            .unwrap_or(REQUEUE_CAP)
            .max(REQUEUE_RUNNING),
        _ => REQUEUE_RUNNING,
    }
}

/// Cap a requeue so the schedule/readiness is re-evaluated within the heartbeat.
fn cap(d: Duration) -> Duration {
    d.min(REQUEUE_CAP)
}

/// Deterministic, ≤52-char, DNS-1123-safe per-slot replication Job name:
/// `<cr>-repl-<unix_slot>` (truncate+hash long names, like maintenance).
fn replication_job_name(cr: &str, slot: DateTime<Utc>) -> String {
    const MAX: usize = 52;
    let suffix = format!("-repl-{}", slot.timestamp());
    let budget = MAX.saturating_sub(suffix.len());
    if cr.len() <= budget {
        format!("{cr}{suffix}")
    } else {
        let hash = short_hash(cr);
        let keep = budget.saturating_sub(hash.len() + 1);
        let trunc: String = cr.chars().take(keep).collect();
        format!("{trunc}-{hash}{suffix}")
    }
}

/// Whether any non-terminal replication Job is owned by this CR (single-flight gate).
async fn has_active_replication_job(job_api: &Api<Job>, cr_name: &str) -> Result<bool> {
    let selector =
        format!("{COMPONENT_LABEL}={REPLICATION_COMPONENT},{REPLICATION_INSTANCE_LABEL}={cr_name}");
    let jobs = job_api
        .list(&ListParams::default().labels(&selector))
        .await?;
    Ok(jobs.items.iter().any(|j| job_terminal_state(j).is_none()))
}

/// Patch the kstatus Ready conditions (+ optional phase + destinationBackend) only
/// when the `Ready` condition changes, so the reconcile does not hot-loop on its own
/// status writes (transition-guarded). `phase` is the optional phase string to set.
async fn patch_ready_if_changed(
    api: &Api<RepositoryReplication>,
    name: &str,
    repl: &RepositoryReplication,
    outcome: io::ReadyOutcome,
    reason: &str,
    message: &str,
    phase: Option<&str>,
) -> Result<()> {
    let existing: Vec<_> = repl
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let current = existing
        .iter()
        .find(|c| c.type_ == "Ready")
        .map(|c| (c.status.clone(), c.reason.clone()));
    let target_status = match outcome {
        io::ReadyOutcome::Ready => "True",
        _ => "False",
    };
    if current.as_ref() == Some(&(target_status.to_string(), reason.to_string())) {
        return Ok(());
    }
    let observed_gen = repl.metadata.generation.unwrap_or(0);
    let conditions = io::set_ready(&existing, Some(observed_gen), outcome, reason, message);
    let mut status = serde_json::json!({
        "observedGeneration": observed_gen,
        "conditions": conditions,
        // Mirror the destination backend kind for the print column (deterministic).
        "destinationBackend": repl.spec.destination.kind_str(),
    });
    if let Some(p) = phase {
        status["phase"] = serde_json::json!(p);
    }
    io::patch_status(api, name, status).await?;
    Ok(())
}

/// `error_policy` for the `RepositoryReplication` controller.
pub fn error_policy(
    obj: std::sync::Arc<RepositoryReplication>,
    err: &Error,
    ctx: std::sync::Arc<Context>,
) -> Action {
    error_policy_for("RepositoryReplication", obj.as_ref(), err, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::backend::{Backend, FilesystemBackend, S3Backend};
    use kopiur_api::common::{
        CronSpec, Encryption, RepositoryKind, RepositoryMode, RepositoryRef, SecretKeyRef,
    };
    use kopiur_api::{RepositoryReplicationSpec, RepositoryReplicationStatus};

    fn repl_with(cron: &str, status: Option<RepositoryReplicationStatus>) -> RepositoryReplication {
        let mut r = RepositoryReplication::new(
            "offsite",
            RepositoryReplicationSpec {
                source_ref: RepositoryRef {
                    kind: RepositoryKind::Repository,
                    name: "nas-primary".into(),
                    namespace: None,
                },
                destination: Backend::S3(S3Backend {
                    bucket: "mirror".into(),
                    prefix: None,
                    endpoint: None,
                    region: None,
                    auth: None,
                    tls: None,
                }),
                schedule: CronSpec {
                    cron: cron.into(),
                    jitter: None,
                    timezone: None,
                },
                mover: None,
                suspend: false,
                sync: None,
            },
        );
        r.metadata.uid = Some("uid-repl-1".into());
        r.status = status;
        r
    }

    fn sample_repo() -> ResolvedRepository {
        ResolvedRepository {
            backend: Backend::Filesystem(FilesystemBackend {
                path: "/repo".into(),
                volume: None,
            }),
            encryption: Encryption {
                password_secret_ref: SecretKeyRef {
                    name: "s".into(),
                    namespace: None,
                    key: None,
                },
            },
            repo_namespace: Some("ns".into()),
            mover_defaults: None,
            identity_defaults: None,
            schedule_defaults: None,
            on_namespace_delete: Default::default(),
            credential_projection_allowed: false,
            owner_ref: Default::default(),
            mode: RepositoryMode::ReadWrite,
            deletion_protection: None,
            mass_deletion_ack: None,
        }
    }

    #[test]
    fn first_ever_reconcile_is_due() {
        let r = repl_with("0 5 * * *", None);
        assert!(due_slot(&r, Utc::now(), None).is_some());
    }

    #[test]
    fn not_due_right_after_a_run() {
        let now = Utc::now();
        let just = (now - chrono::Duration::seconds(1)).to_rfc3339();
        let status = RepositoryReplicationStatus {
            last_replicated: Some(just),
            ..Default::default()
        };
        let r = repl_with("0 5 * * *", Some(status));
        assert!(
            due_slot(&r, now, None).is_none(),
            "a replication that just ran must not be immediately due again"
        );
    }

    #[test]
    fn requeue_is_capped() {
        let now = Utc::now();
        let just = (now - chrono::Duration::seconds(1)).to_rfc3339();
        let status = RepositoryReplicationStatus {
            last_replicated: Some(just),
            ..Default::default()
        };
        let r = repl_with("0 5 * * *", Some(status));
        assert!(cap(next_wakeup(&r, now, None, None)) <= REQUEUE_CAP);
    }

    #[test]
    fn due_slot_honors_repo_schedule_default_timezone() {
        // Mirrors the verification precedence test: no own timezone on the
        // schedule, so the repo default must be what shifts the evaluated slot.
        let now = DateTime::parse_from_rfc3339("2026-06-09T05:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let just = (now - chrono::Duration::hours(2)).to_rfc3339();
        let status = RepositoryReplicationStatus {
            last_replicated: Some(just),
            ..Default::default()
        };
        let r = repl_with("0 5 * * *", Some(status));
        assert!(
            due_slot(&r, now, None).is_some(),
            "UTC (no repo default) → 05:00 UTC has already passed"
        );
        assert!(
            due_slot(&r, now, Some("America/Los_Angeles")).is_none(),
            "repo scheduleDefaults.timezone must shift the evaluated slot"
        );
    }

    #[test]
    fn job_name_deterministic_and_bounded() {
        let slot = DateTime::parse_from_rfc3339("2026-06-09T05:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let n = replication_job_name("offsite", slot);
        assert!(n.len() <= 52);
        assert!(n.starts_with("offsite-repl-"));
        assert_eq!(n, replication_job_name("offsite", slot));
        let long = "a-very-long-repository-replication-name-blowing-the-dns-budget";
        assert!(replication_job_name(long, slot).len() <= 52);
    }

    #[test]
    fn work_spec_maps_source_and_destination() {
        let r = repl_with("0 5 * * *", None);
        let repo = sample_repo();
        let ws = build_replication_work_spec(&r, &repo, "ns", "offsite");
        // Operation is Replicate with the S3 destination; source is the filesystem repo.
        match &ws.operation {
            Operation::Replicate(op) => {
                assert_eq!(op.destination.kind_str(), "S3");
                assert!(!op.delete_extra);
                // No `spec.sync` set → every knob defaults, reproducing today's argv.
                assert_eq!(op.parallel, None);
                assert_eq!(op.must_exist, None);
                assert_eq!(op.times, None);
                assert_eq!(op.update, None);
                assert_eq!(op.max_download_speed_bytes_per_second, None);
                assert_eq!(op.max_upload_speed_bytes_per_second, None);
            }
            other => panic!("expected replicate op, got {}", other.kind_str()),
        }
        assert_eq!(ws.repository.kind_str(), "Filesystem");
        assert_eq!(ws.target_ref.kind, "RepositoryReplication");
    }

    #[test]
    fn work_spec_maps_every_sync_field_to_the_replicate_op() {
        // #216 controller-glue guard: this is the regression test for the exact
        // bug class the whole change exists to kill — `spec.sync` plumbed through
        // to CRD/validator/workspec but the controller still hardcoding `None`.
        // Every field set on `spec.sync` must land on the corresponding `op` field.
        use kopiur_api::repository_replication::SyncOptions;
        let mut r = repl_with("0 5 * * *", None);
        r.spec.sync = Some(SyncOptions {
            parallel: Some(6),
            delete_extra: true,
            must_exist: Some(true),
            times: Some(false),
            update: Some(true),
            max_download_speed_bytes_per_second: Some(2_000_000),
            max_upload_speed_bytes_per_second: Some(3_000_000),
        });
        let repo = sample_repo();
        let ws = build_replication_work_spec(&r, &repo, "ns", "offsite");
        match &ws.operation {
            Operation::Replicate(op) => {
                assert_eq!(op.parallel, Some(6));
                assert!(op.delete_extra);
                assert_eq!(op.must_exist, Some(true));
                assert_eq!(op.times, Some(false));
                assert_eq!(op.update, Some(true));
                assert_eq!(op.max_download_speed_bytes_per_second, Some(2_000_000));
                assert_eq!(op.max_upload_speed_bytes_per_second, Some(3_000_000));
            }
            other => panic!("expected replicate op, got {}", other.kind_str()),
        }
    }

    #[test]
    fn creds_put_source_plain_and_destination_prefixed() {
        use kopiur_api::common::SecretRef;
        // A source with a password + backend Secret; the destination brings its own.
        let out = replication_creds_env_from(
            vec!["src-pw".into(), "src-s3".into()],
            Some(&SecretRef {
                name: "dst-s3".into(),
                namespace: None,
            }),
        );
        assert_eq!(
            out,
            vec![
                jobs::CredsEnvFrom::plain("src-pw"),
                jobs::CredsEnvFrom::plain("src-s3"),
                jobs::CredsEnvFrom::prefixed("dst-s3", "KOPIUR_DEST_"),
            ]
        );
    }

    #[test]
    fn creds_shared_secret_is_loaded_twice_not_deduped() {
        use kopiur_api::common::SecretRef;
        // Same Secret name on both sides must appear BOTH plain (source) and prefixed
        // (destination) — kopia reads the two copies under different env-var names, so
        // collapsing them to one entry would strip the destination's credentials (#200).
        let out = replication_creds_env_from(
            vec!["shared".into()],
            Some(&SecretRef {
                name: "shared".into(),
                namespace: None,
            }),
        );
        assert_eq!(
            out,
            vec![
                jobs::CredsEnvFrom::plain("shared"),
                jobs::CredsEnvFrom::prefixed("shared", "KOPIUR_DEST_"),
            ]
        );
    }

    #[test]
    fn creds_workload_identity_or_filesystem_destination_adds_nothing() {
        // A destination with no auth Secret (workload identity / filesystem) → only
        // the source entries, none prefixed.
        let out = replication_creds_env_from(vec!["src-pw".into()], None);
        assert_eq!(out, vec![jobs::CredsEnvFrom::plain("src-pw")]);
    }
}
