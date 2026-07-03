//! First-class backup verification scheduling (ADR-0005 §4).
//!
//! Mirrors the `Maintenance` scheduling kernel (`crate::maintenance`): the
//! `SnapshotPolicy` reconciler is the *scheduler*. Each reconcile it decides whether
//! a quick (`kopia snapshot verify`) or deep (scratch-restore) verification is due
//! — using croner + deterministic jitter via [`crate::snapshot_schedule::next_fire`],
//! seeded by the policy UID — then spawns at most one per-slot owned mover Job and
//! tracks it to terminal. The mover evaluates the optional CEL `successExpr` and
//! PATCHes `SnapshotPolicy.status.lastVerified`.
//!
//! Hardening matches maintenance: per-slot deterministic Job names (idempotency),
//! single-flight via a label selector, a repository-ready gate (the caller already
//! gates the policy on its Repository being Ready), and `ttlSecondsAfterFinished` so
//! finished Jobs self-reap.
//!
//! The scheduling decisions ([`due_tier`], [`next_verify_wakeup`]) are **pure** and
//! unit-tested; the Job spawn is thin IO.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::api::ListParams;
use kube::{Api, ResourceExt};

use kopiur_api::common::{CronSpec, ScratchDefaults};
use kopiur_api::{SnapshotPolicy, Verification};
use kopiur_mover::workspec::{
    DeepVerify, MoverOptions, MoverWorkSpec, Operation, QuickVerify, ResolvedIdentity, TargetRef,
    VerifyOp, VerifyTier,
};

use crate::consts::{
    API_VERSION, COMPONENT_LABEL, VERIFY_COMPONENT, VERIFY_INSTANCE_LABEL, VERIFY_SLOT_ANNOTATION,
};
use crate::context::Context;
use crate::error::Result;
use crate::io::{self, ResolvedRepository};
use crate::jobs::{
    self, CacheVolume, DEEP_SCRATCH_PATH, JobLimits, MoverJobInputs, VolumeMountSpec,
};
use crate::snapshot::{backend_to_repository_connect, job_terminal_state, mover_pull_policy_pub};
use crate::snapshot_schedule::{next_fire, parse_go_duration};

/// How long a finished verify Job lingers before TTL-reaping.
const VERIFY_JOB_TTL_SECS: i64 = 3600;
/// Requeue while a verify Job is in flight.
const REQUEUE_RUNNING: Duration = Duration::from_secs(30);
/// Requeue after a failed verify Job (re-check / bounded retry once TTL-reaped).
const REQUEUE_FAILED: Duration = Duration::from_secs(300);
/// Upper bound on any requeue so the schedule is re-evaluated within the heartbeat.
const REQUEUE_CAP: Duration = Duration::from_secs(1800);

/// Which verification tier to run, mirroring `MaintenanceMode`. Deep subsumes quick
/// (a deep restore-test is the stronger proof), so when both are due, deep wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyTierKind {
    /// Blob-level `kopia snapshot verify`.
    Quick,
    /// Scratch-restore restorability test.
    Deep,
}

impl VerifyTierKind {
    /// Stable one-char tag for the per-slot Job name.
    fn tag(self) -> &'static str {
        match self {
            VerifyTierKind::Quick => "q",
            VerifyTierKind::Deep => "d",
        }
    }
}

/// The instant after which to search for `tier`'s next slot: its last run (from
/// `status.lastVerified` for the freshest known verify) or a year ago so the first
/// reconcile fires. Both tiers share `lastVerified` (a single surfaced timestamp),
/// which is conservative: a just-run quick verify briefly defers a due deep one,
/// re-evaluated on the next reconcile.
fn tier_after(last_verified: Option<DateTime<Utc>>) -> DateTime<Utc> {
    last_verified.unwrap_or_else(|| Utc::now() - chrono::Duration::days(365))
}

/// The next cron slot for a verification `CronSpec` strictly after `after`, seeded by
/// the policy UID for a stable per-replica spread and evaluated in the cron's own
/// `timezone` (absent ⇒ the repository's `scheduleDefaults.timezone`, else UTC).
fn slot_for(
    seed: &str,
    spec: &CronSpec,
    after: DateTime<Utc>,
    repo_tz: Option<&str>,
) -> Result<DateTime<Utc>> {
    let jitter = spec.jitter.as_deref().and_then(parse_go_duration);
    let tz = kopiur_api::common::resolve_tz_with_default(spec.timezone.as_deref(), repo_tz);
    next_fire(&spec.cron, jitter, seed, after, tz)
}

/// Decide which verification tier is due now, preferring deep (it subsumes quick).
/// Returns the tier + its scheduled slot, or `None` if nothing is due. Pure given
/// the policy's `verification`, the seed, the last-verified time, `now`, and the
/// repository's `scheduleDefaults.timezone` (`repo_tz`, GitHub #174 item 3).
pub fn due_tier(
    verification: &Verification,
    seed: &str,
    last_verified: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    repo_tz: Option<&str>,
) -> Option<(VerifyTierKind, DateTime<Utc>)> {
    let after = tier_after(last_verified);
    if let Some(d) = &verification.deep
        && let Ok(slot) = slot_for(seed, &d.schedule, after, repo_tz)
        && now >= slot
    {
        return Some((VerifyTierKind::Deep, slot));
    }
    if let Some(q) = &verification.quick
        && let Ok(slot) = slot_for(seed, q, after, repo_tz)
        && now >= slot
    {
        return Some((VerifyTierKind::Quick, slot));
    }
    None
}

/// How long until the next verification slot (either tier), measured from
/// `last_verified`. Floored at the running cadence, capped by `REQUEUE_CAP`.
pub fn next_verify_wakeup(
    verification: &Verification,
    seed: &str,
    last_verified: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    repo_tz: Option<&str>,
) -> Duration {
    let after = tier_after(last_verified);
    let mut earliest: Option<DateTime<Utc>> = None;
    for spec in [
        verification.quick.as_ref(),
        verification.deep.as_ref().map(|d| &d.schedule),
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(slot) = slot_for(seed, spec, after, repo_tz) {
            earliest = Some(earliest.map_or(slot, |e| e.min(slot)));
        }
    }
    match earliest {
        Some(slot) if slot > now => (slot - now)
            .to_std()
            .unwrap_or(REQUEUE_CAP)
            .min(REQUEUE_CAP)
            .max(REQUEUE_RUNNING),
        _ => REQUEUE_RUNNING,
    }
}

/// Deterministic, ≤52-char, DNS-1123-safe per-slot verify Job name:
/// `<policy>-vfy-<q|d>-<unix_slot>` (truncate+hash long policy names, like
/// maintenance).
fn verify_job_name(policy: &str, tier: VerifyTierKind, slot: DateTime<Utc>) -> String {
    const MAX: usize = 52;
    let suffix = format!("-vfy-{}-{}", tier.tag(), slot.timestamp());
    let budget = MAX.saturating_sub(suffix.len());
    if policy.len() <= budget {
        format!("{policy}{suffix}")
    } else {
        let hash = short_hash(policy);
        let keep = budget.saturating_sub(hash.len() + 1);
        let trunc: String = policy.chars().take(keep).collect();
        format!("{trunc}-{hash}{suffix}")
    }
}

/// A short, stable 8-hex-char FNV-1a hash for name truncation (matches maintenance).
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (h & 0xffff_ffff))
}

/// The verification scheduling step, called by the `SnapshotPolicy` reconciler
/// (after it has confirmed the policy is non-suspended and its Repository is Ready).
/// Returns `Some(requeue)` when verification is configured (so the caller honors the
/// verify cadence), or `None` when `spec.verification` is absent (no behavior change).
pub async fn verify_step(
    config: &SnapshotPolicy,
    ctx: &Context,
    repo: &ResolvedRepository,
    namespace: &str,
) -> Result<Option<Duration>> {
    let Some(verification) = config.spec.verification.as_ref() else {
        return Ok(None);
    };
    let name = config.name_any();
    let seed = config.uid().unwrap_or_else(|| name.clone());
    let now = Utc::now();
    let last_verified = config
        .status
        .as_ref()
        .and_then(|s| s.last_verified.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    // GitHub #174 item 3: a per-cron `timezone` wins; else the repository's
    // `scheduleDefaults.timezone`; else UTC.
    let repo_tz = repo
        .schedule_defaults
        .as_ref()
        .and_then(|d| d.timezone.as_deref());

    let Some((tier, slot)) = due_tier(verification, &seed, last_verified, now, repo_tz) else {
        return Ok(Some(
            next_verify_wakeup(verification, &seed, last_verified, now, repo_tz).min(REQUEUE_CAP),
        ));
    };

    let job_name = verify_job_name(&name, tier, slot);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    match job_api.get_opt(&job_name).await? {
        Some(job) => match job_terminal_state(&job) {
            // Success: the mover stamped lastVerified; sleep until the next slot.
            Some(true) => Ok(Some(
                next_verify_wakeup(verification, &seed, Some(now), now, repo_tz).min(REQUEUE_CAP),
            )),
            Some(false) => Ok(Some(REQUEUE_FAILED)),
            None => Ok(Some(REQUEUE_RUNNING)),
        },
        None => {
            if has_active_verify_job(&job_api, &name).await? {
                return Ok(Some(REQUEUE_RUNNING));
            }
            spawn_verify_job(
                config,
                ctx,
                repo,
                namespace,
                &name,
                &job_name,
                verification,
                tier,
                slot,
            )
            .await?;
            tracing::info!(policy = %name, ?tier, slot = %slot.to_rfc3339(), "spawned verification Job");
            Ok(Some(REQUEUE_RUNNING))
        }
    }
}

/// Build + apply the per-slot verification mover Job.
#[allow(clippy::too_many_arguments)]
async fn spawn_verify_job(
    config: &SnapshotPolicy,
    ctx: &Context,
    repo: &ResolvedRepository,
    namespace: &str,
    policy_name: &str,
    job_name: &str,
    verification: &Verification,
    tier: VerifyTierKind,
    slot: DateTime<Utc>,
) -> Result<()> {
    let work_spec =
        build_verify_work_spec(config, repo, namespace, policy_name, verification, tier);

    let mut labels = BTreeMap::new();
    labels.insert(COMPONENT_LABEL.to_string(), VERIFY_COMPONENT.to_string());
    labels.insert(VERIFY_INSTANCE_LABEL.to_string(), policy_name.to_string());
    let mut annotations = BTreeMap::new();
    annotations.insert(VERIFY_SLOT_ANNOTATION.to_string(), slot.to_rfc3339());

    // Filesystem repos need the repo volume mounted; object stores reach the backend
    // over the network. Mounted read-write (verify reads, deep restore writes scratch).
    let repo_volume =
        io::filesystem_repo_mount_source(&repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repo.backend).unwrap_or_default(),
            read_only: false,
        });
    let owner = io::owner_ref_for(config, "SnapshotPolicy")?;

    let mover_identity = io::ensure_mover_identity(
        &ctx.client,
        namespace,
        &[&repo.backend],
        ctx.mover_service_account.as_deref(),
        &ctx.mover_role_kind,
        &ctx.mover_clusterrole,
    )
    .await?;
    mover_identity.decorate_labels(&mut labels);

    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        job_name,
        &owner,
        repo,
        config
            .spec
            .credential_projection
            .as_ref()
            .is_some_and(|p| p.enabled),
        io::repo_kind_str(config.spec.repository.kind),
        &config.spec.repository.name,
    )
    .await?;
    if creds.projected > 0 {
        ctx.metrics
            .inc_secrets_projected(namespace, creds.projected);
    }
    let creds_secrets = creds.names;

    // The verify mover inherits the repository's moverDefaults (security context,
    // placement, resources, TTL, cache) merged under the recipe's mover (ADR-0004
    // §1/§2). The cache *volume* is resolved separately below (verify never attaches
    // the backup's persistent cache PVC); the cache *budgets* ride the work-spec.
    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.mover_defaults.as_ref(),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.security_context.as_ref()),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.pod_security_context.as_ref()),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.resources.as_ref()),
        config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.ttl_seconds_after_finished),
    );
    let limits = JobLimits {
        ttl_seconds_after_finished: resolved_mover
            .ttl_seconds_after_finished
            .or(Some(VERIFY_JOB_TTL_SECS)),
        ..JobLimits::default()
    };

    let inputs = MoverJobInputs {
        name: job_name,
        namespace,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: mover_pull_policy_pub(),
        limits,
        resources: resolved_mover.resources.clone(),
        security_context: resolved_mover.security_context.clone(),
        pod_security_context: resolved_mover.pod_security_context.clone(),
        node_selector: resolved_mover.node_selector.clone(),
        tolerations: resolved_mover.tolerations.clone(),
        affinity: resolved_mover.affinity.clone(),
        labels,
        source_volume: None,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
        annotations,
        // Inherit moverDefaults.cache (overlaid by the recipe's mover.cache) for the
        // kopia cache volume — but always per-run ephemeral, never the backup's warm
        // persistent PVC (see `verify_cache_volume`). Same `effective_cache` source as
        // the cache budgets in `build_verify_work_spec`.
        cache_volume: crate::cache::verify_cache_volume(
            crate::cache::effective_cache(
                repo,
                config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
            )
            .as_ref(),
        ),
        // Deep verify restores into DEEP_SCRATCH_PATH; mount a writable volume there
        // (sized PVC if `capacity` is set, else emptyDir). Quick verify needs none.
        scratch_volume: match tier {
            VerifyTierKind::Deep => verification
                .deep
                .as_ref()
                .map(|deep| scratch_volume(crate::cache::effective_scratch(repo, deep).as_ref())),
            VerifyTierKind::Quick => None,
        },
        readiness_exec: None,
    };
    let cm = jobs::build_config_map(&inputs)?;
    let job = jobs::build_job(&inputs);
    io::apply_mover_objects(&ctx.client, namespace, job_name, &cm, &job).await?;
    Ok(())
}

/// Build the verify mover work spec for a tier. Pure (no IO) so the tier→work-spec
/// mapping is unit-testable; the identity is the recipe's resolved source identity
/// so the deep tier restores the right snapshot.
pub fn build_verify_work_spec(
    config: &SnapshotPolicy,
    repo: &ResolvedRepository,
    namespace: &str,
    policy_name: &str,
    verification: &Verification,
    tier: VerifyTierKind,
) -> MoverWorkSpec {
    let tier = match tier {
        VerifyTierKind::Quick => VerifyTier::Quick(QuickVerify {
            verify_files_percent: verification.verify_files_percent,
            max_errors: None,
            parallel: None,
        }),
        VerifyTierKind::Deep => VerifyTier::Deep(DeepVerify {
            scratch_path: DEEP_SCRATCH_PATH.to_string(),
            // The mover resolves the latest snapshot for the identity itself.
            snapshot_id: None,
        }),
    };
    // The source identity (first source) so a deep restore targets the right path.
    let identity = verify_identity(config, namespace, repo);
    MoverWorkSpec {
        version: 1,
        operation: Operation::Verify(VerifyOp {
            tier,
            success_expr: verification.success_expr.clone(),
        }),
        identity,
        repository: backend_to_repository_connect(&repo.backend),
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "SnapshotPolicy".to_string(),
            name: policy_name.to_string(),
            namespace: namespace.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        cache: crate::cache::cache_tuning(
            crate::cache::effective_cache(
                repo,
                config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
            )
            .as_ref(),
        ),
        throttle: io::throttle_spec(repo.mover_defaults.as_ref()),
    }
}

/// Resolve the deep-verify scratch volume from the **effective** scratch config
/// (`moverDefaults.scratch` overlaid by `verification.deep`, via
/// [`crate::cache::effective_scratch`]). Mirrors [`crate::cache::resolve_cache_volume`]'s
/// capacity gate: a sized `capacity` provisions a fresh generic ephemeral volume (a
/// PVC bound to the pod's lifetime, auto-GC'd, honoring `storageClassName`); an unset
/// `capacity` falls back to an `emptyDir` (node-ephemeral, zero-config, writable by
/// the non-root mover via the pod's `fsGroup`). Never `Pvc` — scratch is discarded
/// after each run, so a persistent PVC would be wrong. Pure (no IO): unlike the
/// cache there is no owned PVC to provision, so this is unit-testable directly.
fn scratch_volume(effective: Option<&ScratchDefaults>) -> CacheVolume {
    match effective.and_then(|s| s.capacity.clone()) {
        Some(capacity) => CacheVolume::Ephemeral {
            capacity,
            storage_class: effective.and_then(|s| s.storage_class_name.clone()),
        },
        None => CacheVolume::EmptyDir,
    }
}

/// Whether a policy's deep-verify scratch `storageClassName` is a silent no-op
/// (set, but with no effective `capacity`, so scratch is an `emptyDir` — which has
/// no StorageClass), with the actionable message to surface. Computed from the
/// **merged** `moverDefaults.scratch` + `verification.deep`
/// (via [`crate::cache::effective_scratch`]). `None` when the policy has no
/// `verification.deep` (nothing to report); `Some { ignored: false }` is the normal
/// consistent state (so the condition self-clears `True`→`False` once a capacity is
/// added — never an orphaned `True`).
pub struct ScratchStorageClassState {
    /// `true` ⇒ the storageClass is being ignored (no-op); `false` ⇒ honored/consistent.
    pub ignored: bool,
    /// Human-/machine-actionable message for the condition + Warning Event.
    pub message: String,
}

/// See [`ScratchStorageClassState`].
pub fn scratch_storage_class_state(
    repo: &ResolvedRepository,
    verification: &Verification,
) -> Option<ScratchStorageClassState> {
    let deep = verification.deep.as_ref()?;
    let effective = crate::cache::effective_scratch(repo, deep);
    let storage_class = effective
        .as_ref()
        .and_then(|s| s.storage_class_name.clone());
    let has_capacity = effective
        .as_ref()
        .and_then(|s| s.capacity.as_ref())
        .is_some();
    Some(match (storage_class, has_capacity) {
        (Some(sc), false) => ScratchStorageClassState {
            ignored: true,
            message: format!(
                "deep-verify scratch storageClassName '{sc}' has no effect without a capacity \
                 (an emptyDir has no StorageClass); set verification.deep.capacity or \
                 moverDefaults.scratch.capacity to provision a sized PVC"
            ),
        },
        _ => ScratchStorageClassState {
            ignored: false,
            message: "deep-verify scratch volume configuration is consistent".to_string(),
        },
    })
}

/// Resolve the recipe's source identity for the verify run (reuses the api kernel),
/// falling back to a sentinel for the quick tier (which does not restore).
fn verify_identity(
    config: &SnapshotPolicy,
    namespace: &str,
    repo: &ResolvedRepository,
) -> ResolvedIdentity {
    let first = config.spec.sources.first();
    let pvc_name = first.and_then(|s| s.pvc.as_ref().map(|p| p.name.clone()));
    let nfs_source_path = first.and_then(|s| s.nfs.as_ref().map(|n| n.path.clone()));
    let source_path_override = first.and_then(|s| s.source_path_override.clone());
    let inputs = kopiur_api::IdentityInputs {
        object_name: &config.name_any(),
        namespace,
        overrides: config.spec.identity.as_ref(),
        defaults: repo.identity_defaults.as_ref(),
        labels: config.metadata.labels.as_ref(),
        annotations: config.metadata.annotations.as_ref(),
        pvc_name: pvc_name.as_deref(),
        default_source_path: nfs_source_path.as_deref(),
        source_path_override: source_path_override.as_deref(),
    };
    match kopiur_api::resolve_identity(&inputs) {
        Ok(r) => ResolvedIdentity {
            username: r.username,
            hostname: r.hostname,
            source_path: r.source_path.unwrap_or_default(),
        },
        // A malformed identity is already rejected at admission; fall back to a
        // sentinel so the quick tier still runs (it doesn't use the path).
        Err(_) => ResolvedIdentity {
            username: "kopiur-verify".to_string(),
            hostname: namespace.to_string(),
            source_path: String::new(),
        },
    }
}

/// Whether any non-terminal verify Job is owned by this policy (single-flight gate).
async fn has_active_verify_job(job_api: &Api<Job>, policy_name: &str) -> Result<bool> {
    let selector =
        format!("{COMPONENT_LABEL}={VERIFY_COMPONENT},{VERIFY_INSTANCE_LABEL}={policy_name}");
    let jobs = job_api
        .list(&ListParams::default().labels(&selector))
        .await?;
    Ok(jobs.items.iter().any(|j| job_terminal_state(j).is_none()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::common::CronSpec;
    use kopiur_api::snapshot_policy::DeepVerification;

    fn verification(quick: Option<&str>, deep: Option<&str>) -> Verification {
        Verification {
            quick: quick.map(|c| CronSpec {
                cron: c.into(),
                jitter: None,
                timezone: None,
            }),
            deep: deep.map(|c| DeepVerification {
                schedule: CronSpec {
                    cron: c.into(),
                    jitter: None,
                    timezone: None,
                },
                storage_class_name: None,
                capacity: None,
            }),
            success_expr: None,
            verify_files_percent: None,
        }
    }

    #[test]
    fn first_ever_reconcile_is_due_and_prefers_deep() {
        // No lastVerified → both due; deep wins (it subsumes quick).
        let v = verification(Some("*/5 * * * *"), Some("0 3 * * 0"));
        let (tier, _slot) = due_tier(&v, "seed", None, Utc::now(), None).expect("due");
        assert_eq!(tier, VerifyTierKind::Deep);
    }

    #[test]
    fn quick_only_is_due_when_no_deep() {
        let v = verification(Some("*/5 * * * *"), None);
        let (tier, _) = due_tier(&v, "seed", None, Utc::now(), None).expect("due");
        assert_eq!(tier, VerifyTierKind::Quick);
    }

    #[test]
    fn not_due_right_after_a_run() {
        let now = Utc::now();
        let just = now - chrono::Duration::seconds(1);
        let v = verification(Some("*/5 * * * *"), Some("0 3 * * 0"));
        assert!(
            due_tier(&v, "seed", Some(just), now, None).is_none(),
            "a tier that just ran must not be immediately due again"
        );
    }

    #[test]
    fn no_schedules_is_never_due() {
        let v = verification(None, None);
        assert!(due_tier(&v, "seed", None, Utc::now(), None).is_none());
    }

    #[test]
    fn wakeup_is_capped() {
        let now = Utc::now();
        let just = now - chrono::Duration::seconds(1);
        // Daily deep, ran moments ago → next ~24h out, but capped to the heartbeat.
        let v = verification(None, Some("0 3 * * *"));
        assert!(next_verify_wakeup(&v, "seed", Some(just), now, None) <= REQUEUE_CAP);
    }

    #[test]
    fn due_tier_honors_repo_schedule_default_timezone() {
        // A cron pinned to a specific wall-clock hour is due/not-due depending on
        // which timezone it's evaluated in — proof the repo default actually
        // reaches the scheduling decision, not just the CronSpec's own field.
        let now = DateTime::parse_from_rfc3339("2026-06-09T05:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let last_verified = Some(now - chrono::Duration::hours(2));
        // 05:00 UTC has already passed; 05:00 America/Los_Angeles (UTC-7 in June)
        // is still ~6.5h out. No own timezone on the CronSpec — must come from
        // the repo default.
        let v = verification(Some("0 5 * * *"), None);
        assert!(
            due_tier(&v, "seed", last_verified, now, None).is_some(),
            "UTC (no repo default) → 05:00 UTC has already passed"
        );
        assert!(
            due_tier(&v, "seed", last_verified, now, Some("America/Los_Angeles")).is_none(),
            "repo scheduleDefaults.timezone must shift the evaluated slot"
        );
    }

    #[test]
    fn due_tier_own_timezone_wins_over_repo_default() {
        let now = DateTime::parse_from_rfc3339("2026-06-09T05:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let last_verified = Some(now - chrono::Duration::hours(2));
        let mut v = verification(Some("0 5 * * *"), None);
        v.quick.as_mut().unwrap().timezone = Some("UTC".into());
        // Own timezone (UTC) says the slot is due; the repo default
        // (America/Los_Angeles, which would push the slot hours into the future)
        // must be ignored.
        assert!(
            due_tier(&v, "seed", last_verified, now, Some("America/Los_Angeles")).is_some(),
            "the CronSpec's own timezone must win over the repo default"
        );
    }

    #[test]
    fn verify_job_name_is_deterministic_and_bounded() {
        let slot = DateTime::parse_from_rfc3339("2026-06-09T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let n = verify_job_name("postgres-data", VerifyTierKind::Quick, slot);
        assert!(n.len() <= 52);
        assert!(n.starts_with("postgres-data-vfy-q-"));
        assert_eq!(
            n,
            verify_job_name("postgres-data", VerifyTierKind::Quick, slot)
        );
        // Quick vs deep differ.
        assert_ne!(
            n,
            verify_job_name("postgres-data", VerifyTierKind::Deep, slot)
        );
        // Long names truncate+hash within budget.
        let long = "a-very-long-snapshot-policy-name-that-blows-the-dns-label-budget";
        assert!(verify_job_name(long, VerifyTierKind::Deep, slot).len() <= 52);
    }

    // --- work-spec mapping (tier → VerifyOp) ---

    fn sample_policy(verification: Verification) -> SnapshotPolicy {
        use kopiur_api::snapshot_policy::{PvcSource, Source};
        SnapshotPolicy::new(
            "pg",
            kopiur_api::SnapshotPolicySpec {
                repository: kopiur_api::common::RepositoryRef {
                    kind: Default::default(),
                    name: "r".into(),
                    namespace: None,
                },
                identity: None,
                sources: vec![Source {
                    pvc: Some(PvcSource {
                        name: "data".into(),
                    }),
                    pvc_selector: None,
                    nfs: None,
                    source_path_override: None,
                    source_path_strategy: None,
                }],
                copy_method: Default::default(),
                volume_snapshot_class_name: None,
                group_by: None,
                retention: None,
                default_deletion_policy: None,
                compression: None,
                files: None,
                extra_args: vec![],
                error_handling: None,
                upload: None,
                verification: Some(verification),
                preflight: None,
                suspend: false,
                hooks: None,
                mover: None,
                credential_projection: None,
            },
        )
    }

    fn sample_repo() -> ResolvedRepository {
        use kopiur_api::backend::{Backend, FilesystemBackend};
        use kopiur_api::common::{Encryption, RepositoryMode, SecretKeyRef};
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
        }
    }

    #[test]
    fn quick_work_spec_carries_quick_tier_and_success_expr() {
        let mut v = verification(Some("0 4 * * *"), None);
        v.success_expr = Some("stats.errors == 0".into());
        v.verify_files_percent = Some(10);
        let policy = sample_policy(v.clone());
        let repo = sample_repo();
        let ws = build_verify_work_spec(&policy, &repo, "ns", "pg", &v, VerifyTierKind::Quick);
        match &ws.operation {
            Operation::Verify(op) => {
                assert_eq!(op.success_expr.as_deref(), Some("stats.errors == 0"));
                match &op.tier {
                    VerifyTier::Quick(q) => assert_eq!(q.verify_files_percent, Some(10)),
                    other => panic!("expected quick tier, got {}", other.kind_str()),
                }
            }
            other => panic!("expected verify op, got {}", other.kind_str()),
        }
        // The identity carries the recipe's resolved source path (/pvc/data).
        assert_eq!(ws.identity.source_path, "/pvc/data");
        assert_eq!(ws.target_ref.kind, "SnapshotPolicy");
    }

    #[test]
    fn deep_work_spec_carries_deep_tier_with_scratch_path() {
        let v = verification(None, Some("0 5 * * 0"));
        let policy = sample_policy(v.clone());
        let repo = sample_repo();
        let ws = build_verify_work_spec(&policy, &repo, "ns", "pg", &v, VerifyTierKind::Deep);
        match &ws.operation {
            Operation::Verify(op) => match &op.tier {
                VerifyTier::Deep(d) => {
                    assert_eq!(d.scratch_path, DEEP_SCRATCH_PATH);
                    assert!(d.snapshot_id.is_none());
                }
                other => panic!("expected deep tier, got {}", other.kind_str()),
            },
            other => panic!("expected verify op, got {}", other.kind_str()),
        }
    }

    // --- scratch volume resolution (capacity gates emptyDir vs ephemeral PVC) ---
    // The repo↔recipe merge itself is covered in `crate::cache` tests; here we test
    // the capacity gate over the already-merged effective `ScratchDefaults`.

    fn scratch(capacity: Option<&str>, storage_class: Option<&str>) -> ScratchDefaults {
        ScratchDefaults {
            storage_class_name: storage_class.map(Into::into),
            capacity: capacity.map(Into::into),
        }
    }

    #[test]
    fn scratch_volume_defaults_to_emptydir_when_capacity_unset() {
        // Zero-config: the common "just enable deep verify" case. An emptyDir is
        // writable by the non-root mover (like the kopia cache), fixing the
        // original `mkdir /scratch: permission denied`. Also covers no effective
        // config at all (None).
        assert_eq!(scratch_volume(None), CacheVolume::EmptyDir);
        assert_eq!(
            scratch_volume(Some(&scratch(None, None))),
            CacheVolume::EmptyDir
        );
    }

    #[test]
    fn scratch_volume_storageclass_without_capacity_is_still_emptydir() {
        // Mirrors resolve_cache_volume: capacity is the gate. storageClassName has
        // no effect on an emptyDir, so without capacity we stay emptyDir.
        assert_eq!(
            scratch_volume(Some(&scratch(None, Some("openebs-hostpath")))),
            CacheVolume::EmptyDir
        );
    }

    #[test]
    fn scratch_volume_with_capacity_is_a_sized_ephemeral_volume() {
        assert_eq!(
            scratch_volume(Some(&scratch(Some("50Gi"), Some("fast-ssd")))),
            CacheVolume::Ephemeral {
                capacity: "50Gi".into(),
                storage_class: Some("fast-ssd".into()),
            }
        );
        // capacity alone → cluster default storage class.
        assert_eq!(
            scratch_volume(Some(&scratch(Some("10Gi"), None))),
            CacheVolume::Ephemeral {
                capacity: "10Gi".into(),
                storage_class: None,
            }
        );
    }

    fn deep_verification(storage_class: Option<&str>, capacity: Option<&str>) -> Verification {
        Verification {
            quick: None,
            deep: Some(DeepVerification {
                schedule: CronSpec {
                    cron: "0 5 * * 0".into(),
                    jitter: None,
                    timezone: None,
                },
                storage_class_name: storage_class.map(Into::into),
                capacity: capacity.map(Into::into),
            }),
            success_expr: None,
            verify_files_percent: None,
        }
    }

    #[test]
    fn scratch_storage_class_state_flags_only_storageclass_without_capacity() {
        let repo = sample_repo(); // no moverDefaults.scratch

        // No verification.deep at all → no condition to surface.
        assert!(
            scratch_storage_class_state(&repo, &verification(Some("0 4 * * *"), None)).is_none()
        );

        // Zero-config deep (emptyDir) → honored (not ignored).
        assert!(
            !scratch_storage_class_state(&repo, &deep_verification(None, None))
                .unwrap()
                .ignored
        );

        // storageClass without capacity → ignored (the no-op the user must fix).
        let st =
            scratch_storage_class_state(&repo, &deep_verification(Some("fast-ssd"), None)).unwrap();
        assert!(st.ignored);
        assert!(st.message.contains("fast-ssd"));
        assert!(st.message.contains("capacity"));

        // storageClass + capacity → honored.
        assert!(
            !scratch_storage_class_state(&repo, &deep_verification(Some("fast-ssd"), Some("50Gi")))
                .unwrap()
                .ignored
        );
    }

    #[test]
    fn scratch_storage_class_state_honors_repo_supplied_capacity() {
        // Repo-level moverDefaults.scratch.capacity makes a policy-only storageClass
        // NOT a no-op — the merged result has a capacity. (Guards the webhook
        // false-positive we deliberately avoid by validating post-merge.)
        let mut repo = sample_repo();
        repo.mover_defaults = Some(kopiur_api::common::MoverDefaults {
            scratch: Some(ScratchDefaults {
                storage_class_name: None,
                capacity: Some("100Gi".into()),
            }),
            ..Default::default()
        });
        let st =
            scratch_storage_class_state(&repo, &deep_verification(Some("fast-ssd"), None)).unwrap();
        assert!(
            !st.ignored,
            "repo-supplied capacity should make the SC honored"
        );
    }
}
