//! The **slow-mover fixture**: make mover Jobs take a deterministic, observable
//! amount of time.
//!
//! A real backup in the harness (a couple of files on the kind node's hostPath
//! repo) finishes in well under a second. That is far too fast to observe a
//! queue, so any scenario about *concurrency* — a per-repository cap
//! serializing runs, a restore jumping ahead of backups, `Replace` cancelling a
//! running backup — has nothing to look at. This module swaps the operator's
//! mover image for [`consts::SLOW_MOVER_IMAGE`], whose entrypoint sleeps before
//! exec'ing the real mover (docker/Dockerfile.mover-slow), so every mover Job
//! holds its slot for a window the scenario chooses.
//!
//! # How the delay reaches the mover pod
//!
//! There are exactly two channels, and this module uses both:
//!
//! 1. **The image** — `KOPIUR_MOVER_IMAGE` on the operator's controller
//!    Deployment picks which image mover Jobs run. Patching it (+ rollout wait)
//!    is what turns the fixture on. This is cluster-wide: while enabled, EVERY
//!    mover Job the operator stamps is a slow one.
//! 2. **The credentials Secret** — the operator does not pass arbitrary env
//!    through to mover pods (`kopiur_mover::jobs` only forwards the work spec,
//!    the kopia cache vars and the fixed OTLP/log passthrough), but every mover
//!    pod gets `envFrom: secretRef` over the whole repository credentials
//!    Secret, and pod env overrides the image's `ENV`. So writing
//!    [`DELAY_ENV`]/[`DELAY_OPS_ENV`] into a repository's credentials Secret
//!    tunes the delay for that repository's movers at runtime, with NO operator
//!    change. A repository whose Secret carries neither key falls back to the
//!    image-baked [`DEFAULT_DELAY`].
//!
//! # Cancellation
//!
//! A sleeping mover pod terminates PROMPTLY on delete: the fixture entrypoint
//! traps SIGTERM, kills its sleep and exits 143, rather than sitting out the
//! pod's whole `terminationGracePeriodSeconds` waiting for SIGKILL. So a
//! scenario that cancels a running backup (`Replace`) sees the pod go away in
//! about a second, not thirty.
//!
//! # Restore contract
//!
//! Enabling reshapes the RUNNING operator (a Deployment rollout), exactly like
//! `mass_deletion.rs`'s `set_delete_job_cap`. A leaked slow image makes every
//! later scenario in the same `--test-threads=1` run mysteriously slow, so it
//! MUST be restored on every exit path.
//!
//! [`SlowMoverGuard`] cannot restore from `Drop` — restoring is async and the
//! test's runtime is already unwinding by then — so its `Drop` only prints a
//! loud warning. **Prefer [`with_slow_mover`]**, which restores whether the body
//! returns `Ok`, returns `Err`, or `?`-propagates:
//!
//! ```no_run
//! # async fn demo(world: &kopiur_e2e::World) -> anyhow::Result<()> {
//! use std::time::Duration;
//! use kopiur_e2e::slow_mover;
//!
//! slow_mover::with_slow_mover(world, Duration::from_secs(45), || async {
//!     // ... everything in here sees slow mover Jobs ...
//!     Ok(())
//! })
//! .await
//! # }
//! ```
//!
//! If a scenario needs the guard form instead, call
//! [`SlowMoverGuard::restore`] on every path (`?`-propagate inside the window,
//! restore before returning) — the discipline `mass_deletion.rs` documents.

use std::time::Duration;

use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Patch, PatchParams};
use kube::{Api, Client};

use crate::{consts, world::World};

/// Env the fixture image reads for its pre-exec sleep, in whole seconds. `"0"`
/// disables the delay. Set on a repository's credentials Secret to override the
/// image-baked [`DEFAULT_DELAY`] for that repository's mover pods.
pub const DELAY_ENV: &str = "KOPIUR_E2E_MOVER_DELAY_SECONDS";

/// Env restricting the delay to specific work-spec operations (comma-separated
/// [`MoverOp::wire_key`] values). Unset ⇒ every operation is delayed.
pub const DELAY_OPS_ENV: &str = "KOPIUR_E2E_MOVER_DELAY_OPS";

/// The delay baked into the fixture image (`ARG DELAY_SECONDS=60` in
/// docker/Dockerfile.mover-slow) — what a mover sleeps when its credentials
/// Secret carries no [`DELAY_ENV`].
pub const DEFAULT_DELAY: Duration = Duration::from_secs(60);

/// The credentials Secret [`enable`]/[`disable`] tune by default: the shared
/// filesystem-backend Secret every `Need::Filesystem` scenario's repository
/// uses. Override with [`SlowMover::creds_secrets`] for an object-store repo.
pub const DEFAULT_CREDS_SECRET: &str = consts::SECRET_FS_CREDS;

/// A mover work-spec operation, by its JSON discriminant.
///
/// Mirrors `kopiur_mover::workspec::Operation` — externally tagged, `camelCase`
/// — as DELIBERATE literals: the e2e crate does not depend on the mover crate
/// (the same pattern `mass_deletion.rs` uses for the batch-delete label), so a
/// rename there must fail an e2e run loudly rather than silently widen a filter.
/// Exhaustive `match` in [`MoverOp::wire_key`]: a new variant forces a new arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoverOp {
    /// `Operation::Snapshot` — a backup run.
    Snapshot,
    /// `Operation::Restore`.
    Restore,
    /// `Operation::SnapshotDelete`.
    SnapshotDelete,
    /// `Operation::SnapshotDeleteBatch` — the per-repository batch delete.
    SnapshotDeleteBatch,
    /// `Operation::BootstrapRepository`.
    BootstrapRepository,
    /// `Operation::Maintenance`.
    Maintenance,
    /// `Operation::SnapshotPin`.
    SnapshotPin,
    /// `Operation::Verify`.
    Verify,
    /// `Operation::Replicate` — blob-level repository replication.
    Replicate,
    /// `Operation::BrowseSession` — the interactive read-only session pod.
    BrowseSession,
    /// `Operation::SnapshotReplicate` — logical snapshot replication.
    SnapshotReplicate,
}

impl MoverOp {
    /// The operation's key in the work-spec JSON (`"operation":{"<key>":...}`),
    /// which is what the fixture entrypoint matches on.
    pub fn wire_key(self) -> &'static str {
        match self {
            MoverOp::Snapshot => "snapshot",
            MoverOp::Restore => "restore",
            MoverOp::SnapshotDelete => "snapshotDelete",
            MoverOp::SnapshotDeleteBatch => "snapshotDeleteBatch",
            MoverOp::BootstrapRepository => "bootstrapRepository",
            MoverOp::Maintenance => "maintenance",
            MoverOp::SnapshotPin => "snapshotPin",
            MoverOp::Verify => "verify",
            MoverOp::Replicate => "replicate",
            MoverOp::BrowseSession => "browseSession",
            MoverOp::SnapshotReplicate => "snapshotReplicate",
        }
    }
}

/// The [`DELAY_OPS_ENV`] value for `ops` (empty ⇒ `None`, meaning "delay every
/// operation").
fn ops_value(ops: &[MoverOp]) -> Option<String> {
    (!ops.is_empty()).then(|| {
        ops.iter()
            .map(|o| o.wire_key())
            .collect::<Vec<_>>()
            .join(",")
    })
}

/// A configured slow-mover activation. Build one when [`enable`]'s defaults do
/// not fit — a non-filesystem repository's credentials Secret, or a delay that
/// should apply only to some operations.
///
/// ```no_run
/// # async fn demo(world: &kopiur_e2e::World) -> anyhow::Result<()> {
/// use std::time::Duration;
/// use kopiur_e2e::slow_mover::{MoverOp, SlowMover};
///
/// // Only backups crawl; restores (and bootstrap) stay fast.
/// let guard = SlowMover::new(Duration::from_secs(30))
///     .ops(&[MoverOp::Snapshot])
///     .creds_secrets(&["kopia-s3-creds"])
///     .enable(world)
///     .await?;
/// guard.restore().await
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SlowMover {
    delay: Duration,
    ops: Vec<MoverOp>,
    creds_secrets: Vec<String>,
}

impl SlowMover {
    /// A slow mover with `delay` applied to every operation, tuned through
    /// [`DEFAULT_CREDS_SECRET`].
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            ops: Vec::new(),
            creds_secrets: vec![DEFAULT_CREDS_SECRET.to_string()],
        }
    }

    /// Delay only these operations; everything else runs at full speed. An empty
    /// slice restores the default (delay everything).
    #[must_use]
    pub fn ops(mut self, ops: &[MoverOp]) -> Self {
        self.ops = ops.to_vec();
        self
    }

    /// The credentials Secret(s) in [`consts::OPERATOR_NS`] to write the delay
    /// knobs into — one per repository whose movers should honor `delay`.
    /// Repositories outside this set still run the fixture image, but fall back
    /// to the image-baked [`DEFAULT_DELAY`].
    #[must_use]
    pub fn creds_secrets(mut self, names: &[&str]) -> Self {
        self.creds_secrets = names.iter().map(|n| (*n).to_string()).collect();
        self
    }

    /// Write the delay knobs, then point the operator at the fixture image and
    /// wait for the rollout.
    ///
    /// Order matters: the Secret is written FIRST so no mover pod can be created
    /// against the fixture image before its delay value exists (a pod that
    /// raced it would silently sleep [`DEFAULT_DELAY`] instead).
    pub async fn enable(self, world: &World) -> Result<SlowMoverGuard> {
        let client = world.client().clone();
        let delay_secs = self.delay.as_secs();
        for secret in &self.creds_secrets {
            set_delay_knobs(
                &client,
                secret,
                Some(delay_secs),
                ops_value(&self.ops).as_deref(),
            )
            .await?;
        }
        eprintln!(
            "[slow_mover] enabling {} (delay {delay_secs}s, ops: {}, secrets: {:?})",
            consts::SLOW_MOVER_IMAGE,
            ops_value(&self.ops).unwrap_or_else(|| "<all>".to_string()),
            self.creds_secrets,
        );
        set_mover_image(&client, consts::SLOW_MOVER_IMAGE).await?;
        Ok(SlowMoverGuard {
            client,
            creds_secrets: self.creds_secrets,
            restored: false,
        })
    }
}

/// Make every mover Job sleep `delay` before doing its work, for repositories
/// using [`DEFAULT_CREDS_SECRET`].
///
/// Reshapes the running operator; the caller MUST restore — see
/// [`with_slow_mover`], which does it for you.
pub async fn enable(world: &World, delay: Duration) -> Result<SlowMoverGuard> {
    SlowMover::new(delay).enable(world).await
}

/// Undo [`enable`]: put the real mover image back (rollout wait) and clear the
/// delay knobs from [`DEFAULT_CREDS_SECRET`].
///
/// Idempotent — safe to call when the fixture was never enabled, or twice.
/// Restores in the mirror order of [`SlowMover::enable`]: the image first, so no
/// new mover can be created slow once this returns, then the Secret.
///
/// Only clears [`DEFAULT_CREDS_SECRET`]. If you enabled with
/// [`SlowMover::creds_secrets`], restore through the guard (or
/// [`with_slow_mover`]) so every Secret you touched is cleaned up.
pub async fn disable(world: &World) -> Result<()> {
    restore(world.client(), &[DEFAULT_CREDS_SECRET.to_string()]).await
}

/// Run `body` with the slow mover enabled, restoring on EVERY exit path
/// (success, error, or panic-free `?`-propagation from inside `body`).
///
/// This is the recommended form: unlike a guard, it cannot be forgotten. When
/// both the body and the restore fail, the body's error is returned (it is the
/// cause) and the restore failure is printed.
pub async fn with_slow_mover<T, F, Fut>(world: &World, delay: Duration, body: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    with_slow_mover_config(world, SlowMover::new(delay), body).await
}

/// [`with_slow_mover`] for a configured [`SlowMover`] (filtered operations, or
/// non-default credentials Secrets).
pub async fn with_slow_mover_config<T, F, Fut>(
    world: &World,
    config: SlowMover,
    body: F,
) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let guard = config.enable(world).await?;
    let outcome = body().await;
    let restored = guard.restore().await;
    match (outcome, restored) {
        (Ok(v), Ok(())) => Ok(v),
        (Ok(_), Err(e)) => Err(e.context("slow-mover fixture could not be restored")),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(restore_err)) => {
            eprintln!(
                "[slow_mover] WARNING: restore ALSO failed while unwinding: {restore_err:#} \
                 — the operator may still be running {}",
                consts::SLOW_MOVER_IMAGE
            );
            Err(e)
        }
    }
}

/// Handle to an active slow-mover window. Restore it with
/// [`SlowMoverGuard::restore`] on every exit path.
///
/// `Drop` CANNOT restore (restoring is async; by the time a guard drops the
/// test's runtime is unwinding), so it only warns. Prefer [`with_slow_mover`].
// No `Debug`: `kube::Client` does not implement it.
pub struct SlowMoverGuard {
    client: Client,
    creds_secrets: Vec<String>,
    restored: bool,
}

impl SlowMoverGuard {
    /// Put the real mover image back and clear the delay knobs from every
    /// Secret this guard wrote. Consumes the guard, so its `Drop` stays quiet.
    pub async fn restore(mut self) -> Result<()> {
        self.restored = true;
        restore(&self.client, &self.creds_secrets).await
    }
}

impl Drop for SlowMoverGuard {
    fn drop(&mut self) {
        if !self.restored {
            eprintln!(
                "[slow_mover] WARNING: SlowMoverGuard dropped WITHOUT restore() — the operator \
                 is still running {} and every later scenario in this run will be slow. Drop \
                 cannot restore (it is async); use kopiur_e2e::slow_mover::with_slow_mover, or \
                 call guard.restore() on every exit path.",
                consts::SLOW_MOVER_IMAGE
            );
        }
    }
}

/// Shared restore path: real image first (rollout wait), then clear the knobs.
async fn restore(client: &Client, creds_secrets: &[String]) -> Result<()> {
    eprintln!("[slow_mover] restoring {}", consts::MOVER_IMAGE);
    set_mover_image(client, consts::MOVER_IMAGE).await?;
    for secret in creds_secrets {
        set_delay_knobs(client, secret, None, None).await?;
    }
    Ok(())
}

/// Patch the controller Deployment's [`consts::MOVER_IMAGE_ENV`] and wait for the
/// rollout, so the only running controller pod is the one carrying `image`.
///
/// Strategic-merge on the container by name; `env` merges by the entry's `name`,
/// so this updates the value the chart renders rather than duplicating it. Same
/// shape as `mass_deletion.rs::set_delete_job_cap`.
///
/// The wait is [`rollout_complete`] rather than [`wait::deployment_ready`]: the
/// latter's `is_deployment_completed` looks only at `Progressing=True/
/// NewReplicaSetAvailable`, which is still true of the PREVIOUS rollout for the
/// moment between this patch landing and the deployment controller acting on it
/// — so it can return while the old (fast-mover) pod is the one running. For a
/// delete-job cap that costs nothing; here it would let a scenario stamp a
/// full-speed mover Job and lose its premise as a timing flake.
async fn set_mover_image(client: &Client, image: &str) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), consts::OPERATOR_NS);
    let patched = api
        .patch(
            consts::CONTROLLER_DEPLOYMENT,
            &PatchParams::default(),
            &Patch::Strategic(serde_json::json!({
                "spec": { "template": { "spec": { "containers": [
                    { "name": consts::CONTROLLER_CONTAINER,
                      "env": [ { "name": consts::MOVER_IMAGE_ENV, "value": image } ] }
                ]}}}
            })),
        )
        .await
        .with_context(|| {
            format!(
                "patching {}/{} {} to {image}",
                consts::OPERATOR_NS,
                consts::CONTROLLER_DEPLOYMENT,
                consts::MOVER_IMAGE_ENV
            )
        })?;
    let generation = patched.metadata.generation.unwrap_or_default();

    crate::wait_until(
        &format!("controller Deployment rolls out {image}"),
        crate::default_timeout(),
        crate::poll_interval(),
        || async {
            let live = api.get(consts::CONTROLLER_DEPLOYMENT).await?;
            Ok(rollout_complete(&live, generation).then_some(()))
        },
    )
    .await
}

/// True once the deployment controller has OBSERVED `generation` and finished
/// its rollout with no old pods left (`replicas == desired`, so a surge pod from
/// the previous template has terminated).
fn rollout_complete(deployment: &Deployment, generation: i64) -> bool {
    let desired = deployment
        .spec
        .as_ref()
        .and_then(|s| s.replicas)
        .unwrap_or(1);
    let Some(status) = deployment.status.as_ref() else {
        return false;
    };
    status.observed_generation.unwrap_or(0) >= generation
        && status.updated_replicas.unwrap_or(0) == desired
        && status.available_replicas.unwrap_or(0) == desired
        && status.replicas.unwrap_or(0) == desired
}

/// Set (or, with `None`s, remove) the fixture's env knobs on a credentials
/// Secret in [`consts::OPERATOR_NS`], which mover pods pick up via `envFrom`.
///
/// A JSON-merge patch under a field manager of its own: `World::ensure` re-applies
/// these Secrets with server-side apply as `kopiur-e2e`, and SSA prunes fields
/// that manager owns but no longer sends — so writing the knobs under the SAME
/// manager would let a later `ensure` silently drop them mid-scenario.
/// `stringData` is write-only (the API server folds it into `data`), hence the
/// removal side patches `data` with nulls.
async fn set_delay_knobs(
    client: &Client,
    secret: &str,
    delay_secs: Option<u64>,
    ops: Option<&str>,
) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), consts::OPERATOR_NS);
    let pp = PatchParams {
        field_manager: Some("kopiur-e2e-slow-mover".to_string()),
        ..Default::default()
    };
    let patch = match delay_secs {
        Some(secs) => serde_json::json!({ "stringData": {
            DELAY_ENV: secs.to_string(),
            // An unset filter must CLEAR a previous one, not inherit it.
            DELAY_OPS_ENV: ops.unwrap_or_default(),
        }}),
        None => serde_json::json!({ "data": { DELAY_ENV: null, DELAY_OPS_ENV: null } }),
    };
    api.patch(secret, &pp, &Patch::Merge(&patch))
        .await
        .with_context(|| {
            format!(
                "patching the slow-mover env knobs onto Secret {}/{secret} — is it provisioned \
                 (world.ensure(&[Need::Filesystem]))?",
                consts::OPERATOR_NS
            )
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every [`MoverOp`] must map to a distinct, lowerCamelCase key — the shape
    /// the fixture entrypoint matches (`"operation":{"<key>":`).
    #[test]
    fn wire_keys_are_distinct_lower_camel_case() {
        let all = [
            MoverOp::Snapshot,
            MoverOp::Restore,
            MoverOp::SnapshotDelete,
            MoverOp::SnapshotDeleteBatch,
            MoverOp::BootstrapRepository,
            MoverOp::Maintenance,
            MoverOp::SnapshotPin,
            MoverOp::Verify,
            MoverOp::Replicate,
            MoverOp::BrowseSession,
            MoverOp::SnapshotReplicate,
        ];
        let keys: std::collections::BTreeSet<&str> = all.iter().map(|o| o.wire_key()).collect();
        assert_eq!(keys.len(), all.len(), "duplicate MoverOp::wire_key");
        for key in keys {
            let first = key.chars().next().expect("non-empty key");
            assert!(
                first.is_ascii_lowercase() && key.chars().all(|c| c.is_ascii_alphanumeric()),
                "`{key}` is not the camelCase discriminant serde renders for Operation"
            );
        }
    }

    #[test]
    fn ops_value_is_none_for_the_unfiltered_default() {
        assert_eq!(ops_value(&[]), None);
        assert_eq!(
            ops_value(&[MoverOp::Snapshot, MoverOp::Restore]).as_deref(),
            Some("snapshot,restore")
        );
    }

    fn deployment(
        generation: i64,
        observed: i64,
        replicas: i32,
        updated: i32,
        available: i32,
    ) -> Deployment {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": "kopiur-controller", "generation": generation },
            "spec": { "replicas": 1, "selector": { "matchLabels": {} },
                      "template": { "metadata": {}, "spec": { "containers": [] } } },
            "status": {
                "observedGeneration": observed,
                "replicas": replicas,
                "updatedReplicas": updated,
                "availableReplicas": available,
            },
        }))
        .expect("valid Deployment")
    }

    /// The whole point of the custom wait: a status that predates the patch must
    /// NOT count as rolled out, however healthy it looks.
    #[test]
    fn rollout_is_incomplete_until_the_new_generation_is_observed() {
        // Fully healthy, but the controller has not seen generation 7 yet — this
        // is exactly the stale-status window that would hand a scenario the old
        // (fast-mover) pod.
        assert!(!rollout_complete(&deployment(7, 6, 1, 1, 1), 7));
        assert!(rollout_complete(&deployment(7, 7, 1, 1, 1), 7));
    }

    /// Mid-rollout the old pod is still up (surge ⇒ replicas > desired), or the
    /// new one is not available yet.
    #[test]
    fn rollout_is_incomplete_while_old_or_unready_pods_remain() {
        assert!(
            !rollout_complete(&deployment(7, 7, 2, 1, 1), 7),
            "surge pod"
        );
        assert!(
            !rollout_complete(&deployment(7, 7, 1, 1, 0), 7),
            "new pod not available"
        );
        assert!(
            !rollout_complete(&deployment(7, 7, 1, 0, 1), 7),
            "new pod not yet updated"
        );
    }

    /// A Deployment with no status at all (freshly created) is not rolled out.
    #[test]
    fn rollout_is_incomplete_without_status() {
        let mut d = deployment(1, 1, 1, 1, 1);
        d.status = None;
        assert!(!rollout_complete(&d, 1));
    }

    #[test]
    fn builder_defaults_target_the_shared_filesystem_creds_secret() {
        let cfg = SlowMover::new(Duration::from_secs(5));
        assert_eq!(cfg.creds_secrets, vec![DEFAULT_CREDS_SECRET.to_string()]);
        assert!(cfg.ops.is_empty(), "the default delays every operation");
        let cfg = cfg.creds_secrets(&["a", "b"]).ops(&[MoverOp::Snapshot]);
        assert_eq!(cfg.creds_secrets, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(cfg.ops, vec![MoverOp::Snapshot]);
    }
}
