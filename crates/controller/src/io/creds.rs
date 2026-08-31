use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::core::ObjectMeta;
use kube::{Api, ResourceExt};

use kopiur_api::common::RepositoryKind;

use crate::consts::PROJECTED_FROM_ANNOTATION;
use crate::error::{Error, Result};

use super::{CredsSecretRef, ResolvedRepository, apply, mover_creds_secret_refs};

/// Context for the missing-credentials message: which Secrets the mover Job needs
/// in its namespace, and where the referencing repository keeps them (so the
/// message can name the cross-namespace mismatch).
pub struct CredsContext<'a> {
    /// Secret names the mover Job loads via `envFrom`, required in the Job's ns.
    pub secret_names: &'a [String],
    /// `Repository` or `ClusterRepository` — the kind of the referencing repo.
    pub repo_kind: &'a str,
    /// Name of the referencing repository.
    pub repo_name: &'a str,
    /// Namespace the repository's credential Secret lives in, when explicit (a
    /// `ClusterRepository` pins it, e.g. `kopiur-system`). `None` ⇒ same-namespace
    /// reference (a namespaced `Repository`).
    pub repo_secret_namespace: Option<&'a str>,
}

/// The actionable message for a credentials Secret missing from the mover Job's
/// namespace (ADR §4.12; the project's what/why/how-to-fix rule). Pure so the
/// exact text is unit-asserted. Names the missing Secret and namespace, explains
/// why (the mover loads it via namespace-local `envFrom`), states where the repo
/// currently keeps it, and gives concrete fixes.
pub fn missing_creds_message(secret: &str, job_ns: &str, ctx: &CredsContext) -> String {
    let mut msg = format!(
        "credentials Secret `{secret}` does not exist in namespace `{job_ns}`, where the mover \
         Job runs and loads it via envFrom — envFrom is namespace-local and cannot read a Secret \
         from another namespace."
    );
    match ctx.repo_secret_namespace {
        // Cross-namespace mismatch (typically a ClusterRepository whose Secret is
        // pinned to the operator namespace): name both ends and offer both fixes.
        Some(src) if src != job_ns => {
            msg.push_str(&format!(
                " The {kind} `{name}` keeps that Secret in namespace `{src}`. Fix: create a \
                 Secret `{secret}` in `{job_ns}` with the same keys (e.g. `KOPIA_PASSWORD`, plus \
                 backend keys like `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`), or use a \
                 namespaced Repository whose secret lives in `{job_ns}`.",
                kind = ctx.repo_kind,
                name = ctx.repo_name,
            ));
        }
        // Same-namespace reference: the Secret simply isn't there yet.
        _ => {
            msg.push_str(&format!(
                " The {kind} `{name}` references it from namespace `{job_ns}`. Fix: create a \
                 Secret `{secret}` in `{job_ns}` with the repository credentials (e.g. \
                 `KOPIA_PASSWORD`, plus any backend keys).",
                kind = ctx.repo_kind,
                name = ctx.repo_name,
            ));
        }
    }
    msg
}

/// The `repo_kind` string for a [`RepositoryKind`] (for [`CredsContext`] messages).
pub fn repo_kind_str(kind: RepositoryKind) -> &'static str {
    match kind {
        RepositoryKind::Repository => "Repository",
        RepositoryKind::ClusterRepository => "ClusterRepository",
    }
}

/// The actionable message for the first credential Secret missing from the mover
/// Job's namespace, or `None` if all are present. Lets a caller surface the
/// blocking condition + Event before requeueing (see [`crate::io::publish_missing_creds_event`]).
pub async fn first_missing_cred(
    client: &kube::Client,
    job_ns: &str,
    ctx: &CredsContext<'_>,
) -> Result<Option<String>> {
    let api: Api<Secret> = Api::namespaced(client.clone(), job_ns);
    for name in ctx.secret_names {
        if api.get_opt(name).await?.is_none() {
            return Ok(Some(missing_creds_message(name, job_ns, ctx)));
        }
    }
    Ok(None)
}

/// Verify every credential Secret the mover Job needs exists in its namespace,
/// before launching a Job that would otherwise hang on a missing-Secret `envFrom`.
/// Returns an actionable [`Error::MissingDependency`] (Transient — a GitOps apply
/// may add the Secret shortly) naming the first missing Secret. Used by the
/// bootstrap paths (repository/cluster-repository), whose Secret is same-namespace;
/// the Snapshot/Restore paths use [`first_missing_cred`] to also surface a condition.
pub async fn ensure_creds_present(
    client: &kube::Client,
    job_ns: &str,
    ctx: &CredsContext<'_>,
) -> Result<()> {
    match first_missing_cred(client, job_ns, ctx).await? {
        Some(msg) => Err(Error::MissingDependency(msg)),
        None => Ok(()),
    }
}

/// Stable, per-CR prefix for projected credential Secret names
/// (`{prefix}-creds-{idx}`).
///
/// Deliberately NOT constructible from an arbitrary string: every constructor
/// takes only the consuming CR's name, so a per-run (slot-timestamped) mover
/// Job name can never reach a Secret name again. Pre-#231 versions named copies
/// after the Job, so every recurring run of a *long-lived* CR (Maintenance,
/// verification) minted a NEW Secret with no delete path — unbounded
/// accumulation of live credential copies, the sibling of the per-run work-spec
/// ConfigMap leak (#224). With a stable name the force-SSA [`super::apply`]
/// refreshes ONE object per (CR, source) in place. The kind slug keeps two CR
/// kinds sharing a name (Snapshot `db` + Restore `db`) from fighting over one
/// Secret; the residual user-crafted collision (a Snapshot literally named
/// `db-restore`) is caught by the [`projected_ownership`] guard as an actionable
/// error.
///
/// **A stable name bounds the copy count per CR — NOT per run.** [`Self::snapshot_backup`]
/// keys on a `Snapshot`, which is *itself* the per-run object, so "one copy per CR"
/// is still one copy per backup: the same leak, entering through the front door
/// (#240). Do not read the ownerRef as the lifetime. A copy lives only while some
/// mover Job can still load it via `envFrom`; past that it is reaped at the
/// consuming CR's terminal arm, with [`crate::sweep`] as the backstop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredsPrefix(String);

impl CredsPrefix {
    /// Backup mover of a `Snapshot` — bare CR name, keeping the pre-#231
    /// on-cluster Secret name (`{cr}-creds-{idx}`) on the common path.
    ///
    /// The `Snapshot` is per-run, so unlike every other constructor here this one
    /// yields a per-run Secret name. #234 renamed nothing on this path and the leak
    /// survived it; the fix is lifetime, not naming (see the type's docs).
    pub fn snapshot_backup(cr: &str) -> Self {
        Self(cr.to_string())
    }
    /// Restore mover of a `Restore` (its populate stage shares the copy).
    pub fn restore(cr: &str) -> Self {
        Self(format!("{cr}-restore"))
    }
    /// Pin/unpin mover of a `Snapshot`.
    pub fn snapshot_pin(cr: &str) -> Self {
        Self(format!("{cr}-pin"))
    }
    /// Per-repository BATCH deletion mover (mass-deletion protection). Named after
    /// the batch Job, not any one member: the batch runs at the repository's home
    /// namespace with projection HARDCODED OFF, so no per-CR credential copy is
    /// ever minted — this prefix only names/reaps a hypothetical leftover.
    pub fn snapshot_delete_batch(job: &str) -> Self {
        Self(job.to_string())
    }
    /// Maintenance mover of a `Maintenance` CR. Shared by cron quick/full and
    /// manual runs: the per-CR single-flight gate means no concurrent writers,
    /// and every mode resolves the same source Secrets.
    pub fn maintenance(cr: &str) -> Self {
        Self(format!("{cr}-maint"))
    }
    /// Verification mover of a `SnapshotPolicy` (both tiers; single-flight).
    pub fn verification(policy: &str) -> Self {
        Self(format!("{policy}-vfy"))
    }
    /// Per-repository verify mover of a MULTI-repo `SnapshotPolicy` (#368):
    /// `{policy}-vfy-{r6}` with `r6` = [`crate::naming::repo_tag6`] over the
    /// normalized repo key, so the N concurrent per-repo verifies never share
    /// a projected-Secret name for different repositories. The single-repo
    /// shape keeps [`Self::verification`]'s byte-identical prefix.
    pub fn verification_for_repo(policy: &str, repo6: &str) -> Self {
        Self(format!("{policy}-vfy-{repo6}"))
    }
    /// Replication mover of a `RepositoryReplication` (never projects today;
    /// the slug exists so a future opt-in cannot collide — the cleanup GETs
    /// under this prefix are guaranteed misses, an accepted per-run cost).
    pub fn replication(cr: &str) -> Self {
        Self(format!("{cr}-repl"))
    }
    /// Bootstrap mover of a `Repository` (never projects; same-namespace).
    pub fn bootstrap(repo: &str) -> Self {
        Self(format!("{repo}-bootstrap"))
    }
    /// SEED-SOURCE credentials of a seeding bootstrap mover (issue #380). One
    /// bootstrap pod touches TWO repositories in migrate mode — this one and
    /// the source it copies from — so the source side gets its own stable
    /// prefix, DISTINCT from [`Self::bootstrap`]: a shared prefix would make
    /// the source projection clobber this repository's own `-creds-0` copy.
    pub fn seed(repo: &str) -> Self {
        Self(format!("{repo}-seed"))
    }
    /// SOURCE-side credentials of a `SnapshotReplication`'s mover (issue #368).
    /// One mover pod touches TWO repository CRs, each with its own credential
    /// Secrets, so the two sides get DISTINCT stable prefixes — a shared prefix
    /// would make the second projection clobber the first's `-creds-0` copy.
    pub fn snapshot_replication_src(cr: &str) -> Self {
        Self(format!("{cr}-srepl-src"))
    }
    /// DESTINATION-side credentials of a `SnapshotReplication`'s mover (the
    /// `KOPIUR_DEST_`-prefixed envFrom side). See
    /// [`Self::snapshot_replication_src`].
    pub fn snapshot_replication_dst(cr: &str) -> Self {
        Self(format!("{cr}-srepl-dst"))
    }

    /// The projected copy's Secret name for the `idx`-th credential source,
    /// capped to a valid DNS label (long CR names truncate + hash over the
    /// FULL name, so per-idx names stay distinct and deterministic).
    pub fn secret_name(&self, idx: usize) -> String {
        crate::naming::capped_name(&format!("{}-creds-{idx}", self.0))
    }
}

/// Build a kopiur-managed copy of a source credential `Secret` for `job_ns`,
/// owned by the consuming CR (`owner`) — the owner and the copy are always in the
/// same namespace, so the ownerRef is valid (cross-namespace ownerRefs are
/// forbidden by Kubernetes, and would never GC). Pure (no IO) so the shape is
/// unit-testable; `now` is the injected clock. Copies `data`/`stringData`
/// verbatim and preserves the source `type`; records the source in
/// [`PROJECTED_FROM_ANNOTATION`] and marks the copy stable-per-CR via
/// [`crate::consts::CREDS_SCOPE_LABEL`] (its absence is how the sweep recognizes
/// legacy per-run copies). Not marked immutable — it is re-applied (refreshed
/// from source) on every run.
///
/// The ownerRef alone does NOT bound the copy's life: a `Snapshot` is a *per-run*
/// CR that outlives its mover Job by the whole retention window, so a copy owned
/// by one sits in the workload namespace holding live repository credentials long
/// after anything could read it. The copy's real lifetime is "while a mover Job can
/// still load it via `envFrom`" — enforced by the terminal-arm reap and the sweep,
/// with [`crate::consts::PROJECTED_AT_ANNOTATION`] as the freshness clock and the
/// guarantee that a re-projection is a real write (see that const's docs).
pub fn build_projected_secret(
    name: &str,
    job_ns: &str,
    owner: OwnerReference,
    src: &Secret,
    now: chrono::DateTime<chrono::Utc>,
) -> Secret {
    let src_ns = src.metadata.namespace.clone().unwrap_or_default();
    let src_name = src.metadata.name.clone().unwrap_or_default();
    let labels = BTreeMap::from([
        (
            crate::consts::MANAGED_BY_LABEL.to_string(),
            crate::consts::MANAGED_BY_VALUE.to_string(),
        ),
        (
            crate::consts::COMPONENT_LABEL.to_string(),
            crate::consts::CREDS_COMPONENT.to_string(),
        ),
        (
            crate::consts::CREDS_SCOPE_LABEL.to_string(),
            crate::consts::CREDS_SCOPE_CR.to_string(),
        ),
    ]);
    let annotations = BTreeMap::from([
        (
            PROJECTED_FROM_ANNOTATION.to_string(),
            format!("{src_ns}/{src_name}"),
        ),
        (
            crate::consts::PROJECTED_AT_ANNOTATION.to_string(),
            now.to_rfc3339(),
        ),
    ]);
    Secret {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(job_ns.to_string()),
            labels: Some(labels),
            annotations: Some(annotations),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        data: src.data.clone(),
        string_data: src.string_data.clone(),
        type_: src.type_.clone().or_else(|| Some("Opaque".to_string())),
        immutable: None,
    }
}

/// Who holds a would-be projection target name (the stable-name collision
/// guard). Pure + exhaustive so the fail-closed decision is unit-tested.
#[derive(Debug)]
pub enum ProjectedOwnership<'a> {
    /// No Secret of that name exists — free to apply.
    Free,
    /// Exists and its controller ownerRef uid matches the consuming CR — ours
    /// to refresh.
    OwnedBySelf,
    /// Exists but is controlled by a different owner (`Some`) or carries no
    /// controller ownerRef at all (`None`, e.g. a user-created Secret). Never
    /// overwrite: force-SSA would silently flip the ownerRef and data.
    OwnedByOther(Option<&'a OwnerReference>),
}

/// Classify an existing Secret at a projection target name against the
/// consuming CR's uid. `None` (not found) is `Free` — a recreated same-name CR
/// (new uid) must pass once GC reaped the old copy.
///
/// Takes the Secret's **metadata** only (#382 M2b): every input this decision
/// needs (the controller ownerRef) lives there, so callers holding either a
/// full `Secret` or a metadata-only `PartialObjectMeta<Secret>` read share it.
pub fn projected_ownership<'a>(
    existing: Option<&'a ObjectMeta>,
    owner_uid: &str,
) -> ProjectedOwnership<'a> {
    let Some(meta) = existing else {
        return ProjectedOwnership::Free;
    };
    let controller = meta
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|r| r.controller == Some(true));
    match controller {
        Some(r) if r.uid == owner_uid => ProjectedOwnership::OwnedBySelf,
        other => ProjectedOwnership::OwnedByOther(other),
    }
}

/// Actionable message when the stable projection name is already held by a
/// different owner (two CRs whose names collide under the `-creds-` scheme,
/// e.g. a Snapshot literally named `db-restore` next to a Restore `db`).
/// The what/why/fix rule.
fn projection_name_conflict_message(
    proj_name: &str,
    job_ns: &str,
    existing: Option<&OwnerReference>,
    ctx: &CredsContext,
) -> String {
    let holder = match existing {
        Some(r) => format!("it is controlled by {} `{}`", r.kind, r.name),
        None => "it carries no kopiur controller ownerReference, so it is \
                 not managed by kopiur (likely user-created)"
            .to_string(),
    };
    format!(
        "credential projection blocked for {kind} `{name}`: target Secret `{proj_name}` in \
         namespace `{job_ns}` already exists and {holder}, and overwriting it would hand one \
         consumer another owner's (or the user's) Secret. Fix: rename one of the colliding \
         resources, or delete the conflicting Secret if it is stale.",
        kind = ctx.repo_kind,
        name = ctx.repo_name,
    )
}

/// Whether a Secret is a stable per-CR projected copy owned by `owner_uid` —
/// the fail-closed gate for every resolve-path delete (index shrink, disabled
/// projection). Requires BOTH the [`crate::consts::CREDS_SCOPE_LABEL`] marker
/// (legacy per-run copies belong to the sweep, user Secrets to the user) AND a
/// matching controller ownerRef. Pure so it is unit-tested. Metadata-typed
/// (#382 M2b): labels + ownerRefs are all it reads, so the reap pre-check can
/// feed it from a metadata-only GET and the Secret payload never crosses the
/// wire.
pub fn is_reapable_projected_copy(meta: &ObjectMeta, owner_uid: &str) -> bool {
    let marked = meta.labels.as_ref().is_some_and(|l| {
        l.get(crate::consts::CREDS_SCOPE_LABEL).map(String::as_str)
            == Some(crate::consts::CREDS_SCOPE_CR)
    });
    marked
        && matches!(
            projected_ownership(Some(meta), owner_uid),
            ProjectedOwnership::OwnedBySelf
        )
}

/// Outcome of one best-effort reap of a projected copy (see [`reap_projected_copy`]).
enum ReapSignal {
    /// Deleted (or vanished/changed concurrently — treated as done).
    Deleted,
    /// No Secret of that name exists.
    Absent,
    /// Exists but is not ours to delete (no marker / different owner).
    Kept,
    /// The API call failed. Distinct from [`Self::Kept`]: nothing is owed on a copy
    /// that was never ours, but a copy we could not *reach* may still be there, and a
    /// caller that treats the two alike would record the reap as settled and never
    /// look again.
    Errored,
    /// The operator lacks the `secrets` delete verb (HTTP 403) — the chart's
    /// projection grant predates the sweep. Stop trying; never fail the run.
    Forbidden,
}

/// Delete of ONE stable projected copy, fail-closed: only a marker-labeled
/// Secret controller-owned by `owner_uid` is deleted, via the sweep's shared
/// precondition-pinned delete kernel (a concurrent re-apply bumps the RV and
/// the delete 409s — the copy is spared and re-evaluated next run).
/// 404/409/403 never propagate as errors.
///
/// The pre-check is a METADATA-ONLY GET (#382 M2b): the reapability decision
/// reads labels + ownerRefs, and the precondition pin reads uid +
/// resourceVersion — all metadata — so the Secret payload never crosses the
/// wire on this (per-run, often multi-index) path.
async fn reap_projected_copy(dst: &Api<Secret>, name: &str, owner_uid: &str) -> Result<ReapSignal> {
    let Some(existing) = dst.get_metadata_opt(name).await? else {
        return Ok(ReapSignal::Absent);
    };
    if !is_reapable_projected_copy(&existing.metadata, owner_uid) {
        return Ok(ReapSignal::Kept);
    }
    use crate::sweep::DeleteOutcome;
    match crate::sweep::delete_with_preconditions(
        dst,
        name,
        existing.uid(),
        existing.resource_version(),
    )
    .await?
    {
        DeleteOutcome::Deleted => Ok(ReapSignal::Deleted),
        // Gone or changed since our GET: spared, re-evaluated next run.
        DeleteOutcome::Spared => Ok(ReapSignal::Kept),
        DeleteOutcome::Forbidden => Ok(ReapSignal::Forbidden),
    }
}

/// [`reap_projected_copy`] made **infallible** for the cleanup paths: shared
/// Deleted/Forbidden logging (the actionable 403 remediation text lives in
/// exactly one place), and any other error is warned and swallowed — cleanup
/// must never fail a run whose credentials already resolved. `what` names the
/// copy's state for the log line (e.g. "stale", "leftover").
async fn reap_quietly(
    dst: &Api<Secret>,
    name: &str,
    owner_uid: &str,
    job_ns: &str,
    what: &str,
) -> ReapSignal {
    let signal = match reap_projected_copy(dst, name, owner_uid).await {
        Ok(signal) => signal,
        Err(e) => {
            tracing::warn!(secret = %name, namespace = %job_ns, what, error = %e,
                "projected credentials copy cleanup failed (skipped; retried next run)");
            return ReapSignal::Errored;
        }
    };
    match signal {
        ReapSignal::Deleted => {
            tracing::info!(secret = %name, namespace = %job_ns, what,
                "reaped projected credentials copy");
        }
        ReapSignal::Forbidden => {
            tracing::warn!(secret = %name, namespace = %job_ns, what,
                flag = crate::consts::CREDENTIAL_PROJECTION_FLAG,
                "cannot reap projected credentials copy: the operator lacks the \
                 `secrets` delete verb. Upgrade the Helm release so the credentialProjection \
                 grant includes delete, or remove the Secret by hand");
        }
        ReapSignal::Absent | ReapSignal::Kept | ReapSignal::Errored => {}
    }
    signal
}

/// Reap stale trailing projected copies after the source-Secret set shrank
/// (e.g. a backend re-config folded the auth keys into the password Secret:
/// 2 refs -> 1 leaves `{prefix}-creds-1` behind). Walks indices upward from
/// `start_idx` until a name is absent, foreign, forbidden, or errored.
/// Infallible by design — a failure here must never fail the run.
async fn shrink_trailing_copies(
    dst: &Api<Secret>,
    prefix: &CredsPrefix,
    start_idx: usize,
    owner_uid: &str,
    job_ns: &str,
) {
    // When the live source set already covers every index the projector can
    // ever write, no trailing copy can exist — skip the walk entirely instead
    // of paying a guaranteed-Absent probe GET per run (#382 M2b).
    if shrink_walk_is_vacuous(start_idx) {
        return;
    }
    for idx in start_idx.. {
        let name = prefix.secret_name(idx);
        match reap_quietly(dst, &name, owner_uid, job_ns, "stale (source set shrank)").await {
            ReapSignal::Deleted => {}
            ReapSignal::Absent | ReapSignal::Kept | ReapSignal::Errored | ReapSignal::Forbidden => {
                break;
            }
        }
    }
}

/// The most projected copies one consumer can ever have — [`mover_creds_secret_refs`]
/// yields the encryption-password Secret plus, only when it is differently named, the
/// backend's auth Secret.
const MAX_CREDS_IDX: usize = 1;

/// **Pure.** Whether the trailing-copy shrink walk starting at `start_idx`
/// (= the live source-ref count) cannot possibly find anything: the projector
/// never writes an index above [`MAX_CREDS_IDX`], so a walk starting past it
/// would only ever probe names that cannot exist ([`shrink_trailing_copies`]).
fn shrink_walk_is_vacuous(start_idx: usize) -> bool {
    start_idx > MAX_CREDS_IDX
}

/// Reap EVERY projected copy of `prefix` — the whole-projection reap a consumer runs
/// once its mover Job can no longer read them.
///
/// Returns whether the reap is **settled**: nothing of ours is left, so the caller may
/// durably record it and stop looking. `false` means a copy we could not remove may
/// still be out there (a 403, or an API error), so the caller must not stamp it done.
/// Infallible either way — cleanup must never fail a run that already succeeded.
///
/// Unlike [`shrink_trailing_copies`], which walks *past* the live set, this visits
/// every index unconditionally rather than stopping at the first gap. `-creds-0` being
/// absent says nothing about `-creds-1`: an earlier reap could have been interrupted
/// between them, and stopping early would strand a live credential copy in a way that
/// looks exactly like a clean reap.
pub(crate) async fn reap_projection(
    dst: &Api<Secret>,
    prefix: &CredsPrefix,
    owner_uid: &str,
    job_ns: &str,
    what: &str,
) -> ReapOutcome {
    let mut out = ReapOutcome {
        deleted: 0,
        settled: true,
    };
    for idx in 0..=MAX_CREDS_IDX {
        match reap_quietly(dst, &prefix.secret_name(idx), owner_uid, job_ns, what).await {
            ReapSignal::Deleted => out.deleted += 1,
            // Never ours to begin with: nothing owed on this index.
            ReapSignal::Absent | ReapSignal::Kept => {}
            // A copy may survive that we did not remove. Don't let the caller stamp
            // the reap as done — the 403 is actionable (the log names the Helm flag),
            // and the periodic sweep is the backstop for the transient case.
            ReapSignal::Forbidden | ReapSignal::Errored => out.settled = false,
        }
    }
    out
}

/// What one whole-projection reap did (see [`reap_projection`]).
pub(crate) struct ReapOutcome {
    /// Copies actually deleted by this call.
    pub deleted: usize,
    /// Nothing of ours is left: the caller may durably record the reap and stop
    /// looking. `false` ⇒ a copy we could not remove may still exist.
    pub settled: bool,
}

/// The decision for projecting a repository credential Secret across namespaces
/// (ADR-0005 §8). Pure + exhaustive so the fail-closed authorization model is
/// tested in one place. A same-namespace Secret is never a "projection" — it is a
/// verify-in-place, decided separately by the caller (`src_ns == job_ns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionDecision {
    /// Copy the source Secret into the Job's namespace.
    Project,
    /// Do not project: surface the reason. Carries which gate was unmet.
    Deny(ProjectionDenyReason),
}

/// Why a cross-namespace credential projection was denied (ADR-0005 §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionDenyReason {
    /// The consumer never opted in (`credentialProjection.enabled` is false/absent).
    ConsumerNotOptedIn,
    /// The repository owner has not allowed projection
    /// (`ClusterRepository.credentialProjection.allowed` is false/absent).
    OwnerNotAllowed,
}

/// Decide whether to project a repository credential Secret into a **foreign**
/// consumer namespace (ADR-0005 §8). Fail-closed: projection requires BOTH the
/// consumer opt-in (`enabled`) AND the repository-owner allow (`allowed`); operator
/// RBAC is enforced separately at apply time (a `403` → actionable error). Pure +
/// exhaustively matched so the authorization model lives in one tested place.
///
/// (RBAC is the third gate but it is an apply-time IO outcome, not a pure input.)
pub fn projection_decision(consumer_enabled: bool, owner_allowed: bool) -> ProjectionDecision {
    match (consumer_enabled, owner_allowed) {
        (true, true) => ProjectionDecision::Project,
        (false, _) => ProjectionDecision::Deny(ProjectionDenyReason::ConsumerNotOptedIn),
        (true, false) => ProjectionDecision::Deny(ProjectionDenyReason::OwnerNotAllowed),
    }
}

/// Wrap credential Secret names as **verbatim** (unprefixed) `envFrom` entries —
/// the single-backend default every mover but replication uses. kopia reads the
/// plain env-var names (`KOPIA_PASSWORD`, `AWS_*`, …). The replication mover builds
/// its own prefixed destination entry on top of these (issue #200).
pub fn plain_creds(names: Vec<String>) -> Vec<crate::jobs::CredsEnvFrom> {
    names
        .into_iter()
        .map(crate::jobs::CredsEnvFrom::plain)
        .collect()
}

/// The credential Secret names a mover Job should load via `envFrom`, plus how
/// many of them the operator actually projected (copied cross-namespace) this run.
pub struct MoverCreds {
    /// Names to put in the Job's `envFrom`, in order.
    pub names: Vec<String>,
    /// How many `names` are freshly-projected cross-namespace copies (for the
    /// `kopiur_secrets_projected` metric). Same-namespace Secrets are not counted.
    pub projected: u64,
}

/// Resolve the credential Secret names a mover Job should load via `envFrom`,
/// handling both the self-managed default and gated cross-namespace projection
/// (ADR-0005 §8).
///
/// `owner` MUST be a namespaced CR living in `job_ns`: the projected copy
/// carries it as a controller ownerRef, and a cross-namespace (or
/// cluster-scoped) owner on a namespaced Secret is invalid — never GC'd, a
/// permanent credential leak. Every current caller passes the consuming CR
/// resident in `job_ns` (the cascade delete path, whose owner is the
/// repository CR, hardcodes projection off for exactly this reason).
///
/// `consumer_enabled` is the consumer opt-in (`credentialProjection.enabled` on the
/// `SnapshotPolicy`/`Restore`); `owner_allowed` is the repository-owner allow
/// (`ClusterRepository.credentialProjection.allowed`). Per-ref:
///
/// - **Same-namespace** (`src_ns == job_ns`, the common namespaced-`Repository`
///   layout): the Secret is already where the mover needs it. Verify it is present
///   and use its original name — no projection, no gate (there is nothing to copy
///   across a trust boundary). A missing one yields an actionable error.
/// - **Cross-namespace** (a shared `ClusterRepository`): projection is gated by
///   [`projection_decision`] — it requires BOTH `consumer_enabled` AND
///   `owner_allowed`, else it **fails closed** with an actionable
///   [`Error::MissingDependency`] naming the unmet gate. When permitted, read the
///   source Secret and apply the **stable per-CR copy** (named
///   [`CredsPrefix::secret_name`], owned by `owner`) into `job_ns`. A missing
///   source Secret / unresolvable source namespace yields an actionable error; a
///   `403` on apply maps to the Helm RBAC toggle hint (degrade-not-crash —
///   projection needs cluster-wide `secrets` create/patch, the third gate).
///
/// Re-reading the source and re-applying on every run keeps the copy fresh, so
/// there is no drift to reconcile and no source-watch to maintain — and because
/// the name is stable, every run converges on ONE Secret per (CR, source)
/// instead of accumulating per-run copies (#231). Cleanup is uniform and
/// best-effort (never fails the run): trailing indices beyond the current
/// source set are always reaped first, and every ref this run does NOT project
/// (same-namespace, projection disabled, unresolved source) reaps its own
/// stable copy BEFORE verification — so a leftover plaintext copy never
/// outlives the opt-in, the topology, or an in-progress migration.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_mover_creds(
    client: &kube::Client,
    job_ns: &str,
    prefix: &CredsPrefix,
    owner: &OwnerReference,
    refs: &[CredsSecretRef],
    consumer_enabled: bool,
    owner_allowed: bool,
    ctx: &CredsContext<'_>,
) -> Result<MoverCreds> {
    let dst: Api<Secret> = Api::namespaced(client.clone(), job_ns);
    // Trailing indices beyond the current source set are stale regardless of
    // how each ref resolves below — reap them up front (best-effort), so even
    // a run that errors on verification still converges.
    shrink_trailing_copies(&dst, prefix, refs.len(), &owner.uid, job_ns).await;
    let mut names = Vec::with_capacity(refs.len());
    let mut projected = 0u64;
    for (idx, r) in refs.iter().enumerate() {
        let src_ns = match r.namespace.as_deref() {
            Some(ns) => ns,
            // No resolvable source namespace. If the Secret happens to already be in
            // the Job's namespace we'd have matched the same-namespace branch via an
            // explicit ns; with none resolvable we can neither verify-in-place nor
            // project. When the consumer didn't ask for projection, fall back to the
            // self-managed verify in job_ns; otherwise it's the unresolved-source error.
            None => {
                if consumer_enabled {
                    return Err(Error::MissingDependency(projection_unresolved_ns_message(
                        &r.name, ctx,
                    )));
                }
                reap_quietly(
                    &dst,
                    &prefix.secret_name(idx),
                    &owner.uid,
                    job_ns,
                    "leftover (projection not in use)",
                )
                .await;
                // Presence-only verify: a metadata GET keeps the Secret
                // payload off the wire (#382 M2b).
                if dst.get_metadata_opt(&r.name).await?.is_none() {
                    return Err(Error::MissingDependency(missing_creds_message(
                        &r.name, job_ns, ctx,
                    )));
                }
                names.push(r.name.clone());
                continue;
            }
        };
        // Already in the mover's namespace (the common namespaced-Repository case):
        // nothing to copy across a trust boundary. Verify it is present — exactly the
        // self-managed path — and use its original name. No owner/consumer gate here.
        if src_ns == job_ns {
            // A stable copy may linger from an earlier cross-namespace layout
            // (the topology changed, not the opt-in) — reap it BEFORE the
            // verify, so an incomplete migration still cleans up.
            reap_quietly(
                &dst,
                &prefix.secret_name(idx),
                &owner.uid,
                job_ns,
                "leftover (source is now same-namespace)",
            )
            .await;
            // Presence-only verify — metadata GET (#382 M2b).
            if dst.get_metadata_opt(&r.name).await?.is_none() {
                return Err(Error::MissingDependency(missing_creds_message(
                    &r.name, job_ns, ctx,
                )));
            }
            names.push(r.name.clone());
            continue;
        }
        // Cross-namespace. If the consumer never opted in, this is the self-managed
        // path: the user is expected to have placed the Secret in the mover namespace
        // themselves (e.g. a hand-copied ClusterRepository password). Verify it is
        // present in `job_ns` and use its name — never silently project without opt-in.
        if !consumer_enabled {
            // Projection may have been enabled earlier: reap our leftover
            // stable copy BEFORE the verify (an incomplete hand-migration must
            // not leave the plaintext copy behind while this errors).
            reap_quietly(
                &dst,
                &prefix.secret_name(idx),
                &owner.uid,
                job_ns,
                "leftover (projection disabled)",
            )
            .await;
            // Presence-only verify — metadata GET (#382 M2b).
            if dst.get_metadata_opt(&r.name).await?.is_none() {
                return Err(Error::MissingDependency(missing_creds_message(
                    &r.name, job_ns, ctx,
                )));
            }
            names.push(r.name.clone());
            continue;
        }
        // Consumer opted in: projection is gated. Fail closed unless the repository
        // owner also allows it (ADR-0005 §8). (RBAC is the third gate, enforced at
        // apply time below as a 403 → actionable error.)
        match projection_decision(consumer_enabled, owner_allowed) {
            ProjectionDecision::Project => {}
            ProjectionDecision::Deny(reason) => {
                return Err(Error::MissingDependency(projection_denied_message(
                    &r.name, src_ns, job_ns, reason, ctx,
                )));
            }
        }
        // Permitted: project the stable per-CR copy owned by the consuming CR.
        names
            .push(project_one_ref(client, &dst, job_ns, prefix, idx, owner, r, src_ns, ctx).await?);
        projected += 1;
    }
    Ok(MoverCreds { names, projected })
}

/// Project ONE credential source Secret to its stable per-CR name in `job_ns`.
///
/// Guarded against stable-name collisions: the pre-apply GET fails closed when
/// a different owner (or an unmanaged Secret) already holds the name — once a
/// collision exists, every later run of BOTH parties errors actionably instead
/// of force-SSA silently flipping the controller ownerRef back and forth. The
/// post-apply check re-verifies the object our own apply returned; it cannot
/// see a competing apply that lands after ours (the first-ever concurrent
/// write between two colliding CRs is an accepted residual race — the very
/// next run of the losing party trips the pre-apply guard), but it does catch
/// same-manager anomalies where the server did not honor our ownerRef.
#[allow(clippy::too_many_arguments)]
async fn project_one_ref(
    client: &kube::Client,
    dst: &Api<Secret>,
    job_ns: &str,
    prefix: &CredsPrefix,
    idx: usize,
    owner: &OwnerReference,
    r: &CredsSecretRef,
    src_ns: &str,
    ctx: &CredsContext<'_>,
) -> Result<String> {
    let proj_name = prefix.secret_name(idx);
    // Ownership pre-check needs metadata only (#382 M2b).
    let existing = dst.get_metadata_opt(&proj_name).await?;
    match projected_ownership(existing.as_ref().map(|s| &s.metadata), &owner.uid) {
        // Absent, or already ours: safe to (re-)apply.
        ProjectedOwnership::Free | ProjectedOwnership::OwnedBySelf => {}
        ProjectedOwnership::OwnedByOther(holder) => {
            return Err(Error::MissingDependency(projection_name_conflict_message(
                &proj_name, job_ns, holder, ctx,
            )));
        }
    }
    let src_api: Api<Secret> = Api::namespaced(client.clone(), src_ns);
    let src = src_api.get_opt(&r.name).await?.ok_or_else(|| {
        Error::MissingDependency(projection_source_missing_message(
            &r.name, src_ns, job_ns, ctx,
        ))
    })?;
    let secret =
        build_projected_secret(&proj_name, job_ns, owner.clone(), &src, chrono::Utc::now());
    let applied = apply(dst, &proj_name, &secret)
        .await
        .map_err(|e| map_projection_apply_error(e, &proj_name, job_ns))?;
    match projected_ownership(Some(&applied.metadata), &owner.uid) {
        ProjectedOwnership::OwnedBySelf => Ok(proj_name),
        // Unreachable for `Some(&applied)`, but the holder-`None` message
        // ("not managed by kopiur") is exactly right if it ever fires.
        ProjectedOwnership::Free => Err(Error::MissingDependency(
            projection_name_conflict_message(&proj_name, job_ns, None, ctx),
        )),
        ProjectedOwnership::OwnedByOther(holder) => Err(Error::MissingDependency(
            projection_name_conflict_message(&proj_name, job_ns, holder, ctx),
        )),
    }
}

/// Resolve the mover Job's `envFrom` credential Secret names for a consumer run
/// (Snapshot/Restore/Maintenance) against a [`ResolvedRepository`]. Convenience over
/// [`resolve_mover_creds`] that derives the credential references (with their
/// source namespaces) and the [`CredsContext`] from the repository. `owner` is the
/// consuming CR's owner reference, applied to any projected Secret so GC reaps it
/// with that CR; `prefix` is that CR's stable [`CredsPrefix`], which names the
/// copy (one per CR + source, refreshed in place — never per run).
///
/// `consumer_enabled` is the consumer opt-in (`spec.credentialProjection.enabled` on
/// the `SnapshotPolicy`/`Restore`/`Maintenance`); the owner gate
/// (`ClusterRepository.credentialProjection.allowed`) is read from the resolved
/// repository (`repo.credential_projection_allowed`) — a namespaced `Repository`
/// reports `false`, which is harmless because its projection is always a
/// same-namespace no-op (ADR-0005 §8). `repo_kind`/`repo_name` only label the
/// actionable messages (a `Restore` may infer its repository from the source
/// config, so they are plain strings rather than a `RepositoryRef`).
#[allow(clippy::too_many_arguments)]
pub async fn resolve_mover_creds_for(
    client: &kube::Client,
    job_ns: &str,
    prefix: &CredsPrefix,
    owner: &OwnerReference,
    repo: &ResolvedRepository,
    consumer_enabled: bool,
    repo_kind: &str,
    repo_name: &str,
) -> Result<MoverCreds> {
    let refs = mover_creds_secret_refs(
        &repo.backend,
        &repo.encryption,
        repo.repo_namespace.as_deref(),
    );
    let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
    let ctx = CredsContext {
        secret_names: &names,
        repo_kind,
        repo_name,
        repo_secret_namespace: repo.encryption.password_secret_ref.namespace.as_deref(),
    };
    resolve_mover_creds(
        client,
        job_ns,
        prefix,
        owner,
        &refs,
        consumer_enabled,
        repo.credential_projection_allowed,
        &ctx,
    )
    .await
}

/// Actionable message when a cross-namespace credential projection is denied by the
/// fail-closed §8 gate. Names the unmet gate (consumer opt-in vs. repository-owner
/// allow), the Secret + namespaces, and the concrete fix. The what/why/fix rule.
fn projection_denied_message(
    secret: &str,
    src_ns: &str,
    job_ns: &str,
    reason: ProjectionDenyReason,
    ctx: &CredsContext,
) -> String {
    let (why, fix) = match reason {
        ProjectionDenyReason::ConsumerNotOptedIn => (
            "the consumer has not opted in to credential projection",
            "set `spec.credentialProjection.enabled: true` on this SnapshotPolicy/Restore (owner \
             must also set `credentialProjection.allowed: true`), or create the Secret in the \
             mover namespace yourself",
        ),
        ProjectionDenyReason::OwnerNotAllowed => (
            "the ClusterRepository owner has not allowed credential projection",
            "ask the repository owner to set `credentialProjection.allowed: true` on the \
             ClusterRepository, or create the Secret in the mover namespace yourself",
        ),
    };
    format!(
        "cross-namespace credential projection denied for `{secret}`: it lives in `{src_ns}` but \
         the mover Job runs in `{job_ns}`, and {why}. Source: {kind} `{name}`. Fix: {fix}.",
        kind = ctx.repo_kind,
        name = ctx.repo_name,
    )
}

/// Actionable message when projection cannot read a source Secret because its
/// source namespace is unresolvable (a `ClusterRepository` reference that omits
/// `namespace`). The what/why/fix rule (ADR §4.12).
fn projection_unresolved_ns_message(secret: &str, ctx: &CredsContext) -> String {
    format!(
        "credential Secret `{secret}` for {kind} `{name}` has no resolvable source namespace, so \
         projection cannot read it to copy into the mover Job's namespace. Fix: set an explicit \
         `namespace` on the Secret reference (a {kind} reference must pin one), or disable \
         `spec.credentialProjection` and manage the Secret in each mover namespace yourself.",
        kind = ctx.repo_kind,
        name = ctx.repo_name,
    )
}

/// Actionable message when projection is enabled but the *source* Secret is absent
/// from its source namespace (so there is nothing to copy). The what/why/fix rule.
fn projection_source_missing_message(
    secret: &str,
    src_ns: &str,
    job_ns: &str,
    ctx: &CredsContext,
) -> String {
    format!(
        "credential Secret `{secret}` not found in source namespace `{src_ns}`, so {kind} \
         `{name}` cannot project it into `{job_ns}` where the mover Job runs. Fix: create Secret \
         `{secret}` in `{src_ns}` with the repository credentials (e.g. `KOPIA_PASSWORD`, plus \
         backend keys like `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`).",
        kind = ctx.repo_kind,
        name = ctx.repo_name,
    )
}

/// Map a credential-projection apply failure to an actionable error. A `403`
/// means the operator lacks the cluster-wide `secrets` create/patch RBAC that
/// projection requires; point the admin at the Helm toggle that grants it.
/// Other errors pass through unchanged. Transient (re-driven once RBAC is fixed).
fn map_projection_apply_error(e: Error, proj_name: &str, job_ns: &str) -> Error {
    if let Error::Kube(kube::Error::Api(resp)) = &e
        && resp.code == 403
    {
        return Error::MissingDependency(format!(
            "the operator is not permitted to write the projected credentials Secret \
             `{proj_name}` in namespace `{job_ns}` (HTTP 403). Credential projection needs \
             cluster-wide `secrets` create/patch RBAC. Fix: set `{flag}: true` \
             in the Helm chart (grants the operator ClusterRole those verbs), or disable \
             `spec.credentialProjection` on the repository and manage the Secret in `{job_ns}`.",
            flag = crate::consts::CREDENTIAL_PROJECTION_FLAG,
        ));
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::Status;

    fn api_error(code: u16) -> Error {
        Error::Kube(kube::Error::Api(Box::new(Status {
            code,
            message: "boom".into(),
            reason: "Forbidden".into(),
            ..Default::default()
        })))
    }

    fn owner(name: &str) -> OwnerReference {
        OwnerReference {
            api_version: "kopiur.home-operations.com/v1alpha1".into(),
            kind: "Snapshot".into(),
            name: name.into(),
            uid: "uid-123".into(),
            controller: Some(true),
            block_owner_deletion: Some(false),
        }
    }

    fn ctx() -> CredsContext<'static> {
        CredsContext {
            secret_names: &[],
            repo_kind: "ClusterRepository",
            repo_name: "shared",
            repo_secret_namespace: Some("kopiur-system"),
        }
    }

    // --- projection_decision: the §8 fail-closed authorization gate ----------

    #[test]
    fn projection_allowed_only_when_consumer_and_owner_both_agree() {
        assert_eq!(projection_decision(true, true), ProjectionDecision::Project);
    }

    #[test]
    fn projection_denied_when_owner_disallows_even_if_consumer_enabled() {
        // The headline §8 fix: a tenant opting in cannot copy the shared repo password
        // unless the ClusterRepository owner allows it.
        assert_eq!(
            projection_decision(true, false),
            ProjectionDecision::Deny(ProjectionDenyReason::OwnerNotAllowed)
        );
    }

    #[test]
    fn projection_denied_when_consumer_not_opted_in() {
        assert_eq!(
            projection_decision(false, true),
            ProjectionDecision::Deny(ProjectionDenyReason::ConsumerNotOptedIn)
        );
        assert_eq!(
            projection_decision(false, false),
            ProjectionDecision::Deny(ProjectionDenyReason::ConsumerNotOptedIn)
        );
    }

    #[test]
    fn projection_denied_message_names_the_unmet_gate() {
        let owner_msg = projection_denied_message(
            "repo-pw",
            "kopiur-system",
            "team-a",
            ProjectionDenyReason::OwnerNotAllowed,
            &ctx(),
        );
        assert!(owner_msg.contains("owner has not allowed"));
        assert!(owner_msg.contains("credentialProjection.allowed: true"));
        assert!(owner_msg.contains("`repo-pw`"));

        let consumer_msg = projection_denied_message(
            "repo-pw",
            "kopiur-system",
            "team-a",
            ProjectionDenyReason::ConsumerNotOptedIn,
            &ctx(),
        );
        assert!(consumer_msg.contains("consumer has not opted in"));
        assert!(consumer_msg.contains("credentialProjection.enabled: true"));
    }

    #[test]
    fn creds_prefix_names_are_stable_and_kind_distinct() {
        // Stable: constructors take only the CR name — no slot, mode, or
        // timestamp can reach the Secret name, so recurring runs converge on
        // one object per (CR, idx) instead of accumulating (#231).
        let all = [
            CredsPrefix::snapshot_backup("app").secret_name(0),
            CredsPrefix::restore("app").secret_name(0),
            CredsPrefix::snapshot_pin("app").secret_name(0),
            CredsPrefix::snapshot_delete_batch("app-batch").secret_name(0),
            CredsPrefix::maintenance("app").secret_name(0),
            CredsPrefix::verification("app").secret_name(0),
            CredsPrefix::replication("app").secret_name(0),
            CredsPrefix::bootstrap("app").secret_name(0),
            CredsPrefix::snapshot_replication_src("app").secret_name(0),
            CredsPrefix::snapshot_replication_dst("app").secret_name(0),
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "kind slugs must be pairwise distinct");
            }
        }
        // The common backup path keeps its pre-fix on-cluster name.
        assert_eq!(
            CredsPrefix::snapshot_backup("app").secret_name(0),
            "app-creds-0"
        );
        assert_eq!(
            CredsPrefix::maintenance("app").secret_name(0),
            "app-maint-creds-0"
        );
        assert_eq!(
            CredsPrefix::verification("nightly").secret_name(1),
            "nightly-vfy-creds-1"
        );
        assert_eq!(
            CredsPrefix::restore("app").secret_name(0),
            "app-restore-creds-0"
        );
        // The two snapshot-replication sides must never share a copy name.
        assert_eq!(
            CredsPrefix::snapshot_replication_src("offsite").secret_name(0),
            "offsite-srepl-src-creds-0"
        );
        assert_eq!(
            CredsPrefix::snapshot_replication_dst("offsite").secret_name(0),
            "offsite-srepl-dst-creds-0"
        );
    }

    #[test]
    fn creds_prefix_caps_long_names_deterministically() {
        let long = "n".repeat(80);
        let a = CredsPrefix::maintenance(&long).secret_name(0);
        assert!(a.len() <= 63, "{} chars", a.len());
        assert_eq!(a, CredsPrefix::maintenance(&long).secret_name(0));
        assert_ne!(a, CredsPrefix::maintenance(&long).secret_name(1));
    }

    // --- projected_ownership: the stable-name collision guard ----------------

    fn secret_owned_by(uid: &str) -> Secret {
        let mut o = owner("other");
        o.uid = uid.into();
        Secret {
            metadata: ObjectMeta {
                name: Some("app-creds-0".into()),
                owner_references: Some(vec![o]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn ownership_free_when_absent() {
        assert!(matches!(
            projected_ownership(None, "uid-123"),
            ProjectedOwnership::Free
        ));
    }

    #[test]
    fn ownership_self_when_controller_uid_matches() {
        let s = secret_owned_by("uid-123");
        assert!(matches!(
            projected_ownership(Some(&s.metadata), "uid-123"),
            ProjectedOwnership::OwnedBySelf
        ));
    }

    #[test]
    fn ownership_other_when_uid_differs_or_controller_ref_missing() {
        let s = secret_owned_by("uid-999");
        assert!(matches!(
            projected_ownership(Some(&s.metadata), "uid-123"),
            ProjectedOwnership::OwnedByOther(Some(_))
        ));
        // A same-named Secret with NO controller ownerRef (e.g. a user-created
        // one) must also fail closed — never overwrite it.
        let bare = Secret {
            metadata: ObjectMeta {
                name: Some("app-creds-0".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            projected_ownership(Some(&bare.metadata), "uid-123"),
            ProjectedOwnership::OwnedByOther(None)
        ));
    }

    #[test]
    fn name_conflict_message_names_both_owners() {
        let mut other = owner("db");
        other.kind = "Restore".into();
        let msg = projection_name_conflict_message("db-creds-0", "team-a", Some(&other), &ctx());
        // what
        assert!(msg.contains("`db-creds-0`"));
        assert!(msg.contains("`team-a`"));
        // why: who owns it now, and who wanted it
        assert!(msg.contains("Restore `db`"));
        assert!(msg.contains("ClusterRepository `shared`"));
        // fix
        assert!(msg.contains("rename"));

        let unowned = projection_name_conflict_message("db-creds-0", "team-a", None, &ctx());
        assert!(unowned.contains("not managed by kopiur"));
        // Guard the message class against mangled string continuations (a
        // missing `\` embeds the source indentation as literal spaces).
        assert!(
            !msg.contains("  "),
            "message must not embed space runs: {msg}"
        );
        assert!(
            !unowned.contains("  "),
            "message must not embed space runs: {unowned}"
        );
    }

    // --- is_reapable_projected_copy: shrink/disable cleanup fail-closed gate --

    #[test]
    fn reapable_requires_marker_label_and_owner_uid() {
        let src = Secret {
            metadata: ObjectMeta {
                name: Some("repo-pw".into()),
                namespace: Some("kopiur-system".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let ours = build_projected_secret(
            "app-maint-creds-1",
            "team-a",
            owner("app"),
            &src,
            chrono::Utc::now(),
        );
        assert!(is_reapable_projected_copy(&ours.metadata, "uid-123"));
        // Different owner uid: not ours to delete.
        assert!(!is_reapable_projected_copy(&ours.metadata, "uid-999"));
        // No marker label (a legacy per-run copy, or a user Secret): never
        // reaped by the resolve path — the sweep owns the legacy case.
        let mut legacy = ours.clone();
        legacy
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .remove(crate::consts::CREDS_SCOPE_LABEL);
        assert!(!is_reapable_projected_copy(&legacy.metadata, "uid-123"));
    }

    #[test]
    fn projected_secret_copies_data_and_is_owned_and_labeled() {
        let mut src = Secret {
            metadata: ObjectMeta {
                name: Some("repo-pw".into()),
                namespace: Some("kopiur-system".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        src.data = Some(BTreeMap::from([(
            "KOPIA_PASSWORD".to_string(),
            k8s_openapi::ByteString(b"hunter2".to_vec()),
        )]));

        let s = build_projected_secret(
            "job-creds-0",
            "team-a",
            owner("job"),
            &src,
            chrono::Utc::now(),
        );

        assert_eq!(s.metadata.name.as_deref(), Some("job-creds-0"));
        assert_eq!(s.metadata.namespace.as_deref(), Some("team-a"));
        // Owned by the consuming CR in the SAME namespace → valid ownerRef, native GC.
        let owners = s.metadata.owner_references.as_ref().unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].kind, "Snapshot");
        assert_eq!(owners[0].controller, Some(true));
        // Data copied verbatim; type defaulted to Opaque; not immutable (refreshable).
        assert_eq!(s.data, src.data);
        assert_eq!(s.type_.as_deref(), Some("Opaque"));
        assert_eq!(s.immutable, None);
        // Managed labels + source annotation for discoverability.
        let labels = s.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels
                .get("app.kubernetes.io/managed-by")
                .map(String::as_str),
            Some("kopiur")
        );
        assert_eq!(
            labels
                .get("app.kubernetes.io/component")
                .map(String::as_str),
            Some("credentials")
        );
        // The scope marker distinguishes stable per-CR copies from legacy
        // per-run ones, which the periodic sweep reaps (#231).
        assert_eq!(
            labels
                .get(crate::consts::CREDS_SCOPE_LABEL)
                .map(String::as_str),
            Some(crate::consts::CREDS_SCOPE_CR)
        );
        let ann = s.metadata.annotations.as_ref().unwrap();
        assert_eq!(
            ann.get(PROJECTED_FROM_ANNOTATION).map(String::as_str),
            Some("kopiur-system/repo-pw")
        );
        assert!(ann.contains_key(crate::consts::PROJECTED_AT_ANNOTATION));
    }

    /// The sweep pins every delete to the `resourceVersion` it classified, so a
    /// run that re-projects between the sweep's LIST and its DELETE must bump that
    /// `resourceVersion` or the sweep will delete a Secret a live Job is about to
    /// load. A force-SSA of a byte-identical object is a no-op at the apiserver —
    /// no write, no RV bump — and everything else here is a pure function of
    /// (name, ns, owner, source). `projected-at` is the ONLY moving part, so this
    /// inequality is what makes that precondition real. Do not "optimize" it away.
    #[test]
    fn reprojection_differs_so_the_apply_is_a_real_write() {
        let src = Secret {
            metadata: ObjectMeta {
                name: Some("repo-pw".into()),
                namespace: Some("kopiur-system".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let t0 = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = chrono::DateTime::from_timestamp(1_700_003_600, 0).unwrap();

        let first = build_projected_secret("app-creds-0", "team-a", owner("app"), &src, t0);
        let second = build_projected_secret("app-creds-0", "team-a", owner("app"), &src, t1);

        assert_ne!(
            first.metadata.annotations, second.metadata.annotations,
            "a re-projection must produce a different object, or the SSA no-ops \
             and the sweep's resourceVersion precondition cannot fire"
        );
        // Everything a consumer reads is still identical — only the clock moved.
        assert_eq!(first.data, second.data);
        assert_eq!(first.metadata.labels, second.metadata.labels);
    }

    #[test]
    fn source_missing_message_is_actionable() {
        let msg = projection_source_missing_message("repo-pw", "kopiur-system", "team-a", &ctx());
        // names what, where (source), why (cannot project to job ns), and how to fix.
        assert!(msg.contains("`repo-pw`"));
        assert!(msg.contains("`kopiur-system`"));
        assert!(msg.contains("`team-a`"));
        assert!(msg.contains("ClusterRepository `shared`"));
        assert!(msg.contains("KOPIA_PASSWORD"));
    }

    #[test]
    fn unresolved_ns_message_points_at_explicit_namespace() {
        let msg = projection_unresolved_ns_message("repo-pw", &ctx());
        assert!(msg.contains("no resolvable source namespace"));
        assert!(msg.contains("explicit `namespace`"));
        assert!(msg.contains("spec.credentialProjection"));
    }

    #[test]
    fn apply_403_maps_to_rbac_toggle_hint() {
        let mapped = map_projection_apply_error(api_error(403), "job-creds-0", "team-a");
        match mapped {
            Error::MissingDependency(m) => {
                assert!(m.contains("HTTP 403"));
                assert!(m.contains("features.credentialProjection.enabled: true"));
                assert!(m.contains("`job-creds-0`"));
            }
            other => panic!("expected MissingDependency, got {other:?}"),
        }
    }

    #[test]
    fn non_403_error_passes_through_unchanged() {
        assert!(matches!(
            map_projection_apply_error(api_error(500), "job-creds-0", "team-a"),
            Error::Kube(_)
        ));
    }

    // --- shrink_walk_is_vacuous: skip the trailing-copy probe when no
    // trailing index can exist (#382 M2b) ------------------------------------
    #[test]
    fn shrink_walk_vacuous_only_past_the_max_index() {
        // 0 or 1 live refs: index 1 (or 0) may hold a stale trailing copy —
        // the walk must run.
        assert!(!shrink_walk_is_vacuous(0));
        assert!(!shrink_walk_is_vacuous(1));
        // The full set (password + backend auth) covers every index the
        // projector ever writes — nothing past it can exist, skip the probe.
        assert!(shrink_walk_is_vacuous(MAX_CREDS_IDX + 1));
        assert!(shrink_walk_is_vacuous(MAX_CREDS_IDX + 2));
    }

    // --- reap pre-check is a METADATA read (#382 M2b): the reapability
    // decision + precondition pin need only metadata, so the request must
    // carry the PartialObjectMetadata Accept header (the Secret payload stays
    // off the wire). -----------------------------------------------------------
    mod reap_metadata_wire {
        use super::*;
        use std::sync::{Arc, Mutex};

        use http::{Request, Response, StatusCode};
        use kube::Client;
        use kube::client::Body;

        /// Records each request's `Accept` header; answers everything 404.
        fn accept_logging_client(log: Arc<Mutex<Vec<String>>>) -> Client {
            let svc = tower::service_fn(move |req: Request<Body>| {
                let log = log.clone();
                async move {
                    let accept = req
                        .headers()
                        .get(http::header::ACCEPT)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    log.lock().unwrap().push(accept);
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .header("content-type", "application/json")
                            .body(Body::from(
                                serde_json::to_vec(&serde_json::json!({
                                    "kind": "Status", "apiVersion": "v1",
                                    "status": "Failure", "reason": "NotFound",
                                    "code": 404,
                                }))
                                .unwrap(),
                            ))
                            .unwrap(),
                    )
                }
            });
            Client::new(svc, "team-a")
        }

        #[tokio::test]
        async fn reap_precheck_requests_partial_object_metadata() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let api: Api<Secret> = Api::namespaced(accept_logging_client(log.clone()), "team-a");
            let signal = reap_projected_copy(&api, "app-creds-0", "uid-123")
                .await
                .expect("404 pre-check is the benign Absent outcome");
            assert!(matches!(signal, ReapSignal::Absent));
            let accepts = log.lock().unwrap();
            assert_eq!(accepts.len(), 1);
            assert!(
                accepts[0].contains("as=PartialObjectMetadata"),
                "the pre-check must be a metadata-only GET, sent Accept: {}",
                accepts[0]
            );
        }
    }
}
