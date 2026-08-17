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
pub(crate) async fn reap_seed_projection(ctx: &Context, job_ns: &str, sctx: &SeedContext<'_>) {
    let secrets: Api<k8s_openapi::api::core::v1::Secret> =
        Api::namespaced(ctx.client.clone(), job_ns);
    let prefix = io::CredsPrefix::seed(sctx.name);
    let outcome =
        io::reap_projection(&secrets, &prefix, &sctx.owner.uid, job_ns, "seed finished").await;
    if outcome.deleted > 0 {
        tracing::info!(
            repo = %sctx.name,
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
