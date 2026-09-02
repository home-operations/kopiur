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
use kopiur_api::common::{MoverDefaults, RepositoryKind, RepositoryRef, Throttle};
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
    /// The seed cannot run: write `Seeded=False` from this park's gate row with
    /// its message, then re-check. Nothing is launched.
    Park(SeedPark),
    /// Everything resolved: launch a seeding bootstrap Job with this payload.
    Armed(Box<ArmedSeed>),
}

/// A refusal to launch a seed, as a condition to write.
///
/// The gate row rides the decision rather than being fixed at the writer,
/// because the park arms do NOT share a reason: most of them mean "the source
/// is not usable yet" (`WaitingForSeedSource`), while a workload-identity
/// conflict between the two backends is a different problem with a different
/// fix. Carrying the row keeps the condition written FROM the registry
/// (`io::upsert_gate`), so the triple the CLI's doctor matches and the one
/// stamped here cannot drift.
pub(crate) struct SeedPark {
    /// The registry row this park's condition is written from.
    pub gate: &'static kopiur_api::gates::StructuralGate,
    /// The actionable park message (what is blocked / why / how to fix).
    pub message: String,
}

impl SeedPark {
    /// A park on the "the seed source is not usable (yet)" gate — the arms an
    /// operator fixes by changing the SOURCE side or the reference to it.
    fn source_not_usable(message: String) -> Self {
        Self {
            gate: &kopiur_api::gates::SEED_SOURCE_NOT_READY_GATE,
            message,
        }
    }
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
    /// The source's resolved backend/encryption/CA surface — and its
    /// `moverDefaults`, which is where the REPLICA side's throttle comes from
    /// (kopia's limits are per connection, so the replica's cap can only be the
    /// replica's own default, never the seeded repository's).
    pub repo: &'a ResolvedRepository,
}

/// **Pure.** The per-CR override for ONE side of a migrate seed's copy, or
/// `None` when this seed caps nothing (blob mode included — `spec.seed.migrate`
/// is refused alongside `from.backend`, so the block cannot exist there).
///
/// Two named accessors rather than one indexed helper because the two sides are
/// two different repositories with two different failure modes, and a caller
/// that picked the wrong one would produce a run that caps the link it meant to
/// leave alone.
fn seed_source_throttle(seed: &SeedSpec) -> Option<&Throttle> {
    seed.migrate
        .as_ref()
        .and_then(|m| m.throttle.as_ref())
        .and_then(|t| t.source.as_ref())
}

/// The DESTINATION-side counterpart of [`seed_source_throttle`]: the override
/// for THIS repository, the one being seeded.
fn seed_destination_throttle(seed: &SeedSpec) -> Option<&Throttle> {
    seed.migrate
        .as_ref()
        .and_then(|m| m.throttle.as_ref())
        .and_then(|t| t.destination.as_ref())
}

/// **Pure.** The throttle a bootstrap work spec carries — the cap that lands on
/// every connection the bootstrap mover opens to THIS repository.
///
/// Ordinarily that is just this repository's own `moverDefaults.throttle`. While
/// a seed is ARMED it also picks up `spec.seed.migrate.throttle.destination`,
/// field by field: a seeding bootstrap's local connect is the write side of the
/// heaviest transfer kopiur performs, and `kopia snapshot migrate` has no speed
/// flags of its own, so the limits persisted into that connection's config are
/// the only cap it can honor.
///
/// Gated on `armed` deliberately. The destination override describes the SEED
/// RUN, not the repository; leaving it in force on every later connect to the
/// now-initialized repository would cap routine work with a number chosen for a
/// one-time copy. `armed == false` therefore reproduces the pre-#374 value byte
/// for byte.
pub(crate) fn seed_bootstrap_throttle(
    armed: bool,
    seed: Option<&SeedSpec>,
    defaults: Option<&MoverDefaults>,
) -> kopiur_mover::workspec::ThrottleSpec {
    io::merged_throttle(
        defaults,
        seed.filter(|_| armed).and_then(seed_destination_throttle),
    )
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
    // Resolved together with the source it belongs to, so the two can never
    // describe different repositories: kopia's limits are per CONNECTION, and
    // the replica's cap comes from the REPLICA's own `moverDefaults.throttle`
    // (overlaid field-wise by `spec.seed.migrate.throttle.source`) — never from
    // the repository being seeded. Blob mode caps its copy through `sync-to`'s
    // own speed flags instead, so it carries none.
    let (from, replica_throttle) = match (&seed.from, source) {
        (SeedSource::Backend(backend), _) => (
            SeedConnectSource::Backend(Box::new(backend_to_repository_connect(
                backend,
                blob_ca_bundle_pem,
            ))),
            Default::default(),
        ),
        (SeedSource::Repository(_), Some(src)) => (
            SeedConnectSource::Repository(Box::new(SeedRepositoryConnect {
                kind: io::repo_kind_str(src.kind).to_string(),
                name: src.name.to_string(),
                namespace: src.repo.repo_namespace.clone(),
                connect: backend_to_repository_connect(
                    &src.repo.backend,
                    src.repo.ca_bundle_pem.clone(),
                ),
            })),
            io::merged_throttle(src.repo.mover_defaults.as_ref(), seed_source_throttle(seed)),
        ),
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
        migrate: seed.migrate.as_ref().map(|m| SeedMigrateSpec {
            parallel: m.parallel,
            latest_only: m.latest_only,
            policies: crate::snapshot_replication::policy_copy_mode_spec(m.policies),
        }),
        allow_empty_source: seed.allow_empty_source,
        resume,
        replica_throttle,
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
/// storage, not manifests, so there is no per-snapshot copy count; its
/// `snapshotCount` is the SOURCE listing the mover took before the copy, which
/// for a byte-for-byte mirror is also what this repository ends up holding).
///
/// # Why `existing` — the fold must be BYTE-STABLE across re-folds
///
/// `finalize_bootstrap` re-enters on EVERY reconcile for as long as the
/// finished bootstrap Job lingers (it is deleted only on the probe arm), and it
/// re-reads the same result ConfigMap each time — so a seeding Job's outcome is
/// re-folded, unchanged, hundreds of times. Two properties make that pass a
/// no-op under [`crate::io::status_patch_is_noop`], and both need `existing`:
///
/// * `seededAt` is stamped ONCE, at the first fold, and reused verbatim
///   afterwards. Re-stamping `now` made every pass a fresh status write, which
///   bumped `resourceVersion`, re-triggered this reconciler through its own
///   primary watch and spun the repository at ~4 reconciles/second until the
///   Job's TTL. (A seed happens exactly once, so "when it completed" is a fact,
///   not a heartbeat — the same rule `seed_marker_patch` already applies to
///   `startedAt`.)
/// * the seed-attempt marker (`startedAt`) is carried FORWARD into the patch.
///   A merge patch would preserve it either way, but the no-op guard compares
///   the whole `seed` value: a partial sub-object can never equal the stored
///   one, so omitting the marker re-writes forever on its own.
pub(crate) fn seed_success_fold(
    outcome: &SeedOutcome,
    existing: Option<&SeedStatus>,
    now: &str,
) -> SeedSuccess {
    let mode = seed_mode_of(outcome);
    let mut status = serde_json::json!({
        "mode": mode,
        "source": outcome.source,
    });
    if let Some(started_at) = existing.and_then(|s| s.started_at.as_deref()) {
        status["startedAt"] = serde_json::Value::String(started_at.to_string());
    }
    if outcome.performed {
        // Set once; every later re-fold of the same lingering result reuses it.
        let seeded_at = existing.and_then(|s| s.seeded_at.as_deref()).unwrap_or(now);
        status["seededAt"] = serde_json::Value::String(seeded_at.to_string());
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

/// #380: fold a bootstrap FAILURE's seed story into the conditions array (which
/// a status patch replaces wholesale). A seed failure gets a `Seeded=False`
/// condition beside `Bootstrapped=False`; a failure with NOTHING to do with the
/// seed (an `AuthFailure` against this repository's own backend, a result-less
/// Job) drops any standing `Seeded` condition instead — see
/// [`drop_seed_condition`] for why a stale one lies. Shared by the
/// `Repository`/`ClusterRepository` failure finalizers.
pub(crate) fn seed_condition_fold(
    generation: Option<i64>,
    failure: &crate::io::BootstrapFailure,
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    match failure.seed_reason() {
        Some(seed_reason) => crate::io::upsert_condition(
            conditions,
            kopiur_api::consts::SEEDED_CONDITION,
            false,
            seed_reason,
            &failure.condition_message(),
            generation,
        ),
        None => drop_seed_condition(conditions),
    }
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
        "copying this repository's initial contents from {source_description}; not Ready until \
         the copy finishes (phase Initializing, or Degraded while an earlier attempt is \
         retried). The seeding Job's deadline is {deadline_secs}s \
         (spec.seed.failurePolicy.activeDeadlineSeconds); watch the bootstrap Job's pod logs for \
         progress."
    )
}

/// **Pure.** The park message when a migrate seed's SOURCE repository is
/// missing or not `Ready`.
pub(crate) fn waiting_for_seed_source_message(source_description: &str, why: &str) -> String {
    format!(
        "spec.seed copies this repository's initial contents from {source_description}, but \
         {why}. This repository stays Pending until the source is usable. Fix: bring the source \
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
         repository (path `{path}`, no `volume`) reachable only on the controller's filesystem \
         — a seeding Job would mount nothing and find no repository. Fix: give the source \
         Repository a `backend.filesystem.volume` (a PVC or inline NFS export), or seed from a \
         network-reachable backend via spec.seed.from.backend."
    )
}

/// **Pure.** Every refusal that can only be decided once a migrate seed's
/// SOURCE backend has been resolved from its `Repository`/`ClusterRepository`
/// reference.
///
/// All three of these rules ARE enforced at admission for a BLOB seed, where
/// the source backend is written inline and a spec-only validator can see it
/// (`validate_seed_blob_source` -> `SeedSourceSameAsRepository`,
/// `SeedMountPathCollision`, `validate_replication_auth`). A migrate seed hides
/// its source behind a reference admission cannot follow, so the same three
/// rules are re-applied here, against the resolved backend, using the SAME
/// shared helpers — the two arms of each rule cannot mean different things.
///
/// Extracted as a pure function so the decision (including WHICH gate row each
/// refusal parks on) is testable without a cluster; `arm_migrate_seed`
/// delegates and does nothing but turn `Some` into a park.
///
/// Order is deliberate, most-fundamental first: two backends that resolve to
/// the same storage are the same repository (so a shared filesystem path is a
/// consequence, not the problem), and only genuinely distinct repositories can
/// have a meaningful credential pairing.
pub(crate) fn migrate_source_backend_park(
    source_description: &str,
    local: &Backend,
    source: &Backend,
) -> Option<SeedPark> {
    use kopiur_api::validate::{
        AuthPairKind, replication_destination_differs, replication_filesystem_mount_collision,
        validate_replication_auth,
    };
    // 1. The source resolves to THIS repository's own storage. `repo_key`
    //    self-reference detection at admission is by CR NAME, so a second
    //    `Repository` CR over the same bucket/PVC sails through it — and a seed
    //    from it would read and write one location.
    if !replication_destination_differs(local, source) {
        return Some(SeedPark::source_not_usable(
            seed_source_same_storage_message(source_description, local),
        ));
    }
    // 2. Two DISTINCT filesystem repositories sharing one in-pod path. Both
    //    defaulting to `/repo` is the most probable authoring of a migrate
    //    seed, and the seeding Job mounts both — so without this the failure is
    //    a raw apiserver rejection of duplicate volumeMounts at Job-create
    //    time, with nothing on the CR to explain it.
    if let Some(path) = replication_filesystem_mount_collision(local, source) {
        return Some(SeedPark::source_not_usable(
            seed_mount_path_collision_message(source_description, &path),
        ));
    }
    // 3. The credentials cannot share one pod. Same argument order
    //    `validate_seed_blob_source` uses, which matters for the one-sided arms.
    if validate_replication_auth(local, source, AuthPairKind::Seed).is_err() {
        return Some(SeedPark {
            gate: &kopiur_api::gates::SEED_SOURCE_AUTH_CONFLICT_GATE,
            message: seed_source_auth_conflict_message(source_description, local, source),
        });
    }
    None
}

/// **Pure.** The park message for a migrate seed whose source repository
/// resolves to the SAME storage this repository is being created on.
fn seed_source_same_storage_message(source_description: &str, local: &Backend) -> String {
    format!(
        "spec.seed reads from {source_description}, which resolves to this repository's own \
         {kind} storage (spec.backend) — the seed would read and write one location. Admission \
         catches a self-reference only BY NAME; a second CR over one bucket/PVC has the same \
         storage. Fix: point spec.seed.from.repository at the repository holding the surviving \
         history, or drop spec.seed if this one already has it.",
        kind = local.kind_str()
    )
}

/// **Pure.** The park message for a migrate seed whose source repository shares
/// this repository's in-pod filesystem `path`.
fn seed_mount_path_collision_message(source_description: &str, path: &str) -> String {
    format!(
        "spec.seed reads from {source_description}, whose filesystem backend mounts at {path:?} \
         — the same in-pod path as this repository's backend. One pod mounts BOTH, and two \
         volumes cannot share one mountPath, so the Job is rejected. Fix: give one repository a \
         distinct backend.filesystem.path (e.g. /seed-source); the path only sets where the \
         volume mounts in kopiur's pods, so changing it moves no data."
    )
}

/// **Pure.** The park message for a migrate seed whose LOCAL backend and
/// resolved SOURCE backend disagree on workload identity.
///
/// The VERDICT is `kopiur_api::validate::validate_replication_auth` — the same
/// function admission runs on a blob seed, so the two arms of the rule cannot
/// mean different things. Only the WORDING is rebuilt here, because this arm
/// has something admission cannot see: the seed source is a REFERENCE, so the
/// park has to name the `Repository`/`ClusterRepository` it resolved to before
/// any of this is actionable. It also names the federating ServiceAccount on
/// the one-sided arm, where the validator names neither.
fn seed_source_auth_conflict_message(
    source_description: &str,
    local_backend: &Backend,
    source_backend: &Backend,
) -> String {
    use kopiur_api::creds::backend_workload_identity;
    // Exhaustive over the four pairings. Only the middle three can reach here
    // (the validator accepts a no-identity pair outright, and a both-federated
    // pair only when the two ServiceAccounts already agree), but the fourth
    // arm still has to say something true rather than be an `unreachable!` in
    // a reconcile path.
    let detail = match (
        backend_workload_identity(local_backend),
        backend_workload_identity(source_backend),
    ) {
        (Some((a, _)), Some((b, _))) => format!(
            "this repository federates as ServiceAccount {:?} and the seed source as {:?}",
            a.service_account_name, b.service_account_name
        ),
        (Some((a, _)), None) => format!(
            "this repository federates as ServiceAccount {:?}, the same-kind source uses a \
             static credential Secret the pod would pick up as the wrong identity",
            a.service_account_name
        ),
        (None, Some((b, _))) => format!(
            "the seed source federates as ServiceAccount {:?}, the same-kind local uses a \
             static credential Secret the pod would pick up as the wrong identity",
            b.service_account_name
        ),
        (None, None) => {
            "the two backends' credentials cannot both be used from one pod".to_string()
        }
    };
    format!(
        "spec.seed copies this repository's initial contents from {source_description}, but the \
         two backends' credentials cannot share one seeding pod: {detail}. A bootstrap Job runs \
         as exactly ONE ServiceAccount, so kopiur will not launch a seed that fails part-way on \
         a cloud auth error. Fix: put both backends' auth.workloadIdentity on the SAME \
         ServiceAccount (access to both stores), or give both sides static credential Secrets in \
         the bootstrap Job's namespace (a Repository's own; a ClusterRepository's operator \
         namespace, unless encryption.passwordSecretRef.namespace pins another)."
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
            return Ok(SeedArming::Park(SeedPark::source_not_usable(
                waiting_for_seed_source_message(source_description, "it does not exist (yet)"),
            )));
        }
        Err(e) => return Err(e),
    };
    if !io::repository_ready_cached(ctx, rref, sctx.source_default_ns).await? {
        return Ok(SeedArming::Park(SeedPark::source_not_usable(
            waiting_for_seed_source_message(source_description, "it is not Ready"),
        )));
    }
    // The one bare-path arm admission cannot see: it would have to read the
    // SOURCE CR to know its backend shape.
    if let Some(path) = filesystem_repo_path(&source.backend)
        && filesystem_repo_mount_source(&source.backend).is_none()
    {
        return Ok(SeedArming::Park(SeedPark::source_not_usable(
            bare_path_seed_source_message(source_description, &path),
        )));
    }
    // The three rules admission enforces on a BLOB seed but cannot reach
    // through a repository REFERENCE: same storage, a shared in-pod filesystem
    // path, and a credential pairing that cannot share one pod. Decided by one
    // pure function so the gate row each refusal parks on is testable.
    if let Some(park) =
        migrate_source_backend_park(source_description, sctx.repo_backend, &source.backend)
    {
        return Ok(SeedArming::Park(park));
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
            kind: match namespace {
                Some(_) => kopiur_api::common::RepositoryKind::Repository,
                None => kopiur_api::common::RepositoryKind::ClusterRepository,
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
            concurrency: None,
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

    /// The #374 seed glue guard: two repositories, two kopia connections, two
    /// independently resolved caps — and neither side's knobs may reach the
    /// other. A crossed merge is invisible in production until the wrong link
    /// gets saturated, which is exactly what this feature exists to prevent.
    #[test]
    fn seed_throttles_resolve_per_side_from_each_repositorys_own_defaults() {
        use kopiur_api::common::{MoverDefaults, Throttle};
        let throttle = |up, down, r, w| Throttle {
            upload_bytes_per_second: up,
            download_bytes_per_second: down,
            read_ops_per_second: r,
            write_ops_per_second: w,
        };
        // The REPLICA brings its own defaults; so does the repository being
        // seeded. The CR overrides ONE knob per side, so a correct merge shows
        // the override AND that side's surviving default — and nothing else.
        let mut src = resolved_source(s3("source-bucket"), Some("backups"));
        src.mover_defaults = Some(MoverDefaults {
            throttle: Some(throttle(None, Some(1), Some(2), None)),
            ..Default::default()
        });
        let local_defaults = MoverDefaults {
            throttle: Some(throttle(Some(3), None, None, Some(4))),
            ..Default::default()
        };
        let seed = seed_spec(serde_json::json!({
            "from": { "repository": { "name": "offsite" } },
            "migrate": { "throttle": {
                "source": { "downloadBytesPerSecond": 11 },
                "destination": { "uploadBytesPerSecond": 22 },
            } },
        }));
        let source = SeedSourceRepository {
            kind: RepositoryKind::Repository,
            name: "offsite",
            repo: &src,
        };
        let op = seed_op_for(&seed, None, Some(&source), false).expect("migrate op");
        // REPLICA side: the CR's download override wins, the replica repo's
        // readOps default survives, and NOTHING from the local repository's
        // defaults leaks in.
        assert_eq!(op.replica_throttle.download_bytes_per_second, Some(11));
        assert_eq!(op.replica_throttle.read_ops_per_second, Some(2));
        assert_eq!(
            op.replica_throttle.upload_bytes_per_second, None,
            "the SEEDED repository's upload default must not reach the replica"
        );
        assert_eq!(
            op.replica_throttle.write_ops_per_second, None,
            "the SEEDED repository's writeOps default must not reach the replica"
        );

        // LOCAL (destination) side rides the bootstrap work spec's own throttle:
        // the CR's upload override wins, this repository's writeOps default
        // survives, and the REPLICA's knobs stay out.
        let armed = seed_bootstrap_throttle(true, Some(&seed), Some(&local_defaults));
        assert_eq!(armed.upload_bytes_per_second, Some(22));
        assert_eq!(armed.write_ops_per_second, Some(4));
        assert_eq!(
            armed.download_bytes_per_second, None,
            "the REPLICA's download default must not reach the seeded repository"
        );
        assert_eq!(armed.read_ops_per_second, None);

        // NOT armed — every ordinary bootstrap, including every later connect to
        // the now-initialized repository: byte-for-byte this repository's own
        // defaults, with the seed-run override nowhere in sight.
        let unarmed = seed_bootstrap_throttle(false, Some(&seed), Some(&local_defaults));
        assert_eq!(unarmed.upload_bytes_per_second, Some(3));
        assert_eq!(unarmed.write_ops_per_second, Some(4));
        assert!(unarmed.download_bytes_per_second.is_none());

        // A migrate seed that caps nothing leaves both sides on their
        // repositories' defaults (and an all-empty pair skips `throttle set`).
        let plain = seed_spec(serde_json::json!({
            "from": { "repository": { "name": "offsite" } },
        }));
        let op = seed_op_for(&plain, None, Some(&source), false).expect("migrate op");
        assert_eq!(op.replica_throttle.download_bytes_per_second, Some(1));
        assert!(seed_bootstrap_throttle(true, Some(&plain), None).is_empty());

        // BLOB mode has no source repository CR at all — its copy is capped by
        // `sync-to`'s own speed flags — so the replica block stays empty rather
        // than inheriting this repository's defaults.
        let blob = seed_spec(serde_json::json!({
            "from": { "backend": { "s3": { "bucket": "offsite" } } },
            "sync": { "maxDownloadSpeedBytesPerSecond": 20000000 },
        }));
        let op = seed_op_for(&blob, None, None, false).expect("blob op");
        assert!(op.replica_throttle.is_empty());
        // …and admission refuses `migrate` beside `from.backend`, so a blob seed
        // can never carry a destination override either.
        assert!(seed_bootstrap_throttle(true, Some(&blob), None).is_empty());
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
        let fold = seed_success_fold(&performed, None, now);
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
        let fold = seed_success_fold(&blob, None, now);
        assert_eq!(fold.status["seed"]["snapshotCount"], serde_json::json!(7));
        assert!(fold.status["seed"].get("snapshotsCopied").is_none());

        // The STANDING NO-OP: no seededAt, no counts (nothing was opened, so
        // reporting 0 would be a lie), its own reason and metric label, and NO
        // Event — nothing happened.
        let noop = SeedOutcome::already_initialized(SeedModeSpec::Blob, "S3".into());
        let fold = seed_success_fold(&noop, None, now);
        assert_eq!(fold.reason, kopiur_api::consts::ALREADY_INITIALIZED_REASON);
        assert_eq!(
            fold.outcome,
            crate::metrics::SeedOutcomeLabel::AlreadyInitialized
        );
        assert!(fold.status["seed"].get("seededAt").is_none());
        assert!(fold.status["seed"].get("snapshotCount").is_none());
        assert!(fold.event.is_none());
    }

    /// HOT-LOOP REGRESSION (#396 follow-up). `finalize_bootstrap` re-enters on
    /// every reconcile while the finished bootstrap Job lingers and re-folds the
    /// SAME `SeedOutcome` out of the same result ConfigMap. That steady-state
    /// pass must be a no-op under `io::status_patch_is_noop`, or the write bumps
    /// `resourceVersion`, the primary watch re-delivers the object and the
    /// repository spins until the Job's TTL.
    ///
    /// Shaped like the e2e's `e2e-seed-mig-dst`: a completed MIGRATE seed on a
    /// `Ready` repository. Both failure modes are covered — the wall clock
    /// advancing between passes, and the marker (`startedAt`) the stored
    /// sub-object carries but the fold used to omit.
    #[test]
    fn re_folding_a_finished_seed_is_a_byte_stable_no_op() {
        use kopiur_api::repository::RepositoryStatus;

        let outcome = SeedOutcome::performed(
            SeedModeSpec::Migrate,
            "Repository/e2e-seed-mig-src".into(),
            1,
            Some(1),
        );
        // The marker stamped by `seed_marker_patch` before the seeding Job ran.
        let marker = SeedStatus {
            started_at: Some("2026-08-26T03:33:20+00:00".into()),
            ..Default::default()
        };

        // FIRST fold: nothing recorded yet, so `now` is what gets stamped.
        let first = seed_success_fold(&outcome, Some(&marker), "2026-08-26T03:33:26+00:00");
        assert_eq!(
            first.status["seed"]["seededAt"],
            serde_json::json!("2026-08-26T03:33:26+00:00"),
            "the first fold stamps the completion time"
        );
        assert_eq!(
            first.status["seed"]["startedAt"],
            serde_json::json!("2026-08-26T03:33:20+00:00"),
            "the seed-attempt marker rides the patch, or the no-op compare below \
             can never hold"
        );

        // What that first pass persisted, read back the way the reconciler reads
        // it (`serde_json::to_value(&repo.status)`).
        let stored = RepositoryStatus {
            phase: Some(kopiur_api::RepositoryPhase::Ready),
            observed_generation: Some(1),
            unique_id: Some("c36584a9".into()),
            backend: Some("Filesystem".into()),
            seed: Some(SeedStatus {
                started_at: marker.started_at.clone(),
                seeded_at: Some("2026-08-26T03:33:26+00:00".into()),
                mode: Some(SeedMode::Migrate),
                source: Some("Repository/e2e-seed-mig-src".into()),
                snapshot_count: Some(1),
                snapshots_copied: Some(1),
            }),
            ..Default::default()
        };
        let current = serde_json::to_value(Some(&stored)).expect("status serializes");
        let existing = stored.seed.as_ref();

        // EVERY later pass, minutes later on the wall clock, is a no-op.
        let mut later = serde_json::json!({
            "phase": "Ready",
            "backend": "Filesystem",
            "uniqueId": "c36584a9",
            "observedGeneration": 1,
        });
        merge_seed_status(
            &mut later,
            &seed_success_fold(&outcome, existing, "2026-08-26T03:51:54+00:00").status,
        );
        assert!(
            io::status_patch_is_noop(Some(&current), &later),
            "the steady-state re-fold must not re-write status (hot loop): {later}"
        );

        // The standing no-op arm carries the marker forward too, so an
        // `AlreadyInitialized` repository is equally quiet.
        let adopted = SeedOutcome::already_initialized(SeedModeSpec::Blob, "S3".into());
        let fold = seed_success_fold(&adopted, Some(&marker), "2026-08-26T03:51:54+00:00");
        assert_eq!(
            fold.status["seed"]["startedAt"],
            serde_json::json!("2026-08-26T03:33:20+00:00")
        );
        assert!(fold.status["seed"].get("seededAt").is_none());
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
            None,
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
        // The PROGRESS message must name the phase the launch patch actually
        // writes — `health::launch_phase` yields `Initializing` (or keeps
        // `Degraded` while an earlier attempt is retried), never `Pending`.
        // `Pending` belongs to the PARK, which is the one arm that returns
        // before the launch patch, and its own message says so.
        assert!(messages[0].contains("Initializing"), "{}", messages[0]);
        assert!(
            !messages[0].contains("Pending"),
            "the seeding progress message must not claim a phase the launch \
             patch never writes: {}",
            messages[0]
        );
        assert!(messages[1].contains("not Ready"), "{}", messages[1]);
        assert!(messages[1].contains("Pending"), "{}", messages[1]);
        assert!(messages[1].contains("Fix:"), "{}", messages[1]);
        assert!(messages[2].contains("volume"), "{}", messages[2]);
        assert!(messages[2].contains("Fix:"), "{}", messages[2]);
    }

    /// An S3 backend federating as `sa`, and one with static keys, for the
    /// migrate-mode workload-identity matrix below.
    fn s3_wi(bucket: &str, sa: &str) -> Backend {
        serde_json::from_value(serde_json::json!({
            "s3": { "bucket": bucket, "auth": { "workloadIdentity": { "serviceAccountName": sa } } }
        }))
        .expect("backend")
    }
    fn s3_static(bucket: &str) -> Backend {
        serde_json::from_value(serde_json::json!({
            "s3": { "bucket": bucket, "auth": { "secretRef": { "name": "keys" } } }
        }))
        .expect("backend")
    }
    fn gcs_wi(bucket: &str, sa: &str) -> Backend {
        serde_json::from_value(serde_json::json!({
            "gcs": { "bucket": bucket, "auth": { "workloadIdentity": { "serviceAccountName": sa } } }
        }))
        .expect("backend")
    }

    #[test]
    fn the_migrate_backend_parks_pick_the_right_gate_row_for_the_right_problem() {
        // The DECISION half, including gate SELECTION — which is the part a
        // reason-equality assertion cannot reach (a row's `reason` equals its
        // own const by construction). Every case pins the gate row the park
        // carries, so re-pointing an arm at the wrong row fails here.
        use kopiur_api::gates::{SEED_SOURCE_AUTH_CONFLICT_GATE, SEED_SOURCE_NOT_READY_GATE};

        let fs = |path: &str, pvc: &str| -> Backend {
            serde_json::from_value(serde_json::json!({
                "filesystem": { "path": path, "volume": { "pvc": { "name": pvc } } }
            }))
            .expect("backend")
        };
        // gate == None means "no park at all".
        let cases: &[(&str, Backend, Backend, Option<&'static str>)] = &[
            // Distinct storage, distinct in-pod paths, no federation: allowed.
            (
                "two ordinary filesystem repositories",
                fs("/repo", "local"),
                fs("/seed-src", "source"),
                None,
            ),
            (
                "no workload identity on either object store",
                s3_static("local"),
                s3_static("source"),
                None,
            ),
            (
                "both federate as the SAME ServiceAccount",
                s3_wi("local", "kopiur-dr"),
                s3_wi("source", "kopiur-dr"),
                None,
            ),
            // A GCS static key travels as a --credentials-file path, never
            // ambient env, so it cannot be picked up by the federated side.
            (
                "GCS federation beside a static S3 side is safe",
                gcs_wi("local", "kopiur-dr"),
                s3_static("source"),
                None,
            ),
            // A SECOND repository CR over the same storage: admission's
            // self-reference check is by NAME, so only this catches it.
            (
                "the source resolves to this repository's own storage",
                fs("/repo", "shared"),
                fs("/repo", "shared"),
                Some(SEED_SOURCE_NOT_READY_GATE.reason),
            ),
            (
                "the same bucket under a different CR",
                s3_static("shared"),
                s3_static("shared"),
                Some(SEED_SOURCE_NOT_READY_GATE.reason),
            ),
            // The most probable authoring of a migrate seed: both left at the
            // default `/repo`, over genuinely different volumes.
            (
                "two distinct filesystem repositories at one in-pod path",
                fs("/repo", "local"),
                fs("/repo", "source"),
                Some(SEED_SOURCE_NOT_READY_GATE.reason),
            ),
            (
                "both federate as DIFFERENT ServiceAccounts",
                s3_wi("local", "kopiur-new"),
                s3_wi("source", "kopiur-old"),
                Some(SEED_SOURCE_AUTH_CONFLICT_GATE.reason),
            ),
            (
                "local federates, same-kind source is static",
                s3_wi("local", "kopiur-dr"),
                s3_static("source"),
                Some(SEED_SOURCE_AUTH_CONFLICT_GATE.reason),
            ),
            (
                "source federates, same-kind local is static",
                s3_static("local"),
                s3_wi("source", "kopiur-dr"),
                Some(SEED_SOURCE_AUTH_CONFLICT_GATE.reason),
            ),
        ];
        for (what, local, source, want) in cases {
            let park = migrate_source_backend_park("Repository/offsite", local, source);
            match (park, want) {
                (None, None) => {}
                (Some(p), Some(reason)) => {
                    assert_eq!(
                        p.gate.reason, *reason,
                        "{what}: parked on the wrong gate row"
                    );
                    // A park that doctor cannot explain is worse than none.
                    assert_eq!(
                        kopiur_api::gates::STRUCTURAL_GATES
                            .iter()
                            .filter(|g| g.matches(
                                kopiur_api::consts::SEEDED_CONDITION,
                                kopiur_api::gates::CONDITION_FALSE,
                                p.gate.reason
                            ))
                            .count(),
                        1,
                        "{what}: the park's row must be registered exactly once"
                    );
                }
                (got, _) => panic!(
                    "{what}: expected {want:?}, got {:?}",
                    got.map(|p| p.gate.reason)
                ),
            }
        }
        // The auth arm must be reached only AFTER the two storage arms: two
        // sides on one bucket with mismatched federation is a same-storage
        // problem, and telling the operator to align ServiceAccounts would send
        // them somewhere that cannot help.
        let overlapping = migrate_source_backend_park(
            "Repository/offsite",
            &s3_wi("shared", "kopiur-new"),
            &s3_wi("shared", "kopiur-old"),
        )
        .expect("an overlapping pair must park");
        assert_eq!(overlapping.gate.reason, SEED_SOURCE_NOT_READY_GATE.reason);
    }

    #[test]
    fn every_migrate_backend_park_message_says_what_why_and_how_to_fix_it() {
        // The MESSAGE half. The auth wording is rebuilt rather than borrowed
        // from the validator (which cannot name the resolved source CR, and
        // names no ServiceAccount on the one-sided arm), so it needs its own
        // pin.
        let both = seed_source_auth_conflict_message(
            "Repository/offsite",
            &s3_wi("local", "kopiur-new"),
            &s3_wi("source", "kopiur-old"),
        );
        let one_sided = seed_source_auth_conflict_message(
            "ClusterRepository/archive",
            &s3_wi("local", "kopiur-new"),
            &s3_static("source"),
        );
        let other_side = seed_source_auth_conflict_message(
            "Repository/offsite",
            &s3_static("local"),
            &s3_wi("source", "kopiur-old"),
        );
        let same_storage =
            seed_source_same_storage_message("Repository/offsite", &s3_static("shared"));
        let collision = seed_mount_path_collision_message("Repository/offsite", "/repo");

        for m in [&both, &one_sided, &other_side, &same_storage, &collision] {
            // WHAT is blocked, and the source it names.
            assert!(m.contains("spec.seed"), "{m}");
            assert!(
                m.contains("Repository/offsite") || m.contains("ClusterRepository/"),
                "{m}"
            );
            // ...and HOW to fix it, concretely.
            assert!(m.contains("Fix:"), "{m}");
            assert!(m.len() > 80, "too terse to be actionable: {m}");
            // The C1 wrapped-whitespace regression: a badly-authored Rust line
            // continuation leaves a run of spaces users read verbatim.
            assert!(!m.contains("   "), "wrapped source whitespace in: {m}");
        }

        // Auth: WHY (one pod, one SA) plus BOTH ways out, so neither is a dead
        // end — and a namespace instruction that is true for BOTH kinds (a
        // ClusterRepository's seeding Job does not run in "this namespace").
        for m in [&both, &one_sided, &other_side] {
            assert!(m.contains("ONE ServiceAccount"), "{m}");
            assert!(m.contains("auth.workloadIdentity"), "{m}");
            assert!(m.contains("static credential Secret"), "{m}");
            assert!(m.contains("ClusterRepository"), "{m}");
            assert!(
                !m.contains("this namespace can read"),
                "the remediation must not assume the CR's own namespace — a ClusterRepository's \
                 bootstrap Job runs in the operator's (or its passwordSecretRef's): {m}"
            );
        }
        // The both-federated arm must name BOTH ServiceAccounts — naming only
        // one leaves the reader guessing which side to change.
        assert!(both.contains("kopiur-new"), "{both}");
        assert!(both.contains("kopiur-old"), "{both}");
        // The one-sided arms name the federated side and say why a same-kind
        // static partner is the problem.
        assert!(one_sided.contains("kopiur-new"), "{one_sided}");
        assert!(other_side.contains("kopiur-old"), "{other_side}");

        // Same storage: says WHY admission let it through, so the operator does
        // not go looking for a webhook bug.
        assert!(same_storage.contains("BY NAME"), "{same_storage}");
        assert!(same_storage.contains("S3"), "{same_storage}");
        // Collision: names the path AND that changing it moves no data.
        assert!(collision.contains("/repo"), "{collision}");
        assert!(collision.contains("mountPath"), "{collision}");
        assert!(collision.contains("moves no data"), "{collision}");
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
