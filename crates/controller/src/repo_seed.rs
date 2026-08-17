//! `spec.seed` — the controller half of seeding a brand-new repository from an
//! existing replica (issue #380).
//!
//! Shared by BOTH repository reconcilers, because everything here is identical
//! for a namespaced `Repository` and a cluster-scoped `ClusterRepository` once
//! the caller supplies the three namespaces that actually differ: the one the
//! bootstrap Job runs in, the one a namespace-less `seed.from.repository`
//! reference resolves in, and the one the seed backend's `tls.caBundleRef`
//! ConfigMap is read from. See [`SeedContext`].
//!
//! The load-bearing idea is the **seed-attempt marker**
//! (`kopiur_api::seed::SeedStatus::started_at`), stamped into `status.seed`
//! BEFORE a seeding bootstrap Job is created and never cleared. It is what
//! distinguishes "a seed this operator started did not finish" (resume the
//! copy) from "this backend was initialized by somebody else" (an ordinary
//! adoption, which keeps the no-clobber `AlreadyInitialized` no-op). A resuming
//! migrate re-runs `kopia snapshot migrate` into whatever repository is at the
//! backend and then re-stamps its maintenance owner, with no kopia-side
//! backstop — blob mode gets one for free, since `sync-to` refuses a
//! destination whose format blob differs from the source's — so the marker, and
//! nothing weaker, decides `resume`.

use k8s_openapi::api::core::v1::{EnvVar, EnvVarSource, SecretKeySelector};
use kube::api::Api;

use kopiur_api::backend::Backend;
use kopiur_api::common::{RepositoryKind, RepositoryRef};
use kopiur_api::seed::{SeedMode, SeedSource, SeedSpec, SeedStatus};
use kopiur_mover::bootstrap::SeedOutcome;
use kopiur_mover::jobs::VolumeMountSpec;
use kopiur_mover::repo_meta::{
    backend_to_repository_connect, filesystem_repo_mount_source, filesystem_repo_path,
};
use kopiur_mover::workspec::{
    SeedConnectSource, SeedMigrateSpec, SeedOpSpec, SeedRepositoryConnect, SeedSyncSpec,
};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::io::{self, ResolvedRepository};
use crate::jobs::{CredsEnvFrom, JobLimits};

/// The three namespaces (plus the owner) that are all a repository kind has to
/// supply for the seeding machinery to work on either kind.
pub(crate) struct SeedContext<'a> {
    /// Namespace the bootstrap Job runs in — where projected copies land and
    /// where every `envFrom` Secret must resolve.
    pub job_ns: &'a str,
    /// Namespace a `kind: Repository` seed reference with no explicit
    /// `namespace` resolves in: the repository's own for a namespaced
    /// `Repository`, the operator's for a cluster-scoped `ClusterRepository`
    /// (which has none) — the same rule its credential `secretRef`s follow.
    pub source_default_ns: &'a str,
    /// Referrer namespace for resolving a BLOB seed backend's
    /// `tls.caBundleRef` ConfigMap: `Some(namespace)` for a namespaced
    /// `Repository`, `None` for a `ClusterRepository` (which selects the
    /// operator-namespace arm of `io::resolve_backend_ca`).
    pub ca_referrer_ns: Option<&'a str>,
    /// The repository CR's own name (names the projected-Secret prefix and the
    /// park messages).
    pub name: &'a str,
    /// The repository CR's owner reference — carried by any projected copy, so
    /// GC reaps it with the repository.
    pub owner: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
    /// Which repository kind is being seeded — the co-resident-Secret rule
    /// differs (a `ClusterRepository`'s seed Secret must pin NO namespace),
    /// and it labels the defensive re-validation's messages.
    pub kind: RepositoryKind,
    /// This repository's own backend, for the defensive re-validation (the
    /// bare-path and mount-collision rules compare the two sides).
    pub repo_backend: &'a Backend,
    /// This repository's access mode — `spec.seed` on a `ReadOnly` repository
    /// is refused (the seed could never complete).
    pub repo_mode: kopiur_api::common::RepositoryMode,
    /// This repository's `spec.create`, for the inert-format-knob rule.
    pub repo_create: Option<&'a kopiur_api::common::CreateBehavior>,
}

/// What the seed-arming pass decided.
///
/// Closed enum + exhaustive `match` at the two call sites: "launch a seeding
/// Job", "park and re-check", and "there is no seed" are three genuinely
/// different reconcile outcomes, and a fourth cannot be added without both
/// reconcilers answering for it.
pub(crate) enum SeedArming {
    /// No seed is armed — build the bootstrap exactly as before #380.
    NotArmed,
    /// The seed cannot run yet: write `Seeded=False`/`WaitingForSeedSource`
    /// with this message and re-check. Nothing is launched.
    Park {
        /// The actionable park message (what is missing / why / how to fix).
        message: String,
    },
    /// Everything resolved: launch a seeding bootstrap Job with this payload.
    Armed(Box<ArmedSeed>),
}

/// A fully-resolved seed, ready to ride a bootstrap Job.
pub(crate) struct ArmedSeed {
    /// The mover payload (`BootstrapRepositoryOp::seed`).
    pub op: SeedOpSpec,
    /// Which copy mechanism this is — for the marker, the metric and the logs.
    pub mode: SeedMode,
    /// `SeedSource::describe()`, the ONE rendering of the source that reaches
    /// `status.seed.source`, the mover's outcome and the Event.
    pub source_description: String,
    /// EXTRA `envFrom` entries for the seed source, all under
    /// `KOPIUR_SEED_` so its keys cannot collide with this repository's
    /// identically-named ambient ones.
    pub creds: Vec<CredsEnvFrom>,
    /// `KOPIUR_SEED_KOPIA_PASSWORD` (migrate mode only — a blob mirror shares
    /// this repository's own password by construction).
    pub extra_env: Vec<EnvVar>,
    /// A filesystem seed source's volume, carried in the Job builder's spare
    /// `source_volume` slot (the bootstrap's own `repo_volume` slot is taken).
    /// Admission guarantees the two mount paths differ.
    pub source_volume: Option<VolumeMountSpec>,
    /// The seed source's backend, so the Job's run identity is resolved against
    /// BOTH backends — one pod touches two of them, and a workload identity on
    /// either names the ServiceAccount the pod runs as.
    pub source_backend: Backend,
    /// How many source Secrets were projected cross-namespace this pass (for
    /// `kopiur_secrets_projected`).
    pub projected: u64,
}

/// The migrate-mode seed source, resolved. Kept as a struct so [`seed_op_for`]
/// stays pure and testable without a cluster.
pub(crate) struct SeedSourceRepository<'a> {
    /// The source's kind (drives the wire `kind` string).
    pub kind: RepositoryKind,
    /// The source CR's name.
    pub name: &'a str,
    /// The source's resolved backend/encryption/CA surface.
    pub repo: &'a ResolvedRepository,
}

/// **Pure.** Build the mover's seed payload from `spec.seed`.
///
/// `blob_ca_bundle_pem` is the resolved `tls.caBundleRef` content of a BLOB
/// seed's inline backend — resolved by the async caller, exactly like the
/// repository's own bundle, because every kopia invocation carries its CA
/// inline. `source` is the resolved source repository for a MIGRATE seed (its
/// own CA rides `ResolvedRepository::ca_bundle_pem`).
///
/// Returns `None` when the pair is inconsistent — a migrate seed with no
/// resolved source. That state is unreachable from the caller (which resolves
/// or parks first) and is deliberately NOT a silent fallback: launching a seed
/// against a half-resolved source is the one mistake this whole feature exists
/// to avoid.
///
/// Exhaustive over [`SeedSource`]: a new source shape cannot compile until it
/// decides what the mover receives.
pub(crate) fn seed_op_for(
    seed: &SeedSpec,
    blob_ca_bundle_pem: Option<String>,
    source: Option<&SeedSourceRepository<'_>>,
    resume: bool,
) -> Option<SeedOpSpec> {
    let from = match (&seed.from, source) {
        (SeedSource::Backend(backend), _) => SeedConnectSource::Backend(Box::new(
            backend_to_repository_connect(backend, blob_ca_bundle_pem),
        )),
        (SeedSource::Repository(_), Some(src)) => {
            SeedConnectSource::Repository(Box::new(SeedRepositoryConnect {
                kind: io::repo_kind_str(src.kind).to_string(),
                name: src.name.to_string(),
                namespace: src.repo.repo_namespace.clone(),
                connect: backend_to_repository_connect(
                    &src.repo.backend,
                    src.repo.ca_bundle_pem.clone(),
                ),
            }))
        }
        (SeedSource::Repository(_), None) => return None,
    };
    Some(SeedOpSpec {
        from,
        // ONE renderer for the source string. The controller resolves a migrate
        // reference's namespace before this point, so a mover-side rendering
        // would print `Repository/backups/nas` where the CRD-side one prints
        // `Repository/nas` — two spellings of one repository across status,
        // logs and the CLI.
        source_description: seed.from.describe(),
        sync: seed.sync.map(|s| SeedSyncSpec {
            parallel: s.parallel,
            max_download_speed_bytes_per_second: s.max_download_speed_bytes_per_second,
            max_upload_speed_bytes_per_second: s.max_upload_speed_bytes_per_second,
        }),
        migrate: seed.migrate.map(|m| SeedMigrateSpec {
            parallel: m.parallel,
            latest_only: m.latest_only,
            policies: crate::snapshot_replication::policy_copy_mode_spec(m.policies),
        }),
        allow_empty_source: seed.allow_empty_source,
        resume,
    })
}

/// **Pure.** The bootstrap Job's limits when a seed is armed: the deadline from
/// `spec.seed.failurePolicy` (default 24h — a seed copies a whole repository
/// once, and the 120s a routine connect gets is orders of magnitude too short),
/// the backoff from the same block, and the repository's own TTL.
///
/// `armed == false` returns `base` untouched, so every non-seeding bootstrap —
/// including every later connect to the now-initialized repository — keeps the
/// short deadline byte-for-byte.
pub(crate) fn seed_job_limits(armed: bool, seed: Option<&SeedSpec>, base: JobLimits) -> JobLimits {
    let Some(seed) = seed.filter(|_| armed) else {
        return base;
    };
    JobLimits {
        active_deadline_seconds: Some(kopiur_api::seed::seed_active_deadline_seconds(seed)),
        backoff_limit: seed
            .failure_policy
            .as_ref()
            .and_then(|fp| fp.backoff_limit)
            .unwrap_or(base.backoff_limit),
        ttl_seconds_after_finished: base.ttl_seconds_after_finished,
    }
}

/// **Pure.** The `status.seed` marker patch stamped BEFORE a seeding bootstrap
/// Job is created, or `None` when nothing would change.
///
/// `started_at` is preserved across attempts rather than restamped: the marker
/// is a fact about the FIRST attempt, and rewriting it every relaunch would
/// churn `resourceVersion` (re-triggering this reconciler through its own
/// primary watch) for no information gained. `mode`/`source` follow the live
/// spec so a repointed seed says what it is now attempting.
///
/// Deliberately carries **no conditions**: the launch path's status patch is
/// conditions-free, and adding one here would replace the whole conditions
/// array from a possibly-stale cached copy.
pub(crate) fn seed_marker_patch(
    existing: Option<&SeedStatus>,
    mode: SeedMode,
    source_description: &str,
    now: &str,
) -> Option<serde_json::Value> {
    // A repository whose seed already COMPLETED never gets a new attempt
    // marker. `seed_armed` should already have kept us away (a finished seed
    // pinned `status.uniqueId` in the same patch that stamped `seededAt`), but
    // this function and `kopiur_api::seed::seed_resume` read the same two
    // fields and must not be able to contradict each other: a marker stamped
    // over a completed seed would make `seededAt` and `startedAt` describe two
    // different attempts, and the next reconcile would have to guess which.
    if existing.is_some_and(|s| s.seeded_at.as_deref().is_some_and(|t| !t.is_empty())) {
        return None;
    }
    let started_at = existing
        .and_then(|s| s.started_at.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or(now);
    let unchanged = existing.is_some_and(|s| {
        s.started_at.as_deref() == Some(started_at)
            && s.mode == Some(mode)
            && s.source.as_deref() == Some(source_description)
    });
    if unchanged {
        return None;
    }
    Some(serde_json::json!({
        "seed": {
            "startedAt": started_at,
            "mode": mode,
            "source": source_description,
        }
    }))
}

/// How a successful seed folds into the repository's status (issue #380).
pub(crate) struct SeedSuccess {
    /// The `status.seed` merge patch.
    pub status: serde_json::Value,
    /// `reason` for the `Seeded=True` condition: `Seeded` when data was copied,
    /// `AlreadyInitialized` for the standing no-op.
    pub reason: &'static str,
    /// The condition message.
    pub message: String,
    /// The metric `outcome` label.
    pub outcome: crate::metrics::SeedOutcomeLabel,
    /// The `RepositorySeeded` Event note, or `None` for the no-op (nothing
    /// happened, so nothing to announce).
    pub event: Option<String>,
}

/// **Pure.** Map the mover's [`SeedOutcome`] onto status, condition, metric and
/// Event.
///
/// `snapshotCount`/`snapshotsCopied` are mirrored verbatim rather than
/// recomputed: the outcome's counts are what the seed actually observed, and
/// `snapshotsCopied` is migrate-only by construction (a blob copy moves
/// storage, not manifests, so the post-seed catalog listing reports its
/// snapshot count on `status.storageStats` instead).
pub(crate) fn seed_success_fold(outcome: &SeedOutcome, now: &str) -> SeedSuccess {
    let mode = seed_mode_of(outcome);
    let mut status = serde_json::json!({
        "mode": mode,
        "source": outcome.source,
    });
    if outcome.performed {
        status["seededAt"] = serde_json::Value::String(now.to_string());
        if let Some(n) = outcome.snapshot_count {
            status["snapshotCount"] = serde_json::json!(n);
        }
        if let Some(n) = outcome.snapshots_copied {
            status["snapshotsCopied"] = serde_json::json!(n);
        }
        let copied = outcome
            .snapshots_copied
            .or(outcome.snapshot_count)
            .unwrap_or(0);
        let message = format!(
            "seeded this repository from {} ({} mode); {copied} snapshot(s) present",
            outcome.source,
            mode.as_str()
        );
        SeedSuccess {
            status: serde_json::json!({ "seed": status }),
            reason: kopiur_api::consts::SEEDED_REASON,
            message: message.clone(),
            outcome: crate::metrics::SeedOutcomeLabel::Seeded,
            event: Some(format!(
                "{message}. Re-apply your SnapshotPolicies to adopt this history — and review \
                 their retention and defaultDeletionPolicy first: GFS prunes beyond-budget \
                 restore points as soon as a policy adopts them."
            )),
        }
    } else {
        SeedSuccess {
            status: serde_json::json!({ "seed": status }),
            reason: kopiur_api::consts::ALREADY_INITIALIZED_REASON,
            message: format!(
                "spec.seed is a no-op: this repository was already initialized, so nothing was \
                 copied from {}",
                outcome.source
            ),
            outcome: crate::metrics::SeedOutcomeLabel::AlreadyInitialized,
            event: None,
        }
    }
}

/// **Pure.** Drop any `Seeded` condition from `conditions`.
///
/// Conditions are upsert-only everywhere else in this codebase, which is right
/// for a condition that always has a current answer — but `Seeded` does not:
/// once `spec.seed` is gone (or the bootstrap failed for a reason that says
/// nothing about the seed), there IS no true `Seeded` value, and leaving the
/// last one standing states something false. Three ways that bites, all of them
/// user-visible and none self-healing:
///
/// * a repository parked on `WaitingForSeedSource`, whose owner gives up,
///   removes `spec.seed` and enables `create` — it goes `Ready` carrying a
///   permanent phantom "blocked on Seeded=False";
/// * `spec.seed` removed while a seeding Job is in flight — the repository goes
///   `Ready` still claiming a copy is in progress;
/// * a seed-armed bootstrap that dies on a NON-seed failure (an `AuthFailure`
///   against this repository's own backend, say) — `Seeded=False/Seeding`
///   stands beside terminal `Failed`, telling the operator a copy is running
///   when nothing is.
///
/// A dropped condition is the honest answer in all three: `Bootstrapped` and
/// `Ready` already carry the real story.
pub(crate) fn drop_seed_condition(
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    conditions
        .iter()
        .filter(|c| c.type_ != kopiur_api::consts::SEEDED_CONDITION)
        .cloned()
        .collect()
}

/// **Pure.** Whether a previously-recorded `Seeded=False` reason is one of the
/// FAILURE reasons, as opposed to the park/progress ones.
///
/// The seeding-progress writer consults this so it leaves a recorded failure
/// standing instead of overwriting it with `Seeding` on every relaunch. Without
/// that, each ~2-minute retry cycle would rewrite the condition twice
/// (`<class>` → `Seeding` → `<class>`), making every cycle a fresh status
/// TRANSITION — and the failure Event and the
/// `kopiur_repository_seed_total{outcome="failed"}` increment are both gated on
/// exactly that, so a dead seed source would mint ~30 Events and ~30 counter
/// increments an hour instead of one per real change.
///
/// Keeping the failure reason visible is also the better report: "the last
/// attempt failed with X, and kopiur is retrying" beats "seeding", which is
/// what a bare relaunch would say.
pub(crate) fn seed_failure_recorded(
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> bool {
    conditions.iter().any(|c| {
        c.type_ == kopiur_api::consts::SEEDED_CONDITION
            && c.status == kopiur_api::gates::CONDITION_FALSE
            && kopiur_api::consts::SEED_FAILURE_REASONS.contains(&c.reason.as_str())
    })
}

/// The API-side [`SeedMode`] of a mover outcome. Exhaustive over the wire enum,
/// which is what keeps `status.seed.mode` and the metric label speaking the
/// same vocabulary as the CRD.
pub(crate) fn seed_mode_of(outcome: &SeedOutcome) -> SeedMode {
    match outcome.mode {
        kopiur_mover::workspec::SeedModeSpec::Blob => SeedMode::Blob,
        kopiur_mover::workspec::SeedModeSpec::Migrate => SeedMode::Migrate,
    }
}

/// **Pure.** The message for the `Seeded=False`/`Seeding` progress condition
/// written while a seeding bootstrap Job is in flight.
pub(crate) fn seeding_message(source_description: &str, deadline_secs: i64) -> String {
    format!(
        "copying this repository's initial contents from {source_description}; the repository \
         stays Pending until the copy finishes. A first seed transfers the whole repository, so \
         this legitimately runs for a long time — the seeding Job's deadline is {deadline_secs}s \
         (spec.seed.failurePolicy.activeDeadlineSeconds). Watch the bootstrap Job's pod logs for \
         progress."
    )
}

/// **Pure.** The park message when a migrate seed's SOURCE repository is
/// missing or not `Ready`.
pub(crate) fn waiting_for_seed_source_message(source_description: &str, why: &str) -> String {
    format!(
        "spec.seed copies this repository's initial contents from {source_description}, but \
         {why}, so there is nothing to copy from yet. This repository stays Pending and \
         re-checks; it will seed by itself once the source is usable. Fix: bring the source \
         repository up (check its own status/conditions), or point spec.seed.from.repository at \
         one that is."
    )
}

/// **Pure.** The park message for a migrate seed whose source is a BARE-PATH
/// filesystem repository.
///
/// Admission refuses a bare path for `seed.from.backend` and for the seeded
/// repository itself, but it cannot see through a `seed.from.repository`
/// reference to the source CR's backend — so this is the one bare-path arm that
/// has to be caught at reconcile time. It parks rather than launching because a
/// bare path is reachable only from the controller process: the seeding mover
/// would mount nothing, connect to a path that does not exist, and grind on
/// `SeedSourceNotFound` every two minutes forever.
pub(crate) fn bare_path_seed_source_message(source_description: &str, path: &str) -> String {
    format!(
        "spec.seed reads from {source_description}, whose backend is a BARE-PATH filesystem \
         repository (path `{path}` with no `volume`). Seeding runs in a mover Job, and a bare \
         path exists only on the controller's own filesystem — the Job would mount nothing and \
         find no repository there. Fix: give the source Repository a `backend.filesystem.volume` \
         (a PVC or an inline NFS export) so the mover can mount it, or seed from a backend \
         reachable over the network via spec.seed.from.backend."
    )
}

/// Resolve everything a seeding bootstrap Job needs, or decide to park.
///
/// The IO half of the arming pass, shared by both repository kinds:
///
/// 1. blob mode — resolve the seed backend's `tls.caBundleRef` (every kopia
///    invocation carries its CA inline) and verify its credential Secret is in
///    the Job's namespace, loaded under `KOPIUR_SEED_`;
/// 2. migrate mode — resolve the source repository CR, gate on it being
///    `Ready`, resolve (and optionally project) its credential Secrets under
///    `KOPIUR_SEED_`, and put its kopia password on
///    `KOPIUR_SEED_KOPIA_PASSWORD`;
/// 3. either mode — mount a filesystem source's volume in the Job's spare
///    volume slot.
///
/// `resume` comes from the durable seed-attempt marker and nothing else (see
/// `kopiur_api::seed::seed_resume`); it is threaded through rather than
/// re-derived here so the decision has exactly one home.
pub(crate) async fn arm_seed(
    ctx: &Context,
    seed: Option<&SeedSpec>,
    armed: bool,
    resume: bool,
    sctx: &SeedContext<'_>,
) -> Result<SeedArming> {
    let Some(seed) = seed.filter(|_| armed) else {
        return Ok(SeedArming::NotArmed);
    };
    // Defensive re-validation — one validator, two callers (the admission
    // webhook and here). A seeding bootstrap is the one bootstrap that WRITES
    // to a second repository, so a CR that reached etcd with the webhook
    // disabled (or downgraded, or bypassed) must not get there on the strength
    // of a rule nobody re-checked. Scoped to the seed rules and gated on
    // `spec.seed` being present, so it is a no-op for every repository that
    // predates #380.
    let errs = kopiur_api::validate::validate_repository_seed(
        Some(seed),
        sctx.repo_backend,
        sctx.repo_mode,
        sctx.repo_create,
        sctx.kind,
    );
    if let Some(first) = errs.into_iter().next() {
        return Err(Error::Validation(first.to_string()));
    }
    let source_description = seed.from.describe();
    match &seed.from {
        SeedSource::Backend(backend) => {
            arm_blob_seed(ctx, seed, backend, &source_description, sctx, resume).await
        }
        SeedSource::Repository(rref) => {
            arm_migrate_seed(ctx, seed, rref, &source_description, sctx, resume).await
        }
    }
}

/// Blob mode: a bare mirror backend, copied with `kopia repository sync-to`.
/// The mirror shares THIS repository's kopia password by construction (the copy
/// inherits its format), so only the STORAGE credentials ride the seed prefix.
async fn arm_blob_seed(
    ctx: &Context,
    seed: &SeedSpec,
    backend: &Backend,
    source_description: &str,
    sctx: &SeedContext<'_>,
    resume: bool,
) -> Result<SeedArming> {
    // The seed backend is inline on THIS CR, so its `tls.caBundleRef` resolves
    // against the same namespace the repository's own backend does.
    let ca_bundle_pem = io::resolve_backend_ca(
        &ctx.client,
        backend,
        sctx.ca_referrer_ns,
        ctx.operator_namespace.as_deref(),
    )
    .await?;
    // Static credentials must already co-reside with the Job (`envFrom` is
    // namespace-local, and admission enforces the co-residency rule); a
    // workload-identity or filesystem source carries none.
    let mut creds = Vec::new();
    if let Some(secret) = io::backend_auth_secret_ref(backend) {
        let names = [secret.name.clone()];
        io::ensure_creds_present(
            &ctx.client,
            sctx.job_ns,
            &io::CredsContext {
                secret_names: &names,
                repo_kind: "spec.seed source backend",
                repo_name: sctx.name,
                repo_secret_namespace: secret.namespace.as_deref(),
            },
        )
        .await?;
        creds.push(CredsEnvFrom::prefixed(
            secret.name.clone(),
            kopiur_api::creds::SEED_ENV_PREFIX,
        ));
    }
    let Some(op) = seed_op_for(seed, ca_bundle_pem, None, resume) else {
        return Err(Error::Invariant(
            "a blob seed resolved to no mover payload; this is a kopiur bug".into(),
        ));
    };
    Ok(SeedArming::Armed(Box::new(ArmedSeed {
        op,
        mode: SeedMode::Blob,
        source_description: source_description.to_string(),
        creds,
        extra_env: Vec::new(),
        source_volume: seed_source_volume(backend),
        source_backend: backend.clone(),
        projected: 0,
    })))
}

/// Migrate mode: another repository CR, copied with `kopia snapshot migrate`.
/// Two independently-encrypted repositories, so the source needs its OWN kopia
/// password (`KOPIUR_SEED_KOPIA_PASSWORD`) on top of its storage credentials.
async fn arm_migrate_seed(
    ctx: &Context,
    seed: &SeedSpec,
    rref: &RepositoryRef,
    source_description: &str,
    sctx: &SeedContext<'_>,
    resume: bool,
) -> Result<SeedArming> {
    // Missing and not-Ready are ONE park, deliberately: both are "the source is
    // not usable yet", both clear the same way, and a repository that is being
    // applied alongside this one passes through the first on its way to the
    // second.
    let source = match io::resolve_repository_ref_cached(ctx, rref, sctx.source_default_ns).await {
        Ok(s) => s,
        Err(Error::MissingDependency(_)) => {
            return Ok(SeedArming::Park {
                message: waiting_for_seed_source_message(
                    source_description,
                    "it does not exist (yet)",
                ),
            });
        }
        Err(e) => return Err(e),
    };
    if !io::repository_ready_cached(ctx, rref, sctx.source_default_ns).await? {
        return Ok(SeedArming::Park {
            message: waiting_for_seed_source_message(source_description, "it is not Ready"),
        });
    }
    // The one bare-path arm admission cannot see: it would have to read the
    // SOURCE CR to know its backend shape.
    if let Some(path) = filesystem_repo_path(&source.backend)
        && filesystem_repo_mount_source(&source.backend).is_none()
    {
        return Ok(SeedArming::Park {
            message: bare_path_seed_source_message(source_description, &path),
        });
    }
    let consumer_enabled = seed
        .credential_projection
        .as_ref()
        .is_some_and(|p| p.enabled);
    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        sctx.job_ns,
        &io::CredsPrefix::seed(sctx.name),
        &sctx.owner,
        &source,
        consumer_enabled,
        io::repo_kind_str(rref.kind),
        &rref.name,
    )
    .await?;
    // Belt-and-braces before launching a Job that would otherwise hang on a
    // missing-Secret `envFrom` (the resolved names: verbatim, or the projected
    // copies' names when projection renamed them).
    io::ensure_creds_present(
        &ctx.client,
        sctx.job_ns,
        &io::CredsContext {
            secret_names: &creds.names,
            repo_kind: io::repo_kind_str(rref.kind),
            repo_name: &rref.name,
            repo_secret_namespace: source.encryption.password_secret_ref.namespace.as_deref(),
        },
    )
    .await?;
    let extra_env = vec![seed_password_env(&creds.names, &source)?];
    let source_repo = SeedSourceRepository {
        kind: rref.kind,
        name: &rref.name,
        repo: &source,
    };
    let Some(op) = seed_op_for(seed, None, Some(&source_repo), resume) else {
        return Err(Error::Invariant(
            "a migrate seed with a resolved source produced no mover payload; this is a kopiur \
             bug"
            .into(),
        ));
    };
    let source_volume = seed_source_volume(&source.backend);
    Ok(SeedArming::Armed(Box::new(ArmedSeed {
        op,
        mode: SeedMode::Migrate,
        source_description: source_description.to_string(),
        creds: creds
            .names
            .into_iter()
            .map(|n| CredsEnvFrom::prefixed(n, kopiur_api::creds::SEED_ENV_PREFIX))
            .collect(),
        extra_env,
        source_volume,
        source_backend: source.backend.clone(),
        projected: creds.projected,
    })))
}

/// A filesystem seed source's volume, mounted read-only at its own path. Object
/// stores reach the backend over the network and mount nothing; a bare path
/// never reaches here (admission refuses it for a backend source, and
/// [`arm_migrate_seed`] parks on it for a repository source).
///
/// Read-only because seeding never writes to the source, in either mode —
/// `sync-to` pushes FROM the connected source, and `snapshot migrate` reads it.
fn seed_source_volume(backend: &Backend) -> Option<VolumeMountSpec> {
    filesystem_repo_mount_source(backend).map(|source| VolumeMountSpec {
        source,
        mount_path: filesystem_repo_path(backend).unwrap_or_default(),
        read_only: true,
    })
}

/// **Pure.** The `KOPIUR_SEED_KOPIA_PASSWORD` Job env var: a
/// `valueFrom.secretKeyRef` on the RESOLVED source password Secret (the
/// projected copy's name when projection renamed it), so the plaintext never
/// rides the Job spec.
///
/// Index 0 is the password ref: `io::mover_creds_secret_refs` yields it FIRST
/// (order-stable, pinned by its own tests) and `resolve_mover_creds` preserves
/// ref order — the same contract `snapshot_replication::dest_password_ref`
/// relies on.
fn seed_password_env(resolved_names: &[String], source: &ResolvedRepository) -> Result<EnvVar> {
    let name = resolved_names.first().cloned().ok_or_else(|| {
        Error::Invariant(
            "seed source credential resolution yielded no Secret names — the encryption password \
             Secret is mandatory, so this is a kopiur bug"
                .into(),
        )
    })?;
    let key = source
        .encryption
        .password_secret_ref
        .key
        .clone()
        .unwrap_or_else(|| io::DEFAULT_PASSWORD_KEY.to_string());
    Ok(EnvVar {
        name: kopiur_api::creds::SEED_KOPIA_PASSWORD_ENV.to_string(),
        value: None,
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name,
                key,
                optional: Some(false),
            }),
            ..Default::default()
        }),
    })
}

/// Reap the projected copies of a migrate seed's SOURCE credentials, once the
/// seeding Job can no longer read them (the seed finalized, either way).
///
/// Best-effort and idempotent, like every other projection reap: it runs
/// unconditionally on a finalized seed rather than only when projection was
/// opted in, so a copy left behind by a since-disabled opt-in is still cleaned
/// up. Never fails the reconcile.
pub(crate) async fn reap_seed_projection(
    ctx: &Context,
    job_ns: &str,
    repo_name: &str,
    owner_uid: &str,
) {
    let secrets: Api<k8s_openapi::api::core::v1::Secret> =
        Api::namespaced(ctx.client.clone(), job_ns);
    let prefix = io::CredsPrefix::seed(repo_name);
    let outcome = io::reap_projection(&secrets, &prefix, owner_uid, job_ns, "seed finished").await;
    if outcome.deleted > 0 {
        tracing::info!(
            repo = %repo_name,
            deleted = outcome.deleted,
            "reaped projected seed-source credential copies"
        );
    }
}
/// **Pure.** Merge a `{"seed": {…}}` fold into an assembled status patch.
///
/// `status_patch` is built key-by-key before ONE `patch_status_if_changed`, so
/// two `status_patch["seed"] = …` statements would not merge — the second would
/// drop the first outright, silently. This is the same hazard the `parameters`
/// block in `finalize_bootstrap` documents, given a named helper because the
/// seed fold and any future one both have to respect it.
pub(crate) fn merge_seed_status(status_patch: &mut serde_json::Value, fold: &serde_json::Value) {
    let Some(seed) = fold.get("seed") else {
        return;
    };
    match status_patch.get_mut("seed") {
        Some(existing) => {
            if let (Some(existing), Some(add)) = (existing.as_object_mut(), seed.as_object()) {
                for (k, v) in add {
                    existing.insert(k.clone(), v.clone());
                }
            }
        }
        None => {
            status_patch["seed"] = seed.clone();
        }
    }
}

/// Count a FAILED seed on `kopiur_repository_seed_total` (#380), if a seed was
/// armed at all.
///
/// `mode` is read from the live `spec.seed` rather than from the mover's
/// outcome, because a failure deliberately carries none: a half-populated
/// outcome would read as a seed that partly worked. The mode is a property of
/// the source shape, so the spec is an honest source for it.
///
/// Called only from a GUARDED write's `wrote` branch, so a failure that keeps
/// being re-confirmed counts once per real transition, not once per requeue.
pub(crate) fn record_seed_failure(
    metrics: &crate::metrics::Metrics,
    seed: Option<&SeedSpec>,
    kind: &str,
    ns: &str,
    name: &str,
    armed: bool,
) {
    let Some(seed) = seed.filter(|_| armed) else {
        return;
    };
    metrics.inc_repository_seed(
        kind,
        ns,
        name,
        seed.from.mode(),
        crate::metrics::SeedOutcomeLabel::Failed,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::common::Encryption;
    use kopiur_mover::workspec::{PolicyCopyModeSpec, RepositoryConnect, SeedModeSpec};

    /// Build a `SeedSpec` the way the apiserver hands one over: a JSON value
    /// (what a decoded CR body is) into the typed struct. Never `serde_yaml`
    /// straight into a typed value — 0.9 mis-encodes externally-tagged enums,
    /// and `from.backend` is one.
    fn seed_spec(v: serde_json::Value) -> SeedSpec {
        serde_json::from_value(v).expect("typed")
    }

    fn resolved_source(backend: Backend, namespace: Option<&str>) -> ResolvedRepository {
        ResolvedRepository {
            backend,
            encryption: Encryption {
                password_secret_ref: kopiur_api::common::SecretKeyRef {
                    name: "src-pw".into(),
                    namespace: namespace.map(str::to_string),
                    key: None,
                },
            },
            repo_namespace: namespace.map(str::to_string),
            mover_defaults: None,
            identity_defaults: None,
            schedule_defaults: None,
            on_namespace_delete: Default::default(),
            credential_projection_allowed: false,
            mode: Default::default(),
            owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                api_version: "kopiur.home-operations.com/v1alpha1".into(),
                kind: "Repository".into(),
                name: "nas".into(),
                uid: "uid-src".into(),
                ..Default::default()
            },
            deletion_protection: None,
            mass_deletion_ack: None,
            catalog: None,
            ca_bundle_pem: Some("SOURCE-CA".into()),
        }
    }

    fn s3(bucket: &str) -> Backend {
        serde_json::from_value(serde_json::json!({ "s3": { "bucket": bucket } })).expect("backend")
    }

    #[test]
    fn seed_op_for_covers_both_source_shapes_and_carries_one_source_rendering() {
        // BLOB: the inline backend + the CA bundle the async caller resolved.
        // Every kopia invocation carries its CA inline, so a seed backend whose
        // bundle was dropped would fail TLS against a private-CA endpoint.
        let blob = seed_spec(serde_json::json!({
            "from": { "backend": { "s3": { "bucket": "offsite" } } },
            "sync": { "parallel": 8 },
        }));
        let op = seed_op_for(&blob, Some("SEED-CA".into()), None, false).expect("blob op");
        assert_eq!(op.mode(), SeedModeSpec::Blob);
        assert_eq!(op.source_description, "S3");
        assert_eq!(op.sync.and_then(|s| s.parallel), Some(8));
        assert!(op.migrate.is_none());
        assert!(!op.resume);
        match op.from {
            SeedConnectSource::Backend(b) => match *b {
                RepositoryConnect::S3 {
                    bucket,
                    ca_bundle_pem,
                    ..
                } => {
                    assert_eq!(bucket, "offsite");
                    assert_eq!(ca_bundle_pem.as_deref(), Some("SEED-CA"));
                }
                other => panic!("expected an S3 connect, got {other:?}"),
            },
            other => panic!("expected a backend source, got {other:?}"),
        }

        // MIGRATE: the resolved source repository, and its OWN CA bundle (which
        // rides `ResolvedRepository`, not the blob parameter).
        let migrate = seed_spec(serde_json::json!({
            "from": { "repository": { "name": "offsite" } },
            "migrate": { "latestOnly": true },
        }));
        let src = resolved_source(s3("source-bucket"), Some("backups"));
        let source = SeedSourceRepository {
            kind: RepositoryKind::Repository,
            name: "offsite",
            repo: &src,
        };
        let op = seed_op_for(&migrate, None, Some(&source), true).expect("migrate op");
        assert_eq!(op.mode(), SeedModeSpec::Migrate);
        assert!(op.resume, "resume must ride the op verbatim");
        // The ONE rendering: `describe()` on the CR's own spec, NOT the
        // resolved namespace. Re-deriving it mover-side would print
        // `Repository/backups/offsite` here and `Repository/offsite` in the
        // CRD-side status — two spellings of one repository.
        assert_eq!(op.source_description, "Repository/offsite");
        assert!(op.sync.is_none());
        let m = op.migrate.expect("migrate tuning");
        assert!(m.latest_only);
        // kopia's own default COPIES policies; an imported retention policy
        // would delete manifests behind the operator's back.
        assert_eq!(m.policies, PolicyCopyModeSpec::None);
        match op.from {
            SeedConnectSource::Repository(r) => {
                assert_eq!(r.kind, "Repository");
                assert_eq!(r.name, "offsite");
                // The RESOLVED namespace, so logs/messages name the real object.
                assert_eq!(r.namespace.as_deref(), Some("backups"));
                match r.connect {
                    RepositoryConnect::S3 { ca_bundle_pem, .. } => {
                        assert_eq!(ca_bundle_pem.as_deref(), Some("SOURCE-CA"));
                    }
                    other => panic!("expected an S3 connect, got {other:?}"),
                }
            }
            other => panic!("expected a repository source, got {other:?}"),
        }
    }

    #[test]
    fn a_migrate_seed_without_a_resolved_source_yields_no_payload() {
        // Unreachable from the caller (it resolves or parks first) and
        // deliberately NOT a silent fallback: launching a seed against a
        // half-resolved source is the one mistake this feature exists to avoid.
        let migrate =
            seed_spec(serde_json::json!({ "from": { "repository": { "name": "offsite" } } }));
        assert!(seed_op_for(&migrate, None, None, false).is_none());
    }

    #[test]
    fn only_an_armed_seed_changes_the_bootstrap_job_limits() {
        let base = JobLimits {
            active_deadline_seconds: Some(120),
            backoff_limit: 2,
            ttl_seconds_after_finished: Some(3600),
        };
        let seed =
            seed_spec(serde_json::json!({ "from": { "repository": { "name": "offsite" } } }));

        // Not armed: byte-for-byte the pre-#380 limits, even with a seed in
        // spec — that is every connect to an already-initialized repository.
        let unarmed = seed_job_limits(false, Some(&seed), base);
        assert_eq!(unarmed.active_deadline_seconds, Some(120));
        assert_eq!(unarmed.backoff_limit, 2);

        // Armed, no failurePolicy: the 24h seed default, base backoff + TTL.
        let armed = seed_job_limits(true, Some(&seed), base);
        assert_eq!(
            armed.active_deadline_seconds,
            Some(kopiur_api::seed::DEFAULT_SEED_BOOTSTRAP_DEADLINE_SECS)
        );
        assert_eq!(armed.backoff_limit, 2);
        assert_eq!(armed.ttl_seconds_after_finished, Some(3600));

        // Armed with an explicit policy: it wins on both knobs.
        let tuned = seed_spec(serde_json::json!({
            "from": { "repository": { "name": "offsite" } },
            "failurePolicy": { "activeDeadlineSeconds": 43200, "backoffLimit": 5 },
        }));
        let armed = seed_job_limits(true, Some(&tuned), base);
        assert_eq!(armed.active_deadline_seconds, Some(43_200));
        assert_eq!(armed.backoff_limit, 5);

        // No seed at all: untouched (JobLimits is not PartialEq, so compare
        // the three fields the helper can touch).
        let untouched = seed_job_limits(true, None, base);
        assert_eq!(
            untouched.active_deadline_seconds,
            base.active_deadline_seconds
        );
        assert_eq!(untouched.backoff_limit, base.backoff_limit);
        assert_eq!(
            untouched.ttl_seconds_after_finished,
            base.ttl_seconds_after_finished
        );
    }

    #[test]
    fn the_attempt_marker_is_stamped_once_and_never_over_a_finished_seed() {
        let now = "2026-08-17T00:00:00+00:00";
        // First attempt: stamped with `now` plus what is being attempted.
        let patch = seed_marker_patch(None, SeedMode::Migrate, "Repository/offsite", now)
            .expect("first attempt stamps a marker");
        assert_eq!(patch["seed"]["startedAt"], serde_json::json!(now));
        assert_eq!(patch["seed"]["mode"], serde_json::json!("migrate"));
        assert_eq!(
            patch["seed"]["source"],
            serde_json::json!("Repository/offsite")
        );

        // A RELAUNCH preserves the original timestamp — the marker is a fact
        // about the first attempt, and rewriting it every retry would churn
        // resourceVersion and re-trigger the reconciler through its own watch.
        let existing = SeedStatus {
            started_at: Some("2026-08-16T00:00:00+00:00".into()),
            mode: Some(SeedMode::Migrate),
            source: Some("Repository/offsite".into()),
            ..SeedStatus::default()
        };
        assert!(
            seed_marker_patch(
                Some(&existing),
                SeedMode::Migrate,
                "Repository/offsite",
                now
            )
            .is_none(),
            "an unchanged marker must be a no-op, not a rewrite"
        );

        // A REPOINTED seed says what it is now attempting, keeping the original
        // attempt time (the backend may still hold the first attempt's leavings).
        let repointed = seed_marker_patch(Some(&existing), SeedMode::Blob, "S3", now)
            .expect("a repointed seed updates mode/source");
        assert_eq!(
            repointed["seed"]["startedAt"],
            serde_json::json!("2026-08-16T00:00:00+00:00")
        );
        assert_eq!(repointed["seed"]["mode"], serde_json::json!("blob"));

        // A COMPLETED seed never gets a new marker: `startedAt` and `seededAt`
        // must always describe the same attempt, or the resume decision has to
        // guess which.
        let done = SeedStatus {
            seeded_at: Some("2026-08-16T02:00:00+00:00".into()),
            ..existing.clone()
        };
        assert!(seed_marker_patch(Some(&done), SeedMode::Blob, "S3", now).is_none());
    }

    #[test]
    fn the_resume_matrix_the_controller_relies_on() {
        // The four states the reconciler distinguishes, spelled out against the
        // SAME two fields the marker patch writes — this is the guard that
        // stops a migrate-resume writing into a repository kopiur never began
        // seeding.
        use kopiur_api::seed::{seed_armed, seed_resume};
        let armed = |uid: Option<&str>| {
            seed_armed(
                Some(&seed_spec(
                    serde_json::json!({ "from": { "repository": { "name": "offsite" } } }),
                )),
                uid,
            )
        };
        let marker = |started: Option<&str>, seeded: Option<&str>| SeedStatus {
            started_at: started.map(str::to_string),
            seeded_at: seeded.map(str::to_string),
            ..SeedStatus::default()
        };

        // (1) FRESH seed: armed, no marker ⇒ no resume.
        assert!(armed(None));
        assert!(!seed_resume(armed(None), None));

        // (2) RETRY after a recorded attempt ⇒ resume.
        let attempted = marker(Some("2026-08-17T00:00:00Z"), None);
        assert!(seed_resume(armed(None), Some(&attempted)));

        // (3) ALREADY SEEDED: not armed at all (uniqueId pinned), and not a
        //     resume even if something asked.
        assert!(!armed(Some("uid-1")));
        assert!(!seed_resume(armed(Some("uid-1")), Some(&attempted)));

        // (4) ADOPTED repository — `spec.seed` standing in a GitOps manifest
        //     over a backend somebody else initialized. Armed (no uniqueId
        //     yet), but NO marker, so no resume: the mover reports the
        //     documented AlreadyInitialized no-op instead of copying over it.
        let adopted = marker(None, None);
        assert!(armed(None));
        assert!(!seed_resume(armed(None), Some(&adopted)));
    }

    #[test]
    fn a_seed_interrupted_by_suspend_still_resumes_when_the_repository_wakes() {
        // `spec.suspend` returns before the bootstrap path entirely, so nothing
        // clears the marker: the Job keeps running, its result is consumed on
        // resume, and a relaunch after an interrupted attempt still resumes.
        use kopiur_api::seed::seed_resume;
        let mid_seed = SeedStatus {
            started_at: Some("2026-08-17T00:00:00Z".into()),
            mode: Some(SeedMode::Blob),
            source: Some("S3".into()),
            ..SeedStatus::default()
        };
        assert!(seed_resume(true, Some(&mid_seed)));
        // ...and the marker patch stays a no-op across the suspend/resume, so
        // waking the repository does not churn its status.
        assert!(
            seed_marker_patch(
                Some(&mid_seed),
                SeedMode::Blob,
                "S3",
                "2026-08-18T00:00:00Z"
            )
            .is_none()
        );
    }

    #[test]
    fn the_success_fold_distinguishes_a_real_seed_from_the_standing_no_op() {
        let now = "2026-08-17T03:00:00+00:00";
        // A real MIGRATE copy: seededAt + both counts + the Seeded reason, and
        // an Event that warns about adoption-time pruning.
        let performed = SeedOutcome::performed(
            SeedModeSpec::Migrate,
            "Repository/offsite".into(),
            412,
            Some(412),
        );
        let fold = seed_success_fold(&performed, now);
        assert_eq!(fold.reason, kopiur_api::consts::SEEDED_REASON);
        assert_eq!(fold.outcome, crate::metrics::SeedOutcomeLabel::Seeded);
        assert_eq!(fold.status["seed"]["seededAt"], serde_json::json!(now));
        assert_eq!(fold.status["seed"]["mode"], serde_json::json!("migrate"));
        assert_eq!(fold.status["seed"]["snapshotCount"], serde_json::json!(412));
        assert_eq!(
            fold.status["seed"]["snapshotsCopied"],
            serde_json::json!(412)
        );
        let event = fold.event.expect("a real seed announces itself");
        assert!(event.contains("retention"), "{event}");
        assert!(event.contains("SnapshotPolic"), "{event}");

        // A BLOB copy leaves `snapshotsCopied` unset — it moves storage, not
        // manifests — and the post-seed catalog listing reports the count on
        // storageStats instead.
        let blob = SeedOutcome::performed(SeedModeSpec::Blob, "S3".into(), 7, None);
        let fold = seed_success_fold(&blob, now);
        assert_eq!(fold.status["seed"]["snapshotCount"], serde_json::json!(7));
        assert!(fold.status["seed"].get("snapshotsCopied").is_none());

        // The STANDING NO-OP: no seededAt, no counts (nothing was opened, so
        // reporting 0 would be a lie), its own reason and metric label, and NO
        // Event — nothing happened.
        let noop = SeedOutcome::already_initialized(SeedModeSpec::Blob, "S3".into());
        let fold = seed_success_fold(&noop, now);
        assert_eq!(fold.reason, kopiur_api::consts::ALREADY_INITIALIZED_REASON);
        assert_eq!(
            fold.outcome,
            crate::metrics::SeedOutcomeLabel::AlreadyInitialized
        );
        assert!(fold.status["seed"].get("seededAt").is_none());
        assert!(fold.status["seed"].get("snapshotCount").is_none());
        assert!(fold.event.is_none());
    }

    #[test]
    fn merging_the_seed_fold_never_drops_a_key_already_in_the_patch() {
        // `status_patch` is assembled key-by-key before ONE patch, so a plain
        // assignment would drop whatever `seed` already held.
        let mut patch = serde_json::json!({
            "phase": "Ready",
            "seed": { "startedAt": "2026-08-17T00:00:00Z" },
        });
        let fold = seed_success_fold(
            &SeedOutcome::performed(SeedModeSpec::Blob, "S3".into(), 3, None),
            "2026-08-17T03:00:00Z",
        );
        merge_seed_status(&mut patch, &fold.status);
        assert_eq!(
            patch["seed"]["startedAt"],
            serde_json::json!("2026-08-17T00:00:00Z"),
            "the marker survives the success fold"
        );
        assert_eq!(
            patch["seed"]["seededAt"],
            serde_json::json!("2026-08-17T03:00:00Z")
        );
        assert_eq!(patch["phase"], serde_json::json!("Ready"));

        // No prior `seed` key: the fold lands whole.
        let mut patch = serde_json::json!({ "phase": "Ready" });
        merge_seed_status(&mut patch, &fold.status);
        assert_eq!(patch["seed"]["mode"], serde_json::json!("blob"));

        // A fold with no `seed` key is inert (defensive: the helper is the only
        // writer of this sub-object).
        let mut patch = serde_json::json!({ "phase": "Ready" });
        merge_seed_status(&mut patch, &serde_json::json!({}));
        assert!(patch.get("seed").is_none());
    }

    #[test]
    fn every_seed_park_and_progress_message_says_what_why_and_how_to_fix_it() {
        let messages = [
            seeding_message("S3", 86_400),
            waiting_for_seed_source_message("Repository/offsite", "it is not Ready"),
            bare_path_seed_source_message("Repository/offsite", "/srv/kopia"),
        ];
        for m in &messages {
            // The what/why/fix rule, plus the C1 wrapped-whitespace regression:
            // a Rust line continuation authored badly leaves a run of spaces in
            // the rendered string, which users see verbatim in a condition.
            assert!(!m.contains("   "), "wrapped source whitespace in: {m}");
            assert!(m.len() > 80, "too terse to be actionable: {m}");
        }
        assert!(messages[0].contains("86400"), "{}", messages[0]);
        assert!(
            messages[0].contains("spec.seed.failurePolicy"),
            "{}",
            messages[0]
        );
        assert!(messages[1].contains("not Ready"), "{}", messages[1]);
        assert!(messages[1].contains("Fix:"), "{}", messages[1]);
        assert!(messages[2].contains("volume"), "{}", messages[2]);
        assert!(messages[2].contains("Fix:"), "{}", messages[2]);
    }

    #[test]
    fn the_defensive_validator_refuses_the_seeds_admission_would_have() {
        // One validator, two callers. The reconcile-side call is what stands
        // between a CR that reached etcd with the webhook disabled and a
        // seeding mover pointed somewhere it must never go — so pin that the
        // shared validator actually bites on the reconcile-side inputs.
        use kopiur_api::backend::FilesystemBackend;
        let bare = Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        });
        let seed = seed_spec(serde_json::json!({
            "from": { "repository": { "name": "offsite" } }
        }));

        // A bare-path repository can never be seeded: the mover mounts nothing.
        let errs = kopiur_api::validate::validate_repository_seed(
            Some(&seed),
            &bare,
            Default::default(),
            None,
            RepositoryKind::Repository,
        );
        assert!(!errs.is_empty(), "a bare-path repository must be refused");

        // A ReadOnly repository can never complete a seed.
        let errs = kopiur_api::validate::validate_repository_seed(
            Some(&seed),
            &s3("target"),
            kopiur_api::common::RepositoryMode::ReadOnly,
            None,
            RepositoryKind::Repository,
        );
        assert!(!errs.is_empty(), "a ReadOnly repository must be refused");

        // ...and the ordinary shape is accepted, so the gate is not vacuous.
        let errs = kopiur_api::validate::validate_repository_seed(
            Some(&seed),
            &s3("target"),
            Default::default(),
            None,
            RepositoryKind::Repository,
        );
        assert!(errs.is_empty(), "{errs:?}");
    }

    fn cond(
        type_: &str,
        status: &str,
        reason: &str,
    ) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
        k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
            type_: type_.into(),
            status: status.into(),
            reason: reason.into(),
            message: "m".into(),
            observed_generation: None,
            last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ),
        }
    }

    /// The three ways a stale `Seeded=False` used to survive onto a repository
    /// it no longer describes. Conditions are upsert-only everywhere else, so
    /// each of these left a permanent, self-contradicting block that no later
    /// reconcile could clear.
    #[test]
    fn a_seeded_condition_that_no_longer_describes_anything_is_dropped() {
        use kopiur_api::consts as c;
        let keep = cond("Bootstrapped", "True", "Bootstrapped");
        let ready = cond("Ready", "True", "Bootstrapped");

        // (a) parked on WaitingForSeedSource, then the owner removes spec.seed
        //     and enables create — the repository goes Ready, and without the
        //     drop it carries a permanent phantom "blocked on Seeded=False".
        let parked = vec![
            keep.clone(),
            cond(
                c::SEEDED_CONDITION,
                "False",
                c::WAITING_FOR_SEED_SOURCE_REASON,
            ),
            ready.clone(),
        ];
        let out = drop_seed_condition(&parked);
        assert!(!out.iter().any(|x| x.type_ == c::SEEDED_CONDITION));
        assert_eq!(out.len(), 2, "only the Seeded condition is removed");
        assert_eq!(out[0].type_, "Bootstrapped");
        assert_eq!(out[1].type_, "Ready");

        // (b) spec.seed removed MID-FLIGHT: the running Job finishes, its
        //     result carries no seed outcome, and the repository would go Ready
        //     still claiming a copy is in progress.
        let seeding = vec![
            keep.clone(),
            cond(c::SEEDED_CONDITION, "False", c::SEEDING_REASON),
        ];
        assert!(
            !drop_seed_condition(&seeding)
                .iter()
                .any(|x| x.type_ == c::SEEDED_CONDITION)
        );

        // (c) a seed-armed bootstrap dying on a NON-seed failure: an
        //     `AuthFailure` against this repository's own backend says nothing
        //     about the seed, so `Seeded=False/Seeding` beside terminal
        //     `Failed` claims a copy is running when nothing is.
        let mid_seed_auth_failure = vec![
            cond("Bootstrapped", "False", "AuthFailure"),
            cond(c::SEEDED_CONDITION, "False", c::SEEDING_REASON),
        ];
        let out = drop_seed_condition(&mid_seed_auth_failure);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reason, "AuthFailure", "the real story survives");

        // Idempotent, and a no-op when there is nothing to drop.
        assert_eq!(drop_seed_condition(&out).len(), 1);
        assert!(drop_seed_condition(&[]).is_empty());
    }

    /// MINOR-3: the relaunch must NOT overwrite a recorded failure with
    /// `Seeding`, or every ~2-minute retry cycle becomes a fresh status
    /// transition — and the failure Event and the `failed` metric are gated on
    /// exactly that.
    #[test]
    fn a_recorded_seed_failure_suppresses_the_seeding_rewrite() {
        use kopiur_api::consts as c;
        // Every failure reason this build can write suppresses it...
        for reason in c::SEED_FAILURE_REASONS {
            assert!(
                seed_failure_recorded(&[cond(c::SEEDED_CONDITION, "False", reason)]),
                "{reason} must suppress the Seeding rewrite"
            );
        }
        // ...and the park/progress reasons do NOT (a park must be allowed to
        // advance to Seeding when the Job finally launches, and a re-poll of an
        // in-flight seed must be free to refresh its own message).
        assert!(!seed_failure_recorded(&[cond(
            c::SEEDED_CONDITION,
            "False",
            c::WAITING_FOR_SEED_SOURCE_REASON
        )]));
        assert!(!seed_failure_recorded(&[cond(
            c::SEEDED_CONDITION,
            "False",
            c::SEEDING_REASON
        )]));
        // A SUCCEEDED seed does not suppress anything either (polarity matters:
        // the reason set is only meaningful at `False`).
        assert!(!seed_failure_recorded(&[cond(
            c::SEEDED_CONDITION,
            "True",
            c::SEEDED_REASON
        )]));
        // Nor does a same-named reason on a DIFFERENT condition.
        assert!(!seed_failure_recorded(&[cond(
            "Bootstrapped",
            "False",
            c::SEED_SOURCE_EMPTY_REASON
        )]));
        assert!(!seed_failure_recorded(&[]));
    }

    #[test]
    fn the_skew_message_names_every_step_including_the_stranded_job() {
        // The guard is terminal and nothing recycles the finished Job before
        // its ~1h TTL, so an operator who upgrades the image and stops there
        // sees no change for up to an hour and concludes the fix did not work.
        let m = crate::io::seed_mover_too_old_message();
        assert!(!m.contains("   "), "wrapped source whitespace: {m}");
        assert!(m.contains("upgrade the mover image"), "{m}");
        assert!(m.contains("delete the empty repository"), "{m}");
        assert!(m.contains("delete job"), "{m}");
        assert!(m.contains("discovery"), "{m}");
    }

    #[test]
    fn the_seed_creds_prefix_is_distinct_from_the_repositorys_own() {
        // One bootstrap pod touches TWO repositories in migrate mode. A shared
        // prefix would make the source projection clobber this repository's own
        // `-creds-0` copy.
        let own = io::CredsPrefix::bootstrap("nas");
        let seed = io::CredsPrefix::seed("nas");
        assert_ne!(own.secret_name(0), seed.secret_name(0));
        assert_ne!(own.secret_name(1), seed.secret_name(1));
    }
}
