//! e2e: the staging deadline — transient VolumeSnapshot errors never terminally
//! fail a backup (issue #198); only `spec.staging.timeout` does.
//!
//! Two scenarios, both driven by **deterministic VolumeSnapshot status injection**:
//! the tests scale `kube-system/snapshot-controller` to 0 (it is the only writer of
//! VolumeSnapshot `.status`; the CSI hostpath sidecars only write content/PVC state),
//! own the VS status themselves, and restore the controller afterwards. The e2e
//! harness runs test binaries with `--test-threads=1`, so the scale-down window
//! cannot race other tests.
//!
//! 1. **Recover** (the #198 regression): the operator observes an injected,
//!    transient snapshot-controller error (`409 Conflict` text) on the staged
//!    VolumeSnapshot and must keep the `Snapshot` `Pending` (`SourceStaged=False`,
//!    reason `WaitingForVolumeSnapshot`); when the VS then becomes `readyToUse`,
//!    staging recovers (`SourceStaged=True`). Pre-fix code stamped a terminal
//!    `Failed` on first sight of the error, so this scenario times out on it.
//! 2. **Bounded failure**: with `staging: {timeout: "30s"}` and a persistently
//!    erroring, never-ready VS, the `Snapshot` goes terminally `Failed` with reason
//!    `VolumeSnapshotFailed`, `Stalled=True`, NO mover Job, a specific Warning
//!    Event — and NO misleading `InvalidSpec` Event (issue #198 defect 3).
//!
//! Requires the CSI snapshot stack (`mise run //crates/e2e:snapshot-stack`) like
//! `copy_methods.rs`. Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; skips
//! gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use anyhow::{Context as _, bail, ensure};
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::api::storage::v1::StorageClass;
use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// The storage class the `snapshot-stack` mise task installs (see `copy_methods.rs`).
const CSI_STORAGE_CLASS: &str = "csi-hostpath-sc";
/// Where the vendored snapshot-stack manifests deploy the snapshot-controller.
const SNAPSHOT_CONTROLLER_NS: &str = "kube-system";
const SNAPSHOT_CONTROLLER: &str = "snapshot-controller";
/// The exact transient error external-snapshotter reports during its benign
/// finalizer-add retry loop — the real-world trigger of issue #198.
const TRANSIENT_409: &str = "Failed to create snapshot content with error snapshot \
    controller failed to update karakeep-data on API server: Operation cannot be \
    fulfilled on persistentvolumeclaims \"karakeep-data\": the object has been \
    modified; please apply your changes to the latest version and try again";

fn volume_snapshots(client: &kube::Client) -> Api<DynamicObject> {
    let ar = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("snapshot.storage.k8s.io", "v1", "VolumeSnapshot"),
        "volumesnapshots",
    );
    Api::namespaced_with(client.clone(), E2E_NAMESPACE, &ar)
}

/// Scale the snapshot-controller Deployment and wait until its pods match: for 0,
/// until no pods remain (so nothing can race the test's VS status writes); for >0,
/// until the Deployment reports ready (so later tests get a working stack back).
async fn scale_snapshot_controller(client: &kube::Client, replicas: i32) -> anyhow::Result<()> {
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), SNAPSHOT_CONTROLLER_NS);
    deploys
        .patch(
            SNAPSHOT_CONTROLLER,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "replicas": replicas } })),
        )
        .await
        .context("scale snapshot-controller")?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), SNAPSHOT_CONTROLLER_NS);
    wait_until(
        &format!("snapshot-controller at {replicas} replicas"),
        default_timeout(),
        poll_interval(),
        || async {
            let running = pods
                .list(&ListParams::default().labels("app.kubernetes.io/name=snapshot-controller"))
                .await?
                .items
                .len() as i32;
            if replicas == 0 {
                Ok((running == 0).then_some(()))
            } else {
                let ready = deploys
                    .get_opt(SNAPSHOT_CONTROLLER)
                    .await?
                    .and_then(|d| d.status)
                    .and_then(|s| s.ready_replicas)
                    .unwrap_or(0);
                Ok((ready >= replicas).then_some(()))
            }
        },
    )
    .await
}

/// Provision the per-scenario fixtures: a Repository over `subpath`, a CSI source
/// PVC (`WaitForFirstConsumer`; binding is NOT needed for staging), the policy
/// (`copyMethod: Snapshot` + `extra_policy_spec`), and the Snapshot CR. Runs INSIDE
/// the snapshot-controller scale-down window (so the operator's VolumeSnapshot is
/// born into a quiesced world and can never race the real controller), hence
/// Result-returning — the caller restores the controller on every exit path.
async fn setup_scenario(
    client: &kube::Client,
    prefix: &str,
    subpath: &str,
    extra_policy_spec: serde_json::Value,
) -> anyhow::Result<()> {
    ensure_repo(client, subpath).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                &format!("{prefix}-repo"),
                subpath,
                serde_json::json!({}),
            )),
        )
        .await
        .context("create Repository")?;
    pvcs.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": format!("{prefix}-src-pvc"), "namespace": E2E_NAMESPACE },
            "spec": {
                "accessModes": ["ReadWriteOnce"],
                "storageClassName": CSI_STORAGE_CLASS,
                "resources": { "requests": { "storage": "64Mi" } },
            },
        })),
    )
    .await
    .context("create CSI source PVC")?;
    // Merge the scenario's extra fields into the base spec fragment by hand:
    // `merge_spec` expects a FULL manifest (it writes into `base["spec"]`) and would
    // silently drop fields merged into a bare spec fragment like this one.
    let mut policy_spec = serde_json::json!({
        "copyMethod": "Snapshot",
        "sources": [ { "pvc": { "name": format!("{prefix}-src-pvc") } } ],
    });
    if let (Some(base), serde_json::Value::Object(more)) =
        (policy_spec.as_object_mut(), extra_policy_spec)
    {
        base.extend(more);
    }
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                &format!("{prefix}-policy"),
                "Repository",
                &format!("{prefix}-repo"),
                policy_spec,
            )),
        )
        .await
        .context("create SnapshotPolicy")?;
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                prefix,
                &format!("{prefix}-policy"),
                serde_json::json!({}),
            )),
        )
        .await
        .context("create Snapshot")?;
    Ok(())
}

/// Best-effort teardown of everything `setup_scenario` created.
async fn teardown_scenario(client: &kube::Client, prefix: &str) {
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = backups.delete(prefix, &DeleteParams::default()).await;
    let _ = policies
        .delete(&format!("{prefix}-policy"), &DeleteParams::default())
        .await;
    let _ = repos
        .delete(&format!("{prefix}-repo"), &DeleteParams::default())
        .await;
    let _ = pvcs
        .delete(&format!("{prefix}-src-pvc"), &DeleteParams::default())
        .await;
}

/// Wait for the operator-created VolumeSnapshot (`<snapshot>-snap`), then inject a
/// status via the status subresource (the snapshot-controller is scaled down, so the
/// injected status is stable until the test changes it).
async fn inject_vs_status(
    vs_api: &Api<DynamicObject>,
    vs_name: &str,
    status: serde_json::Value,
) -> anyhow::Result<()> {
    wait_until(
        &format!("VolumeSnapshot {vs_name} created by the operator"),
        default_timeout(),
        poll_interval(),
        || async { Ok(vs_api.get_opt(vs_name).await?.map(|_| ())) },
    )
    .await?;
    vs_api
        .patch_status(
            vs_name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "status": status })),
        )
        .await
        .context("patch VolumeSnapshot status")?;
    Ok(())
}

/// Assert the CSI snapshot stack is installed (hard requirement, mirrors
/// `copy_methods.rs`): these scenarios need the VS CRDs + the hostpath driver.
async fn require_snapshot_stack(client: &kube::Client) {
    let scs: Api<StorageClass> = Api::all(client.clone());
    assert!(
        scs.get_opt(CSI_STORAGE_CLASS)
            .await
            .expect("list storageclasses")
            .is_some(),
        "storageclass {CSI_STORAGE_CLASS} not found — run `mise run //crates/e2e:snapshot-stack` \
         (or set KOPIUR_E2E_SKIP_SNAPSHOT_STACK=1 only for shards excluding this file)"
    );
}

/// RECOVER (#198): a transient VolumeSnapshot `status.error` must be observed as a
/// WAIT (phase stays `Pending`), and staging must recover once the VS is ready.
/// Pre-fix code terminally Failed the Snapshot on first sight of the error — this
/// test times out at the `WaitingForVolumeSnapshot` wait on that code.
#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:test)"]
async fn transient_volumesnapshot_error_recovers_and_does_not_fail_the_backup() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    require_snapshot_stack(&client).await;

    let prefix = "e2e-sd-recover";
    // Quiesce the snapshot-controller BEFORE any CR exists: the operator's
    // VolumeSnapshot must be born into a world where only the test writes VS status
    // (a live controller could make it genuinely ready before the injection).
    scale_snapshot_controller(&client, 0)
        .await
        .expect("scale down snapshot-controller");
    let result = async {
        setup_scenario(&client, prefix, "staging-recover", serde_json::json!({})).await?;
        recover_body(&client, prefix).await
    }
    .await;
    // Restore the controller on ALL exit paths before propagating the verdict, so a
    // failure here can't break the snapshot stack for later tests.
    let restored = scale_snapshot_controller(&client, 1).await;
    teardown_scenario(&client, prefix).await;
    restored.expect("restore snapshot-controller");
    result.expect("transient-error recovery scenario");
}

async fn recover_body(client: &kube::Client, prefix: &str) -> anyhow::Result<()> {
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let vs_api = volume_snapshots(client);
    let vs_name = format!("{prefix}-snap");

    // Inject the real-world transient failure: not ready + a 409-conflict error.
    inject_vs_status(
        &vs_api,
        &vs_name,
        serde_json::json!({
            "readyToUse": false,
            "error": { "message": TRANSIENT_409, "time": "2026-07-04T18:30:00Z" },
        }),
    )
    .await?;

    // The operator must OBSERVE the error and still classify staging as a WAIT:
    // SourceStaged=False with reason WaitingForVolumeSnapshot and the error text
    // surfaced in the message. Pre-fix code set reason VolumeSnapshotFailed +
    // phase Failed here, so this wait is the regression trip-wire.
    wait_until(
        "operator observes the VS error as a transient wait",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&backups, prefix).await;
            let cond = s
                .get("conditions")
                .and_then(|c| c.as_array())
                .and_then(|a| {
                    a.iter()
                        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("SourceStaged"))
                })
                .cloned();
            Ok(cond.filter(|c| {
                c.get("status").and_then(|v| v.as_str()) == Some("False")
                    && c.get("reason").and_then(|v| v.as_str()) == Some("WaitingForVolumeSnapshot")
                    && c.get("message")
                        .and_then(|v| v.as_str())
                        .is_some_and(|m| m.contains("the object has been modified"))
            }))
        },
    )
    .await
    .context("the VS error must be observed as WaitingForVolumeSnapshot, not a failure")?;

    // The phase must still be Pending — the whole point of #198.
    let phase = status_json(&backups, prefix)
        .await
        .get("phase")
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    ensure!(
        phase == "Pending",
        "a transient VS error must hold the Snapshot Pending, got phase {phase:?}"
    );

    // The error "clears" (as external-snapshotter does) and the VS becomes ready.
    vs_api
        .patch_status(
            &vs_name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "status": { "readyToUse": true, "restoreSize": "64Mi", "error": null },
            })),
        )
        .await
        .context("mark VolumeSnapshot ready")?;

    // Staging recovers: SourceStaged=True and the staged PVC exists. (The staged PVC
    // can never bind — the VS has no real content — so the run pends at the mover;
    // recovery of STAGING is the asserted success condition.)
    wait_condition(&backups, prefix, "SourceStaged", "True")
        .await
        .context("staging must recover once the VolumeSnapshot is ready")?;
    let staged = pvcs.get_opt(&format!("{prefix}-src")).await?;
    ensure!(
        staged.is_some(),
        "the staged PVC must exist after staging recovered"
    );
    let phase = status_json(&backups, prefix)
        .await
        .get("phase")
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    ensure!(
        phase != "Failed",
        "the backup must not be Failed after staging recovered, got {phase:?}"
    );
    Ok(())
}

/// BOUNDED FAILURE: a VolumeSnapshot that is still erroring when
/// `spec.staging.timeout` passes fails the Snapshot terminally — with the specific
/// `VolumeSnapshotFailed` reason/Event, `Stalled=True`, no mover Job, and NO
/// misleading `InvalidSpec` Event (#198 defect 3).
#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:test)"]
async fn persistent_volumesnapshot_error_fails_at_the_staging_deadline() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    require_snapshot_stack(&client).await;

    let prefix = "e2e-sd-timeout";
    // Same discipline as the recover scenario: quiesce first, so the injected
    // "persistent" error can never be cleared by the real controller.
    scale_snapshot_controller(&client, 0)
        .await
        .expect("scale down snapshot-controller");
    let result = async {
        setup_scenario(
            &client,
            prefix,
            "staging-timeout",
            serde_json::json!({ "staging": { "timeout": "30s" } }),
        )
        .await?;
        timeout_body(&client, prefix).await
    }
    .await;
    let restored = scale_snapshot_controller(&client, 1).await;
    teardown_scenario(&client, prefix).await;
    restored.expect("restore snapshot-controller");
    result.expect("staging-deadline scenario");
}

async fn timeout_body(client: &kube::Client, prefix: &str) -> anyhow::Result<()> {
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let events: Api<Event> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let vs_api = volume_snapshots(client);
    let vs_name = format!("{prefix}-snap");

    // A persistent error: injected once and never cleared (the controller is down),
    // exactly what a genuinely broken class/driver looks like.
    inject_vs_status(
        &vs_api,
        &vs_name,
        serde_json::json!({
            "readyToUse": false,
            "error": { "message": TRANSIENT_409, "time": "2026-07-04T18:30:00Z" },
        }),
    )
    .await?;

    // The 30s staging deadline expires → terminal Failed.
    wait_phase(&backups, prefix, "Failed")
        .await
        .context("the Snapshot must fail once spec.staging.timeout passes")?;

    // The SourceStaged condition carries the specific reason + actionable message.
    let status = status_json(&backups, prefix).await;
    let conds = status
        .get("conditions")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let by_type = |t: &str| {
        conds
            .iter()
            .find(|c| c.get("type").and_then(|v| v.as_str()) == Some(t))
            .cloned()
            .unwrap_or_default()
    };
    let staged = by_type("SourceStaged");
    ensure!(
        staged.get("reason").and_then(|v| v.as_str()) == Some("VolumeSnapshotFailed"),
        "expected VolumeSnapshotFailed on SourceStaged, got {staged:?}"
    );
    let msg = staged
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    ensure!(
        msg.contains("the object has been modified") && msg.contains("spec.staging.timeout"),
        "the terminal message must carry the VS error and name the knob: {msg}"
    );
    // kstatus: terminal Failed is Stalled=True in the SAME status write (no extra
    // TerminalFailed pass needed).
    ensure!(
        by_type("Stalled").get("status").and_then(|v| v.as_str()) == Some("True"),
        "a staging-deadline failure must set Stalled=True: {conds:?}"
    );

    // One-shot discipline: no mover Job was ever minted.
    ensure!(
        jobs.get_opt(prefix).await?.is_none(),
        "no mover Job may exist for a staging-failed backup"
    );

    // Events: the SPECIFIC Warning is published…
    let regarding_backup = |e: &Event| {
        e.regarding.as_ref().is_some_and(|r| {
            r.kind.as_deref() == Some("Snapshot") && r.name.as_deref() == Some(prefix)
        })
    };
    wait_until(
        "VolumeSnapshotFailed Warning Event published",
        default_timeout(),
        poll_interval(),
        || async {
            let list = events.list(&ListParams::default()).await?;
            Ok(list
                .items
                .into_iter()
                .find(|e| {
                    regarding_backup(e) && e.reason.as_deref() == Some("VolumeSnapshotFailed")
                })
                .map(|_| ()))
        },
    )
    .await
    .context("the staging failure must publish its specific Warning Event")?;
    // …and the misleading InvalidSpec one is NOT (#198 defect 3: the old path
    // returned Error::Validation, whose generic mapping said "fix the spec").
    // Anchored AFTER the Failed phase + specific Event were observed, so the absence
    // is meaningful, not a not-yet-arrived race.
    let invalid_spec = events
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|e| regarding_backup(e) && e.reason.as_deref() == Some("InvalidSpec"))
        .count();
    if invalid_spec > 0 {
        bail!(
            "a staging failure must not emit the misleading InvalidSpec Event \
             ({invalid_spec} found) — there is no spec problem to fix"
        );
    }

    // Leak regression: a staging-phase failure must REAP the VolumeSnapshot it
    // already created (the VS is applied BEFORE the readyToUse deadline is
    // evaluated). Pre-fix code never stamped `status.staged` on a staging failure,
    // so every `status.staged.is_some()` cleanup gate skipped and the VS held a
    // backend snapshot until the CR itself was deleted.
    wait_until(
        "failed staging's VolumeSnapshot reaped",
        default_timeout(),
        poll_interval(),
        || async { Ok(vs_api.get_opt(&vs_name).await?.is_none().then_some(())) },
    )
    .await
    .context("the VolumeSnapshot of a staging-failed backup must be cleaned up")?;
    // And `status.staged` records the failed stage (`ready: false`) — the gate the
    // terminal-path cleanups key on.
    let staged = status_json(&backups, prefix)
        .await
        .get("staged")
        .cloned()
        .unwrap_or_default();
    ensure!(
        staged.get("ready").and_then(|v| v.as_bool()) == Some(false),
        "status.staged must be stamped (ready: false) on a staging failure: {staged}"
    );
    Ok(())
}
