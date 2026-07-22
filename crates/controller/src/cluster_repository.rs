//! The `ClusterRepository` reconciler (ADR §3.2, §2.3).
//!
//! Same storage lifecycle as [`crate::repository`] (connect/create, status,
//! catalog scan), plus the cluster-scoped placement rule for discovered
//! `Snapshot` CRs (ADR §2.3): a discovered snapshot is materialized in the
//! namespace named by its identity hostname **if** that namespace exists and is
//! in `allowedNamespaces`; otherwise it falls back to `catalog.fallbackNamespace`
//! — extended (M4, multi-cluster shared repo) to classify each hostname
//! `Bare`/`OwnCluster`/`ForeignCluster` first (`identityDefaults.cluster`) and
//! drop/collect another cluster's snapshots per `catalog.foreignSnapshots`.
//!
//! [`crate::catalog::decide_cluster_placement`] encodes that rule purely and is
//! unit-tested; the existence check and `Snapshot` creation are the thin IO parts
//! ([`crate::catalog::scan`]).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};

use kopiur_api::backend::Backend;
use kopiur_api::common::{CatalogBounds, ForeignSnapshots, RepositoryKind, RepositoryRef};
use kopiur_api::repository::resolve_index_blob_warn_threshold;
use kopiur_api::{ClusterRepository, RepositoryPhase, validate};
use kopiur_kopia::{ConnectSpec, SnapshotListEntry};
use kopiur_mover::bootstrap::{BootstrapResult, RESULT_CONFIGMAP_KEY};
use kopiur_mover::workspec::{
    BootstrapRepositoryOp, MoverOptions, MoverWorkSpec, Operation, ResolvedIdentity, TargetRef,
};

use crate::catalog;
use crate::consts::{
    API_VERSION, CLUSTER_REPOSITORY_LABEL, CLUSTER_REPOSITORY_UID_LABEL,
    REPOSITORY_BOOTSTRAPPED_CONDITION, SERVER_CLEANUP_FINALIZER,
};
use crate::context::Context;
use crate::error::{Error, Result, TERMINAL_HEARTBEAT, error_policy_for};
use crate::health;
use crate::io;
use crate::jobs::{self, JobLimits, MoverJobInputs};
use crate::server::{
    ServerReconcileCtx, generated_secret_name, mirrored_creds_secret_name, reconcile_server,
    server_object_name, server_status_json,
};
use crate::snapshot::backend_to_repository_connect;

/// Resolve where a `ClusterRepository`'s managed (namespaced) `Maintenance` CR
/// should live (ADR §3.7): `spec.maintenance.namespace` if set, else the
/// operator's own namespace (`KOPIUR_NAMESPACE`). `None` when neither is available —
/// `ensure_maintenance` then surfaces an actionable `MaintenanceNamespaceUnresolved`
/// condition rather than guessing.
fn cluster_maintenance_placement(ctx: &Context, repo: &ClusterRepository) -> Option<String> {
    repo.spec
        .maintenance
        .as_ref()
        .and_then(|m| m.namespace.clone())
        .or_else(|| ctx.operator_namespace.clone())
}

/// Project this `ClusterRepository`'s `spec.maintenance` into its managed `Maintenance` CR
/// and refresh the `MaintenanceConfigured` condition (ADR §3.7; §11: ReadOnly cluster repos
/// run no maintenance).
///
/// `conditions` is the condition set to build on — the FRESHLY patched set on the bootstrap
/// finalize path (so this patch doesn't drop the `Bootstrapped` condition written moments
/// earlier), or the cached status elsewhere.
///
/// Called on every reconcile that gets far enough to know the repo's state, including the
/// steady-state pass of an already-`Ready` object-store repo. That last one is #231: the
/// condition used to be written only at the tail of a bootstrap, so a value written during
/// a bootstrap race (the informer store had not yet seen the just-applied Maintenance, or
/// the apply transiently failed) froze — reading `MaintenanceDisabled` for days while the
/// managed `Maintenance` sat right there, owned and running.
async fn ensure_cluster_maintenance(
    ctx: &Context,
    repo: &ClusterRepository,
    name: &str,
    api: &Api<ClusterRepository>,
    conditions: &[Condition],
) {
    if !repo.spec.mode.allows_writes() {
        return;
    }
    let placement = cluster_maintenance_placement(ctx, repo);
    let cluster = repo
        .spec
        .identity_defaults
        .as_ref()
        .and_then(|d| d.cluster.as_deref());
    io::ensure_maintenance(
        ctx,
        api,
        repo,
        &io::event_ref(repo),
        RepositoryKind::ClusterRepository,
        "ClusterRepository",
        "",
        None,
        placement.as_deref(),
        name,
        repo.spec.maintenance.as_ref(),
        cluster,
        conditions,
        repo.metadata.generation,
    )
    .await;
}

/// The `ClusterRepository`'s currently-observed status conditions (empty when no status).
fn cluster_conditions(repo: &ClusterRepository) -> Vec<Condition> {
    repo.status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default()
}

/// This `ClusterRepository` as a [`RepositoryRef`] (cluster-scoped: no namespace),
/// matching how consumers pin it, so the mass-deletion condition's `repo_key`
/// lines up with the deletion path's per-repo count.
fn cluster_repository_ref(repo: &ClusterRepository) -> RepositoryRef {
    RepositoryRef {
        kind: RepositoryKind::ClusterRepository,
        name: repo.name_any(),
        namespace: None,
    }
}

/// The raw `allow-mass-deletion` annotation value on this `ClusterRepository`.
fn mass_deletion_ack_raw(repo: &ClusterRepository) -> Option<&str> {
    repo.metadata
        .annotations
        .as_ref()?
        .get(crate::consts::ALLOW_MASS_DELETION_ANNOTATION)
        .map(String::as_str)
}

/// Fold the non-blocking `MassDeletionHeld` condition into `conditions` from the
/// live Snapshot store (ADR-0005 §6; shared
/// [`crate::snapshot::repo_mass_deletion_conditions`]). Unchanged when the store
/// is unset/unsynced.
async fn fold_mass_deletion(
    ctx: &Context,
    repo: &ClusterRepository,
    conditions: &[Condition],
) -> Vec<Condition> {
    crate::snapshot::repo_mass_deletion_conditions(
        ctx,
        &cluster_repository_ref(repo),
        mass_deletion_ack_raw(repo),
        repo.spec.deletion_protection.as_ref(),
        repo.spec.on_namespace_delete,
        conditions,
        repo.metadata.generation,
    )
    .await
}

/// Which namespace a cluster-scoped repository reads its credential `Secret` from (and
/// therefore runs its bootstrap `Job` in): the reference's explicit `namespace` if set,
/// else the operator's own namespace (`KOPIUR_NAMESPACE`).
///
/// A `ClusterRepository` has no namespace of its own, so "absent = the referrer's
/// namespace" cannot mean anything for it. Rather than refuse the object (#232: the
/// reconcile hard-errored `passwordSecretRef.namespace is required`, contradicting the
/// field's own documentation), default to the operator namespace — the same rule
/// [`cluster_maintenance_placement`] already applies to `spec.maintenance.namespace`, and
/// the namespace where a chart install's repository Secret conventionally lives.
///
/// Only an off-chart install with no `KOPIUR_NAMESPACE` has neither, and that is a real
/// misconfiguration: refuse it with both fixes rather than guess a namespace. Pure.
fn cluster_secret_namespace(
    explicit: Option<&str>,
    operator_namespace: Option<&str>,
) -> Result<String> {
    explicit
        .or(operator_namespace)
        .map(str::to_string)
        .ok_or_else(|| {
            Error::Validation(
                "ClusterRepository encryption.passwordSecretRef.namespace is not set and the \
                 operator's own namespace is unknown (KOPIUR_NAMESPACE is unset), so there is no \
                 namespace to read the repository's credential Secret from — a cluster-scoped \
                 ClusterRepository has none of its own to fall back on. Set \
                 encryption.passwordSecretRef.namespace to the Secret's namespace, or set \
                 KOPIUR_NAMESPACE on the controller (the Helm chart does this automatically)."
                    .into(),
            )
        })
}

/// Reconcile a `ClusterRepository`.
#[tracing::instrument(skip(repo, ctx), fields(kind = "ClusterRepository", name = %repo.name_any()))]
pub async fn reconcile(repo: Arc<ClusterRepository>, ctx: Arc<Context>) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(&repo, &ctx).await;
    ctx.metrics
        .record_reconcile("ClusterRepository", start.elapsed().as_secs_f64());
    record_cluster_repository_status_metrics(&repo, &ctx, result.is_ok()).await;
    result
}

/// Mirror a ClusterRepository's catalog gauges (cluster-scoped, so the `namespace`
/// label is empty). The lifecycle *phase* is no longer written here: it is a
/// store-backed observable gauge (see
/// [`crate::metrics::Metrics::register_resource_observers`]), so a deleted
/// ClusterRepository's series drops on its own. Freshest status is re-read on success.
async fn record_cluster_repository_status_metrics(
    repo: &ClusterRepository,
    ctx: &Context,
    ok: bool,
) {
    let name = repo.name_any();
    if repo.metadata.deletion_timestamp.is_some() || !ok {
        return;
    }
    let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
    if let Ok(Some(latest)) = api.get_opt(&name).await
        && let Some(status) = latest.status.as_ref()
    {
        let snapshots = status.storage_stats.as_ref().and_then(|s| s.snapshot_count);
        let discovered = status
            .catalog
            .as_ref()
            .and_then(|c| c.discovered_backup_count);
        let foreign = status
            .catalog
            .as_ref()
            .and_then(|c| c.foreign_snapshot_count);
        if snapshots.is_some() || discovered.is_some() || foreign.is_some() {
            ctx.metrics
                .set_repo_catalog("", &name, snapshots, discovered, foreign);
        }
    }
}

async fn reconcile_inner(repo: &ClusterRepository, ctx: &Context) -> Result<Action> {
    let errs = validate::validate_cluster_repository(&repo.spec);
    if let Some(first) = errs.into_iter().next() {
        return Err(Error::Validation(first.to_string()));
    }

    let name = repo.name_any();
    let api: Api<ClusterRepository> = Api::all(ctx.client.clone());

    // Deletion: a cluster-scoped owner cannot own its namespaced server children,
    // so owner-ref GC can't reap them — clean up explicitly via the finalizer.
    if repo.metadata.deletion_timestamp.is_some() {
        return handle_cluster_deletion(ctx, repo, &name, &api).await;
    }

    // Ensure the server-cleanup finalizer while a server is configured, so the
    // deletion path above can run before the CR is removed.
    let server_enabled = repo.spec.server.is_some();
    if server_enabled && io::ensure_finalizer(&api, repo, SERVER_CLEANUP_FINALIZER).await? {
        return Ok(Action::requeue(Duration::from_secs(2)));
    }

    // §14(e): a suspended ClusterRepository skips connect/bootstrap and maintenance
    // projection — a declarative pause surfaced via a condition.
    if repo.spec.suspend {
        let conds = repo
            .status
            .as_ref()
            .map(|s| s.conditions.clone())
            .unwrap_or_default();
        let conditions = io::set_ready(
            &conds,
            repo.metadata.generation,
            io::ReadyOutcome::Reconciling,
            "Suspended",
            "ClusterRepository is suspended (spec.suspend); skipping connect and maintenance",
        );
        let current = serde_json::to_value(&repo.status).ok();
        io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            serde_json::json!({ "observedGeneration": repo.metadata.generation, "conditions": conditions }),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(300)));
    }

    // The optional kopia web-UI server runs regardless of backend (the server pod
    // connects to the repository itself). Reconcile it before the backend match
    // below — whose branches return early — so it is applied/migrated/torn down on
    // every reconcile. Children live in `spec.server.namespace` (cluster-scoped
    // owners can't own them); cleanup is via the finalizer + back-reference labels.
    //
    // Degrade, don't crash: the web UI is auxiliary — backups and restores do not depend on
    // it — so a problem here must NOT abort the reconcile before the backend match below,
    // which is what actually bootstraps the repository. It used to `?`, so a server whose
    // credentials Secret is not where we expect (e.g. an absent `passwordSecretRef.namespace`
    // now resolving to the operator namespace rather than the server's) would take the whole
    // repository down with it. Surface it as a Warning and carry on.
    if let Err(e) = reconcile_cluster_server(ctx, repo, &name, &api).await {
        tracing::warn!(error = %e, repo = %name, "failed to reconcile the kopia repository server; the repository itself is unaffected");
        io::publish_warning_event(
            ctx,
            repo,
            "RepositoryServerDegraded",
            "CheckRepositoryServer",
            &format!(
                "the kopia repository server (spec.server) could not be reconciled: {e}. The \
                 repository itself is unaffected — backups and restores continue. If its \
                 credentials Secret lives in the server's namespace, pin it explicitly with \
                 encryption.passwordSecretRef.namespace (a ClusterRepository otherwise reads it \
                 from the operator's namespace)."
            ),
        )
        .await;
    }
    // Once a server is no longer configured (and any teardown above has run), the
    // cleanup finalizer is no longer needed.
    if !server_enabled {
        io::remove_finalizer(&api, repo, SERVER_CLEANUP_FINALIZER).await?;
    }

    // Same connect/create/status lifecycle as Repository. A cluster-scoped secret ref has
    // no referrer namespace to inherit, so an absent one resolves to the operator's
    // namespace (`cluster_secret_namespace`).
    match &repo.spec.backend {
        // A PVC/NFS-backed filesystem repo is NOT reachable from the controller
        // in-process (the controller pod can't mount the repo volume), so — exactly
        // like a namespaced Repository and like object stores — it bootstraps in a
        // short mover Job that mounts the volume. WITHOUT this guard the in-process
        // connect/create below runs in the CONTROLLER pod where the volume isn't
        // mounted, "creates" the repo in the wrong place, and FALSELY reports `Ready`
        // while the real PVC stays empty — so every consumer then fails far away with
        // a cryptic `repository not initialized in the provided storage`. Routing
        // through the mover Job also makes the status honest: a failed bootstrap Job
        // surfaces `Failed`/`Degraded` + an actionable Event via
        // `finalize_cluster_bootstrap`, never a misleading `Ready`.
        Backend::Filesystem(fs) if fs.volume.is_some() => {
            return bootstrap_cluster_via_mover(ctx, repo, &name, &api, &repo.spec.backend).await;
        }
        Backend::Filesystem(fs) => {
            // Read the password Secret up front; its `resourceVersion` drives the
            // hard-stop below (see the namespaced Repository reconciler for the full
            // rationale: a credential fix does not bump `generation`, so the gate must
            // also key on the Secret revision).
            let creds = io::repo_credentials(&repo.spec.encryption);
            let secret_ns = cluster_secret_namespace(
                creds.namespace.as_deref(),
                ctx.operator_namespace.as_deref(),
            )?;
            let (password, cred_version) =
                io::read_repo_credential(&ctx.client, &secret_ns, &creds).await?;

            // Hard-stop: terminally Failed for this spec generation AND the password
            // Secret unchanged since → quiet heartbeat. Reopens on a spec change
            // (generation) or a Secret content edit (resourceVersion; re-triggered by
            // the Secret watch in `lib.rs`).
            if io::terminal_gate_holds(
                repo.status.as_ref().and_then(|s| s.phase),
                repo.status.as_ref().and_then(|s| s.observed_generation),
                repo.metadata.generation,
                repo.status
                    .as_ref()
                    .and_then(|s| s.resolved_credential_version.as_deref()),
                &cred_version,
            ) {
                return Ok(Action::requeue(TERMINAL_HEARTBEAT));
            }

            let client = ctx.kopia.build([("KOPIA_PASSWORD".to_string(), password)]);
            let spec = ConnectSpec::Filesystem {
                path: fs.path.clone().into(),
            };
            if let Err(e) = client
                .repository_connect(&spec, kopiur_kopia::CacheTuning::default())
                .await
            {
                // Part A: never auto-create over a once-`Ready` repository (pinned
                // `uniqueId`) — a wiped/unreachable backend surfaces as a failure,
                // never a silent fresh empty repo.
                let create_enabled = health::auto_create_allowed(
                    kopiur_api::common::create_enabled(repo.spec.create.as_ref()),
                    repo.status.as_ref().and_then(|s| s.unique_id.as_deref()),
                );
                // Try create-then-connect when enabled and the failure isn't
                // "repo already there" (auth/locked); otherwise the connect error
                // is terminal. A terminal failure (connect OR a failed create)
                // surfaces Failed + an actionable Event (e.g. filesystem "Access
                // Denied") rather than an invisible reconcile error with no status.
                let create_opts = kopiur_mover::workspec::CreateOptionsSpec::from_create(
                    repo.spec.create.as_ref(),
                )
                .to_kopia();
                let outcome =
                    if kopiur_mover::bootstrap::should_attempt_create(create_enabled, e.class()) {
                        match client
                            .repository_create(
                                &spec,
                                kopiur_kopia::CacheTuning::default(),
                                &create_opts,
                            )
                            .await
                        {
                            Ok(_) => {
                                client
                                    .repository_connect(&spec, kopiur_kopia::CacheTuning::default())
                                    .await
                            }
                            Err(ce) => Err(ce),
                        }
                    } else {
                        Err(e)
                    };
                if let Err(e) = outcome {
                    let class = e.class();
                    let retryable = class.is_retryable();
                    let phase_enum = if retryable {
                        RepositoryPhase::Degraded
                    } else {
                        RepositoryPhase::Failed
                    };
                    let phase = if retryable { "Degraded" } else { "Failed" };
                    // Stable, volatile-free condition message; full stderr → Event.
                    let conditions =
                        cluster_bootstrap_condition(repo, false, class.as_str(), class.summary());
                    // kstatus conditions so Flux sees a retryable connect as
                    // Reconciling and a terminal one as Stalled, never a frozen
                    // Ready/Suspended (issue #245).
                    let conditions = io::set_ready(
                        &conditions,
                        repo.metadata.generation,
                        io::ready_outcome_for_phase(phase_enum),
                        class.as_str(),
                        class.summary(),
                    );
                    let current = serde_json::to_value(&repo.status).ok();
                    let wrote = io::patch_status_if_changed(
                        &api,
                        &name,
                        current.as_ref(),
                        serde_json::json!({
                            "phase": phase,
                            "backend": "Filesystem",
                            "observedGeneration": repo.metadata.generation,
                            // Pin the Secret revision we just failed with, so a later
                            // content fix (same generation) reopens the hard-stop gate.
                            "resolvedCredentialVersion": cred_version,
                            "conditions": conditions,
                        }),
                    )
                    .await?;
                    if wrote {
                        io::publish_backend_failure(
                            ctx,
                            &io::event_ref(repo),
                            &name,
                            class,
                            &e.to_string(),
                        )
                        .await;
                    }
                    return if retryable {
                        Err(Error::Kopia(e))
                    } else {
                        Ok(Action::requeue(TERMINAL_HEARTBEAT))
                    };
                }
            }
            let status = client.repository_status().await?;
            let allowed_count = allowed_namespace_count(&repo.spec.allowed_namespaces);
            // kstatus Ready/Reconciling/Stalled for the healthy bare-path repo
            // (issue #245), layered on any existing conditions so the merge-patch
            // replace keeps them. A resume through this in-process path must clear
            // the suspend branch's stale entries just like the mover path does.
            let existing = repo
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            // Non-blocking MassDeletionHeld condition from the live Snapshot store.
            let existing = fold_mass_deletion(ctx, repo, &existing).await;
            let conditions = io::set_ready(
                &existing,
                repo.metadata.generation,
                io::ready_outcome_for_phase(RepositoryPhase::Ready),
                "Connected",
                "connected to the existing repository",
            );
            let mut status_patch = serde_json::json!({
                "phase": "Ready",
                "backend": "Filesystem",
                "uniqueId": status.unique_id_hex,
                "allowedNamespaceCount": allowed_count,
                "observedGeneration": repo.metadata.generation,
                "resolvedCredentialVersion": cred_version,
                "conditions": conditions,
            });
            // Record a `Snapshot`'s reverify nudge as honored. This in-process arm
            // re-probes on every reconcile (the connect above) and the annotation patch
            // already retriggered us, so the re-probe is implicit — we only stamp
            // `lastReverifyAt` so the once-honored token is observable in status and
            // consistent with the mover bootstrap path.
            let reverify_token = repo
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(crate::consts::REVERIFY_REQUESTED_ANNOTATION))
                .map(String::as_str);
            if let Some(token) = reverify_token
                && repo
                    .status
                    .as_ref()
                    .and_then(|s| s.last_reverify_at.as_deref())
                    != Some(token)
            {
                status_patch["lastReverifyAt"] = serde_json::Value::String(token.to_string());
            }
            let current = serde_json::to_value(&repo.status).ok();
            io::patch_status_if_changed(&api, &name, current.as_ref(), status_patch).await?;

            // Catalog scan on the `catalog.refreshInterval` cadence: a bare-path
            // filesystem repo re-lists in-process. Each discovered snapshot is
            // placed in the namespace named by its identity hostname when that
            // namespace exists and passes the tenancy gate, else
            // `catalog.fallbackNamespace`, else it is skipped with a Warning Event
            // (`crate::catalog::decide_cluster_placement` + `crate::catalog::scan`).
            let interval = CatalogBounds::effective_refresh_interval(repo.spec.catalog.as_ref());
            if catalog::scan_due(
                repo.metadata.generation,
                repo.status.as_ref().and_then(|s| s.observed_generation),
                cluster_last_refresh_at(repo),
                interval,
                CatalogBounds::periodic_refresh_enabled(repo.spec.catalog.as_ref()),
                cluster_scan_requested_token(repo),
                cluster_scan_requested_honored(repo),
                chrono::Utc::now(),
            ) {
                let listing = client.snapshot_list(None).await?;
                let total = listing.len() as i64;
                run_cluster_catalog_scan(ctx, repo, &name, &listing, total, false, 0).await?;
            }

            // Ensure the managed Maintenance for this ClusterRepository (ADR §3.7).
            // Cluster-scoped, so the metric namespace label is empty and
            // ref-matching ignores namespace. The (namespaced) Maintenance lands in
            // spec.maintenance.namespace, else the operator's own namespace.
            ensure_cluster_maintenance(ctx, repo, &name, &api, &cluster_conditions(repo)).await;
        }
        other => {
            // Object-store backends bootstrap via a short-lived mover Job (ADR
            // §5.4). The Job runs in the credentials Secret's namespace (so its
            // `envFrom` resolves) and is owned by this cluster-scoped CR (a
            // namespaced dependent may have a cluster-scoped owner; GC works).
            return bootstrap_cluster_via_mover(ctx, repo, &name, &api, other).await;
        }
    }

    Ok(Action::requeue(catalog::reconcile_interval(
        repo.spec.catalog.as_ref(),
    )))
}

/// `status.catalog.lastRefreshAt` from the cached object (the refresh-due gate).
fn cluster_last_refresh_at(repo: &ClusterRepository) -> Option<&str> {
    repo.status
        .as_ref()
        .and_then(|s| s.catalog.as_ref())
        .and_then(|c| c.last_refresh_at.as_deref())
}

/// The live `catalog-scan-requested-at` annotation value (opaque token; the
/// writer is the policy reconciler, M6). `None` when never requested.
fn cluster_scan_requested_token(repo: &ClusterRepository) -> Option<&str> {
    repo.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(crate::consts::CATALOG_SCAN_REQUESTED_ANNOTATION))
        .map(String::as_str)
}

/// `status.catalog.scanRequestHonored` from the cached object — the token last
/// retired by a completed scan (equality-compared against the live annotation).
fn cluster_scan_requested_honored(repo: &ClusterRepository) -> Option<&str> {
    repo.status
        .as_ref()
        .and_then(|s| s.catalog.as_ref())
        .and_then(|c| c.scan_request_honored.as_deref())
}

/// `status.catalog.scanRequestAttemptAt` from the cached object — rate-limits
/// how often a pending token may (re-)launch a bootstrap/scan attempt.
fn cluster_scan_requested_attempt_at(repo: &ClusterRepository) -> Option<&str> {
    repo.status
        .as_ref()
        .and_then(|s| s.catalog.as_ref())
        .and_then(|c| c.scan_request_attempt_at.as_deref())
}

/// The `DiscoveredSnapshotUnplaced` Warning event body: names the offending
/// hostnames and both fixes — the pre-M4 fixes (create/allow the namespace, or set
/// a fallback) AND the shared-repo posture (cluster identity + `Ignore`) so a
/// repository intentionally shared across clusters points at the fix that's
/// actually expected there, not just the single-cluster remedies. Pure.
fn unplaced_warning_message(hosts: &[&str]) -> String {
    format!(
        "discovered snapshots were not materialized: identity hostname(s) [{}] name no \
         existing namespace in spec.allowedNamespaces. Why: a ClusterRepository places each \
         discovered Snapshot in the namespace its identity hostname names. Fix: create/allow \
         those namespaces, or set spec.catalog.fallbackNamespace to collect foreign \
         snapshots in one namespace; if this repository is shared across clusters, set \
         identityDefaults.cluster and catalog.foreignSnapshots: Ignore so another cluster's \
         snapshots are counted (status.catalog.foreignSnapshotCount) rather than surfaced \
         as unplaced",
        hosts.join(", ")
    )
}

/// Run the shared catalog scan ([`crate::catalog::scan`]) for a
/// `ClusterRepository` and reflect the outcome: each discovered snapshot lands in
/// the namespace named by its identity hostname when allowed, else
/// `catalog.fallbackNamespace`; snapshots with neither are surfaced via a Warning
/// Event naming the fix (never silently dropped).
#[allow(clippy::too_many_arguments)]
async fn run_cluster_catalog_scan(
    ctx: &Context,
    repo: &ClusterRepository,
    name: &str,
    listing: &[SnapshotListEntry],
    total_snapshot_count: i64,
    listing_truncated: bool,
    // Entries the mover's foreign-suffix prefilter already dropped before this
    // scan ever saw its `listing` (0 for the in-process bare-filesystem path,
    // which never prefilters). Added to `outcome.foreign` — never double-counted,
    // since a mover-dropped entry never reaches this scan's listing at all.
    foreign_prefilter_dropped: i64,
) -> Result<()> {
    let repo_uid = repo
        .uid()
        .ok_or_else(|| Error::Invariant("ClusterRepository has no uid".into()))?;
    let owner_ref = io::owner_ref_for(repo, "ClusterRepository")?;
    let cluster = repo
        .spec
        .identity_defaults
        .as_ref()
        .and_then(|d| d.cluster.as_deref());
    let fallback = repo
        .spec
        .catalog
        .as_ref()
        .and_then(|c| c.fallback_namespace.as_deref());
    let outcome = catalog::scan(
        ctx,
        catalog::ScanOwner::ClusterRepository { name },
        owner_ref,
        &repo_uid,
        catalog::Placement::Cluster {
            allowed: &repo.spec.allowed_namespaces,
            fallback,
        },
        cluster,
        repo.spec.catalog.as_ref(),
        listing,
        listing_truncated,
    )
    .await?;
    let foreign_total = outcome.foreign + foreign_prefilter_dropped;

    if !outcome.unplaced_hosts.is_empty() {
        let hosts: Vec<&str> = outcome.unplaced_hosts.iter().map(String::as_str).collect();
        let msg = unplaced_warning_message(&hosts);
        tracing::warn!(repo = %name, hosts = ?hosts, "discovered snapshots skipped: no placement namespace");
        io::publish_warning_event(ctx, repo, "DiscoveredSnapshotUnplaced", "CatalogScan", &msg)
            .await;
    }

    // Cluster-scoped: the metric namespace label is empty, matching the
    // phase/catalog gauges in `record_cluster_repository_status_metrics`.
    let size_bytes = crate::repository::logical_bytes_under_management(listing);
    ctx.metrics.set_repo_size_bytes("", name, size_bytes);

    let mut catalog_patch = serde_json::json!({
        "discoveredBackupCount": outcome.discovered,
        "lastRefreshAt": chrono::Utc::now().to_rfc3339(),
        "foreignSnapshotCount": foreign_total,
    });
    // Retire a pending `catalog-scan-requested-at` token: ANY completed scan (not
    // just one the token itself triggered) honors it — see the `Repository` twin
    // (`run_catalog_scan`). `scanRequestAttemptAt` is deliberately left as-is;
    // equality retirement makes it inert from here on regardless of its value.
    if let Some(token) = cluster_scan_requested_token(repo) {
        catalog_patch["scanRequestHonored"] = serde_json::Value::String(token.to_string());
    }
    let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
    io::patch_status(
        &api,
        name,
        serde_json::json!({
            "catalog": catalog_patch,
            "storageStats": { "snapshotCount": total_snapshot_count, "totalSizeBytes": size_bytes },
        }),
    )
    .await?;
    Ok(())
}

/// Apply / migrate / tear down the optional kopia web-UI server for a
/// `ClusterRepository`. Children live in `spec.server.namespace` (cluster-scoped
/// owners can't own them), tracked by back-reference labels for `.watches()` and
/// finalizer cleanup.
async fn reconcile_cluster_server(
    ctx: &Context,
    repo: &ClusterRepository,
    name: &str,
    api: &Api<ClusterRepository>,
) -> Result<()> {
    let cluster_server = repo.spec.server.as_ref();
    let desired_ns = cluster_server.map(|s| s.namespace.clone());
    let observed_ns = repo
        .status
        .as_ref()
        .and_then(|s| s.server.as_ref())
        .and_then(|sv| sv.namespace.clone());

    let uid = repo.uid().unwrap_or_default();
    let extra_labels = BTreeMap::from([
        (CLUSTER_REPOSITORY_LABEL.to_string(), name.to_string()),
        (CLUSTER_REPOSITORY_UID_LABEL.to_string(), uid),
    ]);

    // Which namespace the server MIRRORS the credentials Secret from. Same rule the
    // repository itself uses (`cluster_secret_namespace`): the reference's explicit
    // namespace, else the operator's — so "absent" means ONE thing on a ClusterRepository.
    // Were this to keep defaulting to the server namespace while the repository defaulted
    // to the operator's, a user who followed the documented default (Secret in the operator
    // namespace) would bootstrap fine and then get a server pod wedged on a Secret that
    // isn't in its namespace. The server namespace remains the last resort.
    let creds = io::repo_credentials(&repo.spec.encryption);
    let creds_src_namespace = creds
        .namespace
        .clone()
        .or_else(|| ctx.operator_namespace.clone())
        .or_else(|| desired_ns.clone())
        .unwrap_or_default();

    let rc = ServerReconcileCtx {
        client: &ctx.client,
        instance: name,
        backend: &repo.spec.backend,
        encryption: &repo.spec.encryption,
        server: cluster_server.map(|s| &s.server),
        read_only_mode: !repo.spec.mode.allows_writes(),
        target_namespace: desired_ns,
        observed_namespace: observed_ns,
        owner: None,
        extra_labels,
        creds_src_namespace,
        is_cluster: true,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        service_account: ctx.mover_service_account.as_deref(),
    };

    let outcome = reconcile_server(&rc).await?;
    if let Some(status) = server_status_json(&outcome) {
        io::patch_status(api, name, status).await?;
    }
    Ok(())
}

/// On `ClusterRepository` deletion, tear down the server children in their
/// namespace (owner-ref GC can't, being cross-scope), then drop the finalizer.
async fn handle_cluster_deletion(
    ctx: &Context,
    repo: &ClusterRepository,
    name: &str,
    api: &Api<ClusterRepository>,
) -> Result<Action> {
    if !repo
        .finalizers()
        .iter()
        .any(|f| f == SERVER_CLEANUP_FINALIZER)
    {
        return Ok(Action::await_change());
    }

    // The namespace to clean is the last-applied one (pinned to status), falling
    // back to the spec's target.
    let ns = repo
        .status
        .as_ref()
        .and_then(|s| s.server.as_ref())
        .and_then(|sv| sv.namespace.clone())
        .or_else(|| repo.spec.server.as_ref().map(|s| s.namespace.clone()));

    if let Some(ns) = ns {
        io::delete_server_objects(
            &ctx.client,
            &ns,
            &server_object_name(name),
            Some(&generated_secret_name(name)),
        )
        .await?;
        io::delete_secret_if_present(&ctx.client, &ns, &mirrored_creds_secret_name(name)).await?;
    }

    io::remove_finalizer(api, repo, SERVER_CLEANUP_FINALIZER).await?;
    Ok(Action::await_change())
}

/// Drive the mover-Job bootstrap for a `ClusterRepository` whose backend the
/// controller cannot reach in-process — object stores AND volume-backed (PVC / inline
/// NFS) filesystem repos (the Job mounts the repo volume). Mirrors the namespaced
/// [`crate::repository::reconcile`] path; the Job also returns the snapshot listing
/// (`scanCatalog: true`) so [`finalize_cluster_bootstrap`] can run the shared,
/// identity-aware catalog scan (`crate::catalog`) — the cross-namespace/foreign
/// placement itself is decided there
/// ([`crate::catalog::decide_cluster_placement`]), not in this function.
async fn bootstrap_cluster_via_mover(
    ctx: &Context,
    repo: &ClusterRepository,
    name: &str,
    api: &Api<ClusterRepository>,
    backend: &Backend,
) -> Result<Action> {
    // The Job + its result ConfigMap live in the credentials Secret's namespace: the
    // reference's explicit namespace, else the operator's own (`cluster_secret_namespace`).
    let creds = io::repo_credentials(&repo.spec.encryption);
    let job_ns = cluster_secret_namespace(
        creds.namespace.as_deref(),
        ctx.operator_namespace.as_deref(),
    )?;
    let job_name = format!("{name}-bootstrap");
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), &job_ns);

    // Honor a `Snapshot`'s reverify nudge: force a re-probe (ORs into the
    // recycle/create gates below), stamping the token at create time as the loop guard.
    let reverify_token = repo
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(crate::consts::REVERIFY_REQUESTED_ANNOTATION))
        .map(String::as_str);
    let reverify = catalog::reverify_due(
        reverify_token,
        repo.status
            .as_ref()
            .and_then(|s| s.last_reverify_at.as_deref()),
        repo.status.as_ref().and_then(|s| s.phase) == Some(RepositoryPhase::Ready),
    );

    // Backend health probe (`spec.health.probe`, opt-in) — mirrors the namespaced
    // `Repository` path: a once-`Ready` ClusterRepository (pinned `uniqueId`) re-runs
    // its bootstrap as a pure connect probe (Part A forces `auto_create=false`); the
    // phase STAYS `Ready` and the outcome surfaces as the `BackendReachable` condition.
    let bootstrapped_before = repo
        .status
        .as_ref()
        .and_then(|s| s.unique_id.as_deref())
        .is_some();
    let probe_enabled =
        kopiur_api::repository::RepositoryHealthProbeSpec::enabled(repo.spec.health.as_ref());
    let already_ready = repo.status.as_ref().and_then(|s| s.phase) == Some(RepositoryPhase::Ready);
    let probe_attempt_at = repo
        .status
        .as_ref()
        .and_then(|s| s.health.as_ref())
        .and_then(|h| h.probe_attempt_at.clone());
    let probe = health::probe_action(
        already_ready,
        bootstrapped_before,
        probe_enabled,
        repo.status
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.last_probe_at.as_deref()),
        probe_attempt_at.as_deref(),
        kopiur_api::repository::RepositoryHealthProbeSpec::effective_interval(
            repo.spec.health.as_ref(),
        ),
        health::probe_attempt_timeout(health::bootstrap_deadline_seconds(
            repo.spec
                .bootstrap
                .as_ref()
                .and_then(|b| b.failure_policy.as_ref())
                .and_then(|fp| fp.active_deadline_seconds),
        )),
        chrono::Utc::now(),
    );
    let spec_changed =
        repo.metadata.generation != repo.status.as_ref().and_then(|s| s.observed_generation);
    // This finished Job IS the probe's own result (we launched it and stamped
    // `probeAttemptAt`) — interpret it gently and consume it exactly once. Keying on
    // `awaits_result()` rather than `probe_enabled` alone stops a Job left by a STRICT
    // launch from being re-read as a probe, which fabricated `lastHealthyAt` and could
    // route a real bootstrap failure into the alert-only `finalize_cluster_probe_failure`
    // (hard-coded `phase: Ready`), resurrecting a terminally-`Failed` repository.
    // `!reverify`: a reverify nudge keeps its strict semantics (flip to `Failed` on a
    // connect failure) and is never downgraded to an alert-only probe.
    let probe_run = probe.awaits_result() && !spec_changed && !reverify;
    // The LAUNCH-side twin of `probe_run`: keep the phase `Ready` while the probe
    // connects, and stamp `probeAttemptAt` so its result is recognised when it lands.
    let probe_style_launch =
        probe_enabled && bootstrapped_before && already_ready && !reverify && !spec_changed;

    if let Some(job) = job_api.get_opt(&job_name).await? {
        return match crate::snapshot::job_terminal_state(&job) {
            // Still running. A catalog-refresh re-run of an already-Ready repo
            // keeps its phase — flapping Ready→Initializing every refresh would
            // be pure status churn.
            None => {
                if !already_ready {
                    io::patch_status(
                        api,
                        name,
                        serde_json::json!({ "phase": "Initializing", "backend": backend.kind_str() }),
                    )
                    .await?;
                }
                Ok(Action::requeue(Duration::from_secs(15)))
            }
            // Finished — unless the result is stale (catalog refresh due, the spec
            // changed, or a health probe is due since it was taken), in which case
            // recycle the Job so the next reconcile re-runs the bootstrap for a FRESH
            // connect. `probe.launches()` is essential — without it the first probe
            // would re-read the lingering first-bootstrap result and report healthy
            // without ever re-connecting. It is deliberately `launches()`, NOT "a probe
            // is enabled": once the probe's own Job is launched, `probe_action` returns
            // `AwaitingResult` and this arm stands down so the result can be finalized.
            // Recycling then too is the #273 livelock.
            Some(success) => {
                let interval =
                    CatalogBounds::effective_refresh_interval(repo.spec.catalog.as_ref());
                if reverify
                    || probe.launches()
                    || catalog::bootstrap_recycle_due(
                        already_ready,
                        repo.metadata.generation,
                        repo.status.as_ref().and_then(|s| s.observed_generation),
                        cluster_last_refresh_at(repo),
                        interval,
                        CatalogBounds::periodic_refresh_enabled(repo.spec.catalog.as_ref()),
                        cluster_scan_requested_token(repo),
                        cluster_scan_requested_honored(repo),
                        cluster_scan_requested_attempt_at(repo),
                        kopiur_api::consts::DEFAULT_CATALOG_REFRESH_INTERVAL,
                        chrono::Utc::now(),
                    )
                {
                    tracing::debug!(repo = %name, "recycling finished bootstrap Job for a catalog refresh");
                    io::delete_mover_run(&ctx.client, &job_ns, &job_name).await?;
                    return Ok(Action::requeue(Duration::from_secs(5)));
                }
                finalize_cluster_bootstrap(
                    ctx, repo, name, &job_ns, &job_name, api, backend, success, probe_run,
                )
                .await
            }
        };
    }

    // No Job: it either never ran, or the kube TTL controller reaped the finished
    // one (`ttlSecondsAfterFinished`). An already-Ready repo only re-creates it
    // when a re-run is actually warranted (`bootstrap_create_due`: catalog refresh
    // due, or spec changed) — re-creating unconditionally would pin the refresh
    // cadence to the Job TTL instead of `catalog.refreshInterval`.
    if !(reverify
        || probe.launches()
        || catalog::bootstrap_create_due(
            repo.status.as_ref().and_then(|s| s.phase) == Some(RepositoryPhase::Ready),
            repo.metadata.generation,
            repo.status.as_ref().and_then(|s| s.observed_generation),
            cluster_last_refresh_at(repo),
            CatalogBounds::effective_refresh_interval(repo.spec.catalog.as_ref()),
            CatalogBounds::periodic_refresh_enabled(repo.spec.catalog.as_ref()),
            cluster_scan_requested_token(repo),
            cluster_scan_requested_honored(repo),
            cluster_scan_requested_attempt_at(repo),
            kopiur_api::consts::DEFAULT_CATALOG_REFRESH_INTERVAL,
            chrono::Utc::now(),
        ))
    {
        // This is the STEADY STATE of a Ready object-store repo (`bootstrap_create_due` is
        // true for any non-Ready phase, so nothing that has yet to bootstrap gets here).
        // Re-evaluate maintenance coverage before parking: it is the only pass a repo whose
        // bootstrap Job has been TTL-reaped ever makes, and #231 is exactly what happens
        // when the condition is never re-evaluated — a `MaintenanceDisabled` written during
        // a bootstrap race froze for days while the managed `Maintenance` ran happily. Cheap
        // and idempotent: a synchronous informer read, a deterministic server-side apply,
        // and a status patch that is skipped when nothing changed. It also gives the
        // Maintenance watch (`controllers.rs`) a reconcile that actually re-classifies, so
        // deleting the managed CR now re-applies it instead of being silently ignored.
        //
        // Build on a FRESH read of the conditions, not the watch-cached object: a Maintenance
        // event can requeue us moments after the bootstrap finalize patched `Bootstrapped` /
        // `BackendReachable`, while the informer store still holds the pre-patch copy — and
        // the condition write below replaces the whole array, so a stale copy would silently
        // resurrect it and drop those conditions.
        let fresh = api.get_opt(name).await?;
        let conditions = fresh.as_ref().map(cluster_conditions).unwrap_or_default();
        // Non-blocking MassDeletionHeld condition on the steady-state cadence (the
        // ack-drain watch also lands here on an annotation edit). Own guarded
        // write from the FRESH conditions (no clobber, skipped when unchanged);
        // ensure_maintenance then builds on the folded array.
        let conditions = fold_mass_deletion(ctx, repo, &conditions).await;
        let current = fresh
            .as_ref()
            .and_then(|f| serde_json::to_value(&f.status).ok());
        io::patch_status_if_changed(
            api,
            name,
            current.as_ref(),
            serde_json::json!({ "conditions": conditions }),
        )
        .await?;
        ensure_cluster_maintenance(ctx, repo, name, api, &conditions).await;
        return Ok(Action::requeue(cluster_probe_aware_reconcile_interval(
            repo,
        )));
    }

    // Whether we are about to launch a Job BECAUSE OF a pending scan-request
    // token (regardless of whatever else also fired the gate above) — used only
    // to stamp `scanRequestAttemptAt` below, the token arm's own rate limit. It
    // never gates anything else, so recomputing it here (rather than plumbing a
    // bool out of `bootstrap_create_due`) is simplest and stays pure.
    let token_driven_scan_request = catalog::scan_requested_due(
        cluster_scan_requested_token(repo),
        cluster_scan_requested_honored(repo),
        cluster_scan_requested_attempt_at(repo),
        kopiur_api::consts::DEFAULT_CATALOG_REFRESH_INTERVAL,
        chrono::Utc::now(),
    );

    // Part A: only a never-bootstrapped ClusterRepository (no pinned `uniqueId`)
    // may carry `auto_create`; a once-`Ready` one re-runs as a pure connect probe.
    let create_enabled = health::auto_create_allowed(
        kopiur_api::common::create_enabled(repo.spec.create.as_ref()),
        repo.status.as_ref().and_then(|s| s.unique_id.as_deref()),
    );
    let cluster = repo
        .spec
        .identity_defaults
        .as_ref()
        .and_then(|d| d.cluster.as_deref());
    let foreign = CatalogBounds::effective_foreign_snapshots(repo.spec.catalog.as_ref());
    // M6: bootstrap must not stamp/restamp a maintenance owner at all for a
    // ReadOnly repo (the clobber-the-primary's-owner bug this fixes), a
    // deliberate `spec.maintenance.enabled: false` opt-out, or a repository an
    // externally-authored Maintenance already covers (its own ownership.owner
    // has no relation to ours) — see `io::bootstrap_maintenance_owner_plan`.
    let read_only = !repo.spec.mode.allows_writes();
    let maintenance_enabled = repo.spec.maintenance.as_ref().is_none_or(|m| m.enabled);
    let foreign_maintenance = io::maintenance_covered_by_foreign(
        ctx,
        RepositoryKind::ClusterRepository,
        "ClusterRepository",
        name,
        None,
    );
    let work_spec = cluster_bootstrap_work_spec(
        backend,
        name,
        &job_ns,
        create_enabled,
        repo.spec.create.as_ref(),
        repo.spec.mover_defaults.as_ref(),
        cluster,
        foreign,
        read_only,
        maintenance_enabled,
        foreign_maintenance,
        repo.spec.parameters.as_ref(),
    );
    // Preflight the credential Secret(s) the bootstrap mover loads via `envFrom`, in the
    // namespace it will actually run in. Without this the Job launches against a Secret
    // that isn't there, its pod wedges in `CreateContainerConfigError` until the bootstrap
    // deadline, and the failure surfaces as a generic "mover pod crashed or never
    // scheduled" — naming neither the Secret nor the namespace. That matters more now that
    // `job_ns` can be DEFAULTED (#232): if the Secret lives somewhere else, say so.
    let creds_names = io::mover_creds_secrets(backend, &repo.spec.encryption);
    io::ensure_creds_present(
        &ctx.client,
        &job_ns,
        &io::CredsContext {
            secret_names: &creds_names,
            repo_kind: "ClusterRepository",
            repo_name: name,
            // The namespace the REFERENCE names, not the Job's. Passing the Job's namespace
            // would make it tautologically equal and silence the cross-namespace branch of
            // `missing_creds_message` — the branch that exists precisely to say "your
            // ClusterRepository keeps that Secret in X, but the Job runs in Y".
            repo_secret_namespace: repo
                .spec
                .encryption
                .password_secret_ref
                .namespace
                .as_deref(),
        },
    )
    .await?;
    let creds_secrets = io::plain_creds(creds_names);
    let owner = io::owner_ref_for(repo, "ClusterRepository")?;
    // The bootstrap Job runs in the credentials Secret's namespace (`job_ns`).
    // Resolve its run identity there: the user's workload-identity SA
    // (preflighted + bound to the mover role), or the minted mover SA +
    // RoleBinding — it is NOT the operator SA either way (ADR §4.12).
    let mover_identity = io::ensure_mover_identity(
        &ctx.client,
        &job_ns,
        &[backend],
        ctx.mover_service_account.as_deref(),
        ctx.mover_role_kind.as_str(),
        &ctx.mover_clusterrole,
    )
    .await?;
    let mut labels = BTreeMap::new();
    labels.insert(
        "kopiur.home-operations.com/cluster-repository".to_string(),
        name.to_string(),
    );
    mover_identity.decorate_labels(&mut labels);
    // The cluster-repo bootstrap Job inherits the repository's `moverDefaults`
    // (ADR-0004 §1) — same bootstrap-gap fix as the namespaced Repository. The Job
    // lands in `job_ns` (spec.maintenance.namespace or KOPIUR_NAMESPACE); not gated
    // (repo-owner-authored, operator namespace).
    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.spec.mover_defaults.as_ref(),
        None,
        None,
        None,
        None,
        None,
    );
    // A volume-backed filesystem repo mounts its PVC / inline-NFS export read-write at
    // the backend path so the mover can connect/create the kopia repo there. Object
    // stores reach the backend over the network, so they mount nothing.
    let repo_volume =
        io::filesystem_repo_mount_source(backend).map(|source| jobs::VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(backend).unwrap_or_default(),
            read_only: false,
        });
    // spec.bootstrap.failurePolicy lets callers raise the deadline for a slow
    // backend or tune the retry budget; unset keeps the built-in defaults.
    let bootstrap_fp = repo
        .spec
        .bootstrap
        .as_ref()
        .and_then(|b| b.failure_policy.as_ref());
    let inputs = MoverJobInputs {
        name: &job_name,
        namespace: &job_ns,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        // Bound the bootstrap Job so a pod that never schedules (missing mover SA,
        // image-pull failure) becomes terminal-`Failed` instead of hanging — then
        // `finalize_cluster_bootstrap` runs and surfaces a Warning Event. Shared with
        // `health::probe_attempt_timeout`, so the bound on a probe Job's life and the
        // gate that waits for its result can never disagree.
        limits: JobLimits {
            active_deadline_seconds: Some(health::bootstrap_deadline_seconds(
                bootstrap_fp.and_then(|fp| fp.active_deadline_seconds),
            )),
            backoff_limit: bootstrap_fp
                .and_then(|fp| fp.backoff_limit)
                .unwrap_or(JobLimits::default().backoff_limit),
            ttl_seconds_after_finished: resolved_mover.ttl_seconds_after_finished,
        },
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
        result_configmap: Some(&job_name),
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
        annotations: Default::default(),
        // Bootstrap is a short connect/create probe: an emptyDir cache suffices.
        cache_volume: Default::default(),
        scratch_volume: None,
        readiness_exec: None,
    };
    let cm = jobs::build_result_config_map(&inputs);
    let job = jobs::build_job(&inputs)?;
    io::apply_mover_objects(&ctx.client, &job_ns, &job_name, Some(&cm), &job).await?;
    // Stamp the reverify token (loop guard): this request is now honored. A
    // health-probe re-run keeps the phase `Ready` so backups/replication aren't paused.
    let mut create_status = if probe_style_launch {
        serde_json::json!({ "backend": backend.kind_str() })
    } else {
        serde_json::json!({ "phase": "Initializing", "backend": backend.kind_str() })
    };
    if let Some(token) = reverify_token {
        create_status["lastReverifyAt"] = serde_json::Value::String(token.to_string());
    }
    // Launch-side probe stamp (#273) — the loop guard that lets the probe terminate.
    // `lastProbeAt` is only written at FINALIZE, so gating the recycle on it alone
    // deletes every Job whose completion would have cleared it. Stamping here means the
    // next pass reads `AwaitingResult` and lets the finished Job be finalized.
    //
    // `now` for a probe-style launch, an explicit `null` otherwise, so a STRICT relaunch
    // actively clears a stale stamp instead of inheriting it.
    if probe_enabled || probe_attempt_at.is_some() {
        create_status["health"] = health::probe_attempt_health_patch(
            probe_style_launch
                .then(|| chrono::Utc::now().to_rfc3339())
                .as_deref(),
        );
    }
    // Rate-limit backoff for the scan-request token: stamped BEFORE the outcome is
    // known (a probe-style failure never lands in `run_cluster_catalog_scan`, so
    // the honored-write there is not a reliable place to bound retries) — see
    // `catalog::scan_requested_due`. Merge-patched under `catalog` so the other
    // `status.catalog` fields (lastRefreshAt, scanRequestHonored, ...) are untouched.
    if token_driven_scan_request {
        create_status["catalog"] = serde_json::json!({
            "scanRequestAttemptAt": chrono::Utc::now().to_rfc3339(),
        });
    }
    io::patch_status(api, name, create_status).await?;
    tracing::info!(repo = %name, backend = backend.kind_str(), namespace = %job_ns, "launched ClusterRepository bootstrap Job");
    Ok(Action::requeue(Duration::from_secs(15)))
}

/// Build the bootstrap work spec for a `ClusterRepository` object store.
///
/// `cluster`/`foreign` (`identityDefaults.cluster` / the effective
/// `catalog.foreignSnapshots`) drive `catalog_foreign_prefilter_cluster`: the mover
/// pre-filters `ForeignCluster`-classified entries BEFORE its own materialization
/// cap only when cluster identity is actually on AND the effective policy is
/// `Ignore` — under `Fallback` those entries must still come back so the controller
/// can materialize them into `catalog.fallbackNamespace`.
///
/// `read_only`/`maintenance_enabled`/`foreign_maintenance` (M6) drive the
/// maintenance-owner plan via [`io::bootstrap_maintenance_owner_plan`]: the
/// mover restamps a stale owner on EVERY connect-to-existing, not just create
/// (see `maintenance_restamp_target`), so `maintenance_owner: None` is the only
/// way to mean "never stamp/restamp at all" — used for a ReadOnly repo (fixes
/// the consumer-clobbers-the-primary's-owner bug), a deliberate
/// `spec.maintenance.enabled: false`, or a repository an externally-authored
/// Maintenance already covers.
#[allow(clippy::too_many_arguments)]
fn cluster_bootstrap_work_spec(
    backend: &Backend,
    name: &str,
    job_ns: &str,
    auto_create: bool,
    create: Option<&kopiur_api::common::CreateBehavior>,
    mover_defaults: Option<&kopiur_api::common::MoverDefaults>,
    cluster: Option<&str>,
    foreign: ForeignSnapshots,
    read_only: bool,
    maintenance_enabled: bool,
    foreign_maintenance: bool,
    parameters: Option<&kopiur_api::repository::RepositoryParameters>,
) -> MoverWorkSpec {
    let cluster_mode = cluster.is_some_and(|c| !c.is_empty());
    let prefilter_cluster = (cluster_mode && matches!(foreign, ForeignSnapshots::Ignore))
        .then(|| cluster.unwrap_or_default().to_string());
    let cluster_ref = cluster.filter(|c| !c.is_empty());
    let (maintenance_owner, restamp_policy, maintenance_owner_aliases) =
        io::bootstrap_maintenance_owner_plan(
            RepositoryKind::ClusterRepository,
            job_ns,
            name,
            cluster_ref,
            read_only || !maintenance_enabled || foreign_maintenance,
        );
    MoverWorkSpec {
        version: 1,
        operation: Operation::BootstrapRepository(BootstrapRepositoryOp {
            auto_create,
            // The Job returns the snapshot listing so `finalize_cluster_bootstrap`
            // can materialize discovered Snapshots (placed per identity hostname —
            // see `crate::catalog`).
            scan_catalog: true,
            create_options: kopiur_mover::workspec::CreateOptionsSpec::from_create(create),
            // MUTABLE parameters (#258), re-applied on drift on every bootstrap — the
            // opposite of create_options. Empty for a ReadOnly repository: `set-parameters`
            // rewrites the format blob and kopia hard-errors on a read-only connection.
            // Shares `epoch_parameters_for` with the Repository twin so the gate has one
            // definition rather than two that can drift apart.
            epoch_parameters: crate::repository::epoch_parameters_for(read_only, parameters),
            // Stamped on CREATE unconditionally (elsewhere) AND re-stamped on
            // every connect-to-existing when stale (`maintenance_restamp_target`);
            // `None` means neither ever happens — see the doc above.
            maintenance_owner,
            catalog_foreign_prefilter_cluster: prefilter_cluster,
            restamp_policy,
            maintenance_owner_aliases,
            read_only,
        }),
        identity: ResolvedIdentity {
            username: "kopiur-bootstrap".to_string(),
            hostname: name.to_string(),
            source_path: String::new(),
        },
        repository: backend_to_repository_connect(backend),
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "ClusterRepository".to_string(),
            name: name.to_string(),
            namespace: job_ns.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        // Bootstrap is a connect/create probe, not a data run: kopia defaults.
        cache: Default::default(),
        throttle: io::throttle_spec(mover_defaults),
    }
}

/// Reflect a finished `ClusterRepository` bootstrap Job into its status.
#[allow(clippy::too_many_arguments)]
async fn finalize_cluster_bootstrap(
    ctx: &Context,
    repo: &ClusterRepository,
    name: &str,
    job_ns: &str,
    job_name: &str,
    api: &Api<ClusterRepository>,
    backend: &Backend,
    job_succeeded: bool,
    // True when this is a health-probe re-run of an already-`Ready` ClusterRepository
    // (same spec): interpret the result as a probe (phase stays `Ready`) and consume
    // the Job exactly once (deleted after processing).
    probe_run: bool,
) -> Result<Action> {
    let result = read_cluster_bootstrap_result(ctx, job_ns, job_name).await?;

    // Classify the (result, job state) pair as a typed outcome (ADR §5.5): a
    // result-less failed Job and a kopia-rejected connect are distinct,
    // exhaustively-handled modes — never a silent `Failed/Unknown` with no
    // Event — and the success arm *owns* the result, so there is no
    // `.expect()` invariant to get wrong.
    let result = match io::bootstrap_outcome(result, job_succeeded, job_name) {
        io::BootstrapOutcome::ResultPending => {
            tracing::warn!(repo = %name, "bootstrap Job complete but result not readable yet; requeueing");
            return Ok(Action::requeue(Duration::from_secs(5)));
        }
        io::BootstrapOutcome::Failed(failure) => {
            // Health-probe failure on an already-`Ready` ClusterRepository: alert-only.
            // Keep phase `Ready`, surface the debounced `BackendReachable` condition +
            // Warning event, never auto-recreate.
            if probe_run {
                return finalize_cluster_probe_failure(
                    ctx, repo, api, name, job_ns, job_name, &failure,
                )
                .await;
            }
            let reason = failure.reason();
            // A result-less Job failure carries no backend verdict (mover
            // crashed / evicted / deadline / result write hit a down apiserver
            // — the outage incident): recycle the dead Job and retry as
            // `Degraded` instead of parking `Failed` until the Job's TTL
            // finally reaps it. Mirrors `repository.rs::finalize_bootstrap`.
            if failure.recycles_for_retry() {
                io::delete_mover_run(&ctx.client, job_ns, job_name).await?;
                let conditions =
                    cluster_bootstrap_condition(repo, false, reason, &failure.condition_message());
                let conditions = io::set_ready(
                    &conditions,
                    repo.metadata.generation,
                    io::ready_outcome_for_phase(RepositoryPhase::Degraded),
                    reason,
                    &failure.condition_message(),
                );
                let current = serde_json::to_value(&repo.status).ok();
                let wrote = io::patch_status_if_changed(
                    api,
                    name,
                    current.as_ref(),
                    serde_json::json!({
                        "phase": "Degraded",
                        "backend": backend.kind_str(),
                        "observedGeneration": repo.metadata.generation,
                        "conditions": conditions,
                    }),
                )
                .await?;
                if wrote {
                    failure.publish(ctx, &io::event_ref(repo), name).await;
                    tracing::warn!(
                        repo = %name,
                        reason,
                        "ClusterRepository bootstrap Job failed without a result; recycled it \
                         for retry"
                    );
                }
                return Ok(Action::requeue(Duration::from_secs(120)));
            }
            let conditions =
                cluster_bootstrap_condition(repo, false, reason, &failure.condition_message());
            // A terminal bootstrap failure is kstatus-Stalled (issue #245): Flux
            // should fail its health check fast, not hang until timeout.
            let conditions = io::set_ready(
                &conditions,
                repo.metadata.generation,
                io::ready_outcome_for_phase(RepositoryPhase::Failed),
                reason,
                &failure.condition_message(),
            );
            // Guard the write so the Event + warn log fire only on the real transition,
            // not on every 120 s re-read (the message is stable → a true no-op once
            // written, so no reconcile hot-loop and no Event spam).
            let current = serde_json::to_value(&repo.status).ok();
            let wrote = io::patch_status_if_changed(
                api,
                name,
                current.as_ref(),
                serde_json::json!({
                    "phase": "Failed",
                    "backend": backend.kind_str(),
                    "observedGeneration": repo.metadata.generation,
                    "conditions": conditions,
                }),
            )
            .await?;
            if wrote {
                failure.publish(ctx, &io::event_ref(repo), name).await;
                tracing::warn!(repo = %name, reason, "ClusterRepository bootstrap failed");
            }
            return Ok(Action::requeue(Duration::from_secs(120)));
        }
        io::BootstrapOutcome::Succeeded(result) => result,
    };

    let allowed_count = allowed_namespace_count(&repo.spec.allowed_namespaces);
    let mut conditions = cluster_bootstrap_condition(
        repo,
        true,
        "Bootstrapped",
        if result.created {
            "created a new repository"
        } else {
            "connected to the existing repository"
        },
    );
    // Index-blob health (ADR-0005 §13): fold IndexBlobHealth into the same
    // conditions array and stamp the count onto storageStats (merge-patch
    // preserves snapshotCount below). Best-effort in the mover — `None` leaves the
    // prior condition/count. The Warning event fires only on the transition.
    let threshold = resolve_index_blob_warn_threshold(repo.spec.health.as_ref());
    let mut index_blob_event = None;
    let mut storage_stats = serde_json::json!({ "snapshotCount": result.snapshot_count });
    if let Some(count) = result.index_blob_count {
        let upd = health::reconcile_index_blob_health(
            &conditions,
            count,
            threshold,
            repo.metadata.generation,
        );
        conditions = upd.conditions;
        index_blob_event = upd.event;
        storage_stats["indexBlobCount"] = serde_json::json!(count);
    }
    // A successful health probe: clear any prior failure debounce and stamp
    // `lastProbeAt`/`lastHealthyAt`. Safe to stamp unconditionally — a probe run
    // consumes its Job exactly once (deleted below), so this never re-runs.
    let mut health_status = serde_json::Value::Null;
    // Part A invariant: a probe NEVER rebinds identity. Keep the pinned `uniqueId` so
    // a backend re-initialized at the old location by another party (same password)
    // can't silently overwrite it on a successful connect.
    let mut pinned_unique_id: Option<String> = None;
    if probe_run {
        let now = chrono::Utc::now().to_rfc3339();
        let upd = health::reconcile_probe_success(&conditions, &now, repo.metadata.generation);
        conditions = upd.conditions;
        // Explicit-null patch so the merge clears the failure counters (a `None`
        // would be elided and leave the prior streak, re-firing on the next failure).
        health_status = health::probe_success_health_patch(&now);
        if let Some(pinned) = repo.status.as_ref().and_then(|s| s.unique_id.as_deref()) {
            if result.unique_id.as_deref() != Some(pinned) {
                tracing::warn!(
                    repo = %name, pinned, observed = ?result.unique_id,
                    "health probe connected to a DIFFERENT repository at the backend; keeping the pinned uniqueId"
                );
            }
            pinned_unique_id = Some(pinned.to_string());
        }
    }
    // Non-blocking MassDeletionHeld condition from the live Snapshot store,
    // folded into the conditions array before the kstatus set_ready.
    let conditions = fold_mass_deletion(ctx, repo, &conditions).await;
    // Publish the standard kstatus Ready/Reconciling/Stalled conditions for the
    // healthy repository (issue #245), layered onto the conditions built above so
    // the merge-patch replace of `conditions` still carries Bootstrapped and
    // IndexBlobHealth. A resume reaches this path and MUST overwrite the stale
    // `Suspended`-reasoned Ready/Reconciling entries left by the suspend branch —
    // otherwise Flux `wait: true` hangs on a repository that is actually Ready.
    let conditions = io::set_ready(
        &conditions,
        repo.metadata.generation,
        io::ready_outcome_for_phase(RepositoryPhase::Ready),
        "Bootstrapped",
        if result.created {
            "created a new repository"
        } else {
            "connected to the existing repository"
        },
    );
    // Guarded write: this path re-runs on EVERY reconcile while the finished
    // bootstrap Job exists, so the steady-state pass must be a true no-op — a
    // re-write of identical status would bump `resourceVersion` and re-trigger
    // this reconciler through its own primary watch, in a tight loop.
    let mut status_patch = serde_json::json!({
        "phase": "Ready",
        // Report the generation we just bootstrapped, so `observedGeneration` tracks
        // `metadata.generation` after a successful first bootstrap (matching the
        // already-bootstrapped path and the namespaced Repository). Without it a
        // freshly-bootstrapped ClusterRepository shows no observedGeneration until the
        // next spec change. (Fix originally from community PR #82, ChosenQuill.)
        "observedGeneration": repo.metadata.generation,
        "backend": backend.kind_str(),
        "uniqueId": result.unique_id,
        "allowedNamespaceCount": allowed_count,
        "storageStats": storage_stats,
        "conditions": conditions,
    });
    // Mirror the epoch parameters the repository actually reports (#258). The apply is
    // best-effort in the mover, so this is what keeps it honest: a `spec.parameters.epoch`
    // that failed to land stays visible here as drift from spec, rather than as silence.
    if let Some(epoch) = &result.epoch {
        status_patch["parameters"] = serde_json::json!({ "epoch": epoch });
    }
    // The apply is best-effort (see the mover), so a failure leaves the repository Ready
    // with `status.parameters.epoch` silently disagreeing with `spec`. Say so out loud —
    // the whole point of #258 is that a user set a value expecting it to take effect.
    if let Some(err) = &result.epoch_error {
        io::publish_warning_event(
            ctx,
            repo,
            health::EPOCH_PARAMETERS_NOT_APPLIED_REASON,
            health::FIX_EPOCH_PARAMETERS_ACTION,
            err,
        )
        .await;
    }
    if !health_status.is_null() {
        status_patch["health"] = health_status;
    }
    if let Some(pinned) = pinned_unique_id {
        status_patch["uniqueId"] = serde_json::Value::String(pinned);
    }
    let current = serde_json::to_value(&repo.status).ok();
    io::patch_status_if_changed(api, name, current.as_ref(), status_patch).await?;
    // Fire the Warning Event only on the transition into unhealthy. Non-blocking:
    // the cluster repository stays Ready.
    if let Some(w) = index_blob_event {
        io::publish_warning_event(ctx, repo, w.reason, w.action, &w.message).await;
    }
    if result.snapshots_truncated {
        tracing::warn!(
            repo = %name,
            snapshot_count = result.snapshot_count,
            "catalog larger than the materialization cap; not all snapshots were materialized"
        );
    }

    // Materialize/expire discovered Snapshots from the snapshots the Job
    // returned — once per result: after the first scan stamps `lastRefreshAt`,
    // re-reads of the same finished Job skip the (stale) listing, and the next
    // due refresh recycles the Job for a fresh one. `scan_due`'s generation arm
    // covers the spec-change recycle (see the Repository twin): the fresh
    // result must be scanned NOW, not at the next timed refresh.
    let interval = CatalogBounds::effective_refresh_interval(repo.spec.catalog.as_ref());
    if catalog::scan_due(
        repo.metadata.generation,
        repo.status.as_ref().and_then(|s| s.observed_generation),
        cluster_last_refresh_at(repo),
        interval,
        CatalogBounds::periodic_refresh_enabled(repo.spec.catalog.as_ref()),
        cluster_scan_requested_token(repo),
        cluster_scan_requested_honored(repo),
        chrono::Utc::now(),
    ) {
        run_cluster_catalog_scan(
            ctx,
            repo,
            name,
            &result.snapshots,
            result.snapshot_count,
            result.snapshots_truncated,
            result.foreign_suffix_dropped,
        )
        .await?;
    }

    // Ensure the managed Maintenance for this ClusterRepository (§3.7). Build on
    // the conditions we just patched (including `Bootstrapped`), not the stale
    // cached object, so this patch doesn't drop the `Bootstrapped` set above.
    ensure_cluster_maintenance(ctx, repo, name, api, &conditions).await;

    // A probe consumes its Job exactly once (no lingering finished Job → no churn;
    // the next probe is a fresh connect). Requeue on the probe cadence.
    if probe_run {
        delete_cluster_bootstrap_job(ctx, job_ns, job_name).await?;
    }
    Ok(Action::requeue(cluster_probe_aware_reconcile_interval(
        repo,
    )))
}

/// Health-probe failure on an already-`Ready` `ClusterRepository`: alert-only.
/// Keeps the phase `Ready`, folds the debounced `BackendReachable` condition + (on
/// a transition) a Warning event + metric, deletes the consumed Job, and requeues
/// on the probe cadence. NEVER changes the phase and NEVER auto-recreates.
#[allow(clippy::too_many_arguments)]
async fn finalize_cluster_probe_failure(
    ctx: &Context,
    repo: &ClusterRepository,
    api: &Api<ClusterRepository>,
    name: &str,
    job_ns: &str,
    job_name: &str,
    failure: &io::BootstrapFailure,
) -> Result<Action> {
    let kind = if failure.is_repository_absent() {
        health::ProbeFailureKind::Vanished
    } else {
        health::ProbeFailureKind::Unreachable
    };
    let existing = repo
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let now = chrono::Utc::now().to_rfc3339();
    let upd = health::reconcile_probe_failure(
        &existing,
        repo.status.as_ref().and_then(|s| s.health.as_ref()),
        kind,
        kopiur_api::repository::RepositoryHealthProbeSpec::effective_failure_threshold(
            repo.spec.health.as_ref(),
        ),
        &now,
        repo.metadata.generation,
    );
    // The repository stays kstatus-Ready during a probe failure (backups continue
    // by design); the probe detail rides the `BackendReachable` condition folded
    // into `upd.conditions`. Layer Ready on top so its observedGeneration stays
    // current and a resumed-then-probe-flapping repo never shows a stale
    // `Suspended` Ready reason (issue #245).
    let conditions = io::set_ready(
        &upd.conditions,
        repo.metadata.generation,
        io::ready_outcome_for_phase(RepositoryPhase::Ready),
        "Bootstrapped",
        "repository connected; a health probe is failing (see BackendReachable), backups continue",
    );
    let current = serde_json::to_value(&repo.status).ok();
    let wrote = io::patch_status_if_changed(
        api,
        name,
        current.as_ref(),
        serde_json::json!({
            "phase": "Ready",
            // NOT `upd.health` directly: `skip_serializing_if = "Option::is_none"` would
            // ELIDE the `None` for `probeAttemptAt`, leaving the launch stamp in the
            // stored object — so the repo would stay `AwaitingResult` and the next
            // finished Job would read as a probe. This emits the explicit RFC 7386 null.
            "health": health::probe_failure_health_patch(&upd.health),
            "conditions": conditions,
        }),
    )
    .await?;
    if let Some(w) = upd.event {
        let kind_label = match kind {
            health::ProbeFailureKind::Vanished => "vanished",
            health::ProbeFailureKind::Unreachable => "unreachable",
        };
        ctx.metrics
            .inc_health_probe_failure("", name, "ClusterRepository", kind_label);
        if wrote {
            io::publish_warning_event(ctx, repo, w.reason, w.action, &w.message).await;
            tracing::warn!(repo = %name, reason = w.reason, "ClusterRepository health probe raised an alert");
        }
    }
    delete_cluster_bootstrap_job(ctx, job_ns, job_name).await?;
    Ok(Action::requeue(cluster_probe_aware_reconcile_interval(
        repo,
    )))
}

/// Delete a finished cluster bootstrap Job + its result ConfigMap (in the creds
/// Secret's namespace), tolerating a 404. Consumes a probe Job exactly once.
async fn delete_cluster_bootstrap_job(ctx: &Context, job_ns: &str, job_name: &str) -> Result<()> {
    io::delete_mover_run(&ctx.client, job_ns, job_name).await
}

/// The steady-state requeue for a `ClusterRepository`, shortened to the
/// health-probe cadence when the probe is enabled.
fn cluster_probe_aware_reconcile_interval(repo: &ClusterRepository) -> Duration {
    let base = catalog::reconcile_interval(repo.spec.catalog.as_ref());
    if kopiur_api::repository::RepositoryHealthProbeSpec::enabled(repo.spec.health.as_ref()) {
        base.min(
            kopiur_api::repository::RepositoryHealthProbeSpec::effective_interval(
                repo.spec.health.as_ref(),
            ),
        )
    } else {
        base
    }
}

/// Read the [`BootstrapResult`] the mover wrote into the work-spec ConfigMap (in
/// the credentials Secret's namespace).
async fn read_cluster_bootstrap_result(
    ctx: &Context,
    namespace: &str,
    cm_name: &str,
) -> Result<Option<BootstrapResult>> {
    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);
    let Some(cm) = cm_api.get_opt(cm_name).await? else {
        return Ok(None);
    };
    let Some(raw) = cm.data.as_ref().and_then(|d| d.get(RESULT_CONFIGMAP_KEY)) else {
        return Ok(None);
    };
    let result: BootstrapResult = serde_json::from_str(raw)
        .map_err(|e| Error::Invariant(format!("parsing bootstrap result for {cm_name}: {e}")))?;
    Ok(Some(result))
}

/// Upsert the `Bootstrapped` condition onto the ClusterRepository's conditions.
fn cluster_bootstrap_condition(
    repo: &ClusterRepository,
    status: bool,
    reason: &str,
    message: &str,
) -> Vec<Condition> {
    let existing = repo
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    io::upsert_condition(
        &existing,
        REPOSITORY_BOOTSTRAPPED_CONDITION,
        status,
        reason,
        message,
        repo.metadata.generation,
    )
}

/// Count of namespaces a `List`/`All` gate resolves to (selector requires a live
/// namespace list and is reported as 0 here without that lookup).
fn allowed_namespace_count(allowed: &kopiur_api::AllowedNamespaces) -> i64 {
    use kopiur_api::AllowedNamespaces;
    match allowed {
        AllowedNamespaces::List(ns) => ns.len() as i64,
        AllowedNamespaces::All(true) => -1, // sentinel: all
        AllowedNamespaces::All(false) => 0,
        AllowedNamespaces::Selector(_) => 0,
    }
}

/// `error_policy` for the `ClusterRepository` controller.
pub fn error_policy(obj: Arc<ClusterRepository>, err: &Error, ctx: Arc<Context>) -> Action {
    error_policy_for("ClusterRepository", obj.as_ref(), err, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Placement decision tests (`allowed_namespace_is_used_directly`,
    // `disallowed_namespace_falls_back`, `disallowed_and_no_fallback_yields_none`)
    // migrated to `catalog.rs`'s decision-table tests over
    // `catalog::decide_cluster_placement` (M4: the pre-M4 `placement_namespace` this
    // module defined is now folded into that shared, identity-aware decision fn).

    // #232: a ClusterRepository is cluster-scoped, so "absent = the referrer's namespace"
    // has nothing to resolve to. It used to hard-error `passwordSecretRef.namespace is
    // required` on every reconcile, contradicting the field's own documentation; it now
    // defaults to the operator's namespace, exactly like spec.maintenance.namespace.

    #[test]
    fn explicit_secret_namespace_wins_over_the_operator_namespace() {
        assert_eq!(
            cluster_secret_namespace(Some("vault"), Some("kopiur-system")).unwrap(),
            "vault"
        );
    }

    #[test]
    fn absent_secret_namespace_defaults_to_the_operator_namespace() {
        assert_eq!(
            cluster_secret_namespace(None, Some("kopiur-system")).unwrap(),
            "kopiur-system"
        );
    }

    #[test]
    fn absent_secret_namespace_without_an_operator_namespace_is_an_actionable_error() {
        let err = cluster_secret_namespace(None, None).expect_err("must not guess a namespace");
        let msg = err.to_string();
        // Both fixes, so an off-chart install knows either lever will do.
        assert!(
            msg.contains("encryption.passwordSecretRef.namespace"),
            "{msg}"
        );
        assert!(msg.contains("KOPIUR_NAMESPACE"), "{msg}");
        assert!(matches!(err, Error::Validation(_)), "{err:?}");
    }

    #[test]
    fn unplaced_warning_names_the_shared_repo_fix_alongside_the_original_ones() {
        let msg = unplaced_warning_message(&["ghost", "old-host"]);
        assert!(msg.contains("ghost"), "{msg}");
        assert!(msg.contains("old-host"), "{msg}");
        // Original (pre-M4) fixes are preserved.
        assert!(msg.contains("spec.catalog.fallbackNamespace"), "{msg}");
        // M4: the shared-repository posture is now named too.
        assert!(msg.contains("identityDefaults.cluster"), "{msg}");
        assert!(msg.contains("catalog.foreignSnapshots: Ignore"), "{msg}");
    }

    /// M6: the ClusterRepository bootstrap work spec's maintenance-owner gating,
    /// mirroring the namespaced Repository's matrix (whose test carries the full
    /// combinations) — here the kind-specific lease formats are the point.
    #[test]
    fn cluster_bootstrap_work_spec_maintenance_owner_gating() {
        use kopiur_api::backend::FilesystemBackend;
        use kopiur_mover::workspec::RestampPolicy;
        let backend = Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        });
        let build = |read_only: bool, enabled: bool, foreign_m: bool, cluster: Option<&str>| {
            let spec = cluster_bootstrap_work_spec(
                &backend,
                "shared",
                "kopia-system",
                true,
                None,
                None,
                cluster,
                ForeignSnapshots::Fallback,
                read_only,
                enabled,
                foreign_m,
                None,
            );
            match spec.operation {
                Operation::BootstrapRepository(op) => op,
                other => panic!("expected BootstrapRepository, got {other:?}"),
            }
        };

        // Default: the legacy ClusterRepository lease's owner, AnyStale, no aliases.
        let op = build(false, true, false, None);
        assert_eq!(
            op.maintenance_owner.as_deref(),
            Some("kopiur@kopiur-clusterrepository-shared")
        );
        assert_eq!(op.restamp_policy, RestampPolicy::AnyStale);
        assert!(op.maintenance_owner_aliases.is_empty());
        assert!(!op.read_only);

        // Cluster set: cluster-qualified owner + OwnFormatsOnly + legacy alias.
        let op = build(false, true, false, Some("east"));
        assert_eq!(
            op.maintenance_owner.as_deref(),
            Some("kopiur@kopiur.east.clusterrepository.shared")
        );
        assert_eq!(op.restamp_policy, RestampPolicy::OwnFormatsOnly);
        assert_eq!(
            op.maintenance_owner_aliases,
            vec!["kopiur@kopiur-clusterrepository-shared".to_string()]
        );

        // ReadOnly consumer: no stamp + --readonly connect (the clobber fix).
        let op = build(true, true, false, Some("east"));
        assert_eq!(op.maintenance_owner, None);
        assert!(op.read_only);

        // Disabled / foreign-covered: no stamp, normal connect.
        assert_eq!(build(false, false, false, None).maintenance_owner, None);
        assert_eq!(build(false, true, true, None).maintenance_owner, None);
    }
}
