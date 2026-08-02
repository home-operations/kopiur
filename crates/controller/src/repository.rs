//! The `Repository` reconciler (ADR §3.1, §5.4).
//!
//! Responsibilities:
//! 1. Defensive re-validation (`api::validate`).
//! 2. Ensure the repo exists: connect, and create it if `create.enabled` — via a
//!    short-lived Job (ADR §5.4) so a controller restart never strands a kopia
//!    process. Set `status.phase`/`uniqueID`/`backend`/`storageStats`.
//! 3. Periodic catalog scan (`snapshot list`) materializing/expiring
//!    `origin: discovered` `Snapshot` CRs, bounded by `catalog.retain`, on the
//!    `catalog.refreshInterval` cadence (ADR §2.1; rules + pure planner in
//!    [`crate::catalog`]). A bare-path filesystem repo re-lists in-process; a
//!    mover-bootstrapped repo (object store / volume-backed filesystem) re-lists
//!    by recycling its finished bootstrap Job for a fresh result.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::runtime::controller::Action;
use kube::{Api, Resource, ResourceExt};

use kopiur_api::backend::Backend;
use kopiur_api::common::{CatalogBounds, ForeignSnapshots, RepositoryKind, RepositoryRef};
use kopiur_api::repository::resolve_index_blob_warn_threshold;
use kopiur_api::{Repository, RepositoryPhase, validate};
use kopiur_kopia::{ConnectSpec, SnapshotListEntry};
use kopiur_mover::bootstrap::{BootstrapResult, RESULT_CONFIGMAP_KEY};
use kopiur_mover::workspec::{
    BootstrapRepositoryOp, MoverOptions, MoverWorkSpec, Operation, ResolvedIdentity, TargetRef,
};

use crate::catalog;
use crate::consts::{API_VERSION, REPOSITORY_BOOTSTRAPPED_CONDITION};
use crate::context::Context;
use crate::error::{Error, Result, TERMINAL_HEARTBEAT, error_policy_for};
use crate::health;
use crate::io;
use crate::jobs::{self, JobLimits, MoverJobInputs};
use crate::snapshot::backend_to_repository_connect;

/// Logical bytes under management: the sum, over each distinct snapshot source,
/// of the most-recent snapshot's logical `total_size`. Older snapshots of the
/// same source are not added (they would double-count unchanged data). Pure.
pub fn logical_bytes_under_management(listing: &[SnapshotListEntry]) -> i64 {
    use std::collections::HashMap;
    let mut newest: HashMap<&str, &SnapshotListEntry> = HashMap::new();
    for e in listing {
        let key = e.source.path.as_str();
        match newest.get(key) {
            Some(prev) if prev.end_time >= e.end_time => {}
            _ => {
                newest.insert(key, e);
            }
        }
    }
    newest
        .values()
        .map(|e| i64::try_from(e.stats.total_size).unwrap_or(i64::MAX))
        .sum()
}

/// Reconcile a `Repository`.
#[tracing::instrument(skip(repo, ctx), fields(kind = "Repository", namespace = %repo.namespace().unwrap_or_default(), name = %repo.name_any()))]
pub async fn reconcile(repo: Arc<Repository>, ctx: Arc<Context>) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(&repo, &ctx).await;
    ctx.metrics
        .record_reconcile("Repository", start.elapsed().as_secs_f64());
    record_repository_status_metrics(&repo, &ctx, result.is_ok()).await;
    result
}

/// Mirror a Repository's catalog gauges. The lifecycle *phase* is no longer written
/// here: `kopiur_resource_phase` is a store-backed observable gauge (see
/// [`crate::metrics::Metrics::register_resource_observers`]), so a deleted
/// Repository's series drops on its own. The freshest status is re-read on success
/// (the passed object is the pre-reconcile cache copy).
async fn record_repository_status_metrics(repo: &Repository, ctx: &Context, ok: bool) {
    let (Some(ns), name) = (repo.namespace(), repo.name_any()) else {
        return;
    };
    if repo.metadata.deletion_timestamp.is_some() || !ok {
        return;
    }
    let api: Api<Repository> = Api::namespaced(ctx.client.clone(), &ns);
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
                .set_repo_catalog(&ns, &name, snapshots, discovered, foreign);
        }
    }
}

async fn reconcile_inner(repo: &Repository, ctx: &Context) -> Result<Action> {
    if let Err(e) = validate::validate_repository_no_inline_retention(&repo.spec) {
        return Err(Error::Validation(e.to_string()));
    }

    let namespace = repo
        .namespace()
        .ok_or_else(|| Error::Invariant("Repository has no namespace".into()))?;
    let name = repo.name_any();
    let repo_uid = repo
        .uid()
        .ok_or_else(|| Error::Invariant("Repository has no uid".into()))?;
    let api: Api<Repository> = Api::namespaced(ctx.client.clone(), &namespace);

    // §14(e): a suspended Repository skips connect/bootstrap AND maintenance
    // projection entirely — a declarative pause. Surface it via a condition and back
    // off long; nothing else runs.
    if repo.spec.suspend {
        let conds = repo
            .status
            .as_ref()
            .map(|s| s.conditions.clone())
            .unwrap_or_default();
        let conditions = io::set_ready(
            &conds,
            repo.meta().generation,
            io::ReadyOutcome::Reconciling,
            "Suspended",
            "Repository is suspended (spec.suspend); skipping connect and maintenance",
        );
        let current = serde_json::to_value(&repo.status).ok();
        io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            serde_json::json!({ "observedGeneration": repo.meta().generation, "conditions": conditions }),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(300)));
    }

    // The optional kopia web-UI server runs regardless of backend (the server pod
    // connects to the repository itself), so reconcile it before the backend match
    // below — whose object-store/volume branches return early — to ensure it is
    // applied/migrated/torn down on every (non-suspended) reconcile.
    reconcile_repository_server(ctx, repo, &namespace, &name).await?;

    // The controller may run kopia in-process for the FILESYSTEM backend only
    // (ADR §5.4 permits short idempotent ops; a *bare-path* filesystem repo is
    // reachable from the controller's own filesystem via a hostPath/shared mount,
    // or in the e2e harness). A filesystem repo backed by a PVC or an inline NFS
    // export is NOT reachable in-process, so — like object stores — it bootstraps
    // in a short mover Job that mounts the repo volume.
    match &repo.spec.backend {
        // Volume-backed filesystem repos (PVC / inline NFS) and every object store
        // bootstrap via the mover Job. `filesystem_repo_mount_source` returns the
        // volume to mount (None for object stores → no extra volume).
        Backend::Filesystem(fs) if fs.volume.is_some() => {
            return bootstrap_via_mover(
                ctx,
                repo,
                &namespace,
                &name,
                &repo_uid,
                &api,
                &repo.spec.backend,
            )
            .await;
        }
        Backend::Filesystem(fs) => {
            // Read the password Secret up front (one cheap GET). We need it to connect
            // anyway, and its `resourceVersion` drives the hard-stop below: a credential
            // fix after a terminal failure does NOT bump `metadata.generation`, so the
            // gate must also key on the Secret revision.
            let creds = io::repo_credentials(&repo.spec.encryption);
            let (password, cred_version) =
                io::read_repo_credential(&ctx.client, &namespace, &creds).await?;

            // Hard-stop: if we already terminally failed to connect for THIS spec
            // generation AND the password Secret is unchanged since, don't re-hit the
            // backend. A non-retryable failure (e.g. PermissionDenied on the NFS export,
            // or a wrong password) cannot succeed until an input changes — the CR spec
            // (bumps `generation`) or the password Secret (bumps its `resourceVersion`,
            // re-triggered by the Secret watch in `lib.rs`). The 30 min heartbeat keeps
            // us resilient to a watch desync without spamming the backend or the logs.
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

            // Idempotent connect; create on first use when enabled AND the
            // failure does not indicate an existing repo (auth/locked) — the same
            // safe gate the bootstrap mover applies (never recreate over data).
            // `created` tracks whether THIS reconcile is the one that created the
            // repo (mirrors the mover's own `created` flag, `crates/mover/src/
            // main.rs`) — the maintenance-owner stamp below needs the real
            // distinction, not a hardcoded assumption.
            let mut created = false;
            if let Err(e) = client
                .repository_connect(&spec, kopiur_kopia::CacheTuning::default())
                .await
            {
                // Part A: never auto-create over a once-`Ready` repository (pinned
                // `uniqueId`). A wiped/unreachable backend must surface as a failure,
                // never a silent fresh empty repo — re-create is a deliberate action.
                let create_enabled = health::auto_create_allowed(
                    kopiur_api::common::create_enabled(repo.spec.create.as_ref()),
                    repo.status.as_ref().and_then(|s| s.unique_id.as_deref()),
                );
                // Try create-then-connect when enabled and the failure isn't
                // "repo already there" (auth/locked); otherwise the connect error
                // is terminal. A terminal failure (connect OR a failed create)
                // surfaces Failed + an actionable Event (e.g. filesystem "Access
                // Denied") rather than an invisible reconcile error with no status.
                // Create-time-fixed format knobs (encryption/splitter/hash/ECC),
                // resolved from spec.create and honored only on actual create
                // (ADR-0005 §13(a)). Immutable post-create (§7).
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
                                created = true;
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
                    // Reserve `Failed` (terminal, gated) for non-retryable classes;
                    // a retryable backend blip is `Degraded` and keeps retrying on
                    // the 30 s transient cadence.
                    let phase = if retryable { "Degraded" } else { "Failed" };
                    let phase_enum = if retryable {
                        RepositoryPhase::Degraded
                    } else {
                        RepositoryPhase::Failed
                    };
                    // Stable, volatile-free condition message — the full stderr (with
                    // its per-attempt temp filename) goes to the Event only, so the
                    // persisted status is byte-identical across repeated failures and
                    // the guarded write below becomes a true no-op.
                    let conditions =
                        bootstrap_condition(repo, false, class.as_str(), class.summary());
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
                    // Fire the Warning Event only on a real transition (not on every
                    // requeue) — it carries the full stderr for `kubectl describe`.
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
                        // Transient: surface as an Err so error_policy requeues at
                        // the 30 s cadence and we keep trying.
                        Err(Error::Kopia(e))
                    } else {
                        // Terminal: status is written; stop. The gate above makes
                        // subsequent wakes no-ops until the spec changes.
                        Ok(Action::requeue(TERMINAL_HEARTBEAT))
                    };
                }
            }

            // Status: phase/uniqueId/backend/resolvedCredentialVersion.
            let status = client.repository_status().await?;

            // Stamp/self-heal the maintenance owner on this in-process (bare-path
            // filesystem) path, mirroring the mover's bootstrap stamp/self-heal
            // exactly (`crates/mover/src/main.rs`) — including the create-vs-connect
            // split: a repository THIS reconcile just created stamps the desired
            // owner unconditionally (kopia recorded the controller pod's ephemeral
            // identity as owner, and nothing recognizes it yet, so there is no
            // staleness check to apply); a connect-to-existing repository instead
            // goes through the shared `maintenance_restamp_target` decision against
            // its recorded owner. M6 regression fixed here: this used to hardcode
            // `created: false` into `maintenance_restamp_target` even right after an
            // in-process create, so a freshly-created bare-path Repository with
            // `identityDefaults.cluster` set recorded the ephemeral pod identity as
            // owner and `RestampPolicy::OwnFormatsOnly` (required once `cluster` is
            // set) refused to ever restamp it (not empty, not a recognized alias) —
            // the managed Maintenance's `takeoverPolicy: Never` then yielded
            // forever. `maintenance set --owner` is NOT owner-gated, so re-stamping
            // is always safe once we decide to do it.
            //
            // Gated (via `bootstrap_maintenance_owner_plan` returning `None`)
            // exactly like the mover work specs: never for a ReadOnly repo (a
            // consumer must not clobber the primary's owner), a deliberate
            // `spec.maintenance.enabled: false`, or a repository an
            // externally-authored Maintenance already covers. Best-effort —
            // never fail the reconcile over it.
            {
                let cluster = repo
                    .spec
                    .identity_defaults
                    .as_ref()
                    .and_then(|d| d.cluster.as_deref())
                    .filter(|c| !c.is_empty());
                let maintenance_enabled = repo.spec.maintenance.as_ref().is_none_or(|m| m.enabled);
                let foreign_maintenance = io::maintenance_covered_by_foreign(
                    ctx,
                    RepositoryKind::Repository,
                    "Repository",
                    &name,
                    Some(&namespace),
                );
                let suppress =
                    !repo.spec.mode.allows_writes() || !maintenance_enabled || foreign_maintenance;
                let (desired, policy, aliases) = io::bootstrap_maintenance_owner_plan(
                    RepositoryKind::Repository,
                    &namespace,
                    &name,
                    cluster,
                    suppress,
                );
                if let Some(owner) = io::in_process_create_owner_target(created, desired.as_deref())
                {
                    // Unconditional stamp on a repository THIS reconcile created —
                    // see the block comment above.
                    match client.maintenance_set_owner(owner).await {
                        Ok(()) => {
                            tracing::info!(%owner, "stamped maintenance owner on created filesystem repository")
                        }
                        Err(e) => {
                            tracing::warn!(%owner, class = %e.class(), "could not stamp maintenance owner on created repository; maintenance may need takeoverPolicy=Force once")
                        }
                    }
                } else if let Some(desired) = desired.as_deref() {
                    match client.maintenance_info().await {
                        Ok(info) => {
                            // `created` is `false` here (the `if` above already
                            // handled the create case) — a genuine connect-to-existing
                            // self-heal pass.
                            if let Some(owner) = kopiur_mover::workspec::maintenance_restamp_target(
                                false,
                                Some(desired),
                                policy,
                                &aliases,
                                &info.owner,
                            ) {
                                match client.maintenance_set_owner(owner).await {
                                    Ok(()) => {
                                        tracing::info!(%owner, stale = %info.owner, "re-stamped maintenance owner on filesystem repository")
                                    }
                                    Err(e) => {
                                        tracing::warn!(%owner, class = %e.class(), "could not re-stamp maintenance owner; maintenance may need takeoverPolicy=Force once")
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(class = %e.class(), "could not read maintenance owner to self-heal; continuing")
                        }
                    }
                }
            }

            // Index-blob health (ADR-0005 §13): count the index blobs (best-effort)
            // and fold the IndexBlobHealth condition into the conditions we pass on
            // to `ensure_maintenance` (a status patch replaces the whole conditions
            // array, so there must be a single source of truth). The Warning event
            // fires only on the transition into unhealthy.
            let index_blob_count = match client.index_blob_count().await {
                Ok(n) => Some(n),
                Err(e) => {
                    tracing::warn!(class = %e.class(), "could not read index blob count; skipping index-blob health");
                    None
                }
            };
            let mut conditions = repo
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            let mut index_blob_event = None;
            let mut status_patch = serde_json::json!({
                "phase": "Ready",
                "backend": "Filesystem",
                "uniqueId": status.unique_id_hex,
                "observedGeneration": repo.metadata.generation,
                "resolvedCredentialVersion": cred_version,
            });
            if let Some(count) = index_blob_count {
                let threshold = resolve_index_blob_warn_threshold(repo.spec.health.as_ref());
                let upd = health::reconcile_index_blob_health(
                    &conditions,
                    count,
                    threshold,
                    repo.metadata.generation,
                );
                conditions = upd.conditions;
                index_blob_event = upd.event;
                status_patch["conditions"] = serde_json::to_value(&conditions).unwrap_or_default();
                status_patch["storageStats"] = serde_json::json!({ "indexBlobCount": count });
            }
            // Record a `Snapshot`'s reverify nudge as honored. Unlike the mover
            // bootstrap path, this in-process arm re-probes on every reconcile (the
            // connect above) and the annotation patch already retriggered us — so the
            // re-probe is implicit. We only stamp `lastReverifyAt` so the once-honored
            // token is observable in status and consistent with the mover path.
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
            // Non-blocking MassDeletionHeld condition from the live Snapshot store
            // (mirrors IndexBlobHealth), folded into the same conditions array.
            let conditions = fold_mass_deletion(ctx, repo, &conditions).await;
            // Always publish the kstatus Ready conditions for the healthy bare-path
            // repo (issue #245), layered on any conditions built above — even when
            // no index-blob count was read, so a resume clears the suspend branch's
            // stale Ready/Reconciling/Suspended entries and Flux `wait: true` passes.
            let conditions = io::set_ready(
                &conditions,
                repo.metadata.generation,
                io::ready_outcome_for_phase(RepositoryPhase::Ready),
                "Connected",
                "connected to the existing repository",
            );
            status_patch["conditions"] = serde_json::to_value(&conditions).unwrap_or_default();
            let current = serde_json::to_value(&repo.status).ok();
            io::patch_status_if_changed(&api, &name, current.as_ref(), status_patch).await?;
            if let Some(w) = index_blob_event {
                io::publish_warning_event(ctx, repo, w.reason, w.action, &w.message).await;
            }

            // Catalog scan on the `catalog.refreshInterval` cadence — or
            // immediately on a spec change (`scan_due`'s generation arm: a
            // `catalog.retain` edit must expire rows NOW, not at the next timed
            // refresh): a bare-path filesystem repo re-lists in-process (cheap),
            // materializing/expiring discovered Snapshots per `catalog.retain`.
            // Gating the scan also gates the `lastRefreshAt` write, so a Ready
            // repo's status is byte-stable between refreshes (no self-triggered
            // reconcile hot-loop).
            let interval = CatalogBounds::effective_refresh_interval(repo.spec.catalog.as_ref());
            if catalog::scan_due(
                repo.metadata.generation,
                repo.status.as_ref().and_then(|s| s.observed_generation),
                last_refresh_at(repo),
                interval,
                CatalogBounds::periodic_refresh_enabled(repo.spec.catalog.as_ref()),
                scan_requested_token(repo),
                scan_requested_honored(repo),
                chrono::Utc::now(),
            ) {
                let listing = client.snapshot_list(None).await?;
                let total = listing.len() as i64;
                run_catalog_scan(
                    ctx, repo, &namespace, &name, &repo_uid, &listing, total, false, 0,
                )
                .await?;
            }

            // Now that the repo is Ready, ensure its managed Maintenance exists
            // (default-on) and surface the MaintenanceConfigured condition. Built
            // on the `conditions` we just patched (which include IndexBlobHealth),
            // NOT the stale cached object — both writes replace the whole array, so
            // re-reading would drop the condition we set above. A namespaced
            // Repository's Maintenance lives in the repo's namespace. ADR §3.7.
            // §11: a ReadOnly repository runs no maintenance (it serves restores
            // only). Skip the projection so no managed Maintenance is created.
            ensure_repo_maintenance(ctx, repo, &namespace, &name, &api, &conditions).await;
        }
        other => {
            // Object-store backends run connect/create/status/catalog in a
            // short-lived mover Job (ADR §5.4): the controller cannot reach the
            // store in-process. The Job writes its result into the work-spec
            // ConfigMap; the controller (sole writer of the Repository status)
            // reads it back to set phase/uniqueId and materialize the catalog.
            return bootstrap_via_mover(ctx, repo, &namespace, &name, &repo_uid, &api, other).await;
        }
    }

    Ok(Action::requeue(catalog::reconcile_interval(
        repo.spec.catalog.as_ref(),
    )))
}

/// Apply / migrate / tear down the optional kopia web-UI server for a `Repository`.
/// The server objects live in the Repository's own namespace and are owned by it
/// (owner-ref GC reaps them on deletion).
async fn reconcile_repository_server(
    ctx: &Context,
    repo: &Repository,
    namespace: &str,
    name: &str,
) -> Result<()> {
    use crate::server::{ServerReconcileCtx, reconcile_server, server_status_json};

    let desired_ns = repo.spec.server.as_ref().map(|_| namespace.to_string());
    let observed_ns = repo
        .status
        .as_ref()
        .and_then(|s| s.server.as_ref())
        .and_then(|sv| sv.namespace.clone());
    let owner = Some(io::owner_ref_for(repo, "Repository")?);

    let rc = ServerReconcileCtx {
        client: &ctx.client,
        instance: name,
        backend: &repo.spec.backend,
        encryption: &repo.spec.encryption,
        server: repo.spec.server.as_ref(),
        read_only_mode: !repo.spec.mode.allows_writes(),
        target_namespace: desired_ns,
        observed_namespace: observed_ns,
        owner,
        extra_labels: Default::default(),
        creds_src_namespace: namespace.to_string(),
        is_cluster: false,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        service_account: ctx.mover_service_account.as_deref(),
    };

    let outcome = reconcile_server(&rc).await?;
    if let Some(status) = server_status_json(&outcome) {
        let api: Api<Repository> = Api::namespaced(ctx.client.clone(), namespace);
        io::patch_status(&api, name, status).await?;
    }
    Ok(())
}

/// `status.catalog.lastRefreshAt` from the cached object (the refresh-due gate).
fn last_refresh_at(repo: &Repository) -> Option<&str> {
    repo.status
        .as_ref()
        .and_then(|s| s.catalog.as_ref())
        .and_then(|c| c.last_refresh_at.as_deref())
}

/// The live `catalog-scan-requested-at` annotation value (opaque token; the
/// writer is the policy reconciler, M6). `None` when never requested.
fn scan_requested_token(repo: &Repository) -> Option<&str> {
    repo.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(crate::consts::CATALOG_SCAN_REQUESTED_ANNOTATION))
        .map(String::as_str)
}

/// `status.catalog.scanRequestHonored` from the cached object — the token last
/// retired by a completed scan (equality-compared against the live annotation).
fn scan_requested_honored(repo: &Repository) -> Option<&str> {
    repo.status
        .as_ref()
        .and_then(|s| s.catalog.as_ref())
        .and_then(|c| c.scan_request_honored.as_deref())
}

/// `status.catalog.scanRequestAttemptAt` from the cached object — rate-limits
/// how often a pending token may (re-)launch a bootstrap/scan attempt.
fn scan_requested_attempt_at(repo: &Repository) -> Option<&str> {
    repo.status
        .as_ref()
        .and_then(|s| s.catalog.as_ref())
        .and_then(|c| c.scan_request_attempt_at.as_deref())
}

/// The `Repository`'s currently-observed status conditions (empty when it has no status).
fn repo_conditions(
    repo: &Repository,
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    repo.status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default()
}

/// This `Repository` as a [`RepositoryRef`], matching how consumers PIN it
/// (`pinned_repository_ref`: own namespace populated), so the mass-deletion
/// condition's `repo_key` lines up with the deletion path's per-repo count.
fn repository_ref(repo: &Repository) -> RepositoryRef {
    RepositoryRef {
        kind: RepositoryKind::Repository,
        name: repo.name_any(),
        namespace: repo.namespace(),
    }
}

/// The raw `allow-mass-deletion` annotation value on this `Repository`, if any.
fn mass_deletion_ack_raw(repo: &Repository) -> Option<&str> {
    repo.metadata
        .annotations
        .as_ref()?
        .get(crate::consts::ALLOW_MASS_DELETION_ANNOTATION)
        .map(String::as_str)
}

/// Fold the non-blocking `MassDeletionHeld` condition into `conditions` from the
/// live Snapshot store (ADR-0005 §6; delegates to the shared
/// [`crate::snapshot::repo_mass_deletion_conditions`]). Returns `conditions`
/// unchanged when the store is unset/unsynced.
async fn fold_mass_deletion(
    ctx: &Context,
    repo: &Repository,
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    crate::snapshot::repo_mass_deletion_conditions(
        ctx,
        &repository_ref(repo),
        mass_deletion_ack_raw(repo),
        repo.spec.deletion_protection.as_ref(),
        repo.spec.on_namespace_delete,
        conditions,
        repo.metadata.generation,
    )
    .await
}

/// Project this `Repository`'s `spec.maintenance` into its managed `Maintenance` CR and
/// refresh the `MaintenanceConfigured` condition (ADR §3.7; §11: a ReadOnly repository
/// serves restores only and runs no maintenance).
///
/// `conditions` is the set to build on — the freshly patched one on the bootstrap finalize
/// path (a status patch replaces the whole array, so re-reading the cached object there
/// would drop conditions written moments earlier), or the cached status elsewhere.
///
/// The cluster-repo twin (`cluster_repository::ensure_cluster_maintenance`) carries the
/// full rationale for calling this on the steady-state pass as well (#231).
async fn ensure_repo_maintenance(
    ctx: &Context,
    repo: &Repository,
    namespace: &str,
    name: &str,
    api: &Api<Repository>,
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) {
    if !repo.spec.mode.allows_writes() {
        return;
    }
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
        RepositoryKind::Repository,
        "Repository",
        namespace,
        Some(namespace),
        Some(namespace),
        name,
        repo.spec.maintenance.as_ref(),
        cluster,
        conditions,
        repo.metadata.generation,
    )
    .await;
}

/// Drive the mover-Job bootstrap state machine: launch the Job, then on each
/// reconcile reflect its progress (`Initializing` → `Ready`/`Failed`), reading the
/// result the mover wrote into the work-spec ConfigMap. Used for object stores AND
/// for volume-backed filesystem repos (PVC / inline NFS) — neither is reachable
/// from the controller in-process. A filesystem backend's repo volume is mounted
/// read-write so the mover can create/connect the repository.
#[allow(clippy::too_many_arguments)]
async fn bootstrap_via_mover(
    ctx: &Context,
    repo: &Repository,
    namespace: &str,
    name: &str,
    repo_uid: &str,
    api: &Api<Repository>,
    backend: &Backend,
) -> Result<Action> {
    // "discovery" (not "bootstrap"): the FIRST run does bootstrap the
    // repository, but every later run of this Job is a catalog re-scan — the
    // name follows the recurring purpose users actually see.
    let job_name = format!("{name}-discovery");
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);

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

    // Backend health probe (`spec.health.probe`, opt-in). A repo that has reached
    // `Ready` carries a pinned `uniqueId`; a probe re-runs the bootstrap as a pure
    // connect (Part A forces `auto_create=false`), and the result is interpreted as
    // a probe — the phase STAYS `Ready` (backups/replication keep running) and the
    // outcome surfaces as the `BackendReachable` condition (see `finalize_bootstrap`).
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
    // `probeAttemptAt`): interpret it gently (stay `Ready`) and consume it exactly
    // once (`finalize_bootstrap` deletes it). Keying on `awaits_result()` rather than
    // on `probe_enabled` alone is what stops a Job left behind by a STRICT launch
    // from being re-read as a probe — which used to fabricate `lastHealthyAt` from a
    // connect no probe requested and, worse, route a genuine bootstrap failure into
    // the alert-only `finalize_probe_failure` (hard-coded `phase: Ready`), silently
    // resurrecting a terminally-`Failed` repository.
    // `!reverify`/`!spec_changed`: those are real re-bootstraps of new config and keep
    // the strict `Failed`-on-error semantics; they are never downgraded to an alert.
    let probe_run = probe.awaits_result() && !spec_changed && !reverify;
    // The LAUNCH-side twin of `probe_run`: this launch is a probe-style re-run, so
    // (a) don't flip the phase to `Initializing` (that would fail the
    // `repository_ready` gate and pause backups) and (b) stamp `probeAttemptAt` so the
    // result is recognised as a probe when it lands.
    let probe_style_launch =
        probe_enabled && bootstrapped_before && already_ready && !reverify && !spec_changed;

    if let Some(job) = job_api.get_opt(&job_name).await? {
        return match crate::snapshot::job_terminal_state(&job) {
            // Still running: surface Initializing and poll. A catalog-refresh
            // re-run of an already-Ready repo keeps its phase — flapping
            // Ready→Initializing every refresh would be pure status churn.
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
            // Complete or backoff-exhausted: read the structured result — unless
            // the result is stale (catalog refresh due, the spec changed, or a health
            // probe is due) since it was taken: then recycle the Job so the next
            // reconcile re-runs the bootstrap for a FRESH connect. `probe.launches()`
            // is essential — without it the first probe would re-read the lingering
            // first-bootstrap result and report healthy without ever re-connecting.
            // It is deliberately `launches()`, NOT "a probe is enabled": once we have
            // launched the probe's own Job, `probe_action` returns `AwaitingResult` and
            // this arm stands down so `finalize_bootstrap` can consume it. Recycling
            // then too is the #273 livelock — it destroys the very result that would
            // have stamped `lastProbeAt` and cleared the gate.
            Some(success) => {
                let interval =
                    CatalogBounds::effective_refresh_interval(repo.spec.catalog.as_ref());
                if reverify
                    || probe.launches()
                    || catalog::bootstrap_recycle_due(
                        already_ready,
                        repo.metadata.generation,
                        repo.status.as_ref().and_then(|s| s.observed_generation),
                        last_refresh_at(repo),
                        // The Job's completionTime: the timer arm may only
                        // recycle a result finalize has already consumed.
                        job.status
                            .as_ref()
                            .and_then(|s| s.completion_time.as_ref())
                            .map(|t| t.0.to_string())
                            .as_deref(),
                        interval,
                        CatalogBounds::periodic_refresh_enabled(repo.spec.catalog.as_ref()),
                        scan_requested_token(repo),
                        scan_requested_honored(repo),
                        scan_requested_attempt_at(repo),
                        kopiur_api::consts::DEFAULT_CATALOG_REFRESH_INTERVAL,
                        chrono::Utc::now(),
                    )
                {
                    tracing::debug!(repo = %name, "recycling finished bootstrap Job for a catalog refresh");
                    io::delete_mover_run(&ctx.client, namespace, &job_name).await?;
                    return Ok(Action::requeue(Duration::from_secs(5)));
                }
                finalize_bootstrap(
                    ctx, repo, namespace, name, repo_uid, api, backend, &job_name, success,
                    probe_run,
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
            last_refresh_at(repo),
            CatalogBounds::effective_refresh_interval(repo.spec.catalog.as_ref()),
            CatalogBounds::periodic_refresh_enabled(repo.spec.catalog.as_ref()),
            scan_requested_token(repo),
            scan_requested_honored(repo),
            scan_requested_attempt_at(repo),
            kopiur_api::consts::DEFAULT_CATALOG_REFRESH_INTERVAL,
            chrono::Utc::now(),
        ))
    {
        // The STEADY STATE of a Ready mover-bootstrapped repo (`bootstrap_create_due` is
        // true for any non-Ready phase, so nothing pre-bootstrap reaches here). Re-evaluate
        // maintenance coverage before parking — otherwise this repo's `MaintenanceConfigured`
        // condition is only ever written at the tail of a bootstrap, and a value written
        // during a bootstrap race freezes forever (#231). Cheap + idempotent; the status
        // patch is skipped when nothing changed.
        //
        // Build on a FRESH read of the conditions, not the watch-cached object: a Maintenance
        // event can requeue us moments after the bootstrap finalize patched `Bootstrapped` /
        // `BackendReachable`, while the informer store still holds the pre-patch copy — and
        // the condition write below replaces the whole array, so a stale copy would silently
        // resurrect it and drop those conditions.
        let fresh = api.get_opt(name).await?;
        let conditions = fresh.as_ref().map(repo_conditions).unwrap_or_default();
        // Non-blocking MassDeletionHeld condition on the repo's steady-state
        // cadence (the ack-drain watch also lands here on an annotation edit).
        // Own guarded write, sourced from the FRESH conditions so it carries all
        // of them (no clobber) and is skipped when unchanged; ensure_maintenance
        // then builds on the folded array.
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
        ensure_repo_maintenance(ctx, repo, namespace, name, api, &conditions).await;
        return Ok(Action::requeue(probe_aware_reconcile_interval(repo)));
    }

    // Upgrade shim for the `{name}-bootstrap` → `{name}-discovery` rename: the
    // Job name is the lookup key, so a previous operator version's Job is
    // invisible to the match above. Reap it here — reached only when we are
    // about to create the renamed Job — and hold off creating the successor
    // until the legacy Job is fully GONE (foreground delete keeps it visible
    // while its pod terminates), so two movers never run against this
    // repository concurrently mid-upgrade. The scan-request attempt stamp only
    // lands with the successor's creation, so the pending token re-fires this
    // path after the requeue. (A finished old Job is TTL-reaped regardless;
    // this covers the in-flight one. Removable once no supported upgrade path
    // predates the rename.)
    if !io::legacy_bootstrap_cleared(
        &ctx.client,
        namespace,
        name,
        repo.metadata.uid.as_deref().unwrap_or_default(),
    )
    .await?
    {
        return Ok(Action::requeue(Duration::from_secs(5)));
    }

    // Whether we are about to launch a Job BECAUSE OF a pending scan-request
    // token (regardless of whatever else also fired the gate above) — used only
    // to stamp `scanRequestAttemptAt` below, the token arm's own rate limit. It
    // never gates anything else, so recomputing it here (rather than plumbing a
    // bool out of `bootstrap_create_due`) is simplest and stays pure.
    let token_driven_scan_request = catalog::scan_requested_due(
        scan_requested_token(repo),
        scan_requested_honored(repo),
        scan_requested_attempt_at(repo),
        kopiur_api::consts::DEFAULT_CATALOG_REFRESH_INTERVAL,
        chrono::Utc::now(),
    );

    // Build + apply the Job (ConfigMap carries the work spec; the result is
    // written back into the same ConfigMap under `result.json`).
    // Part A: the mover work spec only carries `auto_create` for a never-bootstrapped
    // repo. A once-`Ready` repo (pinned `uniqueId`) re-runs its bootstrap as a pure
    // connect probe — it can never silently recreate over a vanished backend.
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
        RepositoryKind::Repository,
        "Repository",
        name,
        Some(namespace),
    );
    let work_spec = bootstrap_work_spec(
        backend,
        name,
        namespace,
        create_enabled,
        true,
        repo.spec.create.as_ref(),
        repo.spec.mover_defaults.as_ref(),
        cluster,
        foreign,
        read_only,
        maintenance_enabled,
        foreign_maintenance,
        repo.spec.parameters.as_ref(),
    );
    // Resolve the bootstrap Job's run identity in the Repository's namespace:
    // the user's workload-identity SA (preflighted + bound to the mover role),
    // or the minted mover SA + RoleBinding (ADR §4.12).
    let mover_identity = io::ensure_mover_identity(
        &ctx.client,
        namespace,
        &[backend],
        ctx.mover_service_account.as_deref(),
        ctx.mover_role_kind.as_str(),
        &ctx.mover_clusterrole,
    )
    .await?;
    // Resolve the credential Secret(s) the bootstrap mover loads via envFrom:
    // verify the user-managed credential Secret is present. The bootstrap Job runs
    // in the Repository's own namespace, where its Secret already lives — so it
    // never needs projection (projection is a consumer-side opt-in on
    // SnapshotPolicy/Restore/Maintenance, not on the repository).
    let owner = io::owner_ref_for(repo, "Repository")?;
    let refs = io::mover_creds_secret_refs(backend, &repo.spec.encryption, Some(namespace));
    let creds_names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
    let creds = io::resolve_mover_creds(
        &ctx.client,
        namespace,
        &io::CredsPrefix::bootstrap(name),
        &owner,
        &refs,
        false, // consumer opt-in: never project on the bootstrap path
        false, // owner allow: irrelevant on the same-namespace bootstrap path
        &io::CredsContext {
            secret_names: &creds_names,
            repo_kind: "Repository",
            repo_name: name,
            repo_secret_namespace: repo
                .spec
                .encryption
                .password_secret_ref
                .namespace
                .as_deref(),
        },
    )
    .await?;
    let creds_secrets = io::plain_creds(creds.names);
    let mut labels = BTreeMap::new();
    labels.insert(
        "kopiur.home-operations.com/repository".to_string(),
        name.to_string(),
    );
    mover_identity.decorate_labels(&mut labels);
    // A filesystem backend mounts its repo volume (PVC / inline NFS) read-write so
    // the mover can create/connect the repository; object stores mount nothing.
    let repo_volume =
        io::filesystem_repo_mount_source(backend).map(|source| jobs::VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(backend).unwrap_or_default(),
            read_only: false,
        });
    // The bootstrap (connect/create) Job has no recipe `mover`, but inherits the
    // repository's `moverDefaults` — the bootstrap-gap fix (ADR-0004 §1): a
    // filesystem repo on a non-65532-owned directory becomes bootstrappable by
    // setting an effective write identity in `moverDefaults` once, with no
    // special-case knob. For a **PVC** (block CSI) that is `podSecurityContext.fsGroup`
    // or `securityContext.runAsUser`; for **inline NFS** `fsGroup` is silently a
    // no-op (the kubelet never recursively chowns an in-tree NFS mount), so use
    // `podSecurityContext.supplementalGroups` against a group-writable export, or
    // `securityContext.runAsUser` matching the export owner, or remap NAS-side
    // (TrueNAS Mapall). Not subject to the privileged-mover namespace gate: it runs
    // in the repo's own namespace and is authored by the repo owner.
    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.spec.mover_defaults.as_ref(),
        None,
        None,
        None,
        None,
        None,
    );
    // spec.bootstrap.failurePolicy lets callers raise the deadline for a slow
    // backend (e.g. an rclone remote whose connect loads metadata through the
    // rclone/WebDAV bridge) or tune the retry budget; unset keeps the built-in
    // bootstrap defaults.
    let bootstrap_fp = repo
        .spec
        .bootstrap
        .as_ref()
        .and_then(|b| b.failure_policy.as_ref());
    let inputs = MoverJobInputs {
        name: &job_name,
        namespace,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        // Bound the bootstrap Job: a pod that never schedules (missing mover SA,
        // image-pull failure) otherwise never reaches a `Failed` condition, so the
        // controller never finalizes and the repository hangs `Initializing` with
        // no Event. The deadline forces it terminal so `finalize_bootstrap` runs.
        // Shared with `health::probe_attempt_timeout`, so the bound on a probe Job's
        // life and the gate that waits for its result can never disagree.
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
    io::apply_mover_objects(&ctx.client, namespace, &job_name, Some(&cm), &job).await?;
    // Stamp the reverify token (loop guard): this request is now honored.
    // A health-probe re-run of an already-`Ready` repo keeps its phase `Ready`
    // (`probe_style_launch`) so backups/replication are never paused while the
    // probe connects; only a first bootstrap or a spec-change re-bootstrap shows
    // `Initializing`.
    let mut create_status = if probe_style_launch {
        serde_json::json!({ "backend": backend.kind_str() })
    } else {
        serde_json::json!({ "phase": "Initializing", "backend": backend.kind_str() })
    };
    if let Some(token) = reverify_token {
        create_status["lastReverifyAt"] = serde_json::Value::String(token.to_string());
    }
    // Launch-side probe stamp (#273) — the loop guard that lets the probe terminate.
    // `lastProbeAt` is only written at FINALIZE, so it cannot bound the launch: gating
    // the recycle on it alone deletes every Job whose completion would have cleared
    // it. Stamping here means the next pass reads `AwaitingResult` and lets the
    // finished Job through to `finalize_bootstrap`.
    //
    // Written as `now` for a probe-style launch and as an explicit `null` otherwise, so
    // a STRICT relaunch actively clears a stale stamp rather than inheriting it — a
    // stamp left over from an abandoned probe would make this strict result read as a
    // probe. Skipped entirely when there is nothing to say, keeping the patch a no-op
    // for repositories that never enable the probe.
    if probe_enabled || probe_attempt_at.is_some() {
        create_status["health"] = health::probe_attempt_health_patch(
            probe_style_launch
                .then(|| chrono::Utc::now().to_rfc3339())
                .as_deref(),
        );
    }
    // Rate-limit backoff for the scan-request token: stamped BEFORE the outcome is
    // known (a probe-style failure never lands in `run_catalog_scan`, so the
    // honored-write there is not a reliable place to bound retries) — see
    // `catalog::scan_requested_due`. Merge-patched under `catalog` so the other
    // `status.catalog` fields (lastRefreshAt, scanRequestHonored, ...) are untouched.
    if token_driven_scan_request {
        create_status["catalog"] = serde_json::json!({
            "scanRequestAttemptAt": chrono::Utc::now().to_rfc3339(),
        });
    }
    io::patch_status(api, name, create_status).await?;
    tracing::info!(repo = %name, backend = backend.kind_str(), "launched repository bootstrap Job");
    Ok(Action::requeue(Duration::from_secs(15)))
}

/// Build the bootstrap work spec for an object-store backend. Identity is a
/// sentinel (bootstrap connects/creates the repo, it does not snapshot under any
/// identity). `scan_catalog` drives whether the mover returns the snapshot list
/// for discovered-Snapshot materialization.
///
/// `cluster`/`foreign` (`identityDefaults.cluster` / the effective
/// `catalog.foreignSnapshots`) drive `catalog_foreign_prefilter_cluster`, mirroring
/// `cluster_repository::cluster_bootstrap_work_spec`: the mover pre-filters
/// `ForeignCluster`-classified entries BEFORE its own materialization cap only when
/// cluster identity is actually on AND the effective policy is `Ignore` — under
/// `Fallback` those entries must still come back so `run_catalog_scan`'s placement
/// pass can count them (a namespaced Repository has no `fallbackNamespace`, so
/// `decide_namespace_placement` always resolves a `Fallback`-classified foreign
/// entry to `ForeignIgnored` too, but the count still needs the entry present).
///
/// `read_only`/`maintenance_enabled`/`foreign_maintenance` (M6) drive the
/// maintenance-owner plan via [`io::bootstrap_maintenance_owner_plan`]: the
/// mover restamps a stale owner on EVERY connect-to-existing, not just create
/// (see `maintenance_restamp_target`), so `maintenance_owner: None` is the only
/// way to mean "never stamp/restamp at all" — used for a ReadOnly repo (fixes
/// the consumer-clobbers-the-primary's-owner bug), a deliberate
/// `spec.maintenance.enabled: false`, or a repository an externally-authored
/// Maintenance already covers.
/// The epoch parameters to send to a bootstrap mover, or an empty set when there is
/// nothing to apply (#258). Pure; shared by both repository kinds, which have separate
/// work-spec builders.
///
/// A `ReadOnly` repository sends NOTHING. `kopia repository set-parameters` rewrites the
/// repository-global format blob and hard-errors outright on a read-only connection
/// (`unable to write blobcfg blob: ... storage is read-only`). It is the same hazard shape
/// as a consumer cluster clobbering the primary's maintenance owner, and gets the same
/// answer: a ReadOnly repository never mutates repository-global state. Admission rejects
/// the pairing as well, so this is defense in depth rather than the only guard.
pub(crate) fn epoch_parameters_for(
    read_only: bool,
    parameters: Option<&kopiur_api::repository::RepositoryParameters>,
) -> kopiur_mover::workspec::EpochParametersSpec {
    if read_only {
        return Default::default();
    }
    parameters
        .and_then(|p| p.epoch.as_ref())
        .map(kopiur_mover::workspec::EpochParametersSpec::from_api)
        .unwrap_or_default()
}

/// The blob retention to send to a bootstrap mover, or `None` when there is nothing to apply
/// (#332). Pure; shared by both repository kinds, exactly like [`epoch_parameters_for`].
///
/// `None` here means "leave the repository's retention alone" — it is NOT "disable". A
/// `ReadOnly` repository always sends `None`, for the same reason it sends no epoch
/// parameters: `set-parameters` hard-errors on a read-only connection.
pub(crate) fn blob_retention_for(
    read_only: bool,
    parameters: Option<&kopiur_api::repository::RepositoryParameters>,
) -> Option<kopiur_mover::workspec::BlobRetentionSpec> {
    if read_only {
        return None;
    }
    parameters
        .and_then(|p| p.blob_retention.as_ref())
        .and_then(kopiur_mover::workspec::BlobRetentionSpec::from_api)
}

#[allow(clippy::too_many_arguments)]
fn bootstrap_work_spec(
    backend: &Backend,
    name: &str,
    namespace: &str,
    auto_create: bool,
    scan_catalog: bool,
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
            RepositoryKind::Repository,
            namespace,
            name,
            cluster_ref,
            read_only || !maintenance_enabled || foreign_maintenance,
        );
    MoverWorkSpec {
        version: 1,
        operation: Operation::BootstrapRepository(BootstrapRepositoryOp {
            auto_create,
            scan_catalog,
            // Create-time format knobs (encryption/splitter/hash/ECC) honored only
            // when the bootstrap creates the repo (ADR-0005 §13(a)).
            create_options: kopiur_mover::workspec::CreateOptionsSpec::from_create(create),
            // MUTABLE parameters (#258) — the opposite of create_options: re-applied on
            // drift on every bootstrap, including a connect-to-existing. Never sent for a
            // ReadOnly repository: `set-parameters` rewrites the format blob and kopia
            // hard-errors on a read-only connection. (Admission rejects that pairing too;
            // this is the belt to its braces, and the same hazard shape as the
            // consumer-clobbers-the-primary's-maintenance-owner bug M6 fixed.)
            epoch_parameters: epoch_parameters_for(read_only, parameters),
            blob_retention: blob_retention_for(read_only, parameters),
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
            hostname: namespace.to_string(),
            source_path: String::new(),
        },
        repository: backend_to_repository_connect(backend),
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "Repository".to_string(),
            name: name.to_string(),
            namespace: namespace.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        // Bootstrap is a connect/create probe, not a data run: kopia defaults.
        cache: Default::default(),
        // Apply the repo throttle on the bootstrap connection too (§13(e)).
        throttle: io::throttle_spec(mover_defaults),
    }
}

/// Read the [`BootstrapResult`] the mover wrote into the work-spec ConfigMap.
/// `Ok(None)` if the ConfigMap or the result key is not (yet) present.
async fn read_bootstrap_result(
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

/// Reflect a finished bootstrap Job into the Repository status. On success:
/// `Ready` + uniqueId, then materialize discovered Snapshots from the returned
/// snapshots. On failure: `Failed` + an actionable `Bootstrapped=False`
/// condition carrying the kopia error class/message.
#[allow(clippy::too_many_arguments)]
async fn finalize_bootstrap(
    ctx: &Context,
    repo: &Repository,
    namespace: &str,
    name: &str,
    repo_uid: &str,
    api: &Api<Repository>,
    backend: &Backend,
    job_name: &str,
    job_succeeded: bool,
    // True when this is a health-probe re-run of an already-`Ready` repo (same
    // spec): the result is interpreted as a probe (phase stays `Ready`, the
    // `BackendReachable` condition carries the outcome) and the Job is consumed
    // exactly once (deleted after processing).
    probe_run: bool,
) -> Result<Action> {
    let result = read_bootstrap_result(ctx, namespace, job_name).await?;

    // Classify the (result, job state) pair as a typed outcome (ADR §5.5): a
    // result-less failed Job and a kopia-rejected connect are distinct,
    // exhaustively-handled modes — never a silent `Failed/Unknown` with no
    // Event — and the success arm *owns* the result, so there is no
    // `.expect()` invariant to get wrong.
    let result = match io::bootstrap_outcome(result, job_succeeded, job_name) {
        // Result not visible yet (write/propagation race): requeue briefly rather
        // than guessing. A truly result-less Job stays terminal for the next pass.
        io::BootstrapOutcome::ResultPending => {
            tracing::warn!(repo = %name, "bootstrap Job complete but result not readable yet; requeueing");
            return Ok(Action::requeue(Duration::from_secs(5)));
        }
        io::BootstrapOutcome::Failed(failure) => {
            // Health-probe failure on an already-`Ready` repo: alert-only. Keep the
            // phase `Ready` (never halt backups/replication), surface the debounced
            // `BackendReachable` condition + Warning event, and NEVER auto-recreate.
            if probe_run {
                return finalize_probe_failure(ctx, repo, api, name, job_name, &failure).await;
            }
            let reason = failure.reason();
            // A result-less Job failure carries no backend verdict (mover
            // crashed / evicted / deadline / result write hit a down apiserver
            // — the outage incident): recycle the dead Job and retry as
            // `Degraded` (the retryable-class semantics of the in-process
            // path) instead of parking `Failed` and re-reading the same Job
            // every pass until its TTL reaps it. Guarded write + stable
            // message keep repeats a no-op (no hot-loop).
            if failure.recycles_for_retry() {
                io::delete_mover_run(&ctx.client, namespace, job_name).await?;
                let conditions =
                    bootstrap_condition(repo, false, reason, &failure.condition_message());
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
                        "bootstrap Job failed without a result; recycled it for retry"
                    );
                }
                return Ok(Action::requeue(Duration::from_secs(120)));
            }
            let conditions = bootstrap_condition(repo, false, reason, &failure.condition_message());
            // A terminal bootstrap failure is kstatus-Stalled (issue #245): Flux
            // should fail its health check fast, not hang until timeout.
            let conditions = io::set_ready(
                &conditions,
                repo.metadata.generation,
                io::ready_outcome_for_phase(RepositoryPhase::Failed),
                reason,
                &failure.condition_message(),
            );
            // Guard the write so a re-confirmed failure fires the Event + warn log only
            // on the real transition, not on every 120 s re-read (the message is stable,
            // so this becomes a true no-op once written — no reconcile hot-loop).
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
                tracing::warn!(repo = %name, reason, "repository bootstrap failed");
            }
            return Ok(Action::requeue(Duration::from_secs(120)));
        }
        io::BootstrapOutcome::Succeeded(result) => result,
    };

    // Success: Ready + uniqueId + a Bootstrapped=True condition.
    let mut conditions = bootstrap_condition(
        repo,
        true,
        "Bootstrapped",
        if result.created {
            "created a new repository"
        } else {
            "connected to the existing repository"
        },
    );
    // Index-blob health (ADR-0005 §13): fold the IndexBlobHealth condition into the
    // SAME conditions array (a status patch replaces the whole array), and stamp
    // the observed count on storageStats. `index_blob_count` is best-effort in the
    // mover — `None` means the count couldn't be read this run, so leave the prior
    // condition/count untouched. The Warning event fires only on the transition.
    let threshold = resolve_index_blob_warn_threshold(repo.spec.health.as_ref());
    let mut index_blob_event = None;
    let mut status_patch = serde_json::json!({
        "phase": "Ready",
        "backend": backend.kind_str(),
        "uniqueId": result.unique_id,
        // Track the generation this result was taken for — the recycle gate
        // compares it so a spec edit re-runs the bootstrap instead of
        // re-reporting the old repository's identity forever.
        "observedGeneration": repo.metadata.generation,
    });
    // Mirror the parameters the repository actually reports (#258 epoch, #332 blob
    // retention). The apply is best-effort in the mover, so this is what keeps it honest: a
    // `spec.parameters` that failed to land stays visible here as drift from spec, rather
    // than as silence.
    //
    // Built as ONE map and assigned once. `status_patch` is a local Value assembled
    // key-by-key before a single patch, so two `status_patch["parameters"] = json!(..)`
    // statements would not merge — the second would drop the first outright, silently.
    {
        let mut params = serde_json::Map::new();
        if let Some(epoch) = &result.epoch {
            params.insert("epoch".into(), serde_json::json!(epoch));
        }
        if let Some(retention) = &result.blob_retention {
            params.insert("blobRetention".into(), serde_json::json!(retention));
        }
        if !params.is_empty() {
            status_patch["parameters"] = serde_json::Value::Object(params);
        }
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
    if let Some(count) = result.index_blob_count {
        let upd = health::reconcile_index_blob_health(
            &conditions,
            count,
            threshold,
            repo.metadata.generation,
        );
        conditions = upd.conditions;
        index_blob_event = upd.event;
        // Merge-patch: setting storageStats.indexBlobCount preserves snapshotCount.
        status_patch["storageStats"] = serde_json::json!({ "indexBlobCount": count });
    }
    // A successful health probe: clear any prior `BackendReachable=False`/failure
    // debounce and stamp `lastProbeAt`/`lastHealthyAt`. Safe to stamp unconditionally
    // here because a probe run consumes its Job exactly once (deleted below), so this
    // never re-runs on the same result — no status churn.
    if probe_run {
        let now = chrono::Utc::now().to_rfc3339();
        let upd = health::reconcile_probe_success(&conditions, &now, repo.metadata.generation);
        conditions = upd.conditions;
        // Explicit-null patch so the merge clears the failure counters (a `None`
        // would be elided and leave the prior streak, re-firing on the next failure).
        status_patch["health"] = health::probe_success_health_patch(&now);
        // Part A invariant: a probe NEVER rebinds identity. Keep the pinned
        // `uniqueId` so a backend re-initialized at the old location by another party
        // (same password) can't silently overwrite `status.uniqueId` on connect.
        if let Some(pinned) = repo.status.as_ref().and_then(|s| s.unique_id.as_deref()) {
            if result.unique_id.as_deref() != Some(pinned) {
                tracing::warn!(
                    repo = %name, pinned, observed = ?result.unique_id,
                    "health probe connected to a DIFFERENT repository at the backend; keeping the pinned uniqueId"
                );
            }
            status_patch["uniqueId"] = serde_json::Value::String(pinned.to_string());
        }
    }
    // Non-blocking MassDeletionHeld condition from the live Snapshot store
    // (mirrors IndexBlobHealth), folded into the same conditions array before the
    // kstatus set_ready so the merge-patch replace carries it too.
    let conditions = fold_mass_deletion(ctx, repo, &conditions).await;
    // Publish the standard kstatus Ready/Reconciling/Stalled conditions for the
    // healthy repository (issue #245), layered onto the conditions built above so
    // the merge-patch replace of `conditions` still carries Bootstrapped and
    // IndexBlobHealth. A resume reaches this path and MUST overwrite the stale
    // `Suspended`-reasoned entries left by the suspend branch, else Flux
    // `wait: true` hangs on a repository that is actually Ready.
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
    status_patch["conditions"] = serde_json::to_value(&conditions).unwrap_or_default();
    // Guarded write: this path re-runs on EVERY reconcile while the finished
    // bootstrap Job exists, so the steady-state pass must be a true no-op — a
    // re-write of identical status would bump `resourceVersion` and re-trigger
    // this reconciler through its own primary watch, in a tight loop.
    let current = serde_json::to_value(&repo.status).ok();
    io::patch_status_if_changed(api, name, current.as_ref(), status_patch).await?;
    // Fire the Warning Event only on the transition into unhealthy (not on every
    // requeue while it stays unhealthy). Non-blocking: the repo is still Ready.
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
    // covers the spec-change recycle: that fresh result must be scanned NOW
    // (e.g. a tightened `catalog.retain` expires rows), not at the next timed
    // refresh — `repo` is the pre-reconcile cache, so its observedGeneration is
    // still the old one exactly once per spec change.
    let interval = CatalogBounds::effective_refresh_interval(repo.spec.catalog.as_ref());
    if catalog::scan_due(
        repo.metadata.generation,
        repo.status.as_ref().and_then(|s| s.observed_generation),
        last_refresh_at(repo),
        interval,
        CatalogBounds::periodic_refresh_enabled(repo.spec.catalog.as_ref()),
        scan_requested_token(repo),
        scan_requested_honored(repo),
        chrono::Utc::now(),
    ) {
        run_catalog_scan(
            ctx,
            repo,
            namespace,
            name,
            repo_uid,
            &result.snapshots,
            result.snapshot_count,
            result.snapshots_truncated,
            result.foreign_suffix_dropped,
        )
        .await?;
    }

    // Ensure the managed Maintenance for this repo (ADR §3.7). Build on the
    // conditions we just patched (which include `Bootstrapped`), NOT the stale
    // cached object — otherwise this patch would drop the `Bootstrapped`
    // condition we set above (both writes replace the whole conditions array).
    // §11: a ReadOnly repository runs no maintenance — skip the projection.
    ensure_repo_maintenance(ctx, repo, namespace, name, api, &conditions).await;

    // A probe consumes its Job exactly once: delete it so the steady state has no
    // lingering finished Job to re-read (no churn) and the next probe is a fresh
    // connect. Requeue on the probe cadence (or the catalog cadence, whichever is
    // sooner) so the next probe fires on time even when `periodicRefresh` is off.
    if probe_run {
        delete_bootstrap_job(ctx, namespace, job_name).await?;
    }
    Ok(Action::requeue(probe_aware_reconcile_interval(repo)))
}

/// Health-probe failure on an already-`Ready` `Repository`: alert-only. Keeps the
/// phase `Ready`, folds the debounced `BackendReachable` condition + (on a
/// transition) a Warning event + metric, deletes the consumed Job, and requeues on
/// the probe cadence. NEVER changes the phase and NEVER auto-recreates.
async fn finalize_probe_failure(
    ctx: &Context,
    repo: &Repository,
    api: &Api<Repository>,
    name: &str,
    job_name: &str,
    failure: &io::BootstrapFailure,
) -> Result<Action> {
    let namespace = repo
        .metadata
        .namespace
        .as_deref()
        .unwrap_or_default()
        .to_string();
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
    // Phase stays `Ready` (set explicitly so a stray `Initializing`/`Degraded` from
    // an earlier path self-heals); the alert lives entirely in the condition.
    // Layer the kstatus Ready on top (issue #245) so its observedGeneration stays
    // current and a resumed-then-probe-flapping repo never shows a stale
    // `Suspended` Ready reason; the probe detail rides the `BackendReachable`
    // condition already folded into `upd.conditions`.
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
            // NOT `upd.health` directly: every field is `skip_serializing_if =
            // "Option::is_none"`, so the typed `None` for `probeAttemptAt` would be
            // ELIDED from this merge patch and the launch stamp would survive — leaving
            // the repo stuck `AwaitingResult` and making the next finished Job read as a
            // probe. `probe_failure_health_patch` emits the explicit RFC 7386 null.
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
            .inc_health_probe_failure(&namespace, name, "Repository", kind_label);
        if wrote {
            io::publish_warning_event(ctx, repo, w.reason, w.action, &w.message).await;
            tracing::warn!(repo = %name, reason = w.reason, "repository health probe raised an alert");
        }
    }
    delete_bootstrap_job(ctx, &namespace, job_name).await?;
    Ok(Action::requeue(probe_aware_reconcile_interval(repo)))
}

/// Delete a finished bootstrap Job + its result ConfigMap, tolerating a 404 (the
/// kube TTL controller may have reaped it first). Used to consume a probe Job
/// exactly once.
async fn delete_bootstrap_job(ctx: &Context, namespace: &str, job_name: &str) -> Result<()> {
    io::delete_mover_run(&ctx.client, namespace, job_name).await
}

/// The steady-state requeue for a `Repository`, shortened to the health-probe
/// cadence when the probe is enabled (so a sub-5m probe interval actually fires on
/// time, independent of `catalog.periodicRefresh`).
fn probe_aware_reconcile_interval(repo: &Repository) -> Duration {
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

/// Upsert the `Bootstrapped` condition onto the repository's existing conditions.
fn bootstrap_condition(
    repo: &Repository,
    status: bool,
    reason: &str,
    message: &str,
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
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

/// Run the shared catalog scan ([`crate::catalog::scan`]) for a namespaced
/// `Repository` and reflect the outcome: the discovered-row count +
/// `lastRefreshAt` stamp on `status.catalog`, the repository-wide snapshot count
/// on `storageStats`, and the size gauge.
///
/// `listing` is the snapshot set to reconcile against: produced in-process for
/// the bare-path filesystem backend, or carried back from the bootstrap Job for
/// everything else. `total_snapshot_count` is the authoritative repository-wide
/// count (may exceed `listing.len()` when the Job capped the returned entries —
/// `listing_truncated`, see `BootstrapResult::snapshots_truncated`).
/// `foreign_prefilter_dropped` is the count the mover's foreign-suffix prefilter
/// already dropped before this scan ever saw `listing` (`0` when the repository has
/// no cluster identity, or `catalog.foreignSnapshots` isn't `Ignore` — the prefilter
/// is armed only then, see `bootstrap_work_spec`).
#[allow(clippy::too_many_arguments)]
async fn run_catalog_scan(
    ctx: &Context,
    repo: &Repository,
    namespace: &str,
    repo_name: &str,
    repo_uid: &str,
    listing: &[SnapshotListEntry],
    total_snapshot_count: i64,
    listing_truncated: bool,
    foreign_prefilter_dropped: i64,
) -> Result<()> {
    let owner_ref = io::owner_ref_for(repo, "Repository")?;
    let cluster = repo
        .spec
        .identity_defaults
        .as_ref()
        .and_then(|d| d.cluster.as_deref());
    let outcome = catalog::scan(
        ctx,
        catalog::ScanOwner::Repository {
            name: repo_name,
            namespace,
        },
        owner_ref,
        repo_uid,
        catalog::Placement::Namespace(namespace),
        cluster,
        repo.spec.catalog.as_ref(),
        listing,
        listing_truncated,
    )
    .await?;
    let foreign_total = outcome.foreign + foreign_prefilter_dropped;

    // Logical bytes under management is recorded directly from kopia's data, both as
    // the metric gauge and as `storageStats.totalSizeBytes` (the integer form of the
    // human `total_size`), which backup preflight reads as `repository.sizeBytes`.
    let size_bytes = logical_bytes_under_management(listing);
    ctx.metrics
        .set_repo_size_bytes(namespace, repo_name, size_bytes);

    let api: Api<Repository> = Api::namespaced(ctx.client.clone(), namespace);
    let mut catalog_patch = serde_json::json!({
        "discoveredBackupCount": outcome.discovered,
        "lastRefreshAt": chrono::Utc::now().to_rfc3339(),
        "foreignSnapshotCount": foreign_total,
    });
    // Retire a pending `catalog-scan-requested-at` token: ANY completed scan (not
    // just one the token itself triggered) honors it, since the request was for
    // "materialize the catalog", not "run a scan for this specific reason".
    // `scanRequestAttemptAt` is deliberately left as-is — equality retirement
    // (`token == honored`) makes it inert from here on regardless of its value.
    if let Some(token) = scan_requested_token(repo) {
        catalog_patch["scanRequestHonored"] = serde_json::Value::String(token.to_string());
    }
    io::patch_status(
        &api,
        repo_name,
        serde_json::json!({
            "catalog": catalog_patch,
            "storageStats": { "snapshotCount": total_snapshot_count, "totalSizeBytes": size_bytes },
        }),
    )
    .await?;
    Ok(())
}

/// `error_policy` for the `Repository` controller.
pub fn error_policy(obj: Arc<Repository>, err: &Error, ctx: Arc<Context>) -> Action {
    error_policy_for("Repository", obj.as_ref(), err, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use kopiur_kopia::{SnapshotSource, SnapshotStats};

    fn entry(id: &str) -> SnapshotListEntry {
        SnapshotListEntry {
            id: id.into(),
            source: SnapshotSource {
                host: "h".into(),
                user_name: "u".into(),
                path: "/p".into(),
            },
            description: String::new(),
            start_time: Utc::now(),
            end_time: Utc::now(),
            stats: SnapshotStats::default(),
            root_entry: None,
            retention_reason: vec![],
            tags: Default::default(),
        }
    }

    fn entry_sized(
        id: &str,
        path: &str,
        end: chrono::DateTime<Utc>,
        size: u64,
    ) -> SnapshotListEntry {
        let mut e = entry(id);
        e.source.path = path.into();
        e.end_time = end;
        e.stats.total_size = size;
        e
    }

    #[test]
    fn logical_bytes_sums_newest_snapshot_per_source() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::seconds(10);
        let listing = vec![
            // Source /a: older 100, newer 150 → counts 150 (not 250).
            entry_sized("a-old", "/a", t0, 100),
            entry_sized("a-new", "/a", t1, 150),
            // Source /b: single snapshot 40.
            entry_sized("b", "/b", t0, 40),
        ];
        assert_eq!(logical_bytes_under_management(&listing), 190);
        assert_eq!(logical_bytes_under_management(&[]), 0);
    }

    #[test]
    fn bootstrap_work_spec_arms_the_prefilter_only_under_cluster_mode_and_ignore() {
        // M5: mirrors `cluster_repository`'s identical rule — the mover's
        // foreign-suffix prefilter is armed only when there IS a cluster identity
        // AND the effective policy is `Ignore` (never under `Fallback`, which needs
        // the entries back so the scan can count them).
        use kopiur_api::backend::FilesystemBackend;
        let backend = Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        });
        let prefilter = |cluster: Option<&str>, foreign: ForeignSnapshots| {
            let spec = bootstrap_work_spec(
                &backend, "nas", "billing", true, true, None, None, cluster, foreign, false, true,
                false, None,
            );
            match spec.operation {
                Operation::BootstrapRepository(op) => op.catalog_foreign_prefilter_cluster,
                other => panic!("expected BootstrapRepository, got {other:?}"),
            }
        };

        // No cluster identity: never armed, regardless of the foreign policy.
        assert_eq!(prefilter(None, ForeignSnapshots::Ignore), None);
        assert_eq!(prefilter(None, ForeignSnapshots::Fallback), None);
        // Cluster identity + Fallback: must not prefilter.
        assert_eq!(prefilter(Some("east"), ForeignSnapshots::Fallback), None);
        // Cluster identity + Ignore: armed with the cluster suffix.
        assert_eq!(
            prefilter(Some("east"), ForeignSnapshots::Ignore),
            Some("east".to_string())
        );
        // Empty-string cluster behaves like unset (matches classify_hostname's rule).
        assert_eq!(prefilter(Some(""), ForeignSnapshots::Ignore), None);
    }

    /// M6: the bootstrap work spec's maintenance-owner gating matrix. Column
    /// legend: (read_only, maintenance_enabled, foreign_maintenance, cluster).
    #[test]
    fn bootstrap_work_spec_maintenance_owner_gating_matrix() {
        use kopiur_api::backend::FilesystemBackend;
        use kopiur_mover::workspec::RestampPolicy;
        let backend = Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        });
        let build = |read_only: bool, enabled: bool, foreign_m: bool, cluster: Option<&str>| {
            let spec = bootstrap_work_spec(
                &backend,
                "nas",
                "billing",
                true,
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

        // Default (writable, enabled, no foreign, no cluster): stamp exactly
        // as before M6 — AnyStale, the legacy lease's owner, no aliases,
        // read-write connect.
        let op = build(false, true, false, None);
        assert_eq!(
            op.maintenance_owner.as_deref(),
            Some("kopiur@kopiur-billing-nas")
        );
        assert_eq!(op.restamp_policy, RestampPolicy::AnyStale);
        assert!(op.maintenance_owner_aliases.is_empty());
        assert!(!op.read_only);

        // Cluster set: cluster-qualified owner, OwnFormatsOnly, legacy alias.
        let op = build(false, true, false, Some("east"));
        assert_eq!(
            op.maintenance_owner.as_deref(),
            Some("kopiur@kopiur.east.billing.nas")
        );
        assert_eq!(op.restamp_policy, RestampPolicy::OwnFormatsOnly);
        assert_eq!(
            op.maintenance_owner_aliases,
            vec!["kopiur@kopiur-billing-nas".to_string()]
        );
        assert!(!op.read_only);

        // ReadOnly: NO owner stamp at all (the consumer-clobbers-owner fix)
        // AND a --readonly connect, with or without cluster.
        for cluster in [None, Some("east")] {
            let op = build(true, true, false, cluster);
            assert_eq!(op.maintenance_owner, None, "cluster={cluster:?}");
            assert!(op.read_only, "cluster={cluster:?}");
            assert!(
                op.maintenance_owner_aliases.is_empty(),
                "cluster={cluster:?}"
            );
        }

        // Maintenance disabled: no stamp, but still a read-write connect
        // (the repo itself is writable; only owner-stamping is opted out).
        let op = build(false, false, false, Some("east"));
        assert_eq!(op.maintenance_owner, None);
        assert!(!op.read_only);

        // Foreign (externally-authored) Maintenance covers the repo: its
        // ownership.owner has no relation to ours — never stamp.
        let op = build(false, true, true, None);
        assert_eq!(op.maintenance_owner, None);
        assert!(!op.read_only);
    }
}
