//! kopiur-mover: the per-`Snapshot`/`Restore` Job binary (ADR §4.10).
//!
//! Flow:
//! 1. Parse the CLI ([`MoverCli`]: `ready` / `serve [path]` / run-once with an
//!    optional work-spec path, env fallback), parse [`MoverWorkSpec`].
//! 2. Build a [`KopiaClient`], connect to the repository.
//! 3. Run the operation (backup / restore / snapshot-delete), emitting periodic
//!    progress PATCHes (interval configurable via the work spec).
//! 4. PATCH a terminal success/failure status onto the CR `.status` subresource
//!    via `kube::Api::patch_status`. On failure, write a structured failure
//!    block and exit non-zero.
//!
//! The pure mapping layer (work spec parsing, KopiaError → FailureBlock,
//! SnapshotCreateResult → status) lives in [`workspec`] and [`status`] and is
//! fully unit-tested without a cluster. The kube interaction here is
//! intentionally thin and best-effort.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser as _;
use kopiur_api::common::ResolvedIdentity;
use kopiur_api::snapshot::SnapshotInfo;
use kopiur_api::{LeaseAction, lease_action};
use kopiur_kopia::{
    ConnectSpec, KopiaClient, KopiaError, KopiaErrorClass, SnapshotSource, filter_as_of,
    pick_offset,
};
use tracing::{error, info, warn};

use kopiur_mover::bootstrap::{
    BootstrapResult, MAX_RETURNED_SNAPSHOTS, RESULT_CONFIGMAP_KEY, should_attempt_create,
};
use kopiur_mover::cli::{MoverCli, MoverCommand};
use kopiur_mover::credentials;
use kopiur_mover::env::{RESULT_CONFIGMAP, WORK_SPEC_PATH};
use kopiur_mover::error::{KopiaOp, MoverError, Result};
use kopiur_mover::resolve::{match_current_manifest, matches_source};
use kopiur_mover::serve::ServerWorkSpec;
use kopiur_mover::status::{
    StatusReporter, StatusUpdate, lease_blocked_body, maintenance_failed_body,
    maintenance_ran_body, replicate_failed_body, replicate_ok_body, split_api_version,
    verify_failed_body, verify_ok_body,
};
use kopiur_mover::workspec::{
    self, BootstrapRepositoryOp, BrowseSessionOp, KOPIA_KEEP_MAX, KOPIUR_PIN_NAME, MaintenanceOp,
    MoverWorkSpec, Operation, ReplicateOp, RestoreOp, RestoreSelection, RestoreSelector,
    SnapshotAnchor, SnapshotDeleteBatchOp, SnapshotPinOp, VerifyOp, VerifyTier,
    maintenance_restamp_target,
};
#[cfg(test)]
use kopiur_mover::workspec::{SnapshotDeleteItem, SnapshotDeleteOp};

fn main() -> std::process::ExitCode {
    let cli = MoverCli::parse();
    match &cli.command {
        // Readiness-probe mode, BEFORE the work-spec loading path: a
        // browse-session pod's readinessProbe execs `kopiur-mover ready` (the
        // distroless image has no shell to `test -f` with), which must exit 0
        // iff the session marker exists. The decision itself is the pure
        // `session_ready`.
        Some(MoverCommand::Ready) => {
            if session_ready(std::path::Path::new(kopiur_mover::env::READY_MARKER)) {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            }
        }
        // `mover serve [path]` runs the long-lived kopia web UI. The serve path
        // connects then `exec`s kopia, replacing this process, so it never
        // returns on success.
        Some(MoverCommand::Serve { spec }) => run_serve(spec.clone(), cli.kopia_binary()),
        // No subcommand: a run-once operation, selected by the work-spec JSON.
        None => {
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
            match runtime.block_on(run(&cli)) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    error!(error = %e, "mover run failed");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

/// The `serve` entrypoint: connect the repository, then `exec` `kopia server start`.
///
/// On success `exec` replaces this process with kopia, so this never returns; it
/// returns a non-zero `ExitCode` only if loading/connecting/exec fails.
fn run_serve(spec_arg: Option<PathBuf>, kopia_binary: Option<&str>) -> std::process::ExitCode {
    let _telemetry = match kopiur_telemetry::init_tracing("kopiur-mover") {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to init tracing: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    let spec = match server_spec_path(spec_arg).and_then(|p| load_server_spec(&p)) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "loading server work spec");
            return std::process::ExitCode::FAILURE;
        }
    };
    info!(
        repository = spec.repository.kind_str(),
        port = spec.listen_port,
        auth = spec.auth.kind_str(),
        read_only = spec.read_only,
        "loaded server work spec"
    );

    let client = build_serve_client(kopia_binary);

    // Connect to the repository first (short, idempotent) so the server can read
    // the connected repo from the kopia config file. Cache tuning is inherited from
    // the pod environment (KOPIA_CACHE_DIRECTORY), so the default is correct here.
    //
    // When `read_only` is set, connect with `--readonly`: the read-only bit persists in
    // the kopia config, so the long-lived `server start` that follows (and everything the
    // UI does through it) is structurally unable to mutate the repository. kopia 0.23 has
    // no server-level read-only flag, so this connection-level bit is the mechanism.
    let cache = kopiur_kopia::CacheTuning::default();
    let connect_spec = spec.repository.to_connect_spec();
    let connect = if spec.read_only {
        runtime.block_on(client.repository_connect_readonly(&connect_spec, cache))
    } else {
        runtime.block_on(client.repository_connect(&connect_spec, cache))
    };
    if let Err(e) = connect {
        error!(error = %e, read_only = spec.read_only, "repository connect failed before server start");
        return std::process::ExitCode::FAILURE;
    }
    // Drop the tokio runtime before exec — kopia takes over this process entirely.
    drop(runtime);

    let password = std::env::var(kopiur_mover::env::SERVER_PASSWORD).ok();
    info!("starting kopia server (exec)");
    // Returns ONLY on exec failure.
    let err = client.server_start(&spec.to_start_spec(), password.as_deref());
    error!(error = %err, "exec kopia server start failed");
    std::process::ExitCode::FAILURE
}

/// Locate the server work spec: `mover serve <path>` arg, else
/// [`env::SERVER_SPEC_PATH`]. The env fallback is deliberately manual (not
/// clap `#[arg(env)]`) — see [`kopiur_mover::cli`].
fn server_spec_path(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(arg) = arg {
        return Ok(arg);
    }
    if let Ok(env) = std::env::var(kopiur_mover::env::SERVER_SPEC_PATH) {
        return Ok(PathBuf::from(env));
    }
    Err(MoverError::ServerSpecPathMissing)
}

fn load_server_spec(path: &PathBuf) -> Result<ServerWorkSpec> {
    let raw = std::fs::read_to_string(path).map_err(|source| MoverError::ServerSpecRead {
        path: path.clone(),
        source,
    })?;
    let spec: ServerWorkSpec =
        serde_json::from_str(&raw).map_err(|source| MoverError::ServerSpecParse {
            path: path.clone(),
            source,
        })?;
    Ok(spec)
}

/// Build a kopia client for the serve path. Repository/UI credentials, config and
/// cache dirs are inherited from the pod environment (mounted Secret + emptyDir
/// env), so only the binary override and the update-check suppression are set here.
fn build_serve_client(kopia_binary: Option<&str>) -> KopiaClient {
    let mut builder = KopiaClient::builder();
    if let Some(bin) = kopia_binary {
        builder = builder.binary(bin);
    }
    builder = builder.env("KOPIA_CHECK_FOR_UPDATES", "false");
    builder.build()
}

/// Whether the browse-session readiness marker exists — the entire decision the
/// `kopiur-mover ready` probe mode maps to an exit code. Pure over the path so
/// it is unit-testable.
fn session_ready(marker: &std::path::Path) -> bool {
    marker.exists()
}

async fn run(cli: &MoverCli) -> Result<()> {
    // Tracing subscriber (fmt + OTLP traces/logs when configured). The mover is a
    // short-lived Job, so OTLP push is the right model for its metrics — we flush
    // both before returning.
    let _telemetry = kopiur_telemetry::init_tracing("kopiur-mover")?;
    let metrics = MoverMetrics::new();

    // Install the process-level rustls CryptoProvider before building any kube
    // client (the rustls-tls backend panics without it). Idempotent.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let spec = resolve_work_spec(cli.work_spec.clone())?;
    let operation = spec.operation.kind_str().to_string();
    info!(
        operation = %operation,
        target = %spec.target_ref.name,
        namespace = %spec.target_ref.namespace,
        "loaded work spec"
    );

    let client = build_client(&spec, cli.kopia_binary());

    let started = std::time::Instant::now();
    // Build the connect spec once, materializing any file-based backend
    // credentials (SFTP key/known_hosts, GCS service-account JSON, rclone.conf)
    // from the environment into files the kopia subprocess can read. Every flow
    // below connects with this prepared spec.
    let result = match prepare_connect_spec(&spec) {
        Err(e) => {
            error!(error = %e, "failed to materialize backend credentials for the mover");
            Err(e)
        }
        // Bootstrap owns its own connect/create lifecycle (and reports via a
        // result ConfigMap, not the CR status); every other operation connects
        // first, then runs with periodic progress PATCHes.
        Ok(connect) => match &spec.operation {
            Operation::BootstrapRepository(op) => {
                run_bootstrap_flow(&client, &spec, op, &connect, cli.result_configmap()).await
            }
            // Maintenance, like bootstrap, owns its own connect lifecycle: the
            // lease decision needs `kopia maintenance info`, which requires repo
            // access the controller does not have for object stores (ADR §3.7/§5.4).
            Operation::Maintenance(op) => run_maintenance_flow(&client, &spec, op, &connect).await,
            // Verify, like maintenance, owns its own connect lifecycle and PATCHes
            // the SnapshotPolicy `.status` directly (ADR-0005 §4).
            Operation::Verify(op) => run_verify_flow(&client, &spec, op, &connect).await,
            // Replicate connects to the source, then `repository sync-to` the
            // destination; PATCHes the RepositoryReplication `.status` (ADR-0005 §13(d)).
            Operation::Replicate(op) => run_replicate_flow(&client, &spec, op, &connect).await,
            // BrowseSession owns its own (read-only) connect lifecycle and has
            // no status to PATCH — its targetRef names nothing the controller
            // owns; the CLI surfaces failures from the pod logs.
            Operation::BrowseSession(op) => {
                run_browse_session_flow(&client, &spec, op, &connect).await
            }
            _ => {
                // A best-effort status reporter. If we cannot build a kube client
                // (e.g. running outside a cluster), we log instead of failing.
                // SnapshotDeleteBatch's targetRef names the Job itself — nothing
                // the controller owns — and the mover is deliberately NOT granted
                // RBAC to PATCH it, so hand-select the log-only reporter for it:
                // a PATCH must never even be attempted, not just best-effort
                // degraded (this selection is hand-wired, not compiler-enforced —
                // see `wants_log_only_reporter`).
                let reporter = if wants_log_only_reporter(&spec.operation) {
                    StatusReporter::log_only(spec.target_ref.clone())
                } else {
                    StatusReporter::try_new(&spec).await
                };
                match client.repository_connect(&connect, spec.cache).await {
                    Err(e) => {
                        terminal_failure(
                            &reporter,
                            MoverError::Kopia {
                                op: KopiaOp::RepositoryConnect,
                                source: e,
                            },
                        )
                        .await
                    }
                    Ok(()) => {
                        // Apply repository throttle (moverDefaults.throttle, ADR-0005
                        // §13(e)) after connecting, before the data op. A throttle
                        // failure is terminal: an un-throttled run could saturate the
                        // link the user explicitly capped.
                        if !spec.throttle.is_empty()
                            && let Err(e) = client
                                .repository_throttle_set(&spec.throttle.to_kopia())
                                .await
                        {
                            return terminal_failure(
                                &reporter,
                                MoverError::Kopia {
                                    op: KopiaOp::ThrottleSet,
                                    source: e,
                                },
                            )
                            .await;
                        }
                        match execute(&client, &spec, &reporter).await {
                            Ok(update) => {
                                reporter.report(&update).await;
                                info!(
                                    phase = update.phase.as_deref().unwrap_or("done"),
                                    "operation succeeded"
                                );
                                Ok(())
                            }
                            Err(e) => terminal_failure(&reporter, e).await,
                        }
                    }
                }
            }
        },
    };

    // Push the operation outcome metric, then flush OTLP before the Job exits.
    let outcome = if result.is_ok() {
        "succeeded"
    } else {
        "failed"
    };
    metrics.record(&operation, outcome, started.elapsed().as_secs_f64());
    metrics.shutdown();

    result
}

/// Whether `run()`'s generic connect+execute path must hand-select the
/// log-only status reporter instead of `StatusReporter::try_new`'s
/// best-effort kube-client attempt. There is no compiler-enforced link
/// between an [`Operation`] variant and its reporter (unlike the exhaustive
/// `run_operation`/`kind_str` matches), so this hand-wired selection is
/// pulled out to be unit-testable on its own.
///
/// Only [`Operation::SnapshotDeleteBatch`] wants this: its `targetRef` names
/// the Job itself (nothing the controller owns), and the mover is
/// deliberately not granted RBAC to PATCH it.
fn wants_log_only_reporter(op: &Operation) -> bool {
    matches!(op, Operation::SnapshotDeleteBatch(_))
}

/// Directory under the writable kopia-cache `emptyDir` where the mover stages
/// file-based backend credentials (SFTP key/known_hosts, GCS JSON, rclone.conf).
/// Shares the cache mount so it is writable on the read-only-root mover pod.
fn credential_staging_dir() -> PathBuf {
    PathBuf::from(kopiur_kopia::env::DEFAULT_CACHE_DIR).join("creds")
}

/// Build the repository [`ConnectSpec`] for this run, first materializing any
/// file-based backend credentials (SFTP/GCS/rclone) from the environment into
/// files under [`credential_staging_dir`]. Env-only backends (S3/Azure/B2/WebDAV)
/// pass through unchanged.
fn prepare_connect_spec(spec: &MoverWorkSpec) -> Result<ConnectSpec> {
    let mut connect = spec.repository.to_connect_spec();
    credentials::materialize(&mut connect, &credential_staging_dir())?;
    Ok(connect)
}

/// Execute the work-spec operation, emitting periodic "Running" updates while
/// kopia works. Returns the terminal success update or the kopia error.
async fn execute(
    client: &KopiaClient,
    spec: &MoverWorkSpec,
    reporter: &StatusReporter,
) -> Result<StatusUpdate> {
    let interval = Duration::from_secs(spec.options.progress_interval_secs.max(1));

    // Spawn the operation as a future and tick progress alongside it.
    let op = run_operation(client, spec, reporter);
    tokio::pin!(op);

    let mut ticker = tokio::time::interval(interval);
    // First tick fires immediately; skip reporting until the period elapses.
    ticker.tick().await;

    loop {
        tokio::select! {
            result = &mut op => return result,
            _ = ticker.tick() => {
                // A phase-less heartbeat: the controller owns every in-flight
                // phase (and never reads the mover's), so asserting one here only
                // risks a value the target CR's enum forbids — the Restore
                // "Running" 422 this replaced.
                reporter
                    .report(&StatusUpdate::progress(chrono::Utc::now()))
                    .await;
            }
        }
    }
}

/// Dispatch on the operation kind. Exhaustive `match` — a new [`Operation`]
/// variant fails to compile until handled (the project's type-safety thesis).
async fn run_operation(
    client: &KopiaClient,
    spec: &MoverWorkSpec,
    reporter: &StatusReporter,
) -> Result<StatusUpdate> {
    // Each kopia call is wrapped with the `KopiaOp` naming it, so a failure's
    // message/log always says *which* invocation failed.
    let kopia = |op: KopiaOp| move |source: KopiaError| MoverError::Kopia { op, source };
    match &spec.operation {
        Operation::Snapshot(op) => {
            // Record the snapshot under the operator-resolved identity
            // (`username@hostname:sourcePath`), not the mover pod's ambient
            // user/host — ADR §4.2. The catalog, retention, and restore paths
            // all key on this identity.
            let id = &spec.identity;
            let override_source = format!("{}@{}:{}", id.username, id.hostname, id.source_path);
            let identity_scope = format!("{}@{}", id.username, id.hostname);
            // Apply the resolved kopia `policy set` knobs (compression / never-compress
            // / ignore rules / ignore-cache-dirs / backup-side error handling / upload
            // parallelism / extraArgs) against this snapshot's source identity BEFORE
            // creating the snapshot, so SnapshotPolicy.spec.{compression,files,
            // errorHandling,upload,extraArgs} actually reach kopia (ADR-0005 §13(b)/§13(f),
            // ADR-0004 §4b). The path-scoped `policy set` is skipped when nothing is
            // configured, but the identity-scoped one below ALWAYS runs — it also
            // carries the mandatory `KOPIA_KEEP_MAX` retention pin (see its doc
            // comment): kopia's `snapshot create` unconditionally applies the
            // source's OWN retention policy after every create, so an unset
            // identity policy would otherwise fall back to kopia's defaults
            // (keep-latest 10, …), silently deleting manifests a Kopiur `Snapshot`
            // CR still references.
            let mut user_identity_policy = None;
            if !op.policy.is_empty() {
                // kopia rejects `--max-parallel-snapshots` on a path-scoped
                // policy ("only global, username@hostname or @hostname"), so
                // that one knob is folded into the identity-scope policy_set
                // below instead of the path-scoped one (the policy_knobs e2e
                // regression).
                let (path_policy, split_identity_policy) =
                    kopiur_kopia::split_policy_scopes(op.policy.to_kopia());
                client
                    .policy_set(&override_source, &path_policy)
                    .await
                    .map_err(kopia(KopiaOp::PolicySet))?;
                user_identity_policy = split_identity_policy;
            }
            client
                .policy_set(
                    &identity_scope,
                    &identity_retention_policy(user_identity_policy),
                )
                .await
                .map_err(kopia(KopiaOp::PolicySet))?;
            let result = client
                .snapshot_create_with(
                    &op.source_path,
                    &op.tags,
                    Some(&override_source),
                    &op.create_options(),
                )
                .await
                .map_err(kopia(KopiaOp::SnapshotCreate))?;
            // kopia exits non-zero (→ a classified `PermissionDenied` failure above) when
            // unreadable files are FATAL. But under an `ignoreFileErrors`/`ignoreDirErrors`
            // policy it completes (exit 0) while still recording every skipped entry in
            // `rootEntry.summ.errors` — an otherwise-SILENT incomplete backup. Surface it:
            // the count rides on `status.stats.filesFailed` (set in `succeeded_backup`) and
            // the controller raises a warning condition + Event; log it here too.
            let skipped = result.entry_errors();
            if !skipped.is_empty() {
                warn!(
                    skipped = skipped.len(),
                    sample_path = %skipped[0].path,
                    sample_error = %skipped[0].error,
                    "backup completed but {} source entr{} unreadable and EXCLUDED from the \
                     snapshot (ignore-file-errors policy) — it is INCOMPLETE; match the mover to \
                     the workload via mover.inheritSecurityContextFrom.pvcConsumer or a matching \
                     runAsUser to capture them",
                    skipped.len(),
                    if skipped.len() == 1 { "y was" } else { "ies were" },
                );
            }
            Ok(StatusUpdate::succeeded_backup(&result, chrono::Utc::now()))
        }
        Operation::Restore(op) => {
            // Exactly one source kind (externally tagged): a controller-resolved id,
            // or an in-Job selector to resolve here. Exhaustive — a new variant
            // can't compile until handled.
            match &op.source {
                RestoreSelection::Snapshot(id) => {
                    // The recorded id can be STALE — kopia rewrites a snapshot's
                    // manifest id on pin (`UpdateSnapshot`), so a snapshotRef/identity
                    // restore of a snapshot pinned before this fix points at a deleted
                    // manifest. On a not-found, self-heal by re-resolving the live id
                    // from the snapshot's stable identity (source path + start time).
                    let restored_id = restore_with_heal(client, op, id)
                        .await
                        .map_err(kopia(KopiaOp::SnapshotRestore))?;
                    // Restore's terminal success phase is `Completed`, not `Succeeded`
                    // (the Snapshot phase) — the Restore CRD enum rejects `Succeeded`.
                    Ok(StatusUpdate::completed(&restored_id, chrono::Utc::now()))
                }
                // Object-store `fromPolicy`/`identity`-without-id: the controller
                // can't list the backend in-process, so resolve "latest" (offset/asOf)
                // here, where the mover reaches every backend.
                RestoreSelection::Resolve(sel) => {
                    resolve_and_restore(client, op, sel, reporter).await
                }
            }
        }
        Operation::SnapshotDelete(op) => {
            // Delete the recorded snapshot, self-healing a stale id via its
            // anchor. Space reclamation (maintenance) is a separate concern
            // owned by the Maintenance CRD, not the mover.
            delete_one(client, &op.snapshot_id, &op.anchor)
                .await
                .map_err(kopia(KopiaOp::SnapshotDelete))?;
            Ok(StatusUpdate::succeeded(chrono::Utc::now()))
        }
        Operation::SnapshotDeleteBatch(op) => delete_batch(client, op).await,
        Operation::SnapshotPin(op) => {
            // Reconcile kopia's pin state with Snapshot.spec.pin (ADR-0005 §13(c))
            // so kopia's own maintenance/expire honors the pin on object stores.
            // Idempotent: kopia treats a redundant add/remove as a no-op.
            //
            // kopia's pin/unpin REWRITES the manifest id (`UpdateSnapshot` saves a
            // new manifest, deletes the old), so after the op we re-resolve the
            // CURRENT id and report it back — otherwise status.snapshot.kopiaSnapshotID
            // is left pointing at a deleted manifest (breaking snapshotRef restore
            // and the finalizer delete). The start-time anchor is captured BEFORE
            // the pin when the work spec didn't carry one, since the old id is gone
            // afterward.
            let anchor_start = pin_start_anchor(client, op).await;
            if op.pin {
                client
                    .snapshot_pin(&op.snapshot_id, KOPIUR_PIN_NAME)
                    .await
                    .map_err(kopia(KopiaOp::SnapshotPin))?;
            } else {
                client
                    .snapshot_unpin(&op.snapshot_id, KOPIUR_PIN_NAME)
                    .await
                    .map_err(kopia(KopiaOp::SnapshotPin))?;
            }
            match resolve_pinned_info(client, op, &spec.identity.source_path, anchor_start).await {
                Some(info) => Ok(StatusUpdate::succeeded_pin(info, chrono::Utc::now())),
                // Re-resolution was inconclusive (ambiguous match / list failed).
                // The (un)pin itself SUCCEEDED, so never regress to a failure —
                // just leave status.snapshot untouched and warn.
                None => {
                    warn!(
                        snapshot_id = %op.snapshot_id,
                        "snapshot (un)pin succeeded but the new manifest id could not be \
                         re-resolved; status.snapshot.kopiaSnapshotID may be stale until the \
                         next pin reconcile",
                    );
                    Ok(StatusUpdate::succeeded(chrono::Utc::now()))
                }
            }
        }
        // Bootstrap, Maintenance, and Verify are dispatched in `run()` before the
        // connect+execute path; they own their own connect lifecycle and never
        // reach here. Named explicitly (not `_`) so a future Operation variant
        // still fails to compile until handled (ADR §5.5).
        Operation::BootstrapRepository(_) => {
            unreachable!("BootstrapRepository is handled by run_bootstrap_flow, not execute()")
        }
        Operation::Maintenance(_) => {
            unreachable!("Maintenance is handled by run_maintenance_flow, not execute()")
        }
        Operation::Verify(_) => {
            unreachable!("Verify is handled by run_verify_flow, not execute()")
        }
        Operation::Replicate(_) => {
            unreachable!("Replicate is handled by run_replicate_flow, not execute()")
        }
        Operation::BrowseSession(_) => {
            unreachable!("BrowseSession is handled by run_browse_session_flow, not execute()")
        }
    }
}

/// Restore `snapshot_id`, self-healing a stale id. kopia rewrites a snapshot's
/// manifest id on pin (`UpdateSnapshot`), so a snapshotRef/identity restore of a
/// pinned snapshot created before this fix can name a deleted manifest. On a
/// `NotFound`, re-resolve the live id from the snapshot's stable anchors
/// ([`RestoreOp::anchor`]) and retry once. Returns the id actually restored (for
/// `status.logTail`).
async fn restore_with_heal(
    client: &KopiaClient,
    op: &RestoreOp,
    snapshot_id: &str,
) -> std::result::Result<String, KopiaError> {
    match client
        .snapshot_restore_with(snapshot_id, &op.target_path, &op.restore_options())
        .await
    {
        Ok(()) => Ok(snapshot_id.to_string()),
        Err(e) if e.class() == KopiaErrorClass::NotFound => {
            // Only heal when we have anchors AND they resolve to a DIFFERENT live
            // id; otherwise the original not-found is the truthful error.
            if let Some(live) = resolve_live_id(client, &op.anchor).await
                && live != snapshot_id
            {
                warn!(
                    stale = %snapshot_id,
                    live = %live,
                    "restore snapshot id not found; healing to the live manifest \
                     re-resolved from the snapshot's identity (kopia rewrites the id on pin)",
                );
                client
                    .snapshot_restore_with(&live, &op.target_path, &op.restore_options())
                    .await?;
                return Ok(live);
            }
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// Resolve an object-store restore source in-Job and restore it. The controller
/// can only list filesystem repos in-process, so `fromPolicy`/`identity`-without-id
/// defer the listing here, where the mover reaches every backend.
///
/// Determinism + durability: a snapshot a PRIOR pod attempt already pinned to
/// `status.resolved` is reused verbatim (so a Job retry never re-resolves to a
/// different "latest"); a fresh resolution is pinned BEFORE the restore runs (so
/// the choice survives a later terminal-PATCH failure and the controller adopts
/// it as the pin-once record). When nothing matches yet, re-list until the
/// `waitTimeout` deadline (an absolute instant the controller anchored at the
/// Restore's creation) passes, then apply `onMissingSnapshot` — `Continue` leaves
/// the target empty (deploy-or-restore), `Fail` errors.
async fn resolve_and_restore(
    client: &KopiaClient,
    op: &RestoreOp,
    sel: &RestoreSelector,
    reporter: &StatusReporter,
) -> Result<StatusUpdate> {
    use kopiur_api::restore::{ResolutionOutcome, ResolvedRestore};

    let filter = SnapshotSource {
        host: sel.hostname.clone(),
        user_name: sel.username.clone(),
        path: sel.source_path.clone().unwrap_or_default(),
    };

    // Reuse a snapshot a prior attempt of THIS Job already pinned, so a pod retry
    // restores the same id rather than re-resolving "latest" to a snapshot that
    // appeared since (the controller never pins the deferred path, so a non-empty
    // resolved here can only be the mover's own pre-restore pin).
    if let Some(prior) = reporter.resolved().await
        && let Some(id) = prior.kopia_snapshot_id.as_deref()
    {
        info!(
            snapshot = %id,
            identity = %filter.identity(),
            "reusing the snapshot pinned by a prior attempt; restoring",
        );
        let restored_id =
            restore_with_heal(client, op, id)
                .await
                .map_err(|source| MoverError::Kopia {
                    op: KopiaOp::SnapshotRestore,
                    source,
                })?;
        let identity = prior.identity.unwrap_or_else(|| ResolvedIdentity {
            username: sel.username.clone(),
            hostname: sel.hostname.clone(),
            source_path: sel.source_path.clone(),
        });
        return Ok(StatusUpdate::completed_resolved(
            &restored_id,
            identity,
            chrono::Utc::now(),
        ));
    }

    // The webhook validates `asOf` at admission; re-parse defensively here.
    let cutoff = match sel.as_of.as_deref() {
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| MoverError::RestoreAsOfInvalid {
                    as_of: s.to_string(),
                    message: e.to_string(),
                })?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };
    // Absolute wall-clock deadline (anchored at the Restore's creation by the
    // controller), so the wait matches the snapshotRef path and is stable across
    // pod restarts. Defensive parse; unparseable ⇒ resolve once, no wait.
    let deadline = sel
        .wait_deadline
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc));
    const POLL: Duration = Duration::from_secs(10);

    loop {
        let mut list = client
            .snapshot_list(Some(&filter))
            .await
            .map_err(|source| MoverError::Kopia {
                op: KopiaOp::RestoreSnapshotList,
                source,
            })?;
        // Newest-first, then point-in-time filter, then offset (0 = latest).
        list.sort_by_key(|e| std::cmp::Reverse(e.end_time));
        let candidates = filter_as_of(list, cutoff);
        if let Some(entry) = pick_offset(candidates, sel.offset) {
            info!(
                snapshot = %entry.id,
                identity = %filter.identity(),
                offset = sel.offset,
                "resolved restore source to a snapshot; restoring",
            );
            let identity = ResolvedIdentity {
                username: entry.source.user_name.clone(),
                hostname: entry.source.host.clone(),
                source_path: Some(entry.source.path.clone()),
            };
            // Pin the choice BEFORE restoring: a retry reuses it (above), the
            // controller adopts it as pin-once, and it survives a failed terminal
            // PATCH (best-effort; logged on failure).
            reporter
                .pin_resolved(&ResolvedRestore {
                    resolution: Some(ResolutionOutcome::Snapshot),
                    kopia_snapshot_id: Some(entry.id.clone()),
                    identity: Some(identity.clone()),
                    pinned_at: Some(chrono::Utc::now().to_rfc3339()),
                    ..Default::default()
                })
                .await;
            let restored_id = restore_with_heal(client, op, &entry.id)
                .await
                .map_err(|source| MoverError::Kopia {
                    op: KopiaOp::SnapshotRestore,
                    source,
                })?;
            return Ok(StatusUpdate::completed_resolved(
                &restored_id,
                identity,
                chrono::Utc::now(),
            ));
        }
        // No match yet: keep waiting until the deadline truly passes. Sleep the
        // lesser of POLL and the time left, so a sub-POLL window still waits (not
        // zero) and a longer one isn't cut short by up to POLL.
        match deadline {
            Some(d) => {
                let now = chrono::Utc::now();
                if now >= d {
                    break;
                }
                let remaining = (d - now).to_std().unwrap_or(POLL).min(POLL);
                info!(
                    identity = %filter.identity(),
                    "no snapshot matched the restore source yet; re-listing after a short wait",
                );
                tokio::time::sleep(remaining).await;
            }
            None => break,
        }
    }
    // Window closed (or no wait configured): honor onMissingSnapshot exhaustively.
    match sel.on_missing {
        kopiur_api::restore::OnMissingSnapshot::Continue => {
            info!(
                identity = %filter.identity(),
                "no snapshot matched the restore source; onMissingSnapshot=Continue — \
                 leaving the target empty (deploy-or-restore)",
            );
            // Pin the empty outcome before completing, for the same durability/
            // adoption reasons as the snapshot case.
            reporter
                .pin_resolved(&ResolvedRestore {
                    resolution: Some(ResolutionOutcome::NoSnapshot),
                    pinned_at: Some(chrono::Utc::now().to_rfc3339()),
                    ..Default::default()
                })
                .await;
            Ok(StatusUpdate::completed_empty(chrono::Utc::now()))
        }
        kopiur_api::restore::OnMissingSnapshot::Fail => Err(MoverError::RestoreNoSnapshot {
            identity: filter.identity(),
        }),
    }
}

/// Re-resolve the CURRENT live manifest id for a snapshot from its stable
/// anchors (source path + start time), via a fresh `snapshot list`. Returns
/// `None` when there are no usable anchors, the list fails, or the match is
/// ambiguous (so callers never act on the wrong snapshot).
async fn resolve_live_id(client: &KopiaClient, anchor: &SnapshotAnchor) -> Option<String> {
    if anchor.source_path.is_empty() {
        return None;
    }
    let list = client.snapshot_list(None).await.ok()?;
    match_current_manifest(
        &list,
        &anchor.source_path,
        anchor.start_instant(),
        anchor.identity_filter(),
    )
    .map(|e| e.id.clone())
}

/// Whether [`delete_one`]'s stale-id self-heal may attempt re-resolution.
/// Gated on the anchor's `start_time` alone — see [`delete_one`]'s doc for why
/// a path(+identity)-only match is unsafe for a DELETE decision. Pulled out so
/// the gate is unit-testable without spawning kopia.
fn anchor_self_heal_allowed(anchor: &SnapshotAnchor) -> bool {
    anchor.start_time.is_some()
}

/// Delete one snapshot by id, self-healing a stale recorded id via its stable
/// anchor. Shared by the legacy single [`Operation::SnapshotDelete`] arm and
/// the [`Operation::SnapshotDeleteBatch`] loop ([`delete_batch`]), so the
/// self-heal logic — and its safety gate — lives in exactly one place.
///
/// kopia's [`KopiaClient::snapshot_delete`] is idempotent (a "no snapshots
/// matched" miss is tolerated), so a delete-by-id call against a STALE id
/// (kopia rewrites the manifest id on pin) silently no-ops — orphaning the
/// live pinned manifest under `deletionPolicy: Delete`. When the gate below is
/// open, the recorded id's live manifest is re-resolved from the snapshot's
/// stable source path (+ identity, + start time) and deleted too.
///
/// **Anchor-safety gate (data-loss fix, adversarial review):** the self-heal
/// is attempted ONLY when [`anchor_self_heal_allowed`] — i.e. the anchor
/// carries a `start_time`. Without that disambiguator,
/// [`crate::resolve::match_current_manifest`]'s path(+identity)-only fallback
/// can uniquely match a NEWER, unrelated snapshot at the same source
/// path/identity (e.g. the same identity's very next backup) — deleting data
/// that was never targeted. When the gate is closed, only the recorded id is
/// deleted; the self-heal is skipped and logged.
async fn delete_one(
    client: &KopiaClient,
    snapshot_id: &str,
    anchor: &SnapshotAnchor,
) -> std::result::Result<(), KopiaError> {
    client.snapshot_delete(snapshot_id).await?;
    if !anchor_self_heal_allowed(anchor) {
        if !anchor.source_path.is_empty() {
            info!(
                snapshot_id = %snapshot_id,
                "anchor has no start_time; skipping the stale-id self-heal to avoid \
                 deleting an unrelated snapshot that happens to share the source path \
                 (data-loss gate)",
            );
        }
        return Ok(());
    }
    if let Some(live) = resolve_live_id(client, anchor).await
        && live != snapshot_id
    {
        warn!(
            recorded = %snapshot_id,
            live = %live,
            "recorded snapshot id was stale (kopia rewrites the id on pin); \
             deleting the live manifest re-resolved from the snapshot's identity \
             to avoid orphaning it",
        );
        client.snapshot_delete(&live).await?;
    }
    Ok(())
}

/// Delete every [`SnapshotDeleteBatchOp`] member independently: attempt-all-
/// then-fail, never short-circuited by an earlier member's failure. kopia's
/// delete is idempotent, so every retry of the WHOLE batch monotonically
/// shrinks the truly-remaining set — a transient repo blip mid-batch
/// converges on the next Job retry instead of wedging on the first failure.
/// Pulled out of [`run_operation`] so the attempt-all-then-fail ordering is
/// unit-testable against a fake kopia binary without a full work spec /
/// reporter.
async fn delete_batch(client: &KopiaClient, op: &SnapshotDeleteBatchOp) -> Result<StatusUpdate> {
    let mut failed = 0usize;
    for item in &op.items {
        if let Err(e) = delete_one(client, &item.snapshot_id, &item.anchor).await {
            warn!(id = %item.snapshot_id, error = %e, "batch member delete failed; continuing");
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(MoverError::BatchDeleteIncomplete {
            failed,
            total: op.items.len(),
        });
    }
    Ok(StatusUpdate::succeeded(chrono::Utc::now()))
}

/// Capture the start-time anchor for a pin op BEFORE the (un)pin runs. The work
/// spec normally carries it (`status.timing.startTime`); when it doesn't (older
/// CRs), look it up from the still-present pre-pin manifest id, since the id is
/// deleted once the pin rewrites the manifest.
async fn pin_start_anchor(
    client: &KopiaClient,
    op: &SnapshotPinOp,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(t) = op.anchor.start_instant() {
        return Some(t);
    }
    let list = client.snapshot_list(None).await.ok()?;
    list.iter()
        .find(|e| e.id == op.snapshot_id)
        .map(|e| e.start_time)
}

/// After a pin/unpin, re-resolve the snapshot's CURRENT manifest id (kopia
/// rewrote it) into a `SnapshotInfo` for `status.snapshot`. Matches on the
/// snapshot's stable source path (anchor, else the resolved identity) plus the
/// pre-pin start-time anchor, narrowed by `op.anchor.identity_filter()`
/// (`username`/`hostname`) when the anchor carries one — required so a shared
/// repository never cross-matches another namespace's or cluster's snapshot
/// that happens to share the exact same source path (see
/// `kopiur_mover::resolve::match_current_manifest`). `None` when unresolvable
/// or ambiguous.
async fn resolve_pinned_info(
    client: &KopiaClient,
    op: &SnapshotPinOp,
    fallback_source_path: &str,
    start: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<SnapshotInfo> {
    let source_path = if op.anchor.source_path.is_empty() {
        fallback_source_path.to_string()
    } else {
        op.anchor.source_path.clone()
    };
    let list = client.snapshot_list(None).await.ok()?;
    let entry = match_current_manifest(&list, &source_path, start, op.anchor.identity_filter())?;
    Some(SnapshotInfo {
        kopia_snapshot_id: entry.id.clone(),
        identity: ResolvedIdentity {
            username: entry.source.user_name.clone(),
            hostname: entry.source.host.clone(),
            source_path: Some(entry.source.path.clone()),
        },
    })
}

/// Drive a `BootstrapRepository` run: connect/create, write the result to the
/// work-spec ConfigMap (so the controller can read it even on failure), and
/// translate success/failure into the process exit code.
async fn run_bootstrap_flow(
    client: &KopiaClient,
    spec: &MoverWorkSpec,
    op: &BootstrapRepositoryOp,
    connect: &ConnectSpec,
    result_configmap: Option<&str>,
) -> Result<()> {
    info!(
        backend = spec.repository.kind_str(),
        auto_create = op.auto_create,
        repository = %spec.target_ref.name,
        "bootstrapping repository"
    );
    let result = run_bootstrap(client, op, connect).await;
    // Persist BEFORE returning: a failed bootstrap still exits non-zero (so the
    // Job is marked Failed and backoff is bounded), but the controller must be
    // able to read the structured failure to set an actionable Repository status.
    write_bootstrap_result(spec, &result, result_configmap).await;
    if result.success {
        info!(
            backend = spec.repository.kind_str(),
            created = result.created,
            unique_id = ?result.unique_id,
            snapshot_count = result.snapshot_count,
            "repository bootstrap succeeded"
        );
        Ok(())
    } else {
        // Surface the full failure on stdout (class + human message + the kopia
        // stderr tail) so `kubectl logs` on the bootstrap Job tells the whole
        // story without needing the result ConfigMap.
        let (class, message, stderr_tail) = result
            .failure
            .as_ref()
            .map(|f| {
                (
                    f.kopia_error_class.as_str(),
                    f.message.as_str(),
                    f.stderr_tail.as_deref().unwrap_or(""),
                )
            })
            .unwrap_or(("Unknown", "repository bootstrap failed", ""));
        error!(
            backend = spec.repository.kind_str(),
            class, stderr_tail, "repository bootstrap failed terminally: {message}"
        );
        Err(MoverError::BootstrapFailed {
            class: KopiaErrorClass::from_label(class),
            message: message.to_string(),
        })
    }
}

/// Build the identity-scope `policy set` args applied before every `snapshot
/// create` (M0b, confirmed data-loss bug): kopia's six `--keep-*` retention
/// fields, ALWAYS pinned to [`KOPIA_KEEP_MAX`], with `user_identity_policy`
/// (the one knob `split_policy_scopes` moves to the identity scope —
/// currently only `max_parallel_snapshots`) folded in on top when the
/// operator configured it. Pulled out of `run_operation` so the mandatory
/// pin is unit-testable without spawning kopia.
fn identity_retention_policy(
    user_identity_policy: Option<kopiur_kopia::PolicyArgs>,
) -> kopiur_kopia::PolicyArgs {
    kopiur_kopia::PolicyArgs {
        keep_latest: Some(KOPIA_KEEP_MAX),
        keep_hourly: Some(KOPIA_KEEP_MAX),
        keep_daily: Some(KOPIA_KEEP_MAX),
        keep_weekly: Some(KOPIA_KEEP_MAX),
        keep_monthly: Some(KOPIA_KEEP_MAX),
        keep_annual: Some(KOPIA_KEEP_MAX),
        max_parallel_snapshots: user_identity_policy.and_then(|p| p.max_parallel_snapshots),
        ..Default::default()
    }
}

/// Connect for bootstrap, honoring [`BootstrapRepositoryOp::read_only`]
/// (M6): a `mode: ReadOnly` repository's bootstrap connects with `--readonly`
/// (`repository_connect_readonly`) instead of the normal read-write connect —
/// bootstrap is a connect/scan probe, and a read-write connect from a
/// ReadOnly consumer repo was exactly what let it clobber the primary's
/// maintenance owner. Every other mover flow (restore, delete, snapshot)
/// stays on the plain read-write connect regardless.
async fn bootstrap_connect(
    client: &KopiaClient,
    spec: &ConnectSpec,
    cache: kopiur_kopia::CacheTuning,
    read_only: bool,
) -> std::result::Result<(), KopiaError> {
    if read_only {
        client.repository_connect_readonly(spec, cache).await
    } else {
        client.repository_connect(spec, cache).await
    }
}

/// The bootstrap routine: connect-first (adopt an existing repo), create only
/// when gated by [`should_attempt_create`], then read identity + catalog.
async fn run_bootstrap(
    client: &KopiaClient,
    op: &BootstrapRepositoryOp,
    connect: &ConnectSpec,
) -> BootstrapResult {
    let connect_spec = connect.clone();

    // Bootstrap (repo adopt/create) is a controller-driven probe, not a data run, so
    // it connects with kopia's default cache budgets.
    let cache = kopiur_kopia::CacheTuning::default();
    let mut created = false;
    if let Err(e) = bootstrap_connect(client, &connect_spec, cache, op.read_only).await {
        if !should_attempt_create(op.auto_create, e.class()) {
            // We will NOT create. Two distinct decline reasons → two distinct,
            // accurate messages:
            //  - create opt-out (`auto_create` off) + a genuinely-absent repo
            //    (`NotFound`) ⇒ actionable "set spec.create.enabled: true": the repo
            //    just needs initializing. Scoped to `NotFound` only — an unreachable
            //    backend (`RepositoryUnavailable`) or a denied bucket (`AccessDenied`)
            //    is NOT "uninitialized", and telling the user to enable create there
            //    would be wrong advice.
            //  - everything else (a repo exists we can't open — auth/locked; an
            //    access/permission problem; or auto_create on but blocked) ⇒ surface
            //    the real kopia class; recreating would mask it or risk a 2nd repo.
            //
            // A `NotFound` is only "uninitialized" when the backend answered and the
            // format blob is absent (`repository not initialized`). A missing path /
            // unbound mount also classifies `NotFound` ("no such file or directory")
            // but is a backend/mount fault, NOT an empty repository — surface it as a
            // real failure so the controller's health probe never misreads a
            // mis-mounted volume as a vanished repository (and never nudges a recreate).
            if !op.auto_create
                && e.class() == KopiaErrorClass::NotFound
                && e.stderr_tail()
                    .is_some_and(kopiur_kopia::notfound_is_uninitialized)
            {
                return BootstrapResult::not_initialized();
            }
            return BootstrapResult::failed(&e);
        }
        info!(class = %e.class(), "connect failed; attempting repository create");
        if let Err(ce) = client
            .repository_create(&connect_spec, cache, &op.create_options())
            .await
        {
            return BootstrapResult::failed(&ce);
        }
        if let Err(ce) = bootstrap_connect(client, &connect_spec, cache, op.read_only).await {
            return BootstrapResult::failed(&ce);
        }
        created = true;
        // Stamp the stable, lease-derived maintenance owner on a repo we just
        // CREATED (kopia auto-assigned this pod's ephemeral identity). Without
        // this, every maintenance mover sees a foreign owner and a
        // `takeoverPolicy: Never` yields forever. Best-effort: a failed stamp
        // is recoverable later via takeoverPolicy=Force (degrade-not-crash).
        if let Some(owner) = &op.maintenance_owner {
            match client.maintenance_set_owner(owner).await {
                Ok(()) => info!(%owner, "stamped maintenance owner on created repository"),
                Err(e) => warn!(
                    %owner,
                    class = %e.class(),
                    "could not stamp maintenance owner on created repository; \
                     maintenance will need takeoverPolicy=Force once"
                ),
            }
        }
    }

    // Self-heal a stale maintenance owner on connect-to-EXISTING. The stable,
    // lease-derived owner is only stamped on CREATE (above); a repo created by an
    // older operator (or where that stamp failed) keeps kopia's auto-assigned
    // EPHEMERAL pod identity as the owner, so every later maintenance mover sees a
    // foreign owner and — with the default `takeoverPolicy: Never` — yields forever:
    // full maintenance never runs and index blobs accumulate ("too many index
    // blobs"). `maintenance set --owner` is NOT owner-gated (only `run` is), so we
    // re-stamp the stable owner here, making maintenance match without a manual
    // `takeoverPolicy: Force`. Best-effort — a failed read/stamp degrades to the
    // pre-existing Force-recovery path rather than failing the bootstrap.
    if !created && let Some(desired) = op.maintenance_owner.as_deref() {
        match client.maintenance_info().await {
            Ok(info) => {
                if let Some(owner) = maintenance_restamp_target(
                    created,
                    Some(desired),
                    op.restamp_policy,
                    &op.maintenance_owner_aliases,
                    &info.owner,
                ) {
                    match client.maintenance_set_owner(owner).await {
                        Ok(()) => info!(
                            %owner,
                            stale = %info.owner,
                            "re-stamped stale maintenance owner on existing repository"
                        ),
                        Err(e) => warn!(
                            %owner,
                            class = %e.class(),
                            "could not re-stamp stale maintenance owner; maintenance may need takeoverPolicy=Force once"
                        ),
                    }
                }
            }
            Err(e) => warn!(
                class = %e.class(),
                "could not read maintenance owner to self-heal; continuing bootstrap"
            ),
        }
    }

    // One status read serves both the unique id and the epoch-parameter reconcile below —
    // it already sits after the create/connect and after the maintenance-owner block, which
    // is exactly where the mutable parameters can be applied.
    let mut status = match client.repository_status().await {
        Ok(s) => s,
        Err(e) => return BootstrapResult::failed(&e),
    };
    let unique_id = Some(status.unique_id_hex.clone());

    let mut epoch_error: Option<String> = None;
    // Reconcile mutable repository parameters (#258). Applies on the connect-to-existing
    // branch as much as on a create — that is the point: `spec.parameters.epoch` is a
    // declaration about a LIVE repository, and the generation-bump re-bootstrap is what
    // delivers an edit to it.
    //
    // Best-effort, exactly like the maintenance-owner stamp above: a bad parameter must not
    // fail the bootstrap and take an otherwise-healthy repository to `Failed`. The apply
    // stays visible either way — `status.parameters.epoch` mirrors what the repository
    // actually reports, so a failed apply shows up as drift from `spec` rather than as
    // silence.
    //
    // Only on drift. `set-parameters` rewrites the format blob and invalidates every other
    // kopia client's cached copy of it ("you must disconnect and re-connect all other Kopia
    // clients"), so an unconditional apply would churn the whole fleet on every bootstrap.
    if op.read_only {
        // Defense in depth: admission rejects `mode: ReadOnly` + `spec.parameters`, and the
        // controller does not send them — but `set-parameters` HARD-ERRORS on a read-only
        // connection (`storage is read-only`), so never risk it.
        if !op.epoch_parameters.is_empty() {
            warn!("skipping repository set-parameters: this repository is connected read-only");
        }
    } else if let Some(args) = kopiur_mover::workspec::epoch_drift(
        &op.epoch_parameters,
        status.content_format.epoch_parameters.as_ref(),
    ) {
        match client.repository_set_parameters(&args).await {
            Ok(()) => {
                info!(
                    flags = ?args.args(),
                    "applied kopia repository set-parameters (spec.parameters drifted)"
                );
                // Re-read so the mirror reports what LANDED, not the pre-apply observation
                // — otherwise status would show drift immediately after converging. Only
                // on the drift path, so the steady state still costs one status call.
                match client.repository_status().await {
                    Ok(s) => status = s,
                    Err(e) => warn!(
                        class = %e.class(),
                        "set-parameters applied but re-reading status failed; \
                         status.parameters will lag until the next bootstrap"
                    ),
                }
            }
            Err(e) => {
                // Best-effort, like the maintenance-owner restamp above: a bad parameter
                // must not take an otherwise healthy repository to `Failed`. But it must
                // not be silent either — carry the reason back so the controller can raise
                // a Warning event, rather than leaving only this log line and a
                // status.parameters that quietly disagrees with spec.
                warn!(
                    class = %e.class(),
                    "could not apply repository set-parameters; continuing bootstrap — \
                     status.parameters.epoch will show the drift"
                );
                epoch_error = Some(format!(
                    "kopia repository set-parameters failed ({}): {}. spec.parameters.epoch \
                     was NOT applied — status.parameters.epoch reports what the repository \
                     actually has. The bootstrap re-runs on the next spec change; edit \
                     spec.parameters.epoch to retry.",
                    e.class(),
                    e
                ));
            }
        }
    }
    let observed_epoch = status
        .content_format
        .epoch_parameters
        .as_ref()
        .map(kopiur_mover::workspec::observed_epoch);

    // Always list to report an authoritative snapshot count (unaffected by either
    // the foreign-suffix prefilter or the cap below); return the entries for
    // materialization only when scanning is requested.
    let listing = match client.snapshot_list(None).await {
        Ok(l) => l,
        Err(e) => return BootstrapResult::failed(&e),
    };
    let snapshot_count = listing.len() as i64;
    let (snapshots, truncated, foreign_suffix_dropped) = if op.scan_catalog {
        kopiur_mover::bootstrap::prepare_catalog_entries(
            listing,
            op.catalog_foreign_prefilter_cluster.as_deref(),
        )
    } else {
        (Vec::new(), false, 0)
    };
    if truncated {
        warn!(
            snapshot_count,
            returned = MAX_RETURNED_SNAPSHOTS,
            "more snapshots than the materialization cap; only the newest were returned"
        );
    }
    if foreign_suffix_dropped > 0 {
        info!(
            dropped = foreign_suffix_dropped,
            "dropped foreign-cluster snapshot entries before the materialization cap"
        );
    }

    // Index-blob health (best-effort, off the hot path): count the content-index
    // blobs so the controller can warn before maintenance falls far enough behind
    // to degrade backups. A read failure must never fail bootstrap — leave it
    // `None` and the controller keeps the prior count.
    let index_blob_count = match client.index_blob_count().await {
        Ok(n) => Some(n),
        Err(e) => {
            warn!(
                class = %e.class(),
                "could not read index blob count; skipping index-blob health for this run"
            );
            None
        }
    };

    BootstrapResult::ready(
        created,
        unique_id,
        snapshot_count,
        snapshots,
        truncated,
        foreign_suffix_dropped,
        index_blob_count,
    )
    .with_epoch(observed_epoch, epoch_error)
}

/// Apply the ConfigMap size backstop (issue #237) to a bootstrap result, warning
/// if trailing discovered entries had to be trimmed. The count cap
/// (`MAX_RETURNED_SNAPSHOTS`) is not a size cap, so a large catalog could otherwise
/// produce a result the apiserver rejects — wedging the repository at
/// `Bootstrapped: False` forever. The trimming decision itself is the pure,
/// unit-tested [`kopiur_mover::bootstrap::enforce_result_size_budget`].
fn size_guarded_result(result: &BootstrapResult) -> BootstrapResult {
    let guarded = kopiur_mover::bootstrap::enforce_result_size_budget(
        result.clone(),
        kopiur_mover::bootstrap::RESULT_SIZE_BUDGET_BYTES,
    );
    if guarded.snapshots.len() < result.snapshots.len() {
        warn!(
            kept = guarded.snapshots.len(),
            total = result.snapshots.len(),
            "bootstrap result exceeded the ConfigMap size budget; trimmed trailing \
             discovered entries so the write fits (the repository still bootstraps)"
        );
    }
    guarded
}

/// Persist a [`BootstrapResult`] into the work-spec ConfigMap (best-effort). The
/// controller reads it from key [`RESULT_CONFIGMAP_KEY`].
async fn write_bootstrap_result(
    spec: &MoverWorkSpec,
    result: &BootstrapResult,
    result_configmap: Option<&str>,
) {
    let cm_name = match result_configmap {
        Some(n) => n,
        None => {
            warn!("{RESULT_CONFIGMAP} unset; bootstrap result not persisted");
            return;
        }
    };
    let ns = &spec.target_ref.namespace;
    let guarded = size_guarded_result(result);
    match write_result_configmap(cm_name, ns, &guarded).await {
        Ok(()) => info!(configmap = %cm_name, "wrote bootstrap result"),
        // Loud (issue #237): a rejected write leaves the controller unable to read
        // the result, so the Repository stays `Bootstrapped: False` and all backup
        // work is gated. The size guard above should prevent the 1 MiB case, so a
        // failure here points at RBAC or another apiserver rejection worth surfacing.
        Err(e) => error!(
            error = %e,
            configmap = %cm_name,
            entries = guarded.snapshots.len(),
            "failed to write bootstrap result; the repository will stay Bootstrapped: \
             False until this is resolved (check the ConfigMap's size and the mover's RBAC)"
        ),
    }
}

/// Merge-patch the result JSON into the ConfigMap's `data` (adds
/// [`RESULT_CONFIGMAP_KEY`] without disturbing the work-spec key).
async fn write_result_configmap(
    cm_name: &str,
    namespace: &str,
    result: &BootstrapResult,
) -> Result<()> {
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::api::{Patch, PatchParams};

    let client = kube::Client::try_default()
        .await
        .map_err(|source| MoverError::KubeClient {
            source: Box::new(source),
        })?;
    let api: kube::Api<ConfigMap> = kube::Api::namespaced(client, namespace);
    let body = serde_json::json!({
        "data": {
            RESULT_CONFIGMAP_KEY: serde_json::to_string(result)
                .map_err(|source| MoverError::ResultSerialize { source })?
        }
    });
    api.patch(
        cm_name,
        &PatchParams::apply("kopiur.home-operations.com/mover"),
        &Patch::Merge(&body),
    )
    .await
    .map_err(|source| MoverError::ResultConfigMapPatch {
        configmap: cm_name.to_string(),
        namespace: namespace.to_string(),
        source: Box::new(source),
    })?;
    Ok(())
}

/// Drive a `Maintenance` run: connect, read the ownership lease, apply the
/// takeover policy, run `kopia maintenance run` when we hold the lease, and PATCH
/// the `Maintenance` `.status` directly (ADR §3.7). Returns an error (non-zero
/// exit → Job `Failed`) only when a kopia call fails; a *yield* (lease held by
/// another owner under a non-`Force` policy) is a successful no-op run.
async fn run_maintenance_flow(
    client: &KopiaClient,
    spec: &MoverWorkSpec,
    op: &MaintenanceOp,
    connect: &ConnectSpec,
) -> Result<()> {
    info!(
        backend = spec.repository.kind_str(),
        mode = ?op.mode,
        maintenance = %spec.target_ref.name,
        "running maintenance"
    );
    // Connect first: for object stores this pod is the only place with repo
    // access, which is exactly why the lease decision is made here.
    if let Err(e) = client.repository_connect(connect, spec.cache).await {
        patch_maintenance_status(&spec.target_ref, &maintenance_failed_body(&e)).await;
        error!(class = %e.class(), "maintenance connect failed");
        return Err(MoverError::Kopia {
            op: KopiaOp::MaintenanceConnect,
            source: e,
        });
    }

    // Assume the STABLE lease-derived client identity before anything else:
    // this pod's own user@hostname is ephemeral (a fresh pod every run), so
    // kopia's recorded owner can only ever be compared against — and claimed
    // as — the stable identity. (kopia 0.23 has no identity override at
    // connect; `repository set-client` flips it post-connect.)
    let (lease_user, lease_host) = kopiur_api::maintenance::kopia_lease_identity(&op.owner);
    if let Err(e) = client
        .repository_set_client_identity(&lease_user, &lease_host)
        .await
    {
        patch_maintenance_status(&spec.target_ref, &maintenance_failed_body(&e)).await;
        error!(class = %e.class(), "maintenance set-client identity failed");
        return Err(MoverError::Kopia {
            op: KopiaOp::MaintenanceConnect,
            source: e,
        });
    }

    // Read the current lease holder and apply the takeover policy.
    let info = match client.maintenance_info().await {
        Ok(i) => i,
        Err(e) => {
            patch_maintenance_status(&spec.target_ref, &maintenance_failed_body(&e)).await;
            error!(class = %e.class(), "maintenance info failed");
            return Err(MoverError::Kopia {
                op: KopiaOp::MaintenanceInfo,
                source: e,
            });
        }
    };
    // Held by another when kopia's recorded owner is neither empty, OUR stable
    // identity, nor one of our recognized owner-format aliases (the M6
    // migration path — a repo whose managed Maintenance moved to a new lease
    // format still recognizes what it used to stamp as itself). Comparing
    // against `op.owner` (the logical lease string, never a kopia
    // user@hostname) directly was the bug that made every run on a
    // mover-bootstrapped repo yield forever.
    let held_by_other =
        kopiur_api::maintenance::lease_held_by_other(&info.owner, &op.owner, &op.owner_aliases);
    // Shared remediation appended to both blocked outcomes below: this mover
    // cannot tell a hand-authored Maintenance from an operator-managed one
    // (that distinction lives on the CR's ownerReferences, which the work spec
    // doesn't carry), so it is worded neutrally — conditioned on "for
    // operator-managed maintenance" rather than asserted outright. Hand-authored
    // Maintenance CRs are always honored regardless of any Repository's
    // `spec.maintenance`, so telling every reader to flip `enabled: false` would
    // be actively wrong advice for those.
    const REMEDIATION: &str = "for operator-managed maintenance: if another cluster is the \
         designated maintenance runner, set spec.maintenance.enabled: false on this \
         repository's non-owner clusters; to move ownership here instead, set \
         ownership.takeoverPolicy: Force once";
    match lease_action(op.takeover_policy, held_by_other) {
        LeaseAction::Yield => {
            patch_maintenance_status(
                &spec.target_ref,
                &lease_blocked_body(
                    &info.owner,
                    kopiur_api::maintenance::LEASE_HELD_BY_OTHER_REASON,
                    &format!(
                        "maintenance lease held by {}; takeoverPolicy=Never ({REMEDIATION})",
                        info.owner
                    ),
                ),
            )
            .await;
            info!(owner = %info.owner, "maintenance lease held by another owner; yielding");
            Ok(())
        }
        LeaseAction::Prompt => {
            patch_maintenance_status(
                &spec.target_ref,
                &lease_blocked_body(
                    &info.owner,
                    kopiur_api::maintenance::LEASE_TAKEOVER_PROMPT_REASON,
                    &format!("lease held by {}; {REMEDIATION}", info.owner),
                ),
            )
            .await;
            info!(owner = %info.owner, "maintenance lease held; prompting for takeover");
            Ok(())
        }
        action @ (LeaseAction::Claim | LeaseAction::Takeover) => {
            // Claim kopia's maintenance ownership for THIS pod's identity first.
            // kopia rejects `maintenance run` from anyone but the designated owner,
            // and a repo the controller bootstrapped in-process is owned by the
            // controller's identity — so without this the run fails with
            // "maintenance must be run by designated user: …". The operator's own
            // lease (decided above via op.owner/takeover_policy) is the real
            // coordination; this just satisfies kopia's per-connection guard.
            if let Err(e) = client.maintenance_set_owner_me().await {
                patch_maintenance_status(&spec.target_ref, &maintenance_failed_body(&e)).await;
                error!(class = %e.class(), "maintenance ownership claim failed");
                return Err(MoverError::Kopia {
                    op: KopiaOp::MaintenanceSetOwner,
                    source: e,
                });
            }
            if let Err(e) = client.maintenance_run(op.mode).await {
                patch_maintenance_status(&spec.target_ref, &maintenance_failed_body(&e)).await;
                error!(class = %e.class(), "maintenance run failed");
                return Err(MoverError::Kopia {
                    op: KopiaOp::MaintenanceRun,
                    source: e,
                });
            }
            patch_maintenance_status(
                &spec.target_ref,
                &maintenance_ran_body(op, &chrono::Utc::now()),
            )
            .await;
            info!(?action, mode = ?op.mode, "maintenance run succeeded");
            Ok(())
        }
    }
}

/// Probe that the deep-verify scratch path is a writable mount: create the dir
/// tree (kopia's restore would otherwise `mkdir` it), then write and remove a
/// sentinel file. Surfaces a missing or read-only scratch volume as an explicit
/// [`MoverError::ScratchNotWritable`] before kopia turns it into an opaque
/// `mkdir … permission denied`. The probe file is cleaned up on success; on
/// failure the IO error carries the cause (NotFound / PermissionDenied / …).
fn probe_scratch_writable(path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    let probe = std::path::Path::new(path).join(".kopiur-writable");
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)
}

/// Drive a `Verify` run (ADR-0005 §4): connect, run the quick (`kopia snapshot
/// verify`) or deep (scratch-restore) tier, evaluate the optional CEL `successExpr`
/// over the result, and PATCH the `SnapshotPolicy` `.status.lastVerified` on
/// success. Owns its own connect lifecycle like maintenance. Returns an error
/// (non-zero exit → Job `Failed`) when a kopia call fails or `successExpr` rejects.
async fn run_verify_flow(
    client: &KopiaClient,
    spec: &MoverWorkSpec,
    op: &VerifyOp,
    connect: &ConnectSpec,
) -> Result<()> {
    info!(
        backend = spec.repository.kind_str(),
        tier = op.tier.kind_str(),
        policy = %spec.target_ref.name,
        "running verification"
    );
    if let Err(e) = client.repository_connect(connect, spec.cache).await {
        patch_verify_status(&spec.target_ref, &verify_failed_body(&e.to_string())).await;
        error!(class = %e.class(), "verify connect failed");
        return Err(MoverError::Kopia {
            op: KopiaOp::VerifyConnect,
            source: e,
        });
    }

    // Run the tier and collect the result environment for successExpr. A kopia
    // failure is terminal; a clean run yields the stats the predicate inspects.
    let (stats, restored, snapshot_id) = match &op.tier {
        VerifyTier::Quick(q) => {
            // Scope the verify to THIS policy's resolved identity. Without
            // `--sources`, `kopia snapshot verify` verifies every snapshot in
            // the repository, so on a shared ClusterRepository a per-policy
            // quick verify would re-verify every other identity's data and
            // `verifyFilesPercent` would sample the whole repository — issue
            // #250. The identity is the same `username@hostname:path` kopia
            // recorded the snapshot under.
            let mut opts = q.to_kopia();
            opts.sources = vec![spec.identity.source_spec()];
            if let Err(e) = client.snapshot_verify(&opts).await {
                patch_verify_status(&spec.target_ref, &verify_failed_body(&e.to_string())).await;
                error!(class = %e.class(), "snapshot verify failed");
                return Err(MoverError::Kopia {
                    op: KopiaOp::SnapshotVerify,
                    source: e,
                });
            }
            // kopia `snapshot verify` reports no machine-readable file/byte counts on
            // its own, so derive the predicate environment from the snapshot manifest:
            // a healthy quick verify of a non-empty snapshot then satisfies the common
            // `stats.files > 0` predicate instead of always failing on a hardcoded 0.
            // `errors` is 0 — a passing verify found no integrity errors. Best-effort:
            // if the manifest can't be listed we fall back to 0/0/0 and the exit code
            // remains the integrity verdict.
            match resolve_latest_snapshot(client, spec).await {
                Ok(Some(entry)) => (
                    kopiur_api::VerifyStats {
                        files: i64::try_from(entry.stats.file_count).unwrap_or(i64::MAX),
                        bytes: i64::try_from(entry.stats.total_size).unwrap_or(i64::MAX),
                        errors: 0,
                    },
                    None,
                    Some(entry.id),
                ),
                _ => (kopiur_api::VerifyStats::default(), None, None),
            }
        }
        VerifyTier::Deep(d) => {
            // Resolve the snapshot id to restore: the controller's choice, else the
            // newest snapshot for this identity.
            let id = match &d.snapshot_id {
                Some(id) => id.clone(),
                None => match resolve_latest_snapshot(client, spec).await {
                    Ok(Some(entry)) => entry.id,
                    Ok(None) => {
                        let err = MoverError::VerifyNoSnapshot {
                            source_path: spec.identity.source_path.clone(),
                        };
                        patch_verify_status(
                            &spec.target_ref,
                            &verify_failed_body(&err.to_string()),
                        )
                        .await;
                        return Err(err);
                    }
                    Err(e) => {
                        patch_verify_status(&spec.target_ref, &verify_failed_body(&e.to_string()))
                            .await;
                        return Err(MoverError::Kopia {
                            op: KopiaOp::DeepVerifySnapshotList,
                            source: e,
                        });
                    }
                },
            };
            // Preflight: the scratch path must be a writable mount. Without it kopia's
            // restore dies with a cryptic `mkdir /scratch: permission denied` (the
            // non-root mover cannot create a dir under root-owned `/`). Probe first so
            // a missing/read-only scratch mount is a clear, classified, actionable
            // failure naming the fix, not an opaque kopia error.
            if let Err(source) = probe_scratch_writable(&d.scratch_path) {
                let err = MoverError::ScratchNotWritable {
                    path: std::path::PathBuf::from(&d.scratch_path),
                    uid: kopiur_api::common::MOVER_NONROOT_ID,
                    source,
                };
                patch_verify_status(&spec.target_ref, &verify_failed_body(&err.to_string())).await;
                error!(class = %err.kopia_class(), "deep verify scratch path not writable");
                return Err(err);
            }
            if let Err(e) = client
                .snapshot_restore_with(
                    &id,
                    &d.scratch_path,
                    &kopiur_kopia::RestoreOptions {
                        parallel: d.parallel,
                        ..Default::default()
                    },
                )
                .await
            {
                patch_verify_status(&spec.target_ref, &verify_failed_body(&e.to_string())).await;
                error!(class = %e.class(), "deep verify scratch-restore failed");
                return Err(MoverError::Kopia {
                    op: KopiaOp::DeepVerifyRestore,
                    source: e,
                });
            }
            // Count what the scratch-restore produced so `restored.files`/`stats.files`
            // are meaningful to a successExpr. A read failure here is non-fatal: we
            // treat the restore exit code as authoritative and report 0.
            let files = count_files(&d.scratch_path).unwrap_or(0);
            (
                kopiur_api::VerifyStats {
                    files,
                    bytes: 0,
                    errors: 0,
                },
                Some(kopiur_api::RestoredStats {
                    files,
                    checksum_matches: true,
                }),
                Some(id),
            )
        }
    };

    // Evaluate the optional CEL successExpr over the result — killing the silent
    // "0 files" success when the user opted in.
    if let Some(expr) = &op.success_expr {
        let mut snapshot = std::collections::BTreeMap::new();
        if let Some(id) = &snapshot_id {
            snapshot.insert("id".to_string(), id.clone());
        }
        let inputs = kopiur_api::SuccessExprInputs {
            stats,
            snapshot,
            restored,
            tier: op.tier.kind_str().to_string(),
            _marker: std::marker::PhantomData,
        };
        match kopiur_api::eval_success_expr(expr, &inputs) {
            Ok(true) => {}
            Ok(false) => {
                let err = MoverError::SuccessExprFalse { expr: expr.clone() };
                let msg = err.to_string();
                patch_verify_status(&spec.target_ref, &verify_failed_body(&msg)).await;
                warn!("{msg}");
                return Err(err);
            }
            Err(e) => {
                let err = MoverError::SuccessExprEval { source: e };
                patch_verify_status(&spec.target_ref, &verify_failed_body(&err.to_string())).await;
                return Err(err);
            }
        }
    }

    patch_verify_status(
        &spec.target_ref,
        &verify_ok_body(op.tier.kind_str(), &chrono::Utc::now()),
    )
    .await;
    info!(tier = op.tier.kind_str(), "verification succeeded");
    Ok(())
}

/// The newest snapshot for this run's identity: source path AND
/// username/hostname must all match, not path alone — the same path repeats
/// across namespaces (and, in a shared repository, across clusters), so a
/// path-only pick could quick/deep-verify or restore-heal a DIFFERENT
/// source's data. Reuses [`kopiur_mover::resolve::matches_source`], the same
/// identity-aware predicate the delete/pin self-heal matchers use. The full
/// manifest entry is returned so callers can read both its id (deep-verify
/// restore target) and its `stats` (quick-verify predicate environment).
async fn resolve_latest_snapshot(
    client: &KopiaClient,
    spec: &MoverWorkSpec,
) -> Result<Option<kopiur_kopia::SnapshotListEntry>, KopiaError> {
    let mut list = client.snapshot_list(None).await?;
    list.sort_by_key(|e| std::cmp::Reverse(e.end_time));
    let identity = &spec.identity;
    Ok(list.into_iter().find(|e| {
        matches_source(
            e,
            &identity.source_path,
            Some((&identity.username, &identity.hostname)),
        )
    }))
}

/// Best-effort recursive file count under `dir` for the deep-verify result
/// environment. Returns `None` on any IO error (the caller treats it as 0).
fn count_files(dir: &str) -> Option<i64> {
    fn walk(dir: &std::path::Path, count: &mut i64) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                walk(&entry.path(), count)?;
            } else if ft.is_file() {
                *count += 1;
            }
        }
        Ok(())
    }
    let mut count = 0i64;
    walk(std::path::Path::new(dir), &mut count).ok()?;
    Some(count)
}

/// PATCH a raw `{ "status": ... }` merge body onto the `SnapshotPolicy` `.status`
/// (best-effort; logged on failure). Reuses the same dynamic-API pattern as
/// [`patch_maintenance_status`].
async fn patch_verify_status(target: &workspec::TargetRef, body: &serde_json::Value) {
    patch_maintenance_status(target, body).await;
}

/// Drive a `Replicate` run (ADR-0005 §13(d)): connect to the *source* repository,
/// then `kopia repository sync-to <destination>` to mirror its blobs to the
/// destination backend. PATCHes the `RepositoryReplication` `.status`. Owns its own
/// connect lifecycle like maintenance. Returns an error (non-zero exit → Job
/// `Failed`) when a kopia call fails.
async fn run_replicate_flow(
    client: &KopiaClient,
    spec: &MoverWorkSpec,
    op: &ReplicateOp,
    connect: &ConnectSpec,
) -> Result<()> {
    info!(
        source_backend = spec.repository.kind_str(),
        destination_backend = op.destination.kind_str(),
        replication = %spec.target_ref.name,
        "replicating repository"
    );
    // Connect to the source first (this pod is the only place with repo access for
    // object stores — the same rationale as maintenance/verify).
    if let Err(e) = client.repository_connect(connect, spec.cache).await {
        patch_replicate_status(&spec.target_ref, &replicate_failed_body(&e.to_string())).await;
        error!(class = %e.class(), "replication source connect failed");
        return Err(MoverError::Kopia {
            op: KopiaOp::ReplicateConnect,
            source: e,
        });
    }

    // The destination's credentials arrive under the KOPIUR_DEST_ env prefix so they
    // never collide with the source's identically named ones (issue #200). Read them
    // back prefixed: file-based dest creds (SFTP key / GCS JSON / rclone) are staged
    // from the prefixed env into a *separate* dir; the ambient-chain hints of a
    // workload-identity destination stay UNPREFIXED because they belong to the pod's
    // ServiceAccount, not to a credential Secret.
    let raw_env = |key: &str| std::env::var(key).ok();
    let mut dest = op.destination.to_connect_spec();
    if let Err(e) =
        credentials::materialize_with(&mut dest, &credential_staging_dir().join("dest"), &|key| {
            credentials::dest_materialize_lookup(key, &raw_env)
        })
    {
        // The CredentialWrite/CredentialStagingDir variants already name the env
        // key, path, and fix — propagate them untouched.
        patch_replicate_status(&spec.target_ref, &replicate_failed_body(&e.to_string())).await;
        return Err(e);
    }

    // Direct-env destinations (S3/Azure/B2/WebDAV): remap the destination backend's
    // credential vars from their prefixed copies for the sync-to subprocess only,
    // unsetting any the destination does not set so a source credential cannot leak.
    let dest_env = credentials::dest_env_overlay(&dest, &raw_env);

    if let Err(e) = client
        .repository_sync_to_with_env(&dest, &op.sync_options(), &dest_env)
        .await
    {
        patch_replicate_status(&spec.target_ref, &replicate_failed_body(&e.to_string())).await;
        error!(class = %e.class(), "repository sync-to failed");
        return Err(MoverError::Kopia {
            op: KopiaOp::RepositorySyncTo,
            source: e,
        });
    }

    patch_replicate_status(
        &spec.target_ref,
        &replicate_ok_body(op.destination.kind_str(), &chrono::Utc::now()),
    )
    .await;
    info!(
        destination = op.destination.kind_str(),
        "replication succeeded"
    );
    Ok(())
}

/// PATCH a raw `{ "status": ... }` merge body onto the `RepositoryReplication`
/// `.status` (best-effort; logged on failure). Reuses the dynamic-API pattern.
async fn patch_replicate_status(target: &workspec::TargetRef, body: &serde_json::Value) {
    patch_maintenance_status(target, body).await;
}

/// Drive a `BrowseSession` run (M7a): connect to the repository **read-only**
/// (`repository connect --readonly` — the read-only bit persists in the client
/// config, so nothing this pod later execs can mutate the repo), write the
/// readiness marker so the pod's `kopiur-mover ready` probe starts passing,
/// then idle until the TTL elapses and exit cleanly. The CLI drives the actual
/// reads ([`kopiur_kopia::SessionCmd`]) via pod exec while the pod is Ready.
///
/// Unlike maintenance/verify/replicate there is NO status to PATCH: the
/// session's `targetRef` names nothing the controller owns. A failure here
/// logs the actionable error (class + message) and exits non-zero so the Job
/// goes `Failed` and the CLI surfaces the pod logs.
async fn run_browse_session_flow(
    client: &KopiaClient,
    spec: &MoverWorkSpec,
    op: &BrowseSessionOp,
    connect: &ConnectSpec,
) -> Result<()> {
    info!(
        backend = spec.repository.kind_str(),
        ttl_seconds = op.ttl_seconds,
        session = %spec.target_ref.name,
        "starting browse session (read-only connect)"
    );
    if let Err(e) = client
        .repository_connect_readonly(connect, spec.cache)
        .await
    {
        // Mirrors the maintenance connect-failure logging (class + message) so
        // `kubectl logs` on the session pod tells the whole story — minus the
        // status PATCH, which has no target for a browse session.
        error!(class = %e.class(), "browse session read-only connect failed");
        return Err(MoverError::Kopia {
            op: KopiaOp::BrowseConnect,
            source: e,
        });
    }

    // Signal readiness: the marker flips the pod Ready so the CLI knows the
    // session is exec-able. A marker that cannot be written would leave the
    // pod NotReady forever, so it is terminal (non-zero exit), not best-effort.
    let marker = std::path::Path::new(kopiur_mover::env::READY_MARKER);
    std::fs::write(marker, b"ready").map_err(|source| MoverError::ReadyMarkerWrite {
        path: marker.to_path_buf(),
        source,
    })?;
    info!(
        marker = %marker.display(),
        ttl_seconds = op.ttl_seconds,
        "browse session ready; holding the read-only connection until the TTL elapses"
    );

    tokio::time::sleep(Duration::from_secs(op.ttl_seconds)).await;
    info!(
        ttl_seconds = op.ttl_seconds,
        "browse session TTL elapsed; exiting"
    );
    Ok(())
}

/// PATCH a raw `{ "status": ... }` merge body onto the `Maintenance` `.status`
/// (best-effort; logged on failure, like [`StatusReporter`]). Uses a dynamic API
/// so the mover need not depend on the typed CRD struct.
async fn patch_maintenance_status(target: &workspec::TargetRef, body: &serde_json::Value) {
    use kube::api::{Patch, PatchParams};
    use kube::core::{ApiResource, DynamicObject, GroupVersionKind};

    let attempt = async {
        let client =
            kube::Client::try_default()
                .await
                .map_err(|source| MoverError::KubeClient {
                    source: Box::new(source),
                })?;
        let (group, version) = split_api_version(&target.api_version);
        let gvk = GroupVersionKind::gvk(&group, &version, &target.kind);
        let ar = ApiResource::from_gvk(&gvk);
        let api = kube::Api::<DynamicObject>::namespaced_with(client, &target.namespace, &ar);
        api.patch_status(&target.name, &PatchParams::default(), &Patch::Merge(body))
            .await
            .map_err(|source| MoverError::StatusPatch {
                kind: target.kind.clone(),
                namespace: target.namespace.clone(),
                name: target.name.clone(),
                source: Box::new(source),
            })?;
        Ok::<(), MoverError>(())
    };
    if let Err(e) = attempt.await {
        warn!(error = %e, target = %target.name, "maintenance status PATCH failed");
    }
}

/// Report a terminal failure (PATCH the structured failure block) and return
/// the typed error so `main` exits non-zero. Takes ownership: the same
/// [`MoverError`] that built the `status.failure` block (class, stderr tail,
/// retry hint) is what the process exits with — no stringly re-wrap.
async fn terminal_failure(reporter: &StatusReporter, err: MoverError) -> Result<()> {
    let update = StatusUpdate::failed_mover(&err, chrono::Utc::now());
    reporter.report(&update).await;
    error!(
        class = %err.kopia_class(),
        retry = err.retry_recommended(),
        "kopia operation failed terminally"
    );
    Err(err)
}

/// Mover metrics, pushed over OTLP (when configured) before the Job exits. The
/// Prometheus pull endpoint is irrelevant for a short-lived Job, so this only
/// adds value with `OTEL_EXPORTER_OTLP_ENDPOINT` set.
struct MoverMetrics {
    provider: kopiur_telemetry::MetricsProvider,
    operations: opentelemetry::metrics::Counter<u64>,
    duration: opentelemetry::metrics::Histogram<f64>,
}

impl MoverMetrics {
    fn new() -> Self {
        let provider = kopiur_telemetry::MetricsProvider::new("kopiur-mover");
        let m = provider.meter();
        let operations = m
            .u64_counter("kopiur_mover_operations")
            .with_description("Total mover operations by kind and result.")
            .build();
        let duration = m
            .f64_histogram("kopiur_mover_operation_duration_seconds")
            .with_description("Mover operation wall-clock duration in seconds.")
            .build();
        MoverMetrics {
            provider,
            operations,
            duration,
        }
    }

    fn record(&self, operation: &str, result: &str, seconds: f64) {
        use opentelemetry::KeyValue;
        let attrs = [
            KeyValue::new("operation", operation.to_string()),
            KeyValue::new("result", result.to_string()),
        ];
        self.operations.add(1, &attrs);
        self.duration.record(seconds, &attrs);
    }

    fn shutdown(&self) {
        self.provider.shutdown();
    }
}

/// Load the run-once work spec. Resolution order:
/// 1. a positional path arg (manual/debug invocations),
/// 2. the INLINE JSON in [`env::WORK_SPEC`] (how the controller passes it —
///    embedded in the Job env so the run is one self-cleaning object, #224),
/// 3. a file at [`env::WORK_SPEC_PATH`] (legacy ConfigMap-mounting Jobs).
///
/// The env fallbacks are deliberately manual (not clap `#[arg(env)]`) — see
/// [`kopiur_mover::cli`].
fn resolve_work_spec(arg: Option<PathBuf>) -> Result<MoverWorkSpec> {
    if let Some(arg) = arg {
        return load_work_spec(&arg);
    }
    if let Ok(inline) = std::env::var(kopiur_mover::env::WORK_SPEC) {
        return serde_json::from_str(&inline).map_err(|source| MoverError::WorkSpecParse {
            path: PathBuf::from(format!("${}", kopiur_mover::env::WORK_SPEC)),
            source,
        });
    }
    if let Ok(env) = std::env::var(WORK_SPEC_PATH) {
        return load_work_spec(&PathBuf::from(env));
    }
    Err(MoverError::WorkSpecPathMissing)
}

fn load_work_spec(path: &PathBuf) -> Result<MoverWorkSpec> {
    let raw = std::fs::read_to_string(path).map_err(|source| MoverError::WorkSpecRead {
        path: path.clone(),
        source,
    })?;
    let spec: MoverWorkSpec =
        serde_json::from_str(&raw).map_err(|source| MoverError::WorkSpecParse {
            path: path.clone(),
            source,
        })?;
    Ok(spec)
}

fn build_client(spec: &MoverWorkSpec, kopia_binary: Option<&str>) -> KopiaClient {
    let mut builder = KopiaClient::builder();
    if let Some(bin) = kopia_binary {
        builder = builder.binary(bin);
    }
    // Suppress the GitHub update check globally.
    builder = builder.env("KOPIA_CHECK_FOR_UPDATES", "false");
    if let Some(t) = spec.options.operation_timeout_secs {
        builder = builder.default_timeout(Duration::from_secs(t));
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- M0b: identity-scope retention pin (KOPIA_KEEP_MAX) is mandatory ---

    #[test]
    fn identity_retention_policy_always_pins_keep_max_with_no_user_policy() {
        let p = identity_retention_policy(None);
        assert_eq!(p.keep_latest, Some(KOPIA_KEEP_MAX));
        assert_eq!(p.keep_hourly, Some(KOPIA_KEEP_MAX));
        assert_eq!(p.keep_daily, Some(KOPIA_KEEP_MAX));
        assert_eq!(p.keep_weekly, Some(KOPIA_KEEP_MAX));
        assert_eq!(p.keep_monthly, Some(KOPIA_KEEP_MAX));
        assert_eq!(p.keep_annual, Some(KOPIA_KEEP_MAX));
        assert_eq!(p.max_parallel_snapshots, None);
    }

    #[test]
    fn identity_retention_policy_folds_in_user_max_parallel_snapshots() {
        // The one user knob kopia allows only at identity/global scope
        // (`split_policy_scopes`) rides alongside the mandatory pin, not
        // instead of it.
        let user = kopiur_kopia::PolicyArgs {
            max_parallel_snapshots: Some(3),
            ..Default::default()
        };
        let p = identity_retention_policy(Some(user));
        assert_eq!(p.keep_latest, Some(KOPIA_KEEP_MAX));
        assert_eq!(p.keep_annual, Some(KOPIA_KEEP_MAX));
        assert_eq!(p.max_parallel_snapshots, Some(3));
    }

    // --- `kopiur-mover ready` probe mode: the pure marker decision ---

    #[test]
    fn session_ready_is_true_iff_the_marker_exists() {
        let dir = std::env::temp_dir().join(format!("kopiur-ready-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join(".kopiur-session-ready");

        // No marker yet → the probe must fail (pod stays NotReady).
        assert!(!session_ready(&marker));

        // Marker written (what run_browse_session_flow does after a successful
        // read-only connect) → the probe passes.
        std::fs::write(&marker, b"ready").unwrap();
        assert!(session_ready(&marker));

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- mass-deletion protection M1: SnapshotDeleteBatch ---

    fn anchor_without_start_time(source_path: &str) -> SnapshotAnchor {
        SnapshotAnchor {
            source_path: source_path.into(),
            start_time: None,
            username: None,
            hostname: None,
        }
    }

    fn anchor_with_start_time(source_path: &str, start_time: &str) -> SnapshotAnchor {
        SnapshotAnchor {
            source_path: source_path.into(),
            start_time: Some(start_time.into()),
            username: None,
            hostname: None,
        }
    }

    #[test]
    fn anchor_self_heal_gate_requires_start_time() {
        // The data-loss fix: a path (or path+identity) alone is not enough to
        // authorize a delete-path self-heal — only a start_time-bearing
        // anchor may attempt re-resolution.
        assert!(!anchor_self_heal_allowed(&anchor_without_start_time(
            "/pvc/db"
        )));
        assert!(!anchor_self_heal_allowed(&SnapshotAnchor::default()));
        assert!(anchor_self_heal_allowed(&anchor_with_start_time(
            "/pvc/db",
            "2026-06-19T05:54:19Z"
        )));
    }

    #[test]
    fn snapshot_delete_batch_selects_the_log_only_reporter() {
        let batch = Operation::SnapshotDeleteBatch(SnapshotDeleteBatchOp { items: vec![] });
        assert!(wants_log_only_reporter(&batch));
    }

    #[test]
    fn other_operations_do_not_select_the_log_only_reporter() {
        let delete = Operation::SnapshotDelete(SnapshotDeleteOp {
            snapshot_id: "id".into(),
            anchor: SnapshotAnchor::default(),
        });
        assert!(!wants_log_only_reporter(&delete));
    }

    // --- delete_one / delete_batch against a fake kopia binary ---
    //
    // Mirrors `crates/kopia/tests/fake_shim.rs`: a tiny shell script stands in
    // for kopia so the self-heal gate and the attempt-all-then-fail loop are
    // exercised through the real `KopiaClient` subprocess path with no real
    // kopia binary.
    #[cfg(unix)]
    mod delete_shim_tests {
        use std::os::unix::fs::PermissionsExt;

        use super::*;

        struct Shim {
            _dir: tempfile::TempDir,
            path: PathBuf,
        }

        fn shim(script: &str) -> Shim {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("kopia-shim.sh");
            std::fs::write(&path, script).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            Shim { _dir: dir, path }
        }

        fn client_for(shim: &Shim) -> KopiaClient {
            KopiaClient::builder().binary(shim.path.clone()).build()
        }

        #[tokio::test]
        async fn delete_one_skips_self_heal_when_anchor_has_no_start_time() {
            // Regression (data-loss fix, adversarial review): an anchor with a
            // source_path but NO start_time must never trigger the stale-id
            // self-heal, even though the path-only fallback WOULD uniquely
            // match another (unrelated, newer) snapshot at that path. The
            // shim records whether `snapshot list` (the self-heal's
            // re-resolution call) was ever invoked; it must NOT be.
            let marker_dir = tempfile::tempdir().unwrap();
            let marker = marker_dir.path().join("list-called");
            let s = shim(&format!(
                r#"#!/bin/sh
case "$*" in
  *"snapshot list"*)
    touch "{marker}"
    echo '[{{"id":"other","source":{{"host":"prod","userName":"mydb","path":"/pvc/db"}},"startTime":"2026-06-19T05:54:19Z","endTime":"2026-06-19T05:54:19Z"}}]'
    exit 0 ;;
  *"snapshot delete"*) exit 0 ;;
  *) exit 0 ;;
esac
"#,
                marker = marker.display()
            ));
            let client = client_for(&s);
            let anchor = anchor_without_start_time("/pvc/db");
            delete_one(&client, "stale-id", &anchor)
                .await
                .expect("delete_one succeeds even with the gate closed");
            assert!(
                !marker.exists(),
                "snapshot list must never be called when the anchor has no start_time"
            );
        }

        #[tokio::test]
        async fn delete_one_self_heals_when_anchor_has_start_time() {
            // With a start_time anchor, the pre-existing self-heal behavior is
            // preserved: a stale recorded id is healed to the live re-resolved
            // manifest and that one is deleted too.
            let s = shim(
                r#"#!/bin/sh
case "$*" in
  *"snapshot list"*)
    echo '[{"id":"live-id","source":{"host":"prod","userName":"mydb","path":"/pvc/db"},"startTime":"2026-06-19T05:54:19Z","endTime":"2026-06-19T05:54:19Z"}]'
    exit 0 ;;
  *"snapshot delete stale-id"*) exit 0 ;;
  *"snapshot delete live-id"*) exit 0 ;;
  *) echo "unexpected argv: $*" 1>&2; exit 9 ;;
esac
"#,
            );
            let client = client_for(&s);
            let anchor = anchor_with_start_time("/pvc/db", "2026-06-19T05:54:19Z");
            delete_one(&client, "stale-id", &anchor)
                .await
                .expect("delete_one self-heals the live manifest and succeeds");
        }

        #[tokio::test]
        async fn delete_batch_attempts_every_member_even_after_an_earlier_failure() {
            // attempt-all-then-fail: the first member's delete fails (a
            // non-idempotent error, not the "already absent" no-op), but the
            // second must still be attempted — proven by a marker file the
            // shim only touches on the second member's argv.
            let marker_dir = tempfile::tempdir().unwrap();
            let marker = marker_dir.path().join("good-id-deleted");
            let s = shim(&format!(
                r#"#!/bin/sh
case "$*" in
  *"snapshot delete bad-id"*) echo "error deleting snapshots: access denied" 1>&2; exit 1 ;;
  *"snapshot delete good-id"*) touch "{marker}"; exit 0 ;;
  *) exit 0 ;;
esac
"#,
                marker = marker.display()
            ));
            let client = client_for(&s);
            let op = SnapshotDeleteBatchOp {
                items: vec![
                    SnapshotDeleteItem {
                        snapshot_id: "bad-id".into(),
                        anchor: SnapshotAnchor::default(),
                    },
                    SnapshotDeleteItem {
                        snapshot_id: "good-id".into(),
                        anchor: SnapshotAnchor::default(),
                    },
                ],
            };
            let err = delete_batch(&client, &op)
                .await
                .expect_err("one member failed, so the batch is incomplete");
            match err {
                MoverError::BatchDeleteIncomplete { failed, total } => {
                    assert_eq!(failed, 1);
                    assert_eq!(total, 2);
                }
                other => panic!("expected BatchDeleteIncomplete, got {other:?}"),
            }
            assert!(
                marker.exists(),
                "the second member must still be attempted after the first failed"
            );
        }

        #[tokio::test]
        async fn delete_batch_reports_success_when_every_member_succeeds() {
            let s = shim("#!/bin/sh\nexit 0\n");
            let client = client_for(&s);
            let op = SnapshotDeleteBatchOp {
                items: vec![
                    SnapshotDeleteItem {
                        snapshot_id: "a".into(),
                        anchor: SnapshotAnchor::default(),
                    },
                    SnapshotDeleteItem {
                        snapshot_id: "b".into(),
                        anchor: SnapshotAnchor::default(),
                    },
                ],
            };
            let update = delete_batch(&client, &op)
                .await
                .expect("every member succeeds");
            assert_eq!(update.phase.as_deref(), Some("Succeeded"));
        }
    }
}
