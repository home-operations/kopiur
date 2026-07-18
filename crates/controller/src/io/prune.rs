//! Prune discriminator: stamp the `pruned-by` annotation before deleting a
//! `Snapshot` the operator prunes on its own lifecycle (GFS retention /
//! failed-history), so the `Snapshot` finalizer classifies the deletion as an
//! OPERATOR prune (`plan::pruned_by`) and bypasses the mass-deletion breaker +
//! cascade guard. Without the stamp, a normal retention/history prune wave is
//! indistinguishable from an external mass deletion and would trip the breaker.

use kube::Api;
use kube::api::{DeleteParams, Patch, PatchParams};

use kopiur_api::Snapshot;
use kopiur_api::consts::PRUNED_BY_ANNOTATION;
use kopiur_api::snapshot::PrunedBy;

use super::FIELD_MANAGER;
use crate::error::{Error, Result};

/// Stamp `kopiur.home-operations.com/pruned-by: <value>` on `name`. 404-tolerant
/// (a CR that vanished mid-reconcile is a no-op success) and idempotent — an
/// identical merge-patch is an apiserver no-op, so re-running it never churns
/// `resourceVersion`.
pub async fn stamp_pruned_by(api: &Api<Snapshot>, name: &str, pruned_by: PrunedBy) -> Result<()> {
    let patch = serde_json::json!({
        "metadata": { "annotations": { PRUNED_BY_ANNOTATION: pruned_by.annotation_value() } }
    });
    match api
        .patch(
            name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&patch),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// Stamp `pruned-by: <value>` THEN delete `name`. Both steps are 404-tolerant and
/// idempotent (identical merge-patch is an apiserver no-op; deleting an
/// already-terminating/gone CR is harmless). A crash between the two steps
/// re-runs both on the next reconcile: the stamp re-converges (no-op) and the
/// delete is re-issued (no-op if already gone). Ordering matters — stamping
/// BEFORE the delete guarantees the finalizer never observes the deletion
/// without the `pruned-by` classification and so never mistakes an operator
/// prune for an external deletion.
pub async fn annotate_then_delete_snapshot(
    api: &Api<Snapshot>,
    name: &str,
    pruned_by: PrunedBy,
) -> Result<()> {
    stamp_pruned_by(api, name, pruned_by).await?;
    match api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// Bare UNSTAMPED delete of `name` — no `pruned-by` annotation. Used for the
/// `SnapshotPolicy` deletion cascade's `delete_only` set
/// ([`crate::snapshot_policy::PolicyCascadePlan`]): those children must
/// classify as an EXTERNAL deletion (indistinguishable from a `kubectl
/// delete`) so the per-repository mass-deletion breaker gates them — stamping
/// here would launder an externally-classified deletion past the breaker.
/// 404-tolerant and idempotent, mirroring [`annotate_then_delete_snapshot`]'s
/// delete half.
pub async fn delete_snapshot(api: &Api<Snapshot>, name: &str) -> Result<()> {
    match api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(Error::Kube(e)),
    }
}
