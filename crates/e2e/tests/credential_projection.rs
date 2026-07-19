//! End-to-end credential projection: the opt-in `spec.credentialProjection` that
//! lets the operator copy a repository's credential Secret(s) into the namespace
//! where a mover Job runs, so users with many namespaces don't have to pre-create
//! the Secret everywhere.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`, skipping gracefully without a
//! cluster. Driven by `mise run //crates/e2e:test`. The decisive fixture is the
//! `kopiur-e2e-proj` namespace (`Need::ProjectionNs`): it has a source + dest PVC
//! but, unlike the other workload namespaces, **no** credentials Secret — so a
//! mover there can only run if the operator projects the repository's Secret in.
//! `credentialProjection` is a consumer-side opt-in, so it is exercised on each of
//! the three consumers (`SnapshotPolicy`, `Restore`, `Maintenance`).
//!
//! Scenarios, asserting real operator output:
//!
//! 1. **SnapshotPolicy projection ON → Snapshot succeeds where no creds Secret exists,
//!    and the copy is reclaimed when the run ends.** The operator projects a
//!    kopiur-managed `<backup>-creds-0` Secret, the mover runs to `Succeeded`, and the
//!    copy is then deleted and the reap stamped on `status.cleanup.credsReapedAt`.
//! 2. **SnapshotPolicy projection OFF → Snapshot blocks (guards the default).** The
//!    Secret is absent, so the Snapshot stays `Pending` with `CredentialsAvailable=False`.
//! 3. **Restore projection ON → Restore Completes.** A projection-on Snapshot seeds a
//!    snapshot; a projection-on `Restore` then restores it into the creds-less
//!    namespace, projecting its own `<restore>-restore-creds-0` Secret.
//! 4. **Maintenance projection ON → creds projected.** A projection-on `Maintenance`
//!    for a shared `ClusterRepository` gets `<maint>-maint-creds-0` projected (owned
//!    by the Maintenance) so its mover can run `kopia maintenance`.
//! 5. **Stable name across runs (#231 guard).** Two manual maintenance runs of ONE
//!    Maintenance leave exactly ONE projected Secret, named for the CR — the per-run
//!    naming of versions ≤ 0.7.1 minted a new copy per run, forever.
//! 6. **Copies do not accumulate across runs (#240 guard).** Two backups from one
//!    policy leave ZERO projected copies behind.
//! 7. **Deletion survives a policy deleted first (#255 guard).** A `deletionPolicy:
//!    Delete` Snapshot whose `SnapshotPolicy` is deleted BEFORE it still releases its
//!    cleanup finalizer — the delete Job re-projects against the opt-in pinned into
//!    `status.resolved.credentialProjection` at run time.
//!
//! Scenario 7 is the ordering the others structurally cannot catch: every one of them
//! deletes the Snapshot before the policy, keeping the recipe alive exactly as long as
//! the delete path happens to need it. That is not a property of the operator — it is a
//! property of the test order.
//!
//! A projected copy holds live repository credentials, so its lifetime is the mover
//! Job's, NOT the consuming CR's. Scenarios 1 and 6 are the ones that pin this: the
//! ownerRef assertions (`assert_projected_owned_by`) prove the copy is correctly owned,
//! which was true of every copy in a leaking cluster — a `Snapshot` is a per-run CR
//! retained for the whole GFS window, so waiting for ownerRef GC meant waiting months.
//! Assert the population, not the name.

#![cfg(all(unix, feature = "e2e"))]

use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::{Api, ResourceExt};
use serde::de::DeserializeOwned;

use k8s_openapi::api::core::v1::{Secret, ServiceAccount};
use k8s_openapi::api::rbac::v1::RoleBinding;

use kopiur_api::{ClusterRepository, Maintenance, Restore, Snapshot, SnapshotPolicy};
use kopiur_e2e::consts::{PROJECTION_NS, SECRET_S3_CREDS};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// The chart-minted mover identity (release name `kopiur` → `kopiur-mover`).
const MOVER_NAME: &str = "kopiur-mover";
/// In-cluster MinIO endpoint (plain HTTP via `tls.disableTls`).
const S3_ENDPOINT: &str = "minio.kopiur-e2e.svc.cluster.local:9000";

fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

/// A cluster-scoped S3 `ClusterRepository` whose creds live in the operator
/// namespace, opened to all namespaces. Cross-namespace credential projection is
/// fail-closed (ADR-0005 §8): it requires BOTH the consumer opt-in
/// (`spec.credentialProjection.enabled` on the `SnapshotPolicy`/`Restore`/
/// `Maintenance`) AND this owner-side allow (`credentialProjection.allowed: true`).
/// These scenarios exercise the projection-ON path, so the owner must allow it; the
/// fail-closed (allowed=false) path is covered in `adr_0004_0005.rs`.
fn s3_cluster_repository_json(name: &str, bucket: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "ClusterRepository",
        "metadata": { "name": name },
        "spec": {
            "backend": { "s3": {
                "bucket": bucket,
                "endpoint": S3_ENDPOINT,
                "region": "us-east-1",
                "tls": { "disableTls": true },
                "auth": { "secretRef": { "name": SECRET_S3_CREDS, "namespace": E2E_NAMESPACE } }
            }},
            "encryption": {
                "passwordSecretRef": {
                    "name": SECRET_S3_CREDS, "namespace": E2E_NAMESPACE, "key": "KOPIA_PASSWORD"
                }
            },
            "create": { "enabled": true },
            "allowedNamespaces": { "all": true },
            "credentialProjection": { "allowed": true }
        }
    })
}

/// A `SnapshotPolicy` whose `credentialProjection.enabled = project` decides whether
/// the operator copies the repo's creds into this namespace for its backup movers.
fn backup_config_json(ns: &str, name: &str, repo_name: &str, project: bool) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "repository": { "kind": "ClusterRepository", "name": repo_name },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "credentialProjection": { "enabled": project }
        }
    })
}

fn backup_json(ns: &str, name: &str, config: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": ns },
        "spec": { "policyRef": { "name": config }, "deletionPolicy": "Retain" }
    })
}

/// A `Restore` whose `credentialProjection.enabled = project` decides whether the
/// operator copies the repo's creds into this namespace for the restore mover.
fn restore_json(
    ns: &str,
    name: &str,
    repo: &str,
    backup: &str,
    project: bool,
) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "repository": { "kind": "ClusterRepository", "name": repo },
            "source": { "snapshotRef": { "name": backup } },
            "target": { "pvcRef": { "name": "e2e-dst" } },
            "credentialProjection": { "enabled": project }
        }
    })
}

/// A `Maintenance` whose `credentialProjection.enabled = project` decides whether
/// the operator copies the repo's creds into this namespace for the maintenance
/// mover. `quick_cron` schedules the quick tier — pass a far-future cron when only
/// manual runs should drive movers (a deterministic run count).
fn maintenance_json(
    ns: &str,
    name: &str,
    repo: &str,
    project: bool,
    quick_cron: &str,
) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Maintenance",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "repository": { "kind": "ClusterRepository", "name": repo },
            "ownership": { "owner": "kopiur-e2e-proj", "takeoverPolicy": "Force" },
            "schedule": { "quick": { "cron": quick_cron }, "full": { "cron": "0 3 * * 0" } },
            "credentialProjection": { "enabled": project }
        }
    })
}

/// Wait for a kopiur-managed projected credential Secret in `ns` that is
/// controller-owned by the consuming CR `owner_kind`/`owner_name` and carries the
/// password key. Matched by **ownerReference**, not by name: the stable per-CR name
/// varies by consumer kind (`<snapshot>-creds-N`, `<restore>-restore-creds-N`,
/// `<maint>-maint-creds-N`). The valid same-namespace controller ownerRef is the GC
/// contract (Kubernetes reaps the copy with its owner).
async fn assert_projected_owned_by(
    client: &kube::Client,
    ns: &str,
    owner_kind: &str,
    owner_name: &str,
) {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    wait_until(
        &format!("projected Secret owned by {owner_kind}/{owner_name} in {ns}"),
        default_timeout(),
        poll_interval(),
        || async {
            let list = secrets.list(&Default::default()).await?;
            let found = list.items.into_iter().any(|s| {
                let owned = s.metadata.owner_references.as_ref().is_some_and(|os| {
                    os.iter().any(|o| {
                        o.kind == owner_kind && o.name == owner_name && o.controller == Some(true)
                    })
                });
                let managed = s
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("app.kubernetes.io/managed-by"))
                    .map(String::as_str)
                    == Some("kopiur");
                let has_pw = s
                    .data
                    .as_ref()
                    .is_some_and(|d| d.contains_key("KOPIA_PASSWORD"));
                owned && managed && has_pw
            });
            Ok(found.then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| {
        panic!("{owner_kind} {owner_name} must project a credential Secret into {ns}: {e}")
    });
}

/// Poll a namespaced CR until `status.phase == want_phase`.
async fn wait_phase<K>(api: &Api<K>, name: &str, want_phase: &str) -> anyhow::Result<()>
where
    K: kube::Resource + Clone + DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    wait_until(
        &format!("{name} phase={want_phase}"),
        default_timeout(),
        poll_interval(),
        || async {
            match api.get_opt(name).await? {
                Some(obj) => {
                    let v = serde_json::to_value(&obj).unwrap_or_default();
                    let phase = v
                        .get("status")
                        .and_then(|s| s.get("phase"))
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    Ok((phase == want_phase).then_some(()))
                }
                None => Ok(None),
            }
        },
    )
    .await
}

async fn status_json<K>(api: &Api<K>, name: &str) -> serde_json::Value
where
    K: kube::Resource + Clone + DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    match api.get_opt(name).await.ok().flatten() {
        Some(obj) => serde_json::to_value(&obj)
            .ok()
            .and_then(|v| v.get("status").cloned())
            .unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

async fn assert_mover_rbac_minted(client: &kube::Client, ns: &str) {
    let sas: Api<ServiceAccount> = Api::namespaced(client.clone(), ns);
    let rbs: Api<RoleBinding> = Api::namespaced(client.clone(), ns);
    wait_until(
        &format!("ServiceAccount {ns}/{MOVER_NAME} minted"),
        default_timeout(),
        poll_interval(),
        || async { sas.get_opt(MOVER_NAME).await.map(|o| o.map(|_| ())) },
    )
    .await
    .unwrap_or_else(|e| panic!("mover ServiceAccount must be minted in {ns}: {e}"));
    wait_until(
        &format!("RoleBinding {ns}/{MOVER_NAME} minted"),
        default_timeout(),
        poll_interval(),
        || async { rbs.get_opt(MOVER_NAME).await.map(|o| o.map(|_| ())) },
    )
    .await
    .unwrap_or_else(|e| panic!("mover RoleBinding must be minted in {ns}: {e}"));
}

/// **Projection ON.** A `ClusterRepository` with `credentialProjection.enabled`
/// backs a Snapshot in a namespace that has NO creds Secret. The operator projects a
/// kopiur-managed copy there (owned by the Snapshot), the mover runs to `Succeeded`,
/// and deleting the Snapshot garbage-collects the projected Secret.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn projection_enables_backup_in_a_namespace_without_creds() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::ProjectionNs])
        .await
        .expect("provision MinIO + projection namespace (source PVC, no creds Secret)");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), PROJECTION_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), PROJECTION_NS);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), PROJECTION_NS);

    let crepo = "e2e-proj-crepo";
    let cfg = "e2e-proj-cfg";
    let backup = "e2e-proj-backup";
    let projected = format!("{backup}-creds-0");

    // Sanity: the projection namespace really has no source creds Secret.
    assert!(
        secrets.get_opt(SECRET_S3_CREDS).await.unwrap().is_none(),
        "projection namespace must start without the creds Secret"
    );

    // 1. ClusterRepository bootstraps against S3 → Ready.
    crepos
        .create(
            &PostParams::default(),
            &cr(s3_cluster_repository_json(crepo, "kopiur-proj-crepo")),
        )
        .await
        .expect("create S3 ClusterRepository");
    wait_phase(&crepos, crepo, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    // 2. SnapshotPolicy (projection ON) + Snapshot in the creds-less projection namespace.
    configs
        .create(
            &PostParams::default(),
            &cr(backup_config_json(PROJECTION_NS, cfg, crepo, true)),
        )
        .await
        .expect("create SnapshotPolicy with projection");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json(PROJECTION_NS, backup, cfg)),
        )
        .await
        .expect("create Snapshot");

    assert_mover_rbac_minted(&client, PROJECTION_NS).await;

    // 3. The operator projected the credential Secret into the namespace, owned by
    //    the Snapshot and labeled kopiur-managed, with the password key copied. The
    //    copy is transient — it exists only while the mover Job can still load it, and
    //    step 5 asserts it is gone once the run ends — so this reads it while the run
    //    is live (projection precedes Job creation; the Job then runs for seconds).
    let proj = wait_until(
        &format!("projected Secret {PROJECTION_NS}/{projected}"),
        default_timeout(),
        poll_interval(),
        || async { secrets.get_opt(&projected).await },
    )
    .await
    .expect("the operator must project the credential Secret into the mover namespace");
    let owners = proj.metadata.owner_references.unwrap_or_default();
    assert!(
        owners
            .iter()
            .any(|o| o.kind == "Snapshot" && o.name == backup),
        "projected Secret must be owned by its Snapshot (valid same-namespace ownerRef for GC)"
    );
    let labels = proj.metadata.labels.unwrap_or_default();
    assert_eq!(
        labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str),
        Some("kopiur"),
        "projected Secret must be labeled kopiur-managed"
    );
    assert!(
        proj.data
            .as_ref()
            .is_some_and(|d| d.contains_key("KOPIA_PASSWORD")),
        "projected Secret must carry the repository password key"
    );

    // 4. The Snapshot completes — proving the projected creds actually worked.
    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("Snapshot using projected credentials should reach Succeeded");

    // 5. The copy is RECLAIMED now the run is over (#240). This is the assertion the
    //    ownerRef check in step 3 cannot make, and its absence is exactly how the leak
    //    shipped green: the ownerRef was always valid, and the copy was always owned —
    //    by a Snapshot that is retained for the whole GFS window, so GC would not have
    //    come for it for months. A projected copy holds live repository credentials;
    //    its life is the mover Job's, not the CR's.
    wait_until(
        &format!("projected Secret {PROJECTION_NS}/{projected} to be reclaimed"),
        default_timeout(),
        poll_interval(),
        || async {
            secrets
                .get_opt(&projected)
                .await
                .map(|s| if s.is_none() { Some(()) } else { None })
        },
    )
    .await
    .expect("the projected credential copy must be reclaimed once the run is terminal");

    let done = backups.get(backup).await.expect("re-read Snapshot");
    let reaped = done
        .status
        .as_ref()
        .and_then(|s| s.cleanup.as_ref())
        .and_then(|c| c.creds_reaped_at.as_ref());
    assert!(
        reaped.is_some(),
        "the reap must be stamped on status.cleanup.credsReapedAt — the stamp is what \
         makes every later steady-state reconcile of this terminal Snapshot free"
    );

    backups
        .delete(backup, &DeleteParams::default())
        .await
        .expect("delete Snapshot");
    let _ = configs.delete(cfg, &DeleteParams::default()).await;
    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}

/// **The leak guard (#240).** Two backups from one policy, each into a creds-less
/// namespace via projection. Afterwards the namespace must hold **zero** projected
/// copies — the count is flat across runs, not one more per run.
///
/// This asserts the property that actually failed. The pre-existing projection test
/// asserts that the copy exists and is correctly owned, and both were true for every
/// one of the hundreds of copies a leaking cluster accumulated: the copy's NAME was
/// stable per CR, but a `Snapshot` IS the per-run object, so one stable copy per CR
/// was still one live credential copy per backup. Assert the population, not the name.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn projected_copies_do_not_accumulate_across_runs() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::ProjectionNs])
        .await
        .expect("provision MinIO + projection namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), PROJECTION_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), PROJECTION_NS);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), PROJECTION_NS);

    let crepo = "e2e-leak-crepo";
    let cfg = "e2e-leak-cfg";

    crepos
        .create(
            &PostParams::default(),
            &cr(s3_cluster_repository_json(crepo, "kopiur-leak-crepo")),
        )
        .await
        .expect("create S3 ClusterRepository");
    wait_phase(&crepos, crepo, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");
    configs
        .create(
            &PostParams::default(),
            &cr(backup_config_json(PROJECTION_NS, cfg, crepo, true)),
        )
        .await
        .expect("create SnapshotPolicy with projection");

    // Two runs, exactly as a SnapshotSchedule would mint them: a fresh per-run CR each
    // time. Under the bug this leaves two live credential copies behind, and a third
    // run would leave three.
    for run in ["e2e-leak-run-1", "e2e-leak-run-2"] {
        backups
            .create(
                &PostParams::default(),
                &cr(backup_json(PROJECTION_NS, run, cfg)),
            )
            .await
            .unwrap_or_else(|e| panic!("create Snapshot {run}: {e}"));
        wait_phase(&backups, run, "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("Snapshot {run} should reach Succeeded: {e}"));
    }

    // Both runs are terminal, so every copy they projected is dead weight.
    //
    // Counted over THIS test's runs rather than every projected copy in the namespace:
    // a `Maintenance`/verification copy is one-per-CR and is deliberately kept between
    // its cron slots, so a namespace-wide "zero copies" assertion would silently depend
    // on which other scenarios in this file ran first. The property that must hold is
    // the per-RUN one — N backups leave zero copies behind, for any N.
    let survivors = wait_until(
        "the runs' projected credential copies to be reclaimed",
        default_timeout(),
        poll_interval(),
        || async {
            let lp = kube::api::ListParams::default().labels(
                "app.kubernetes.io/managed-by=kopiur,app.kubernetes.io/component=credentials",
            );
            secrets.list(&lp).await.map(|list| {
                let names: Vec<String> = list
                    .items
                    .iter()
                    .map(|s| s.name_any())
                    .filter(|n| n.starts_with("e2e-leak-run-"))
                    .collect();
                names.is_empty().then_some(names)
            })
        },
    )
    .await;

    assert!(
        survivors.is_ok(),
        "a backup's projected credential copy must not survive its run — after both \
         Snapshots reached Succeeded the namespace still holds their copies. That is the \
         leak: one live copy of the repository password per backup, per namespace, kept \
         for the whole GFS retention window because the copy is owned by a per-run CR."
    );

    let _ = backups
        .delete("e2e-leak-run-1", &DeleteParams::default())
        .await;
    let _ = backups
        .delete("e2e-leak-run-2", &DeleteParams::default())
        .await;
    let _ = configs.delete(cfg, &DeleteParams::default()).await;
    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}

/// **The stuck-finalizer guard (#255), now via the policy-cascade Delete mode.** Delete
/// the `SnapshotPolicy` and let its `onPolicyDelete: Delete` cascade remove the child,
/// and a `deletionPolicy: Delete` Snapshot must still finish deleting its kopia snapshot
/// with the recipe already gone.
///
/// On this branch a config-labeled child is owned by the policy-deletion cascade, so the
/// old flow (delete the policy, then explicitly delete the Snapshot) no longer applies:
/// under the default `onPolicyDelete: Retain` the cascade would quiet-RELEASE the child
/// without a kopia delete (a different guarantee). Setting `onPolicyDelete: Delete` makes
/// the cascade issue an UNSTAMPED external delete for the child — the same external
/// deletion a bare `kubectl delete snapshot` would, but triggered BY the policy's own
/// removal — so this one scenario exercises BOTH #255 and the cascade Delete path
/// end-to-end. Nothing forces a user to delete the Snapshot before the policy. Once the
/// run succeeds its projected copy is reaped, so the delete Job must re-project — and the
/// opt-in that authorizes that lives ONLY on the recipe. Reading an absent recipe as
/// "projection off" sent the delete Job hunting for the ClusterRepository's canonical
/// Secret name in the workload namespace, which projection exists precisely because
/// nobody put there: `MissingDependency` every 30s, finalizer held, forever.
///
/// The fix pins the opt-in into `status.resolved.credentialProjection` at run time, so
/// assert the pin too — without it this test would still pass the day someone
/// "fixes" the symptom by defaulting a gone recipe to projection-on, which would
/// silently mint credentials in namespaces that never opted in.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn snapshot_deletion_survives_a_policy_deleted_first() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::ProjectionNs])
        .await
        .expect("provision MinIO + projection namespace (source PVC, no creds Secret)");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), PROJECTION_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), PROJECTION_NS);

    let crepo = "e2e-proj-orphan-crepo";
    let cfg = "e2e-proj-orphan-cfg";
    let backup = "e2e-proj-orphan-backup";

    crepos
        .create(
            &PostParams::default(),
            &cr(s3_cluster_repository_json(crepo, "kopiur-proj-orphan")),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, crepo, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    // onPolicyDelete: Delete — deleting the policy issues an UNSTAMPED external delete
    // for the config-labeled child (not the default quiet-retain cascade), so the child's
    // finalizer runs the real kopia delete with the recipe gone: the #255 property, now
    // reached through the cascade's Delete mode.
    let mut cfg_spec = backup_config_json(PROJECTION_NS, cfg, crepo, true);
    cfg_spec["spec"]["deletion"] = serde_json::json!({ "onPolicyDelete": "Delete" });
    configs
        .create(&PostParams::default(), &cr(cfg_spec))
        .await
        .expect("create projection-on SnapshotPolicy with onPolicyDelete: Delete");
    // deletionPolicy: Delete is what arms the cleanup finalizer — the whole point.
    let mut backup_spec = backup_json(PROJECTION_NS, backup, cfg);
    backup_spec["spec"]["deletionPolicy"] = serde_json::json!("Delete");
    backups
        .create(&PostParams::default(), &cr(backup_spec))
        .await
        .expect("create deletable Snapshot");
    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("Snapshot should reach Succeeded via projected credentials");

    // The run pinned the opt-in it actually executed under. This is the state the
    // delete path reads once the recipe is gone.
    let status = status_json(&backups, backup).await;
    assert_eq!(
        status
            .get("resolved")
            .and_then(|r| r.get("credentialProjection"))
            .and_then(|p| p.get("enabled"))
            .and_then(|e| e.as_bool()),
        Some(true),
        "the run must pin its credentialProjection opt-in into status.resolved — the \
         deletion path has no other honest source once the SnapshotPolicy is gone: {status:#}"
    );

    // The reap is what makes this hard: the run's projected copy is GONE, so the
    // delete Job must re-project from scratch rather than reuse a lingering Secret.
    wait_until(
        &format!("{backup} status.cleanup.credsReapedAt stamped"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&backups, backup).await;
            Ok(s.get("cleanup")
                .and_then(|c| c.get("credsReapedAt"))
                .is_some()
                .then_some(()))
        },
    )
    .await
    .expect("the projected copy must be reaped once the run is terminal");

    // Delete ONLY the recipe. Its `onPolicyDelete: Delete` cascade issues an unstamped
    // external delete for the config-labeled child — the test never touches the Snapshot.
    configs
        .delete(cfg, &DeleteParams::default())
        .await
        .expect("delete the SnapshotPolicy (its Delete cascade removes the child)");
    wait_until(
        &format!("SnapshotPolicy {cfg} gone"),
        default_timeout(),
        poll_interval(),
        || async { Ok(configs.get_opt(cfg).await?.is_none().then_some(())) },
    )
    .await
    .expect("the SnapshotPolicy should delete cleanly once its cascade drains");

    // The child is removed by the cascade, with the recipe already gone: it must
    // re-project creds against the PINNED opt-in, run its delete Job, and release the
    // finalizer. A released `snapshot-cleanup` finalizer (CR gone) is the kopia-side
    // proof — a `deletionPolicy: Delete` external deletion under the breaker threshold
    // resolves to DeleteSnapshot, so the finalizer only clears once the real kopia
    // snapshot delete succeeded (an orphan/retain would never contact the repo).
    wait_until(
        &format!("{backup} removed by the onPolicyDelete cascade"),
        default_timeout(),
        poll_interval(),
        || async { Ok(backups.get_opt(backup).await?.is_none().then_some(())) },
    )
    .await
    .expect(
        "the Snapshot must finish deleting with its SnapshotPolicy already gone — before \
         #255 it hung on kopiur.home-operations.com/snapshot-cleanup forever, because the \
         delete path read the absent recipe as 'projection off' and then demanded a \
         namespace-local Secret that projection was the only thing supplying. Here the \
         cascade's Delete mode is what issues the external delete, so this also covers the \
         policy-cascade Delete path end-to-end",
    );

    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}

/// **Projection OFF (default).** Without `credentialProjection`, a Snapshot in a
/// namespace lacking the creds Secret blocks on `CredentialsAvailable=False` and
/// never launches a mover — the self-managed default is unchanged.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn without_projection_a_backup_blocks_on_missing_credentials() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::ProjectionNs])
        .await
        .expect("provision MinIO + projection namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), PROJECTION_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), PROJECTION_NS);

    let crepo = "e2e-proj-off-crepo";
    let cfg = "e2e-proj-off-cfg";
    let backup = "e2e-proj-off-backup";

    // ClusterRepository → Ready (bootstrap runs in the operator namespace where its
    // Secret lives).
    crepos
        .create(
            &PostParams::default(),
            &cr(s3_cluster_repository_json(crepo, "kopiur-proj-off")),
        )
        .await
        .expect("create S3 ClusterRepository");
    wait_phase(&crepos, crepo, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    // SnapshotPolicy with projection OFF (the default), in the creds-less namespace.
    configs
        .create(
            &PostParams::default(),
            &cr(backup_config_json(PROJECTION_NS, cfg, crepo, false)),
        )
        .await
        .expect("create SnapshotPolicy without projection");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json(PROJECTION_NS, backup, cfg)),
        )
        .await
        .expect("create Snapshot");

    // The Snapshot blocks Pending with an actionable CredentialsAvailable=False — it
    // must NOT progress to Running/Succeeded without the Secret.
    wait_until(
        &format!("{backup} reports CredentialsAvailable=False"),
        default_timeout(),
        poll_interval(),
        || async {
            let status = status_json(&backups, backup).await;
            let blocked = status
                .get("conditions")
                .and_then(|c| c.as_array())
                .map(|conds| {
                    conds.iter().any(|c| {
                        c.get("type").and_then(|t| t.as_str()) == Some("CredentialsAvailable")
                            && c.get("status").and_then(|s| s.as_str()) == Some("False")
                    })
                })
                .unwrap_or(false);
            Ok(blocked.then_some(()))
        },
    )
    .await
    .expect(
        "a non-projecting Snapshot must surface CredentialsAvailable=False when the Secret is absent",
    );
    let phase = status_json(&backups, backup)
        .await
        .get("phase")
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    assert_ne!(
        phase, "Succeeded",
        "the blocked Snapshot must not have succeeded"
    );
    assert_ne!(
        phase, "Running",
        "the blocked Snapshot must not have launched a mover"
    );

    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = configs.delete(cfg, &DeleteParams::default()).await;
    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}

/// **Restore projection.** A `Restore` with `credentialProjection.enabled: true`
/// restores a snapshot into the creds-less projection namespace: the operator
/// projects the repo's creds for the restore mover, and the restore Completes. We
/// first run a (projection-on) Snapshot to produce a snapshot to restore.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn projection_enables_restore_in_a_namespace_without_creds() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::ProjectionNs])
        .await
        .expect("provision MinIO + projection namespace (source + dest PVC, no creds)");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), PROJECTION_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), PROJECTION_NS);
    let restores: Api<Restore> = Api::namespaced(client.clone(), PROJECTION_NS);

    let crepo = "e2e-proj-restore-crepo";
    let cfg = "e2e-proj-restore-cfg";
    let backup = "e2e-proj-restore-backup";
    let restore = "e2e-proj-restore";

    // Repo + a projection-on backup to create a snapshot to restore.
    crepos
        .create(
            &PostParams::default(),
            &cr(s3_cluster_repository_json(crepo, "kopiur-proj-restore")),
        )
        .await
        .expect("create S3 ClusterRepository");
    wait_phase(&crepos, crepo, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");
    configs
        .create(
            &PostParams::default(),
            &cr(backup_config_json(PROJECTION_NS, cfg, crepo, true)),
        )
        .await
        .expect("create SnapshotPolicy with projection");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json(PROJECTION_NS, backup, cfg)),
        )
        .await
        .expect("create Snapshot");
    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("seed Snapshot should Succeed");

    // The Restore (projection ON) into the creds-less namespace.
    restores
        .create(
            &PostParams::default(),
            &cr(restore_json(PROJECTION_NS, restore, crepo, backup, true)),
        )
        .await
        .expect("create Restore with projection");

    // The operator projects the restore mover's creds (owned by the Restore)...
    assert_projected_owned_by(&client, PROJECTION_NS, "Restore", restore).await;
    // ...and the restore runs to completion using them.
    wait_phase(&restores, restore, "Completed")
        .await
        .expect("Restore using projected credentials should reach Completed");

    let _ = restores.delete(restore, &DeleteParams::default()).await;
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = configs.delete(cfg, &DeleteParams::default()).await;
    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}

/// **Maintenance projection.** A `Maintenance` with `credentialProjection.enabled:
/// true` for a shared `ClusterRepository`, in the creds-less projection namespace,
/// gets its credential Secret projected (owned by the Maintenance) so its mover
/// can run `kopia maintenance` — the maintenance path is the one most likely to
/// land in a namespace lacking the Secret.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn projection_enables_maintenance_in_a_namespace_without_creds() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::ProjectionNs])
        .await
        .expect("provision MinIO + projection namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), PROJECTION_NS);

    let crepo = "e2e-proj-maint-crepo";
    let maint = "e2e-proj-maint";

    crepos
        .create(
            &PostParams::default(),
            &cr(s3_cluster_repository_json(crepo, "kopiur-proj-maint")),
        )
        .await
        .expect("create S3 ClusterRepository");
    wait_phase(&crepos, crepo, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    maints
        .create(
            &PostParams::default(),
            &cr(maintenance_json(
                PROJECTION_NS,
                maint,
                crepo,
                true,
                "*/5 * * * *",
            )),
        )
        .await
        .expect("create Maintenance with projection");

    // The maintenance mover runs in the creds-less namespace; the operator mints
    // the mover RBAC and projects the credential Secret (owned by the Maintenance)
    // so the maintenance Job can load it — without projection this path would block
    // on a missing Secret.
    assert_mover_rbac_minted(&client, PROJECTION_NS).await;
    assert_projected_owned_by(&client, PROJECTION_NS, "Maintenance", maint).await;

    let _ = maints.delete(maint, &DeleteParams::default()).await;
    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}

/// Request a manual maintenance run via the run-requested/run-mode annotations
/// and wait until `status.manualRun` pins THIS request (`requestedAt` equality —
/// the controller keys manual runs by the annotation value) with
/// `phase: Succeeded`.
async fn run_manual_maintenance(maints: &Api<Maintenance>, name: &str) {
    let requested = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let patch = serde_json::json!({ "metadata": { "annotations": {
        kopiur_api::consts::RUN_REQUESTED_ANNOTATION: requested,
        kopiur_api::consts::RUN_MODE_ANNOTATION: "quick",
    }}});
    maints
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .expect("annotate Maintenance with a manual run request");
    wait_until(
        &format!("{name} manualRun {requested} Succeeded"),
        default_timeout(),
        poll_interval(),
        || async {
            let status = status_json(maints, name).await;
            let m = status.get("manualRun");
            let done = m
                .and_then(|m| m.get("requestedAt"))
                .and_then(|v| v.as_str())
                == Some(requested.as_str())
                && m.and_then(|m| m.get("phase")).and_then(|v| v.as_str()) == Some("Succeeded");
            Ok(done.then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("manual maintenance run {requested} must succeed: {e}"));
}

/// **Stable projected-Secret name across runs (#231 regression guard).** Two manual
/// maintenance runs of ONE projection-enabled `Maintenance` must leave exactly ONE
/// projected Secret, named for the CR (`<maint>-maint-creds-0`) and carrying the
/// stable-scope marker label. Operator versions ≤ 0.7.1 named the copy after each
/// per-run mover Job, so this test observed TWO Secrets there — one per run,
/// accumulating forever under the long-lived CR.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn maintenance_reuses_one_stable_projected_secret_across_runs() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::ProjectionNs])
        .await
        .expect("provision MinIO + projection namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), PROJECTION_NS);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), PROJECTION_NS);

    let crepo = "e2e-proj-stable-crepo";
    let maint = "e2e-proj-stable";

    crepos
        .create(
            &PostParams::default(),
            &cr(s3_cluster_repository_json(crepo, "kopiur-proj-stable")),
        )
        .await
        .expect("create S3 ClusterRepository");
    wait_phase(&crepos, crepo, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    // Far-future quick cron (Jan 1, 03:00): ONLY the manual runs below spawn
    // movers, so the projected-Secret count is deterministic.
    maints
        .create(
            &PostParams::default(),
            &cr(maintenance_json(
                PROJECTION_NS,
                maint,
                crepo,
                true,
                "0 3 1 1 *",
            )),
        )
        .await
        .expect("create Maintenance with projection and a far-future cron");

    // Two sequential manual runs. Each projects/refreshes the credential copy.
    run_manual_maintenance(&maints, maint).await;
    run_manual_maintenance(&maints, maint).await;

    // Exactly ONE projected Secret is controller-owned by this Maintenance —
    // scoped by ownerRef, not namespace-wide (sibling scenarios share the
    // namespace) — named for the CR and carrying the stable-scope marker.
    let owned: Vec<Secret> = secrets
        .list(&Default::default())
        .await
        .expect("list Secrets in the projection namespace")
        .items
        .into_iter()
        .filter(|s| {
            s.metadata.owner_references.as_ref().is_some_and(|os| {
                os.iter().any(|o| {
                    o.kind == "Maintenance" && o.name == maint && o.controller == Some(true)
                })
            })
        })
        .collect();
    let names: Vec<_> = owned
        .iter()
        .map(|s| s.metadata.name.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        vec![format!("{maint}-maint-creds-0")],
        "two runs must converge on ONE stable projected Secret (pre-#231 versions \
         left one per-run copy each)"
    );
    assert_eq!(
        owned[0]
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("kopiur.home-operations.com/creds-scope"))
            .map(String::as_str),
        Some("cr"),
        "the stable copy must carry the creds-scope marker (what exempts it from \
         the legacy sweep)"
    );

    let _ = maints.delete(maint, &DeleteParams::default()).await;
    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}
