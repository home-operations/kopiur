use super::*;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::DeleteParams;
use kube::runtime::events::{Event, EventType};
use kube::runtime::reflector::Store;
use kube::{Api, Resource};
use serde::de::DeserializeOwned;

use kopiur_api::Maintenance;
use kopiur_api::common::{RepositoryKind, RepositoryRef};
use kopiur_api::maintenance::{
    MaintenanceSpec, Ownership, RepositoryMaintenanceSpec, default_maintenance_schedule,
};
pub use kopiur_mover::workspec::RestampPolicy;

use crate::consts::{
    CHECK_MAINTENANCE_ACTION, MAINTENANCE_APPLY_FAILED_REASON, MAINTENANCE_CONFIGURED_CONDITION,
    MAINTENANCE_CONFIGURED_REASON, MAINTENANCE_DISABLED_REASON,
    MAINTENANCE_NAMESPACE_UNRESOLVED_REASON,
};
use crate::context::Context;

/// True if any `Maintenance` in the shared informer store references the given
/// repository. **Synchronous** — reads the reflector cache built by the
/// Maintenance controller, so a Repository reconcile answers "is maintenance
/// configured for me?" without an `Api::list` round-trip. `namespace` is `None`
/// for a cluster-scoped `ClusterRepository`. Matching is the pure, exhaustive
/// [`RepositoryRef::resolves_to`].
pub fn repository_has_maintenance(
    store: &Store<Maintenance>,
    kind: RepositoryKind,
    name: &str,
    namespace: Option<&str>,
) -> bool {
    store.state().iter().any(|m| {
        let owner_ns = m.metadata.namespace.as_deref().unwrap_or_default();
        m.spec
            .repository
            .resolves_to(owner_ns, kind, name, namespace)
    })
}

/// True if `m` is an operator-*managed* `Maintenance` owned by the repository
/// `(owner_kind, repo_name)` — i.e. it carries a controller `ownerReference` back
/// to that repository. Managed CRs are projected from `spec.maintenance`; a CR a
/// user hand-authored has no such owner reference and is treated as *foreign*.
pub(crate) fn is_managed_by(m: &Maintenance, owner_kind: &str, repo_name: &str) -> bool {
    m.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|o| o.kind == owner_kind && o.name == repo_name && o.controller == Some(true))
}

/// What the reconciler should do with the operator-managed `Maintenance` for a
/// repository, given the inputs. A closed enum matched exhaustively so a new
/// state can't slip past a reconcile branch (ADR §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceAction {
    /// Create or converge the operator-managed `Maintenance`.
    Manage,
    /// Remove the operator-managed `Maintenance` (it exists but is no longer wanted).
    Unmanage,
    /// Nothing to do (not wanted, and none is managed).
    Leave,
    /// Wanted, but the (cluster-repo) placement namespace is unresolved.
    Unresolved,
}

/// What [`ensure_maintenance`] actually OBSERVED this pass, as a closed set — the input to
/// the `MaintenanceConfigured` condition. Separate from [`MaintenanceAction`] (what the
/// operator decided to DO) because the two disagree exactly where #231 bit: the operator
/// intends to `Manage`, its apply fails, and the repo ends up not covered — which the old
/// boolean `covered` flag lumped in with a deliberate opt-out and reported as
/// "maintenance is disabled (spec.maintenance.enabled: false)", a claim that was simply
/// false. Every not-covered state now says why it is not covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceCoverage {
    /// Wanted, but the (cluster-repo) placement namespace is unresolved.
    Unresolved,
    /// An externally-authored `Maintenance` references the repo.
    CoveredByForeign,
    /// The operator's own managed `Maintenance` is applied and covers the repo.
    CoveredByManaged,
    /// A deliberate opt-out: `spec.maintenance.enabled: false` and nothing else covers it.
    DisabledBySpec,
    /// Maintenance is ENABLED, nothing covers the repository, and the operator could not
    /// apply its managed `Maintenance`.
    ///
    /// The apply error is deliberately NOT carried into the condition message: this runs on
    /// every reconcile, and the hot-loop guard that suppresses an unchanged condition is a
    /// byte comparison — an error string that varies between attempts (a `Conflict` naming a
    /// resourceVersion, an API detail that moves) would defeat it and spin status writes and
    /// Events at full speed. The verbatim error goes to the operator log instead, where it
    /// costs nothing to vary.
    ApplyFailed {
        /// Namespace the managed `Maintenance` was to be applied in.
        namespace: String,
    },
}

/// The `MaintenanceConfigured` condition for an observed [`MaintenanceCoverage`]:
/// `(status, reason, message, warn)`. Pure + exhaustive, so every coverage state — and in
/// particular every NOT-covered state — carries a reason and a message that are true of it
/// (#231). `warn` marks the states that also deserve a Warning Event.
pub fn maintenance_condition(
    coverage: &MaintenanceCoverage,
    metric_kind: &str,
    name: &str,
) -> (bool, &'static str, String, bool) {
    match coverage {
        MaintenanceCoverage::Unresolved => (
            false,
            MAINTENANCE_NAMESPACE_UNRESOLVED_REASON,
            format!(
                "managed Maintenance for {metric_kind} {name} cannot be placed: set \
                 spec.maintenance.namespace, or the operator's KOPIUR_NAMESPACE, so the \
                 namespaced Maintenance CR has a home"
            ),
            true,
        ),
        MaintenanceCoverage::CoveredByForeign => (
            true,
            MAINTENANCE_CONFIGURED_REASON,
            format!("an externally-authored Maintenance references {metric_kind} {name}"),
            false,
        ),
        MaintenanceCoverage::CoveredByManaged => (
            true,
            MAINTENANCE_CONFIGURED_REASON,
            format!("the operator manages a Maintenance for {metric_kind} {name}"),
            false,
        ),
        MaintenanceCoverage::DisabledBySpec => (
            false,
            MAINTENANCE_DISABLED_REASON,
            format!(
                "maintenance is disabled for {metric_kind} {name} (spec.maintenance.enabled: \
                 false) and no Maintenance references it; kopia storage will not be reclaimed"
            ),
            false,
        ),
        MaintenanceCoverage::ApplyFailed { namespace } => (
            false,
            MAINTENANCE_APPLY_FAILED_REASON,
            format!(
                "maintenance is ENABLED for {metric_kind} {name}, but the operator could not \
                 apply its managed Maintenance in namespace {namespace} and nothing else covers \
                 this repository; kopia storage will not be reclaimed until it succeeds. It \
                 retries every reconcile. Fix: check the namespace exists and the operator has \
                 RBAC to write Maintenance there; the apply error itself is in the operator log"
            ),
            true,
        ),
    }
}

/// Pure decision for the managed `Maintenance` (the design matrix in the plan):
/// the operator manages its own only when `enabled` AND no `foreign`
/// (user-authored) `Maintenance` already covers the repo. When it shouldn't,
/// any previously-`managed` one is removed. A wanted-but-unplaceable cluster repo
/// is `Unresolved`.
pub fn maintenance_action(
    enabled: bool,
    foreign: bool,
    managed_exists: bool,
    placement_resolved: bool,
) -> MaintenanceAction {
    if enabled && !foreign {
        if placement_resolved {
            MaintenanceAction::Manage
        } else {
            MaintenanceAction::Unresolved
        }
    } else if managed_exists {
        MaintenanceAction::Unmanage
    } else {
        MaintenanceAction::Leave
    }
}

/// Partition the `Maintenance` CRs referencing repository `(kind, name)` into
/// "is there a *foreign* (user-authored) one?" and "the operator-*managed* one,
/// if present". Pure over an iterator of `Maintenance` so it is unit-tested
/// without a cluster. `match_namespace` is the repository's namespace (`None` for
/// a cluster-scoped `ClusterRepository`); `owner_kind` is the literal CR kind
/// (`"Repository"`/`"ClusterRepository"`) used to recognize our own owner ref.
pub fn classify_maintenance(
    items: impl IntoIterator<Item = Maintenance>,
    kind: RepositoryKind,
    owner_kind: &str,
    name: &str,
    match_namespace: Option<&str>,
) -> (bool, Option<Maintenance>) {
    let mut foreign = false;
    let mut managed = None;
    for m in items {
        let owner_ns = m.metadata.namespace.as_deref().unwrap_or_default();
        if !m
            .spec
            .repository
            .resolves_to(owner_ns, kind, name, match_namespace)
        {
            continue;
        }
        if is_managed_by(&m, owner_kind, name) {
            managed = Some(m);
        } else {
            foreign = true;
        }
    }
    (foreign, managed)
}

/// Whether an externally-authored `Maintenance` already covers repository
/// `(kind, name)` — the [`MaintenanceCoverage::CoveredByForeign`] question,
/// answered from the shared informer store so it is cheaply accessible at
/// bootstrap-work-spec build time (M6), not just from inside
/// [`ensure_maintenance`]'s own pass. A repository whose bootstrap discovers
/// foreign coverage skips stamping/restamping a maintenance owner at all
/// (`BootstrapRepositoryOp::maintenance_owner: None`) — a foreign Maintenance
/// manages its own `ownership.owner`, which has no relation to this
/// repository's lease-derived owner.
///
/// Degrade-not-crash: if the shared informer store has not synced yet, this
/// conservatively answers `false` (unknown ⇒ proceed with the normal
/// stamp/restamp path) rather than blocking bootstrap on cache warmth — the
/// SAME cold-cache tradeoff [`ensure_maintenance`] already makes by skipping
/// entirely until synced.
pub fn maintenance_covered_by_foreign(
    ctx: &Context,
    kind: RepositoryKind,
    owner_kind: &str,
    name: &str,
    match_namespace: Option<&str>,
) -> bool {
    if !ctx.maintenance_synced.load(Ordering::Relaxed) {
        return false;
    }
    classify_maintenance(
        ctx.maintenance_store.state().iter().map(|m| (**m).clone()),
        kind,
        owner_kind,
        name,
        match_namespace,
    )
    .0
}

/// The `BootstrapRepositoryOp` maintenance-owner / restamp-policy / alias
/// triple for a repository's bootstrap work spec (M6).
///
/// `suppress` collects every reason bootstrap must not stamp/restamp a
/// maintenance owner AT ALL: `mode: ReadOnly` (a read-only consumer repo
/// connecting read-write to stamp an owner was exactly the bug that let it
/// clobber the primary's — see the bootstrap `read_only` field instead),
/// `spec.maintenance.enabled: false` (a deliberate opt-out), or an
/// externally-authored `Maintenance` already covering the repository (see
/// [`maintenance_covered_by_foreign`] — its own `ownership.owner` has no
/// relation to this repository's lease-derived one). Every one of those is
/// `None`, never merely "stamp with `AnyStale`".
///
/// When not suppressed: `cluster` absent keeps the pre-M6, single-cluster
/// behavior ([`RestampPolicy::AnyStale`], no aliases — at most one cluster's
/// operator ever bootstraps this repository, so any stale owner is provably
/// this operator's own older stamp or kopia's ephemeral pod identity, never
/// another cluster's). `cluster` present cluster-qualifies the owner and
/// requires [`RestampPolicy::OwnFormatsOnly`] plus the PRE-cluster lease as
/// the sole alias — the anti-ping-pong protection two clusters sharing a
/// same-named repository need (see the variant's own doc for why).
pub fn bootstrap_maintenance_owner_plan(
    kind: RepositoryKind,
    namespace: &str,
    name: &str,
    cluster: Option<&str>,
    suppress: bool,
) -> (Option<String>, RestampPolicy, Vec<String>) {
    if suppress {
        return (None, RestampPolicy::AnyStale, Vec::new());
    }
    let owner = kopiur_api::maintenance::kopia_owner_for_lease(
        &kopiur_api::maintenance::managed_lease(kind, namespace, name, cluster),
    );
    match cluster {
        None => (Some(owner), RestampPolicy::AnyStale, Vec::new()),
        Some(_) => {
            let legacy_owner = kopiur_api::maintenance::kopia_owner_for_lease(
                &kopiur_api::maintenance::managed_lease(kind, namespace, name, None),
            );
            (
                Some(owner),
                RestampPolicy::OwnFormatsOnly,
                vec![legacy_owner],
            )
        }
    }
}

/// The maintenance owner an in-process (bare-path filesystem) `Repository`
/// reconcile should stamp UNCONDITIONALLY on the pass that just CREATED the
/// repository — mirrors the mover's create-path stamp
/// (`crates/mover/src/main.rs`) exactly: kopia auto-assigns the controller
/// pod's ephemeral identity as owner on `repository create`, and nothing
/// recognizes that owner yet, so there is no staleness check to apply. This is
/// distinct from — and takes priority over — the connect-to-existing
/// self-heal, which must instead go through [`maintenance_restamp_target`]
/// against the repository's already-recorded owner so it never clobbers a
/// legitimate foreign one.
///
/// Returns `None` on a connect-to-existing pass (`created: false`) so the
/// caller falls through to the self-heal path, and also returns `None`
/// whenever `desired` is `None` — stamping is suppressed altogether
/// (ReadOnly / `spec.maintenance.enabled: false` / foreign-covered) —
/// regardless of `created`.
///
/// Fixes an M6 regression: the in-process create path used to hardcode
/// `created: false` into `maintenance_restamp_target` even right after its own
/// create, so a freshly-created bare-path `Repository` with
/// `identityDefaults.cluster` set recorded the ephemeral pod identity as
/// owner, and `RestampPolicy::OwnFormatsOnly` (required once `cluster` is set)
/// refused to ever restamp it (the recorded owner is neither empty nor a
/// recognized alias) — the managed `Maintenance`'s `takeoverPolicy: Never`
/// then yielded forever. Pre-M6 self-healed this unconditionally; hand-authored
/// owners are a separate concern (see [`bootstrap_maintenance_owner_plan`]'s
/// `suppress` gate).
pub fn in_process_create_owner_target(created: bool, desired: Option<&str>) -> Option<&str> {
    if created { desired } else { None }
}

/// Build the operator-managed `Maintenance` CR projected from a repository's
/// `spec.maintenance` (ADR §3.7). Pure — the reconciler server-side-applies the
/// result. Naming is 1:1 with the repository (at most one `Maintenance` per
/// repository); the `ownership.owner` lease string is deterministic so the same
/// repository always claims the same lease.
///
/// `placement_namespace` is where the (namespaced) `Maintenance` lives: the
/// repository's own namespace for a `Repository`, or the resolved placement
/// namespace for a `ClusterRepository`. The `repository` ref omits a namespace —
/// a `Repository` ref resolves via the Maintenance's own namespace, and a
/// `ClusterRepository` ref must not carry one.
///
/// `cluster` is `spec.identityDefaults.cluster` (M6): when set, the lease is
/// cluster-qualified (`managed_lease`'s 4-segment format) and the PRE-cluster
/// lease is recorded as the sole owner alias — the migration path so a
/// repository that turns cluster identity on doesn't yield its own lease to
/// what now looks like a foreign owner (`lease_held_by_other`). This CR is
/// only ever projected while maintenance is `enabled` (see
/// [`ensure_maintenance`]'s `Manage` arm), so "an alias exists only where
/// maintenance actually runs" holds structurally — there is no managed
/// `Maintenance` at all to carry a stale alias once disabled.
pub fn build_managed_maintenance(
    kind: RepositoryKind,
    name: &str,
    placement_namespace: &str,
    spec: &RepositoryMaintenanceSpec,
    owner: OwnerReference,
    cluster: Option<&str>,
) -> Maintenance {
    let owner_lease =
        kopiur_api::maintenance::managed_lease(kind, placement_namespace, name, cluster);
    let owner_aliases = if cluster.is_some() {
        vec![kopiur_api::maintenance::managed_lease(
            kind,
            placement_namespace,
            name,
            None,
        )]
    } else {
        Vec::new()
    };
    let mut m = Maintenance::new(
        name,
        MaintenanceSpec {
            repository: RepositoryRef {
                kind,
                name: name.to_string(),
                namespace: None,
            },
            schedule: spec
                .schedule
                .clone()
                .unwrap_or_else(default_maintenance_schedule),
            ownership: Ownership {
                owner: owner_lease,
                owner_aliases,
                takeover_policy: spec.takeover_policy.unwrap_or_default(),
            },
            mover: spec.mover.clone(),
            failure_policy: spec.failure_policy.clone(),
            // Repo-managed maintenance runs where the repository's Secret already
            // lives (its own / the operator namespace), so it never needs projection.
            credential_projection: None,
        },
    );
    m.metadata = child_meta(name, placement_namespace, BTreeMap::new(), Some(owner));
    m
}

/// Project a repository's `spec.maintenance` into an operator-managed
/// `Maintenance` CR, honoring an externally-authored one, and surface the
/// `MaintenanceConfigured` status condition + `kopiur_repository_maintenance_configured`
/// gauge. The replacement for the old "warn when missing" check: maintenance is
/// **default-managed** (ADR §3.7), so the common path creates a `Maintenance`
/// rather than nagging.
///
/// Behavior (see also the design matrix in the plan):
/// - `enabled` (default) **and no foreign Maintenance** → server-side-apply the
///   managed `Maintenance` (create or converge). Condition `True`.
/// - a **foreign** (user-authored) `Maintenance` referencing the repo exists →
///   defer to it; delete any stale managed one. Condition `True`. This holds
///   regardless of `enabled` — `enabled: false` never ignores a user's Maintenance.
/// - `enabled: false` and **no** Maintenance covers it → delete any managed one;
///   condition `False` (reason `MaintenanceDisabled`), **no Warning event** (a
///   deliberate opt-out).
/// - `ClusterRepository` whose managed Maintenance has no resolvable placement
///   namespace → condition `False` + Warning (`MaintenanceNamespaceUnresolved`).
/// - enabled, but the managed `Maintenance` **could not be applied** (a failed SSA, or an
///   un-buildable owner ref) → condition `False` + Warning (`MaintenanceApplyFailed`),
///   naming the namespace and the error. This state used to be reported as
///   `MaintenanceDisabled` — "you set spec.maintenance.enabled: false", which was simply
///   untrue and pointed the operator at the wrong knob (#231).
///
/// Every not-covered state says WHY it is not covered, and the condition is re-evaluated on
/// every reconcile (including the steady-state object-store pass), so a wrong value written
/// during a bootstrap race self-corrects instead of freezing (#231). The Warning Event is
/// published only when the condition actually changes — the condition is the durable
/// signal, the Event marks the transition.
///
/// Degrade-not-crash: if the shared informer store has not synced yet, the whole
/// step is skipped (the `.watches` trigger + periodic requeue re-run it warm), so
/// a cold cache never deletes a managed CR or emits a false signal. `metric_kind`
/// doubles as the owner-reference kind (`"Repository"`/`"ClusterRepository"`);
/// `match_namespace` is the repository's namespace for ref-matching (`None` for a
/// `ClusterRepository`); `placement_namespace` is where the namespaced managed
/// `Maintenance` lives (`None` → unresolved, only possible for a `ClusterRepository`).
/// `cluster` is `spec.identityDefaults.cluster` (M6); threaded straight to
/// [`build_managed_maintenance`] so the managed CR's lease format and alias
/// follow the SAME live spec value this reconcile observed — SSA converges it
/// promptly on a cluster change with no separate bootstrap/migration step.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_maintenance<K>(
    ctx: &Context,
    api: &Api<K>,
    obj: &K,
    regarding: &ObjectReference,
    kind: RepositoryKind,
    metric_kind: &str,
    metric_namespace: &str,
    match_namespace: Option<&str>,
    placement_namespace: Option<&str>,
    name: &str,
    maintenance: Option<&RepositoryMaintenanceSpec>,
    cluster: Option<&str>,
    existing_conditions: &[Condition],
    observed_generation: Option<i64>,
) where
    K: Resource<DynamicType = ()> + DeserializeOwned + Clone + std::fmt::Debug,
{
    if !ctx.maintenance_synced.load(Ordering::Relaxed) {
        return;
    }

    let spec = maintenance.cloned().unwrap_or_default();
    let enabled = spec.enabled;

    let (foreign, managed) = classify_maintenance(
        ctx.maintenance_store.state().iter().map(|m| (**m).clone()),
        kind,
        metric_kind,
        name,
        match_namespace,
    );

    // Exhaustive match on the pure decision so every state is handled (ADR §5.5), and the
    // OBSERVED outcome — including "we tried to manage it and could not" — is carried out
    // as a closed [`MaintenanceCoverage`] rather than collapsed into a boolean.
    let coverage = match maintenance_action(
        enabled,
        foreign,
        managed.is_some(),
        placement_namespace.is_some(),
    ) {
        MaintenanceAction::Manage => {
            let ns = placement_namespace.expect("Manage implies a resolved placement namespace");
            match owner_ref_for(obj, metric_kind) {
                Ok(owner) => {
                    let desired = build_managed_maintenance(kind, name, ns, &spec, owner, cluster);
                    let mapi: Api<Maintenance> = Api::namespaced(ctx.client.clone(), ns);
                    match apply(&mapi, name, &desired).await {
                        Ok(_) => MaintenanceCoverage::CoveredByManaged,
                        // An apply that fails while the managed `Maintenance` ALREADY exists
                        // has not un-covered the repo: the CR is right there, owned, holding
                        // its lease, running on schedule. Since this now runs on every
                        // reconcile, treating a transient apiserver blip as "maintenance is
                        // not configured" would flap the condition (and its gauge, and a
                        // Warning) on every hiccup. Only report the failure when nothing
                        // covers the repo — the state a user actually has to fix.
                        Err(e) if managed.is_some() => {
                            tracing::warn!(error = %e, repo = %name, namespace = %ns, "failed to re-apply the managed Maintenance; the existing one still covers this repository");
                            MaintenanceCoverage::CoveredByManaged
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, repo = %name, namespace = %ns, "failed to apply managed Maintenance");
                            MaintenanceCoverage::ApplyFailed {
                                namespace: ns.to_string(),
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, repo = %name, "cannot build owner reference for managed Maintenance");
                    MaintenanceCoverage::ApplyFailed {
                        namespace: ns.to_string(),
                    }
                }
            }
        }
        MaintenanceAction::Unmanage => {
            // Disabled, or a foreign Maintenance now covers the repo: remove the
            // operator-managed one (idempotent; ignore NotFound).
            if let Some(existing) = &managed {
                let mns = existing
                    .metadata
                    .namespace
                    .as_deref()
                    .unwrap_or(metric_namespace);
                let mapi: Api<Maintenance> = Api::namespaced(ctx.client.clone(), mns);
                if let Err(e) = mapi.delete(name, &DeleteParams::default()).await
                    && !matches!(&e, kube::Error::Api(ae) if ae.code == 404)
                {
                    tracing::warn!(error = %e, repo = %name, "failed to delete managed Maintenance");
                }
            }
            coverage_without_managed(foreign)
        }
        MaintenanceAction::Unresolved => MaintenanceCoverage::Unresolved,
        MaintenanceAction::Leave => coverage_without_managed(foreign),
    };

    let covered = matches!(
        coverage,
        MaintenanceCoverage::CoveredByForeign | MaintenanceCoverage::CoveredByManaged
    );
    ctx.metrics
        .set_repository_maintenance_configured(metric_kind, metric_namespace, name, covered);

    let (status, reason, message, warn) = maintenance_condition(&coverage, metric_kind, name);

    // Publish the Warning while the problem PERSISTS, not just when the condition flips.
    // Kubernetes aggregates repeats of an identical Event (bumping its count and
    // lastTimestamp rather than minting new objects), so re-publishing keeps the signal
    // alive for `kubectl describe` and Event-driven alerting; gating it on a condition
    // change instead would emit exactly one Event ever and then go silent forever once it
    // aged out of the Event TTL — while kopia storage quietly never got reclaimed.
    if warn
        && let Err(e) = ctx
            .recorder
            .publish(
                &Event {
                    type_: EventType::Warning,
                    reason: reason.into(),
                    note: Some(message.clone()),
                    action: CHECK_MAINTENANCE_ACTION.into(),
                    secondary: None,
                },
                regarding,
            )
            .await
    {
        tracing::warn!(error = %e, repo = %name, "failed to publish {reason} event");
    }

    let conditions = upsert_condition(
        existing_conditions,
        MAINTENANCE_CONFIGURED_CONDITION,
        status,
        reason,
        &message,
        observed_generation,
    );
    // Skip the write when the upsert changed nothing: this runs on EVERY repo
    // reconcile, and an identical re-write would bump `resourceVersion` and
    // re-trigger the repo controller through its own watch (hot-loop). Every `message`
    // above is a pure function of (coverage, kind, name), so a steady state produces a
    // byte-identical condition and this guard always fires.
    if conditions.as_slice() == existing_conditions {
        return;
    }
    if let Err(e) = patch_status(api, name, serde_json::json!({ "conditions": conditions })).await {
        tracing::warn!(error = %e, repo = %name, "failed to patch MaintenanceConfigured condition");
    }
}

/// Coverage when the operator is NOT managing a `Maintenance` of its own (the `Unmanage` /
/// `Leave` arms). A foreign `Maintenance` still covers the repo; otherwise the only way to
/// reach these arms is `enabled == false` (`maintenance_action` routes an enabled,
/// un-covered repo to `Manage`/`Unresolved`), so "disabled by spec" is provably true here
/// rather than a catch-all guess (#231).
fn coverage_without_managed(foreign: bool) -> MaintenanceCoverage {
    if foreign {
        MaintenanceCoverage::CoveredByForeign
    } else {
        MaintenanceCoverage::DisabledBySpec
    }
}
