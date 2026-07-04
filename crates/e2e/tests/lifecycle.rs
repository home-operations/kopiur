//! End-to-end lifecycle scenarios against a Helm-deployed operator in kind.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`, skipping gracefully without
//! a cluster. Driven by `mise run //crates/e2e:test`, which builds + loads the images,
//! installs the chart (webhook disabled — its admission logic is covered by the
//! unit/integration tiers), provisions a hostPath-backed repo PVC visible to both
//! the controller and the mover Jobs, and a source PVC pre-populated with known
//! data. Run:
//!
//! ```text
//! mise run //crates/e2e:test
//! ```
//!
//! These tests assert on real operator output: a Repository reaching Ready, a
//! Snapshot reaching Succeeded with a real kopia snapshot id, a Restore Completed,
//! schedule-driven Snapshot creation, finalizer-driven snapshot deletion, and a
//! Maintenance lease claim — across six of the seven CRDs.

#![cfg(all(unix, feature = "e2e"))]

use std::time::Duration;

use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, ResourceExt};
use serde::de::DeserializeOwned;

use k8s_openapi::api::core::v1::ServiceAccount;
use k8s_openapi::api::rbac::v1::RoleBinding;

use kopiur_api::{
    ClusterRepository, Maintenance, Repository, Restore, Snapshot, SnapshotPolicy, SnapshotSchedule,
};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, annotate_namespace, apply_secret, default_timeout,
    ensure_namespace, poll_interval, scrape_controller_metrics, wait_until,
};

/// Deserialize a CR from a JSON literal into its typed kube object.
fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

/// The repository password Secret the chart-installed operator reads.
const CREDS_SECRET: &str = "kopia-creds";

fn repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": {
                "passwordSecretRef": { "name": CREDS_SECRET, "key": "KOPIA_PASSWORD" }
            },
            "create": { "enabled": true }
        }
    })
}

fn backup_config_json(name: &str, repo: &str, src_pvc: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": repo },
            "sources": [ { "pvc": { "name": src_pvc } } ],
            // src_pvc is always a statically-provisioned (non-CSI) hostPath PVC in
            // this suite; copyMethod now defaults to Snapshot, which would fail
            // preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 }
        }
    })
}

/// A cluster-scoped `ClusterRepository` pointing at the same hostPath repo the
/// namespaced `Repository` uses. Secret refs MUST carry an explicit namespace
/// (cluster-scoped resources can't infer one), and `allowedNamespaces` opens it
/// to the e2e namespace.
fn cluster_repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "ClusterRepository",
        "metadata": { "name": name },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": {
                "passwordSecretRef": {
                    "name": CREDS_SECRET, "namespace": E2E_NAMESPACE, "key": "KOPIA_PASSWORD"
                }
            },
            "create": { "enabled": true },
            "allowedNamespaces": { "all": true }
        }
    })
}

/// A `SnapshotPolicy` whose repository ref is a `ClusterRepository` (cluster-scoped:
/// note no `namespace` on the ref — that is the whole point of this scenario).
fn cluster_backup_config_json(name: &str, crepo: &str, src_pvc: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "ClusterRepository", "name": crepo },
            "sources": [ { "pvc": { "name": src_pvc } } ],
            // src_pvc is always a statically-provisioned (non-CSI) hostPath PVC in
            // this suite; copyMethod now defaults to Snapshot, which would fail
            // preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 }
        }
    })
}

fn backup_json(name: &str, config: &str, deletion_policy: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "policyRef": { "name": config },
            "deletionPolicy": deletion_policy
        }
    })
}

/// A `Maintenance` (always namespaced) referencing a repository of the given
/// `repo_kind` (`Repository`/`ClusterRepository`) and `repo_name`. `Force`
/// takeover so it claims the lease without waiting on another holder.
fn maintenance_json(name: &str, repo_kind: &str, repo_name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Maintenance",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": repo_kind, "name": repo_name },
            "schedule": {
                "quick": { "cron": "*/5 * * * *" },
                "full": { "cron": "0 3 * * 0" }
            },
            "ownership": { "owner": "kopiur-e2e", "takeoverPolicy": "Force" }
        }
    })
}

/// Poll a namespaced CR until `pred(status_json)` is true, returning the object.
async fn wait_phase<K>(api: &Api<K>, name: &str, want_phase: &str) -> anyhow::Result<()>
where
    K: kube::Resource + Clone + DeserializeOwned + std::fmt::Debug,
    K: serde::Serialize,
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
                    if phase == want_phase {
                        Ok(Some(()))
                    } else {
                        Ok(None)
                    }
                }
                None => Ok(None),
            }
        },
    )
    .await
}

/// Read a CR's `status` as JSON (or `null` if absent).
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

/// Extract a status condition's `status` ("True"/"False"/...) by `type`, or
/// `None` if the condition is absent.
fn condition_status(status: &serde_json::Value, type_: &str) -> Option<String> {
    status
        .get("conditions")
        .and_then(|c| c.as_array())?
        .iter()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some(type_))
        .and_then(|c| c.get("status").and_then(|s| s.as_str()))
        .map(str::to_string)
}

/// Poll a CR (namespaced or cluster-scoped via the passed `Api`) until its
/// `status.conditions[type=type_].status` equals `want`.
async fn wait_condition<K>(api: &Api<K>, name: &str, type_: &str, want: &str) -> anyhow::Result<()>
where
    K: kube::Resource + Clone + DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    wait_until(
        &format!("{name} {type_}={want}"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(api, name).await;
            Ok((condition_status(&s, type_).as_deref() == Some(want)).then_some(()))
        },
    )
    .await
}

/// The headline scenario: Repository → Snapshot (real kopia snapshot) → Restore →
/// finalizer-driven Delete. Proves the entire data path end-to-end.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn backup_restore_delete_lifecycle() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // 1. Repository becomes Ready (controller connects/creates the kopia repo).
    repos
        .create(&PostParams::default(), &cr(repository_json("e2e-repo")))
        .await
        .expect("create Repository");
    wait_phase(&repos, "e2e-repo", "Ready")
        .await
        .expect("Repository should reach Ready");

    // 2. SnapshotPolicy + Snapshot → mover Job → Succeeded with a real snapshot id.
    configs
        .create(
            &PostParams::default(),
            &cr(backup_config_json("e2e-cfg", "e2e-repo", "e2e-src")),
        )
        .await
        .expect("create SnapshotPolicy");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-backup", "e2e-cfg", "Retain")),
        )
        .await
        .expect("create Snapshot");
    wait_phase(&backups, "e2e-backup", "Succeeded")
        .await
        .expect("Snapshot should reach Succeeded");
    let bstatus = status_json(&backups, "e2e-backup").await;
    let snap_id = bstatus
        .get("snapshot")
        .and_then(|s| s.get("kopiaSnapshotID"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        !snap_id.is_empty(),
        "Snapshot status must carry a real kopia snapshot id, got {bstatus}"
    );
    // status.logTail is written by the mover at the terminal transition with the
    // documented `Snapshot created: <id>` line (it was an inert, never-written
    // field before — this guards the wiring end-to-end).
    assert_eq!(
        bstatus.get("logTail").and_then(|v| v.as_str()),
        Some(format!("Snapshot created: {snap_id}").as_str()),
        "status.logTail must carry the snapshot-created line; got {bstatus}"
    );

    // 3. Restore that Snapshot into a target PVC → Completed.
    let restore = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "e2e-restore", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-repo" },
            "source": { "snapshotRef": { "name": "e2e-backup" } },
            "target": { "pvcRef": { "name": "e2e-dst" } }
        }
    });
    restores
        .create(&PostParams::default(), &cr(restore))
        .await
        .expect("create Restore");
    wait_phase(&restores, "e2e-restore", "Completed")
        .await
        .expect("Restore should reach Completed");
    // Same logTail wiring on the Restore side: the mover writes the restored
    // snapshot id at the terminal transition.
    let rstatus = status_json(&restores, "e2e-restore").await;
    assert_eq!(
        rstatus.get("logTail").and_then(|v| v.as_str()),
        Some(format!("Restore completed: snapshot {snap_id}").as_str()),
        "Restore status.logTail must carry the restored snapshot id; got {rstatus}"
    );

    // 4. Delete a Snapshot whose deletionPolicy is Delete → finalizer runs a delete
    //    Job and the CR is removed. (Use a second Snapshot so step 2's Retain one
    //    survives for inspection.)
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-backup-del", "e2e-cfg", "Delete")),
        )
        .await
        .expect("create deletable Snapshot");
    wait_phase(&backups, "e2e-backup-del", "Succeeded")
        .await
        .expect("deletable Snapshot should reach Succeeded");
    backups
        .delete("e2e-backup-del", &DeleteParams::default())
        .await
        .expect("delete Snapshot");
    wait_until(
        "e2e-backup-del removed after finalizer",
        default_timeout(),
        poll_interval(),
        || async {
            match backups.get_opt("e2e-backup-del").await? {
                Some(_) => Ok(None),
                None => Ok(Some(())),
            }
        },
    )
    .await
    .expect("Snapshot CR should be removed once the snapshot-cleanup finalizer runs");
}

/// Regression guard (the "ClusterRepository references are ignored" bug): a
/// `SnapshotPolicy` that references a cluster-scoped `ClusterRepository` must drive
/// a Snapshot all the way to `Succeeded`. Before the fix, the controller resolved
/// every repository ref as a namespaced `Repository` regardless of `kind`, so a
/// `kind: ClusterRepository` config failed with
/// `missing dependency: Repository <ns>/<name>` and never produced a snapshot —
/// this test would time out at `wait_phase(... "Succeeded")`.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn cluster_repository_backup_lifecycle() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // 1. ClusterRepository becomes Ready (cluster-scoped; controller watches it
    //    cluster-wide under the e2e ClusterRole).
    crepos
        .create(
            &PostParams::default(),
            &cr(cluster_repository_json("e2e-crepo")),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, "e2e-crepo", "Ready")
        .await
        .expect("ClusterRepository should reach Ready");

    // 2. A SnapshotPolicy referencing it by `kind: ClusterRepository` (no namespace
    //    on the ref) + a Snapshot → mover Job → Succeeded with a real snapshot id.
    configs
        .create(
            &PostParams::default(),
            &cr(cluster_backup_config_json(
                "e2e-cfg-crepo",
                "e2e-crepo",
                "e2e-src",
            )),
        )
        .await
        .expect("create SnapshotPolicy (ClusterRepository-backed)");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-backup-crepo", "e2e-cfg-crepo", "Retain")),
        )
        .await
        .expect("create Snapshot");
    wait_phase(&backups, "e2e-backup-crepo", "Succeeded")
        .await
        .expect("ClusterRepository-backed Snapshot should reach Succeeded");
    let bstatus = status_json(&backups, "e2e-backup-crepo").await;
    let snap_id = bstatus
        .get("snapshot")
        .and_then(|s| s.get("kopiaSnapshotID"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        !snap_id.is_empty(),
        "ClusterRepository-backed Snapshot must carry a real kopia snapshot id, got {bstatus}"
    );

    // Cleanup the cluster-scoped resource (the namespaced ones GC with the ns).
    let _ = crepos.delete("e2e-crepo", &DeleteParams::default()).await;
}

/// Cross-namespace mover RBAC + credentials (ADR §4.12). A Snapshot in a workload
/// namespace SEPARATE from the operator's must (a) get a least-privilege
/// `kopiur-mover` ServiceAccount + RoleBinding MINTED in that namespace by the
/// controller, and (b) when its credentials Secret is absent there, surface a clear
/// `CredentialsAvailable=False` / `MissingCredentialsSecret` condition rather than
/// launching a Job that hangs. Adding the Secret clears the condition.
///
/// Before the fix the mover Job referenced an SA (and `envFrom` Secret) that only
/// existed in the operator namespace, so a cross-namespace Snapshot wedged in
/// `Running` with the Job stuck in `FailedCreate: serviceaccount ... not found` —
/// this test would time out waiting for the SA to appear.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn cross_namespace_backup_mints_mover_rbac_and_surfaces_missing_creds() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    // A workload namespace distinct from the operator's (E2E_NAMESPACE).
    const APP_NS: &str = "kopiur-e2e-app";
    // The chart-minted mover identity (release name `kopiur` → `kopiur-mover`).
    const MOVER_NAME: &str = "kopiur-mover";

    ensure_namespace(&client, APP_NS)
        .await
        .expect("create workload namespace");

    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), APP_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), APP_NS);
    let sas: Api<ServiceAccount> = Api::namespaced(client.clone(), APP_NS);
    let rbs: Api<RoleBinding> = Api::namespaced(client.clone(), APP_NS);

    // 1. A ClusterRepository (its Secret lives in E2E_NAMESPACE) reaches Ready.
    crepos
        .create(
            &PostParams::default(),
            &cr(cluster_repository_json("e2e-xns-crepo")),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, "e2e-xns-crepo", "Ready")
        .await
        .expect("ClusterRepository should reach Ready");

    // 2. SnapshotPolicy + Snapshot in the WORKLOAD namespace. No creds Secret there yet.
    let cfg = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-xns-cfg", "namespace": APP_NS },
        "spec": {
            "repository": { "kind": "ClusterRepository", "name": "e2e-xns-crepo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 }
        }
    });
    configs
        .create(&PostParams::default(), &cr(cfg))
        .await
        .expect("create SnapshotPolicy in workload ns");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json_ns("e2e-xns-backup", "e2e-xns-cfg", APP_NS)),
        )
        .await
        .expect("create Snapshot in workload ns");

    // 3. The controller mints the mover SA + RoleBinding in the workload namespace
    //    (this is the core of the fix — it happens before the creds check).
    wait_until(
        &format!("ServiceAccount {APP_NS}/{MOVER_NAME} minted"),
        default_timeout(),
        poll_interval(),
        || async { sas.get_opt(MOVER_NAME).await.map(|o| o.map(|_| ())) },
    )
    .await
    .expect("mover ServiceAccount minted in workload namespace");
    let rb = wait_until(
        &format!("RoleBinding {APP_NS}/{MOVER_NAME} minted"),
        default_timeout(),
        poll_interval(),
        || async { rbs.get_opt(MOVER_NAME).await },
    )
    .await
    .expect("mover RoleBinding minted in workload namespace");
    assert_eq!(
        rb.role_ref.name, MOVER_NAME,
        "RoleBinding must target the mover role"
    );

    // 4. Missing creds → clear, actionable CredentialsAvailable=False condition.
    wait_condition(&backups, "e2e-xns-backup", "CredentialsAvailable", "False")
        .await
        .expect("Snapshot should report CredentialsAvailable=False when the Secret is absent");
    let s = status_json(&backups, "e2e-xns-backup").await;
    let cond = s
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("CredentialsAvailable"))
        })
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    assert_eq!(
        cond.get("reason").and_then(|r| r.as_str()),
        Some("MissingCredentialsSecret")
    );
    let msg = cond.get("message").and_then(|m| m.as_str()).unwrap_or("");
    assert!(
        msg.contains(CREDS_SECRET) && msg.contains(APP_NS) && msg.contains("envFrom"),
        "condition message must name the Secret, the namespace, and explain envFrom; got: {msg}"
    );

    // 5. Place the Secret in the workload namespace → the condition clears to True.
    apply_secret(
        &client,
        APP_NS,
        CREDS_SECRET,
        &[("KOPIA_PASSWORD", "e2e-test-password-123")],
    )
    .await
    .expect("apply creds Secret into workload namespace");
    wait_condition(&backups, "e2e-xns-backup", "CredentialsAvailable", "True")
        .await
        .expect("Snapshot CredentialsAvailable should clear once the Secret is present");

    // Cleanup: the cluster-scoped repo + the workload namespace (GCs its CRs).
    let _ = crepos
        .delete("e2e-xns-crepo", &DeleteParams::default())
        .await;
    let nss: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client.clone());
    let _ = nss.delete(APP_NS, &DeleteParams::default()).await;
}

/// Privileged-mover namespace opt-in (ADR §4.11/§G16). A `SnapshotPolicy` whose
/// `spec.mover.securityContext` runs as root must be REFUSED with a clear
/// `MoverPermitted=False` / `PrivilegedMoverNotPermitted` condition until the
/// workload namespace carries the `kopiur.home-operations.com/privileged-movers`
/// annotation — then it clears. Mirrors VolSync's privileged-movers gate: a tenant
/// could otherwise reuse the minted mover ServiceAccount to run root pods.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn privileged_mover_requires_namespace_optin() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    const APP_NS: &str = "kopiur-e2e-priv";

    ensure_namespace(&client, APP_NS)
        .await
        .expect("create workload namespace");
    // Creds present in the workload ns so the run reaches the privileged-mover gate
    // (not the credentials gate).
    apply_secret(
        &client,
        APP_NS,
        CREDS_SECRET,
        &[("KOPIA_PASSWORD", "e2e-test-password-123")],
    )
    .await
    .expect("apply creds Secret");

    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), APP_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), APP_NS);

    crepos
        .create(
            &PostParams::default(),
            &cr(cluster_repository_json("e2e-priv-crepo")),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, "e2e-priv-crepo", "Ready")
        .await
        .expect("ClusterRepository should reach Ready");

    // A SnapshotPolicy whose mover runs as root (the trilium-rain shape).
    let cfg = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-priv-cfg", "namespace": APP_NS },
        "spec": {
            "repository": { "kind": "ClusterRepository", "name": "e2e-priv-crepo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "mover": { "securityContext": { "runAsUser": 0, "runAsGroup": 0 } }
        }
    });
    configs
        .create(&PostParams::default(), &cr(cfg))
        .await
        .expect("create privileged SnapshotPolicy");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json_ns("e2e-priv-backup", "e2e-priv-cfg", APP_NS)),
        )
        .await
        .expect("create Snapshot");

    // Refused: MoverPermitted=False with the privileged-mover reason + opt-in hint.
    wait_condition(&backups, "e2e-priv-backup", "MoverPermitted", "False")
        .await
        .expect("privileged mover must be refused until the namespace opts in");
    let s = status_json(&backups, "e2e-priv-backup").await;
    let cond = s
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("MoverPermitted"))
        })
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    assert_eq!(
        cond.get("reason").and_then(|r| r.as_str()),
        Some("PrivilegedMoverNotPermitted")
    );
    assert!(
        cond.get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .contains("kopiur.home-operations.com/privileged-movers"),
        "message must tell the admin which annotation to set"
    );

    // Opt in → the gate clears. The deadline is deliberately BELOW the 300s
    // structural-backstop requeue: the blocked Snapshot only re-checks that
    // fast because the Namespace watch (`watch::namespace_to_snapshots`)
    // delivers the annotation — this used to be a 30s blind hot-loop instead.
    // (Deliberate trade: a dropped Namespace watch event — the CI black-hole
    // flake class — fails this test rather than hiding behind the backstop;
    // a failure here means watch delivery, not the gate, regressed.)
    annotate_namespace(
        &client,
        APP_NS,
        "kopiur.home-operations.com/privileged-movers",
        "true",
    )
    .await
    .expect("annotate namespace for privileged movers");
    wait_until(
        "e2e-priv-backup MoverPermitted=True (watch-delivered, beats the 300s backstop)",
        Duration::from_secs(240),
        poll_interval(),
        || async {
            let s = status_json(&backups, "e2e-priv-backup").await;
            Ok((condition_status(&s, "MoverPermitted").as_deref() == Some("True")).then_some(()))
        },
    )
    .await
    .expect("the namespace opt-in must un-stick the Snapshot via the Namespace watch");

    // Cleanup.
    let _ = crepos
        .delete("e2e-priv-crepo", &DeleteParams::default())
        .await;
    let nss: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client.clone());
    let _ = nss.delete(APP_NS, &DeleteParams::default()).await;
}

/// `inheritSecurityContextFrom` (ADR §4.11): a mover with no explicit
/// `securityContext` copies one from a live workload pod, so a backup/restore runs as
/// the same UID/GID as the app it protects. This proves the resolution end-to-end —
/// the mover Job's pod template must carry the workload pod's `runAsUser`.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn mover_inherits_security_context_from_workload_pod() {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::Pod;

    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();

    // A labeled workload pod running as a specific non-root UID/GID.
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let workload = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "e2e-inherit-workload",
            "namespace": E2E_NAMESPACE,
            "labels": { "app": "e2e-inherit-workload" }
        },
        "spec": {
            // Pod-level securityContext (fsGroup) AND a container-level one — the mover
            // must inherit BOTH levels, not just the container.
            "securityContext": { "fsGroup": 2000 },
            "containers": [{
                "name": "app",
                "image": "registry.k8s.io/pause:3.9",
                "securityContext": {
                    "runAsUser": 2000,
                    "runAsGroup": 2000,
                    "runAsNonRoot": true
                }
            }]
        }
    });
    pods.create(&PostParams::default(), &cr(workload))
        .await
        .expect("create labeled workload pod");
    wait_until(
        "workload pod Running",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(pods.get_opt("e2e-inherit-workload").await?.filter(|p| {
                p.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .map(|ph| ph == "Running")
                    .unwrap_or(false)
            }))
        },
    )
    .await
    .expect("workload pod should reach Running so its securityContext can be read");

    // A Repository + a SnapshotPolicy whose mover INHERITS the pod's securityContext.
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json("e2e-inherit-repo")),
        )
        .await
        .expect("create Repository");
    wait_phase(&repos, "e2e-inherit-repo", "Ready")
        .await
        .expect("Repository should reach Ready");

    let cfg = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-inherit-cfg", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-inherit-repo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "mover": {
                "inheritSecurityContextFrom": {
                    "workloadSelector": { "podSelector": { "matchLabels": { "app": "e2e-inherit-workload" } } }
                }
            }
        }
    });
    configs
        .create(&PostParams::default(), &cr(cfg))
        .await
        .expect("create SnapshotPolicy with inheritSecurityContextFrom");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json(
                "e2e-inherit-backup",
                "e2e-inherit-cfg",
                "Retain",
            )),
        )
        .await
        .expect("create Snapshot");

    // The mover Job's pod template must carry the inherited UID (2000), proving the
    // controller resolved the workload pod's securityContext into the run.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let job = wait_until(
        "mover Job created with inherited securityContext",
        default_timeout(),
        poll_interval(),
        || async {
            let Some(job) = jobs.get_opt("e2e-inherit-backup").await? else {
                return Ok(None);
            };
            let uid = job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|p| p.containers.first())
                .and_then(|c| c.security_context.as_ref())
                .and_then(|sc| sc.run_as_user);
            Ok(uid.map(|_| job))
        },
    )
    .await
    .expect("mover Job should be created carrying an inherited securityContext");
    let pod_spec = job.spec.and_then(|s| s.template.spec).expect("pod spec");
    // CONTAINER-level: inherited runAsUser.
    let uid = pod_spec
        .containers
        .first()
        .and_then(|c| c.security_context.as_ref())
        .and_then(|sc| sc.run_as_user);
    assert_eq!(
        uid,
        Some(2000),
        "mover must inherit the workload's container runAsUser (2000), got {uid:?}"
    );
    // POD-level: inherited fsGroup (the part container-level inheritance can't carry).
    let fs_group = pod_spec
        .security_context
        .as_ref()
        .and_then(|sc| sc.fs_group);
    assert_eq!(
        fs_group,
        Some(2000),
        "mover must inherit the workload's pod-level fsGroup (2000), got {fs_group:?}"
    );

    // Cleanup (E2E_NAMESPACE persists across tests).
    let _ = repos
        .delete("e2e-inherit-repo", &DeleteParams::default())
        .await;
    let _ = configs
        .delete("e2e-inherit-cfg", &DeleteParams::default())
        .await;
    let _ = backups
        .delete("e2e-inherit-backup", &DeleteParams::default())
        .await;
    let _ = pods
        .delete("e2e-inherit-workload", &DeleteParams::default())
        .await;
}

/// Regression for the production wedge (`invariants` INV-1): inheriting a **root**
/// workload's `runAsUser: 0` must yield a *valid* root mover — `runAsNonRoot: false` +
/// `runAsUser: 0` — NOT the contradictory `runAsNonRoot: true` + `runAsUser: 0` that the
/// kubelet rejects with "container's runAsUser breaks non-root policy", parking the pod in
/// `CreateContainerConfigError` forever. Proves the fix end-to-end: the Job spec carries the
/// normalized context AND the kubelet starts the mover (the Snapshot reaches a terminal phase
/// without wedging) — instead of parking it in `CreateContainerConfigError`.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn root_workload_inherit_yields_valid_root_mover_not_a_wedge() {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::Pod;

    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();

    // Reading root-owned data requires a root mover, which is gated on the namespace
    // opt-in. Opt in for the duration of the test; reset to "false" in cleanup so the
    // shared E2E namespace doesn't stay privileged for later tests.
    annotate_namespace(
        &client,
        E2E_NAMESPACE,
        "kopiur.home-operations.com/privileged-movers",
        "true",
    )
    .await
    .expect("opt the namespace into privileged movers");

    // A labeled workload pod that runs as ROOT (uid 0, no runAsNonRoot) — the matrix /
    // mssql shape that produced the wedge.
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let workload = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "e2e-root-workload",
            "namespace": E2E_NAMESPACE,
            "labels": { "app": "e2e-root-workload" }
        },
        "spec": {
            "containers": [{
                "name": "app",
                "image": "registry.k8s.io/pause:3.9",
                "securityContext": { "runAsUser": 0, "runAsGroup": 0 }
            }]
        }
    });
    pods.create(&PostParams::default(), &cr(workload))
        .await
        .expect("create root workload pod");
    wait_until(
        "root workload pod Running",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(pods.get_opt("e2e-root-workload").await?.filter(|p| {
                p.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .map(|ph| ph == "Running")
                    .unwrap_or(false)
            }))
        },
    )
    .await
    .expect("root workload pod should reach Running so its securityContext can be read");

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json("e2e-root-repo")),
        )
        .await
        .expect("create Repository");
    wait_phase(&repos, "e2e-root-repo", "Ready")
        .await
        .expect("Repository should reach Ready");

    let cfg = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-root-cfg", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-root-repo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "mover": {
                "inheritSecurityContextFrom": {
                    "workloadSelector": { "podSelector": { "matchLabels": { "app": "e2e-root-workload" } } }
                }
            }
        }
    });
    configs
        .create(&PostParams::default(), &cr(cfg))
        .await
        .expect("create SnapshotPolicy inheriting the root workload's context");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-root-backup", "e2e-root-cfg", "Retain")),
        )
        .await
        .expect("create Snapshot");

    // The mover Job's container must carry the NORMALIZED root context: runAsUser:0 AND
    // runAsNonRoot:false — never the contradictory runAsNonRoot:true that wedges.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let job = wait_until(
        "root mover Job created",
        default_timeout(),
        poll_interval(),
        || async { jobs.get_opt("e2e-root-backup").await },
    )
    .await
    .expect("mover Job should be created");
    let sc = job
        .spec
        .and_then(|s| s.template.spec)
        .and_then(|p| p.containers.into_iter().next())
        .and_then(|c| c.security_context)
        .expect("mover container securityContext");
    assert_eq!(
        sc.run_as_user,
        Some(0),
        "mover must inherit the root workload's runAsUser:0"
    );
    assert_eq!(
        sc.run_as_non_root,
        Some(false),
        "runAsNonRoot MUST be normalized to false for a root UID (else CreateContainerConfigError wedge), got {:?}",
        sc.run_as_non_root
    );

    // The regression this guards: the inherited-root context is ACCEPTED by the kubelet and
    // the mover container STARTS, rather than being parked in `CreateContainerConfigError`
    // (the `{runAsNonRoot:true, runAsUser:0}` wedge). Whether the backup then *succeeds*
    // depends on repo/source file permissions — a uid-0 mover keeps the hardened
    // `drop:[ALL]` (no `DAC_OVERRIDE`) and `fsGroup` is a no-op on the hostPath repo, so a
    // root mover reading a 65532-owned filesystem repo is a separate concern other tests
    // cover. So: the Snapshot must reach a TERMINAL phase and must NOT have wedged.
    wait_until(
        "root-inherit Snapshot reaches a terminal phase (started, not wedged)",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&backups, "e2e-root-backup").await;
            let phase = s.get("phase").and_then(|p| p.as_str()).unwrap_or_default();
            Ok(matches!(phase, "Succeeded" | "Failed").then_some(()))
        },
    )
    .await
    .expect("the inherited-root mover must run to a terminal phase, not hang");
    let reason = status_json(&backups, "e2e-root-backup")
        .await
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|conds| {
            conds
                .iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Ready"))
        })
        .and_then(|c| c.get("reason").and_then(|r| r.as_str()))
        .unwrap_or_default()
        .to_string();
    assert_ne!(
        reason, "MoverPodWedged",
        "the inherited-root mover must START (valid root context: runAsUser:0 + \
         runAsNonRoot:false), never wedge in CreateContainerConfigError"
    );

    // Cleanup; reset the namespace opt-in so it doesn't leak to other tests.
    let _ = repos
        .delete("e2e-root-repo", &DeleteParams::default())
        .await;
    let _ = configs
        .delete("e2e-root-cfg", &DeleteParams::default())
        .await;
    let _ = backups
        .delete("e2e-root-backup", &DeleteParams::default())
        .await;
    let _ = pods
        .delete("e2e-root-workload", &DeleteParams::default())
        .await;
    let _ = annotate_namespace(
        &client,
        E2E_NAMESPACE,
        "kopiur.home-operations.com/privileged-movers",
        "false",
    )
    .await;
}

/// Regression for Fix B: a mover pod stuck in a non-starting state (here `Unschedulable`
/// via an impossible CPU request on the recipe's mover) must fail the Snapshot FAST — within
/// `failurePolicy.podStartupDeadlineSeconds` — with the `MoverPodWedged` reason, instead of
/// hanging until the 48h `activeDeadlineSeconds` backstop while the kubelet hammers the API.
/// Also asserts the wedged Job is reaped (no orphaned pod left behind).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn wedged_mover_pod_fails_fast_instead_of_hanging() {
    use k8s_openapi::api::batch::v1::Job;

    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();

    // A normal Repository — its bootstrap mover MUST be able to schedule, so the wedge
    // goes on the *recipe* (below), not moverDefaults (which would also poison bootstrap
    // and the repo would never reach Ready).
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json("e2e-wedge-repo")),
        )
        .await
        .expect("create Repository");
    wait_phase(&repos, "e2e-wedge-repo", "Ready")
        .await
        .expect("Repository should reach Ready");

    // The SnapshotPolicy's mover requests an impossible amount of CPU (10000 cores) → its
    // pod is `Unschedulable` on every node. Requests-only (no matching limit) so it passes
    // the requests<=limits validator; recipe-scoped so ONLY the snapshot mover wedges, not
    // the repo bootstrap. A deterministic wedge the securityContext normalizer can't resolve.
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cfg = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-wedge-cfg", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-wedge-repo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight (before the mover
            // Job is even created) rather than exercising the Unschedulable wedge.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "mover": { "resources": { "requests": { "cpu": "10000" } } }
        }
    });
    configs
        .create(&PostParams::default(), &cr(cfg))
        .await
        .expect("create SnapshotPolicy whose mover can never be scheduled");

    // A short grace so the test fails fast deterministically.
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backup = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": "e2e-wedge-backup", "namespace": E2E_NAMESPACE },
        "spec": {
            "policyRef": { "name": "e2e-wedge-cfg" },
            "deletionPolicy": "Retain",
            "failurePolicy": { "podStartupDeadlineSeconds": 15 }
        }
    });
    backups
        .create(&PostParams::default(), &cr(backup))
        .await
        .expect("create Snapshot with a short wedged-pod grace");

    // It must reach Failed (not hang in Running/Pending) with the wedged-pod reason.
    wait_phase(&backups, "e2e-wedge-backup", "Failed")
        .await
        .expect("a wedged (Unschedulable) mover must fail fast, not hang for ~48h");
    let status = status_json(&backups, "e2e-wedge-backup").await;
    let reason = status
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|conds| {
            conds
                .iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Ready"))
        })
        .and_then(|c| c.get("reason").and_then(|r| r.as_str()))
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        reason, "MoverPodWedged",
        "the failure reason must identify the wedged pod, got {reason:?}"
    );

    // The wedged Job (and its pod) must be reaped — not left orphaned to hammer the API.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    wait_until(
        "wedged mover Job reaped",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(jobs
                .get_opt("e2e-wedge-backup")
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("the wedged Job must be deleted (cascade) so no orphaned pod survives");

    // Cleanup.
    let _ = repos
        .delete("e2e-wedge-repo", &DeleteParams::default())
        .await;
    let _ = configs
        .delete("e2e-wedge-cfg", &DeleteParams::default())
        .await;
    let _ = backups
        .delete("e2e-wedge-backup", &DeleteParams::default())
        .await;
}

/// Explicit `SnapshotPolicy.spec.mover.securityContext` (container) AND
/// `podSecurityContext` (pod, e.g. fsGroup) both reach the backup mover pod — the
/// non-inherit counterpart of the test above, proving the explicit knobs thread through.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn backup_mover_applies_explicit_security_and_pod_context() {
    use k8s_openapi::api::batch::v1::Job;

    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json("e2e-scctx-repo")),
        )
        .await
        .expect("create Repository");
    wait_phase(&repos, "e2e-scctx-repo", "Ready")
        .await
        .expect("Repository Ready");

    let cfg = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-scctx-cfg", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-scctx-repo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "mover": {
                "securityContext": { "runAsUser": 3000, "runAsGroup": 3000, "runAsNonRoot": true },
                "podSecurityContext": { "fsGroup": 3000 }
            }
        }
    });
    configs
        .create(&PostParams::default(), &cr(cfg))
        .await
        .expect("create SnapshotPolicy with explicit mover security contexts");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-scctx-backup", "e2e-scctx-cfg", "Retain")),
        )
        .await
        .expect("create Snapshot");

    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let job = wait_until(
        "backup mover Job created",
        default_timeout(),
        poll_interval(),
        || async { jobs.get_opt("e2e-scctx-backup").await },
    )
    .await
    .expect("backup mover Job should be created");
    let pod = job.spec.and_then(|s| s.template.spec).expect("pod spec");
    assert_eq!(
        pod.containers
            .first()
            .and_then(|c| c.security_context.as_ref())
            .and_then(|sc| sc.run_as_user),
        Some(3000),
        "container securityContext.runAsUser must reach the backup mover"
    );
    assert_eq!(
        pod.security_context.as_ref().and_then(|sc| sc.fs_group),
        Some(3000),
        "podSecurityContext.fsGroup must reach the backup mover pod"
    );

    let _ = repos
        .delete("e2e-scctx-repo", &DeleteParams::default())
        .await;
    let _ = configs
        .delete("e2e-scctx-cfg", &DeleteParams::default())
        .await;
    let _ = backups
        .delete("e2e-scctx-backup", &DeleteParams::default())
        .await;
}

/// A `Snapshot` JSON in an explicit namespace (cross-namespace scenarios).
fn backup_json_ns(name: &str, config: &str, ns: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": ns },
        "spec": { "policyRef": { "name": config }, "deletionPolicy": "Retain" }
    })
}

/// A SnapshotSchedule with an every-minute cron creates a scheduled Snapshot CR.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn schedule_creates_backup() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Reuse / ensure a repo + config exist.
    let _ = repos
        .create(&PostParams::default(), &cr(repository_json("e2e-repo")))
        .await;
    let _ = configs
        .create(
            &PostParams::default(),
            &cr(backup_config_json("e2e-cfg-sched", "e2e-repo", "e2e-src")),
        )
        .await;

    let sched = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotSchedule",
        "metadata": { "name": "e2e-sched", "namespace": E2E_NAMESPACE },
        "spec": {
            "policyRef": { "name": "e2e-cfg-sched" },
            "schedule": { "cron": "* * * * *", "runOnCreate": true }
        }
    });
    schedules
        .create(&PostParams::default(), &cr::<SnapshotSchedule>(sched))
        .await
        .expect("create SnapshotSchedule");

    // Within ~2 minutes a scheduled Snapshot (origin=scheduled) should appear.
    wait_until(
        "a scheduled Snapshot is created",
        default_timeout(),
        poll_interval(),
        || async {
            let list = backups.list(&Default::default()).await?;
            let found = list.items.iter().any(|b| {
                b.labels()
                    .get("kopiur.home-operations.com/origin")
                    .map(String::as_str)
                    == Some("scheduled")
            });
            Ok(found.then_some(()))
        },
    )
    .await
    .expect("schedule should create a Snapshot CR");
}

/// A `SnapshotSchedule` with no `spec.schedule.timezone` inherits its cron
/// timezone from its target policy's repository `scheduleDefaults.timezone`
/// (GitHub #174 item 3). The pinned `status.nextSchedule` is computed in the
/// inherited zone and records it, so a large-offset, DST-free repo default
/// (`Pacific/Kiritimati`, fixed UTC+14) demonstrably shifts the wall-clock slot
/// away from a UTC interpretation.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn schedule_inherits_repository_timezone_default() {
    use chrono::Timelike;

    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Repository whose scheduleDefaults set a large-offset, DST-free zone.
    let mut repo = repository_json("e2e-tz-repo");
    repo["spec"]["scheduleDefaults"] = serde_json::json!({ "timezone": "Pacific/Kiritimati" });
    let _ = repos.create(&PostParams::default(), &cr(repo)).await;
    let _ = configs
        .create(
            &PostParams::default(),
            &cr(backup_config_json("e2e-tz-cfg", "e2e-tz-repo", "e2e-src")),
        )
        .await;

    // No spec.schedule.timezone → inherit the repo default. Daily 03:00, no
    // runOnCreate and no jitter → the pin is a clean wall-clock slot.
    let sched = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotSchedule",
        "metadata": { "name": "e2e-tz-sched", "namespace": E2E_NAMESPACE },
        "spec": {
            "policyRef": { "name": "e2e-tz-cfg" },
            "schedule": { "cron": "0 3 * * *" }
        }
    });
    schedules
        .create(&PostParams::default(), &cr::<SnapshotSchedule>(sched))
        .await
        .expect("create SnapshotSchedule");

    // The controller pins nextSchedule with the inherited zone recorded.
    let pinned = wait_until(
        "nextSchedule is pinned with a timezone",
        default_timeout(),
        poll_interval(),
        || async {
            let s = schedules.get("e2e-tz-sched").await?;
            Ok(s.status
                .and_then(|st| st.next_schedule)
                .filter(|n| n.at.is_some()))
        },
    )
    .await
    .expect("schedule should pin nextSchedule");

    // Primary assertion: the pin records the inherited repository timezone default.
    assert_eq!(
        pinned.timezone.as_deref(),
        Some("Pacific/Kiritimati"),
        "nextSchedule must record the inherited repository timezone default"
    );

    // Zone-consistency: 03:00 in a fixed UTC+14 zone is 13:00 the previous day UTC.
    // Convert the pinned UTC instant back by the fixed +14 offset (Kiritimati has no
    // DST) and assert the local wall clock is 03:00 — i.e. the slot was computed in
    // Kiritimati, NOT UTC (a UTC interpretation would leave the instant at 03:00Z).
    let at = pinned.at.as_deref().expect("pin carries an instant");
    let utc = chrono::DateTime::parse_from_rfc3339(at)
        .expect("pin is RFC3339")
        .with_timezone(&chrono::Utc);
    let local = utc + chrono::Duration::hours(14);
    assert_eq!(
        (local.hour(), local.minute()),
        (3, 0),
        "pinned slot must be 03:00 in Pacific/Kiritimati (got {utc} UTC → {local} local)"
    );
}

/// A `policySelector` schedule with NO `spec.schedule.timezone` fans out to
/// policies whose repositories DISAGREE on `scheduleDefaults.timezone` — pure
/// logic in `kopiur_api::common::effective_timezone` (GitHub #174 item 3):
/// resolution degrades to UTC and the schedule reports
/// `TimezoneDefaultAmbiguous=True`. Then patching one repository so both agree —
/// WITHOUT touching the SnapshotSchedule object — proves the Repository→schedules
/// referent watch (`crates/controller/src/lib.rs`, `watch::repository_to_schedules`)
/// fires promptly: the pinned `status.nextSchedule` recomputes into the
/// now-agreed zone and the ambiguity condition clears.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn schedule_policy_selector_timezone_ambiguity_and_referent_watch() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Two repositories whose scheduleDefaults disagree.
    let mut repo_a = repository_json("e2e-tzamb-repo-a");
    repo_a["spec"]["scheduleDefaults"] = serde_json::json!({ "timezone": "Pacific/Kiritimati" });
    repos
        .create(&PostParams::default(), &cr(repo_a))
        .await
        .expect("create repo a");
    let mut repo_b = repository_json("e2e-tzamb-repo-b");
    repo_b["spec"]["scheduleDefaults"] = serde_json::json!({ "timezone": "America/New_York" });
    repos
        .create(&PostParams::default(), &cr(repo_b))
        .await
        .expect("create repo b");

    // Two policies, one per repo, sharing a label the schedule's policySelector targets.
    let mut cfg_a = backup_config_json("e2e-tzamb-cfg-a", "e2e-tzamb-repo-a", "e2e-src");
    cfg_a["metadata"]["labels"] = serde_json::json!({ "tzband": "amb" });
    configs
        .create(&PostParams::default(), &cr(cfg_a))
        .await
        .expect("create cfg a");
    let mut cfg_b = backup_config_json("e2e-tzamb-cfg-b", "e2e-tzamb-repo-b", "e2e-src");
    cfg_b["metadata"]["labels"] = serde_json::json!({ "tzband": "amb" });
    configs
        .create(&PostParams::default(), &cr(cfg_b))
        .await
        .expect("create cfg b");

    // No spec.schedule.timezone → must inherit from the matched policies' repos.
    let sched = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotSchedule",
        "metadata": { "name": "e2e-tzamb-sched", "namespace": E2E_NAMESPACE },
        "spec": {
            "policySelector": { "matchLabels": { "tzband": "amb" } },
            "schedule": { "cron": "0 3 * * *" }
        }
    });
    schedules
        .create(&PostParams::default(), &cr::<SnapshotSchedule>(sched))
        .await
        .expect("create SnapshotSchedule");

    // 1. Disagreement degrades resolution to UTC, pinned and recorded as such.
    wait_until(
        "nextSchedule pins UTC while the matched repos disagree",
        default_timeout(),
        poll_interval(),
        || async {
            let s = schedules.get("e2e-tzamb-sched").await?;
            let tz = s
                .status
                .and_then(|st| st.next_schedule)
                .and_then(|n| n.timezone);
            Ok((tz.as_deref() == Some("UTC")).then_some(()))
        },
    )
    .await
    .expect("schedule must pin UTC when matched repos' timezone defaults disagree");

    // ...and the ambiguity condition is raised.
    wait_condition(
        &schedules,
        "e2e-tzamb-sched",
        "TimezoneDefaultAmbiguous",
        "True",
    )
    .await
    .expect("disagreeing repo defaults must raise TimezoneDefaultAmbiguous=True");

    // 2. Patch repo_b to agree with repo_a — WITHOUT touching the schedule object.
    let patch =
        serde_json::json!({ "spec": { "scheduleDefaults": { "timezone": "Pacific/Kiritimati" } } });
    repos
        .patch(
            "e2e-tzamb-repo-b",
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
        .expect("patch repo b to agree with repo a");

    // The Repository→schedules referent watch must promptly re-reconcile the
    // schedule and recompute its stale pinned slot into the now-agreed zone. No
    // sleeps: `wait_until`'s poll budget already covers a slow kind node, and a
    // working referent watch resolves this in seconds, not a full requeue cycle.
    wait_until(
        "nextSchedule recomputes into the now-agreed timezone",
        default_timeout(),
        poll_interval(),
        || async {
            let s = schedules.get("e2e-tzamb-sched").await?;
            let tz = s
                .status
                .and_then(|st| st.next_schedule)
                .and_then(|n| n.timezone);
            Ok((tz.as_deref() == Some("Pacific/Kiritimati")).then_some(()))
        },
    )
    .await
    .expect("referent watch must recompute the pinned slot once the repos agree");

    // ...and the ambiguity condition clears.
    wait_condition(
        &schedules,
        "e2e-tzamb-sched",
        "TimezoneDefaultAmbiguous",
        "False",
    )
    .await
    .expect("agreement must clear TimezoneDefaultAmbiguous");
}

/// `policySelector` fan-out (ADR-0005 §10): one schedule firing creates exactly
/// one Snapshot per MATCHING, non-suspended SnapshotPolicy — and nothing for the
/// non-matching or suspended ones. A broken selector fails silently in
/// production (no Snapshots, no errors), which is why this needs e2e coverage.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn schedule_policy_selector_fans_out_to_matching_policies() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let _ = repos
        .create(&PostParams::default(), &cr(repository_json("e2e-repo")))
        .await;
    wait_phase(&repos, "e2e-repo", "Ready")
        .await
        .expect("Repository should reach Ready");

    // Four policies: two matching, one non-matching, one matching-but-suspended.
    let policy = |name: &str, tier: &str, suspend: bool| {
        let mut cfg = backup_config_json(name, "e2e-repo", "e2e-src");
        cfg["metadata"]["labels"] = serde_json::json!({ "tier": tier });
        if suspend {
            cfg["spec"]["suspend"] = serde_json::json!(true);
        }
        cfg
    };
    for (name, tier, suspend) in [
        ("e2e-fan-a", "critical", false),
        ("e2e-fan-b", "critical", false),
        ("e2e-fan-c", "bulk", false),
        ("e2e-fan-d", "critical", true),
    ] {
        let _ = configs
            .create(&PostParams::default(), &cr(policy(name, tier, suspend)))
            .await;
    }

    // Yearly cron + runOnCreate ⇒ exactly ONE immediate fire during the test.
    let sched = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotSchedule",
        "metadata": { "name": "e2e-fan-sched", "namespace": E2E_NAMESPACE },
        "spec": {
            "policySelector": { "matchLabels": { "tier": "critical" } },
            "schedule": { "cron": "0 0 1 1 *", "runOnCreate": true }
        }
    });
    schedules
        .create(&PostParams::default(), &cr::<SnapshotSchedule>(sched))
        .await
        .expect("create fan-out SnapshotSchedule");

    // Exactly the two matching, non-suspended policies get a Snapshot.
    let fan_snapshots = || async {
        let list = backups
            .list(
                &kube::api::ListParams::default()
                    .labels("kopiur.home-operations.com/schedule=e2e-fan-sched"),
            )
            .await?;
        anyhow::Ok(list.items)
    };
    wait_until(
        "fan-out creates the two matching Snapshots",
        default_timeout(),
        poll_interval(),
        || async {
            let items = fan_snapshots()
                .await
                .map_err(|e| kube::Error::Service(e.into()))?;
            Ok((items.len() == 2).then_some(()))
        },
    )
    .await
    .expect("policySelector must fan out to the matching policies");
    // A grace window: still exactly 2 (no fan-out to the bulk/suspended ones,
    // no duplicate fire).
    tokio::time::sleep(Duration::from_secs(20)).await;
    let items = fan_snapshots().await.expect("list fan-out snapshots");
    assert_eq!(
        items.len(),
        2,
        "exactly the two matching, non-suspended policies fan out; got {:?}",
        items.iter().map(|b| b.name_any()).collect::<Vec<_>>()
    );

    let mut targeted: Vec<String> = items
        .iter()
        .filter_map(|b| b.spec.policy_ref.as_ref().map(|p| p.name.clone()))
        .collect();
    targeted.sort();
    assert_eq!(
        targeted,
        ["e2e-fan-a", "e2e-fan-b"],
        "each fan-out Snapshot must reference its own matching policy"
    );
    for b in &items {
        assert_eq!(
            b.labels()
                .get("kopiur.home-operations.com/origin")
                .map(String::as_str),
            Some("scheduled"),
            "fan-out snapshots carry origin=scheduled"
        );
        assert!(
            b.owner_references()
                .iter()
                .any(|o| o.kind == "SnapshotSchedule" && o.name == "e2e-fan-sched"),
            "fan-out snapshots are owned by the schedule"
        );
        assert!(
            b.name_any().starts_with("e2e-fan-sched-e2e-fan-"),
            "fan-out names are per-policy ({})",
            b.name_any()
        );
    }
    // The fan-out refs resolve: both reach Succeeded (a real backup each).
    for b in &items {
        wait_phase(&backups, &b.name_any(), "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("fan-out snapshot {} should succeed: {e}", b.name_any()));
    }

    // Cleanup: delete the schedule (owner GC reaps its snapshots) + policies.
    let _ = schedules
        .delete("e2e-fan-sched", &DeleteParams::default())
        .await;
    for name in ["e2e-fan-a", "e2e-fan-b", "e2e-fan-c", "e2e-fan-d"] {
        let _ = configs.delete(name, &DeleteParams::default()).await;
    }
}

/// A Maintenance claims the repository lease.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn maintenance_claims_lease() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = repos
        .create(&PostParams::default(), &cr(repository_json("e2e-repo")))
        .await;

    let maint = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Maintenance",
        "metadata": { "name": "e2e-maint", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-repo" },
            "schedule": {
                "quick": { "cron": "*/5 * * * *" },
                "full": { "cron": "0 3 * * 0" }
            },
            "ownership": { "owner": "kopiur-e2e", "takeoverPolicy": "Force" }
        }
    });
    maints
        .create(&PostParams::default(), &cr::<Maintenance>(maint))
        .await
        .expect("create Maintenance");

    wait_until(
        "Maintenance claims the lease",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&maints, "e2e-maint").await;
            let claimed = s
                .get("ownership")
                .and_then(|o| o.get("owner"))
                .and_then(|v| v.as_str())
                .map(|o| !o.is_empty())
                .unwrap_or(false);
            Ok(claimed.then_some(()))
        },
    )
    .await
    .expect("Maintenance should claim the lease");

    // kstatus consistency guard: once the repository is Ready and the lease is
    // claimed, the controller heals Maintenance `Ready=True` (set_ready_if_changed,
    // §2). That heal is a SEPARATE status write from the lease/run bookkeeping, so
    // gate on the healed condition rather than asserting it right after the lease
    // claim — same two-pass heal discipline as the restore/snapshot/replication
    // guards (docs/dev/watch-and-reconcile.md, "Two-pass terminal heal").
    wait_condition(&maints, "e2e-maint", "Ready", "True")
        .await
        .expect("a Maintenance whose repository is Ready must heal kstatus Ready=True");
}

/// Drive a Snapshot to Succeeded, then assert the controller exposes the expected
/// metric families with sane values — and that the exposition is valid
/// Prometheus text (a regression guard for the OTel→Prometheus name rewrite).
/// The webhook is disabled in the e2e harness, so webhook metrics are covered by
/// the unit tier, not here.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn metrics_reflect_backup_lifecycle() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // A successful backup so phase/size/duration gauges have real values.
    let _ = repos
        .create(&PostParams::default(), &cr(repository_json("e2e-mx-repo")))
        .await;
    wait_phase(&repos, "e2e-mx-repo", "Ready")
        .await
        .expect("Repository should reach Ready");
    configs
        .create(
            &PostParams::default(),
            &cr(backup_config_json("e2e-mx-cfg", "e2e-mx-repo", "e2e-src")),
        )
        .await
        .expect("create SnapshotPolicy");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-mx-backup", "e2e-mx-cfg", "Retain")),
        )
        .await
        .expect("create Snapshot");
    wait_phase(&backups, "e2e-mx-backup", "Succeeded")
        .await
        .expect("Snapshot should reach Succeeded");

    // True once the per-resource phase gauge `kopiur_resource_phase{…} == 1` is
    // present. The gauge is recorded by the controller's follow-up reconcile
    // AFTER the mover stamps the terminal phase, so the poll must gate on this
    // EXACT series — gating only on the `kopiur_resource_phase` family returns as
    // soon as any phase series exists (e.g. an earlier Running=1), then races the
    // Succeeded-recording reconcile. Regression: the controller debounce widened
    // that gap until the family-only poll lost the race every run.
    fn phase_gauge_is_one(text: &str, kind: &str, name: &str, phase: &str) -> bool {
        text.lines().any(|l| {
            l.starts_with("kopiur_resource_phase{")
                && l.contains(&format!("kind=\"{kind}\""))
                && l.contains(&format!("name=\"{name}\""))
                && l.contains(&format!("phase=\"{phase}\""))
                && l.trim_end().ends_with(" 1")
        })
    }

    // The Prometheus exporter publishes a family only after first observation and
    // the controller's own self-reconcile must record the Succeeded phase, so
    // poll until the key families AND the specific Succeeded gauge are present.
    let text = wait_until(
        "controller /metrics exposes kopiur families with Snapshot Succeeded==1",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                match scrape_controller_metrics(&client).await {
                    Ok(t)
                        if t.contains("kopiur_controller_reconciliations_total")
                            && t.contains("kopiur_snapshot_size_bytes")
                            && phase_gauge_is_one(&t, "Snapshot", "e2e-mx-backup", "Succeeded") =>
                    {
                        Ok(Some(t))
                    }
                    // Not ready yet, or the proxy isn't up — keep polling.
                    _ => Ok(None),
                }
            }
        },
    )
    .await
    .expect("controller should expose the kopiur metric families with Snapshot Succeeded==1");

    // Reconcile loop metrics, per kind.
    assert!(
        text.contains("kopiur_controller_reconciliations_total{")
            && text.contains("kind=\"Snapshot\""),
        "missing per-kind reconciliations counter:\n{text}"
    );
    // Histogram buckets present (validates the OTel histogram → _bucket rewrite).
    assert!(
        text.contains("kopiur_controller_reconcile_duration_seconds_bucket"),
        "missing reconcile duration histogram buckets"
    );
    // Our backup's phase gauge: Succeeded == 1 (guaranteed present — the poll
    // above gates on exactly this series).
    assert!(
        phase_gauge_is_one(&text, "Snapshot", "e2e-mx-backup", "Succeeded"),
        "expected kopiur_resource_phase ...Snapshot...Succeeded == 1:\n{text}"
    );
    // Snapshot stats gauges populated with a positive size.
    let positive_size = text.lines().any(|l| {
        l.starts_with("kopiur_snapshot_size_bytes{")
            && l.contains("name=\"e2e-mx-backup\"")
            && l.rsplit(' ')
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|v| v > 0.0)
    });
    assert!(
        positive_size,
        "expected positive kopiur_snapshot_size_bytes:\n{text}"
    );
    // Process RSS gauge present and positive — guards the new
    // `kopiur_process_resident_memory_bytes` observable (and its /proc/self/statm
    // read) so the controller's footprint stays measurable. The callback observes on
    // every collect, so the same scrape that carries the series above carries this.
    let rss_positive = text.lines().any(|l| {
        l.starts_with("kopiur_process_resident_memory_bytes")
            && l.rsplit(' ')
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|v| v > 0.0)
    });
    assert!(
        rss_positive,
        "expected positive kopiur_process_resident_memory_bytes:\n{text}"
    );
    // Valid Prometheus exposition: HELP/TYPE metadata present.
    assert!(
        text.contains("# TYPE kopiur_controller_reconciliations_total counter"),
        "exposition should carry # TYPE metadata"
    );

    // ---- store-backed metrics semantics (issues #172/#175) ----------------

    let policy_name = "e2e-mx-cfg";

    // The Succeeded Snapshot's phase line AND its size-bytes line carry the
    // `policy` label sourced from spec.policyRef.
    let phase_line = text
        .lines()
        .find(|l| {
            l.starts_with("kopiur_resource_phase{")
                && l.contains("name=\"e2e-mx-backup\"")
                && l.contains("phase=\"Succeeded\"")
        })
        .unwrap_or_else(|| panic!("missing Succeeded phase line for e2e-mx-backup:\n{text}"));
    assert!(
        phase_line.contains(&format!("policy=\"{policy_name}\"")),
        "expected policy label on the phase line:\n{phase_line}"
    );
    let size_line = text
        .lines()
        .find(|l| {
            l.starts_with("kopiur_snapshot_size_bytes{") && l.contains("name=\"e2e-mx-backup\"")
        })
        .unwrap_or_else(|| panic!("missing kopiur_snapshot_size_bytes for e2e-mx-backup:\n{text}"));
    assert!(
        size_line.contains(&format!("policy=\"{policy_name}\"")),
        "expected policy label on the size_bytes line:\n{size_line}"
    );

    // Active-only emission: never a 0-valued kopiur_resource_phase series (the
    // old enumerate-and-reset behavior flooded /metrics with phase="…"}=0 lines
    // for every inactive phase).
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("kopiur_resource_phase") && l.trim_end().ends_with(" 0")),
        "kopiur_resource_phase must never emit a 0-valued series:\n{text}"
    );

    // Per-policy "latest successful backup" family: populated for the policy
    // this scenario drove. `status.timing.durationSeconds` is integer seconds
    // and the e2e backup can complete in under a second, so 0 is a legitimate
    // duration — that series only asserts presence + a parseable, non-negative
    // value. Size and the success timestamp are always strictly positive.
    for (metric, min_inclusive) in [
        ("kopiur_policy_last_backup_duration_seconds", 0.0),
        ("kopiur_policy_last_backup_size_bytes", f64::MIN_POSITIVE),
        (
            "kopiur_policy_last_backup_success_timestamp_seconds",
            f64::MIN_POSITIVE,
        ),
    ] {
        let line = text
            .lines()
            .find(|l| {
                l.starts_with(&format!("{metric}{{"))
                    && l.contains(&format!("policy=\"{policy_name}\""))
            })
            .unwrap_or_else(|| panic!("missing {metric} for policy {policy_name}:\n{text}"));
        let value: f64 = line
            .rsplit(' ')
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("unparseable value for {metric}:\n{line}"));
        assert!(
            value >= min_inclusive,
            "expected {metric} >= {min_inclusive}, got {value}:\n{line}"
        );
    }

    // Completion counter: a terminal (Succeeded) transition recorded once, with
    // the result and policy labels, and — unlike the observable phase gauge —
    // durable across the CR's eventual deletion (issue #175).
    let completed_line = text
        .lines()
        .find(|l| {
            l.starts_with("kopiur_snapshots_completed_total{")
                && l.contains("result=\"succeeded\"")
                && l.contains(&format!("policy=\"{policy_name}\""))
        })
        .unwrap_or_else(|| {
            panic!("missing kopiur_snapshots_completed_total{{result=\"succeeded\"}} for policy {policy_name}:\n{text}")
        });
    let completed_value: f64 = completed_line
        .rsplit(' ')
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("unparseable completed value:\n{completed_line}"));
    assert!(
        completed_value >= 1.0,
        "expected kopiur_snapshots_completed_total >= 1:\n{completed_line}"
    );

    // THE regression pin (issues #172/#175): once the Snapshot CR is deleted (and
    // its finalizer has run), every one of its per-resource series must be absent
    // from the NEXT scrape. Before the store-backed conversion these were sync
    // gauges that could only be overwritten, never dropped, so a deleted CR's
    // series lingered forever — this assertion would time out on that code.
    backups
        .delete("e2e-mx-backup", &DeleteParams::default())
        .await
        .expect("delete Snapshot");
    wait_until(
        "e2e-mx-backup removed after finalizer",
        default_timeout(),
        poll_interval(),
        || async {
            match backups.get_opt("e2e-mx-backup").await? {
                Some(_) => Ok(None),
                None => Ok(Some(())),
            }
        },
    )
    .await
    .expect("Snapshot CR should be removed once the finalizer runs");

    wait_until(
        "kopiur metrics no longer carry any series for the deleted Snapshot",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                match scrape_controller_metrics(&client).await {
                    Ok(t) if !t.lines().any(|l| l.contains("name=\"e2e-mx-backup\"")) => {
                        Ok(Some(()))
                    }
                    // Series still present, or the proxy isn't up — keep polling.
                    _ => Ok(None),
                }
            }
        },
    )
    .await
    .expect(
        "all kopiur_resource_phase/kopiur_snapshot_*/kopiur_snapshot_last_success_timestamp_seconds \
         series for the deleted Snapshot must disappear from /metrics (#172/#175)",
    );
}

/// Default-managed maintenance (ADR §3.7): a `Repository` with no
/// `spec.maintenance` (default-on) gets an operator-*owned* `Maintenance` created
/// for it — same name as the repo, an `ownerReference` back to it, and the default
/// schedule — and the repo reports `MaintenanceConfigured=True`. Replaces the old
/// "warn when no Maintenance references the repo" behavior.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn repository_default_creates_managed_maintenance() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-defmaint-repo";
    let _ = repos
        .create(&PostParams::default(), &cr(repository_json(repo)))
        .await;
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("Repository should reach Ready");

    // The operator creates a Maintenance named after the repo, owned by it.
    let m = wait_managed_maintenance(&maints, repo, "Repository")
        .await
        .expect("operator should create an owned Maintenance for the repo");
    let mv = serde_json::to_value(&m).unwrap_or_default();
    assert_eq!(
        mv.pointer("/spec/schedule/quick/cron")
            .and_then(|v| v.as_str()),
        Some("0 */6 * * *"),
        "managed Maintenance should carry the default quick schedule: {mv}"
    );
    assert_eq!(
        mv.pointer("/spec/repository/name").and_then(|v| v.as_str()),
        Some(repo)
    );
    wait_condition(&repos, repo, "MaintenanceConfigured", "True")
        .await
        .expect("repo with default-managed maintenance should be MaintenanceConfigured=True");
}

/// Opting out: patching `spec.maintenance.enabled: false` removes the
/// operator-managed `Maintenance` and flips the condition to `False` (reason
/// `MaintenanceDisabled`, a deliberate opt-out — no Warning event).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn disabling_maintenance_removes_managed() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-optout-repo";
    let _ = repos
        .create(&PostParams::default(), &cr(repository_json(repo)))
        .await;
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("Repository should reach Ready");
    wait_managed_maintenance(&maints, repo, "Repository")
        .await
        .expect("managed Maintenance should exist before opt-out");

    // Opt out.
    let patch = serde_json::json!({ "spec": { "maintenance": { "enabled": false } } });
    repos
        .patch(repo, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .expect("patch spec.maintenance.enabled=false");

    // The managed Maintenance is removed and the condition reports disabled.
    wait_until(
        "managed Maintenance removed after opt-out",
        default_timeout(),
        poll_interval(),
        || async {
            match maints.get_opt(repo).await? {
                Some(_) => Ok(None),
                None => Ok(Some(())),
            }
        },
    )
    .await
    .expect("operator should delete the managed Maintenance when disabled");
    wait_condition(&repos, repo, "MaintenanceConfigured", "False")
        .await
        .expect("disabled repo should report MaintenanceConfigured=False");
}

/// Foreign precedence: a user-authored `Maintenance` for a repository means the
/// operator must NOT create a duplicate managed one (named after the repo), and
/// the repo reports `MaintenanceConfigured=True`. Disabling `spec.maintenance`
/// must still leave the user's `Maintenance` untouched.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn external_maintenance_is_not_duplicated() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-foreign-repo";
    let foreign = "e2e-foreign-maint";
    // Author the user's Maintenance first.
    maints
        .create(
            &PostParams::default(),
            &cr::<Maintenance>(maintenance_json(foreign, "Repository", repo)),
        )
        .await
        .expect("create user Maintenance");
    let _ = repos
        .create(&PostParams::default(), &cr(repository_json(repo)))
        .await;
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("Repository should reach Ready");

    // Covered by the user's Maintenance.
    wait_condition(&repos, repo, "MaintenanceConfigured", "True")
        .await
        .expect("repo covered by an external Maintenance should be True");

    // The operator must NOT have created a managed Maintenance named after the repo.
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert!(
        maints.get_opt(repo).await.expect("get").is_none(),
        "operator must not duplicate a user-authored Maintenance"
    );

    // Disabling does not remove the user's Maintenance, nor change coverage.
    let patch = serde_json::json!({ "spec": { "maintenance": { "enabled": false } } });
    repos
        .patch(repo, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .expect("patch spec.maintenance.enabled=false");
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert!(
        maints.get_opt(foreign).await.expect("get").is_some(),
        "disabling spec.maintenance must never remove a user-authored Maintenance"
    );
    wait_condition(&repos, repo, "MaintenanceConfigured", "True")
        .await
        .expect("external-covered repo stays True even when spec.maintenance is disabled");
}

/// The cluster-scoped path: a `ClusterRepository` with no `spec.maintenance`
/// default-manages a `Maintenance` placed in the operator's own namespace
/// (`KOPIUR_NAMESPACE`, which the e2e harness installs as the e2e namespace),
/// owned by the (cluster-scoped) ClusterRepository.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn cluster_repository_default_creates_managed_maintenance() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let crepo = "e2e-defmaint-crepo";
    let _ = crepos
        .create(&PostParams::default(), &cr(cluster_repository_json(crepo)))
        .await;
    wait_phase(&crepos, crepo, "Ready")
        .await
        .expect("ClusterRepository should reach Ready");

    let m = wait_managed_maintenance(&maints, crepo, "ClusterRepository")
        .await
        .expect(
            "operator should create an owned Maintenance for the cluster repo in its namespace",
        );
    let mv = serde_json::to_value(&m).unwrap_or_default();
    assert_eq!(
        mv.pointer("/spec/repository/kind").and_then(|v| v.as_str()),
        Some("ClusterRepository")
    );
    wait_condition(&crepos, crepo, "MaintenanceConfigured", "True")
        .await
        .expect("cluster repo with default-managed maintenance should be True");

    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}

/// The `kopiur_repository_maintenance_configured` gauge reflects live state via
/// `/metrics`: `1` once the operator manages a `Maintenance` for the repo
/// (default-on), `0` after opting out. Proves the metric surface end-to-end.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn maintenance_configured_reflected_in_metrics() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-mxmaint-repo";
    let _ = repos
        .create(&PostParams::default(), &cr(repository_json(repo)))
        .await;
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("Repository should reach Ready");

    // Default-on: gauge reads 1 once the managed Maintenance exists.
    wait_until(
        "maintenance_configured gauge == 1",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                Ok(scrape_controller_metrics(&client)
                    .await
                    .ok()
                    .filter(|t| maintenance_gauge_is(t, repo, "1"))
                    .map(|_| ()))
            }
        },
    )
    .await
    .expect("gauge should read 1 for a default-managed repo");

    // Opt out → gauge flips to 0.
    let patch = serde_json::json!({ "spec": { "maintenance": { "enabled": false } } });
    repos
        .patch(repo, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .expect("patch spec.maintenance.enabled=false");
    wait_until(
        "maintenance_configured gauge == 0",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                Ok(scrape_controller_metrics(&client)
                    .await
                    .ok()
                    .filter(|t| maintenance_gauge_is(t, repo, "0"))
                    .map(|_| ()))
            }
        },
    )
    .await
    .expect("gauge should read 0 after opting out");
}

/// True if the exposition has a `kopiur_repository_maintenance_configured` series
/// for `name` whose value equals `want` ("0"/"1").
fn maintenance_gauge_is(text: &str, name: &str, want: &str) -> bool {
    text.lines().any(|l| {
        l.starts_with("kopiur_repository_maintenance_configured{")
            && l.contains(&format!("name=\"{name}\""))
            && l.trim_end().ends_with(&format!(" {want}"))
    })
}

/// Wait until a `Maintenance` named `name` exists and is owned (controller
/// `ownerReference`) by `owner_kind`/`name` — i.e. the operator-managed one.
async fn wait_managed_maintenance(
    maints: &Api<Maintenance>,
    name: &str,
    owner_kind: &str,
) -> anyhow::Result<Maintenance> {
    wait_until(
        &format!("managed Maintenance {name} owned by {owner_kind}"),
        default_timeout(),
        poll_interval(),
        || async {
            match maints.get_opt(name).await? {
                Some(m) => {
                    let owned = m.owner_references().iter().any(|o| {
                        o.kind == owner_kind && o.name == name && o.controller == Some(true)
                    });
                    Ok(owned.then_some(m))
                }
                None => Ok(None),
            }
        },
    )
    .await
}

/// A `Repository` with the kopia web-UI server enabled (no-auth `insecure` mode so
/// the UI is reachable without credentials through the apiserver Service proxy).
/// Object-store (MinIO) backed: the server connects over the network, so — unlike a
/// filesystem repo — it needs no ReadWriteMany repo volume (that path is covered by
/// the controller's reconcile-time RWX check + unit tests).
fn server_repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "s3": {
                "bucket": "kopiur-server-ui",
                "endpoint": "minio.kopiur-e2e.svc.cluster.local:9000",
                "region": "us-east-1",
                "tls": { "disableTls": true },
                "auth": { "secretRef": { "name": kopiur_e2e::consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE } }
            }},
            "encryption": {
                "passwordSecretRef": { "name": kopiur_e2e::consts::SECRET_S3_CREDS, "key": "KOPIA_PASSWORD" }
            },
            "create": { "enabled": true },
            "server": {
                "auth": { "insecure": { "acknowledgeInsecure": true } },
                "service": { "type": "ClusterIP" }
            }
        }
    })
}

/// A read-only-UI variant of [`server_repository_json`]: `spec.server.readOnly: true`
/// so the served kopia connection cannot mutate the repository. Distinct bucket/name
/// from the read-write fixture so the two server tests don't collide.
fn server_readonly_repository_json(name: &str) -> serde_json::Value {
    let mut v = server_repository_json(name);
    v["spec"]["backend"]["s3"]["bucket"] = serde_json::json!("kopiur-server-ui-ro");
    v["spec"]["server"]["readOnly"] = serde_json::json!(true);
    v
}

/// Wait until a `Deployment` reports at least one available replica.
async fn wait_deployment_available(
    deps: &Api<k8s_openapi::api::apps::v1::Deployment>,
    name: &str,
) -> anyhow::Result<()> {
    wait_until(
        &format!("deployment {name} available"),
        default_timeout(),
        poll_interval(),
        || async {
            match deps.get_opt(name).await? {
                Some(d) => {
                    let available = d
                        .status
                        .as_ref()
                        .and_then(|s| s.available_replicas)
                        .unwrap_or(0);
                    Ok((available >= 1).then_some(()))
                }
                None => Ok(None),
            }
        },
    )
    .await
}

/// The kopia web-UI server scenario (`spec.server`): enabling it materializes an
/// owned Deployment + Service, the kopia server comes up serving its embedded UI
/// (proven by GETting the UI through the apiserver Service proxy), and disabling it
/// tears the objects back down.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn server_exposes_repository_ui() {
    use k8s_openapi::api::apps::v1::Deployment;
    use k8s_openapi::api::core::v1::Service;
    use kube::api::{Patch, PatchParams};

    let Some(world) = World::connect().await else {
        return;
    };
    // MinIO provides the S3 backend the server connects to (no RWX volume needed).
    world
        .ensure(&[Need::Minio])
        .await
        .expect("provision MinIO fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let deps: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let svcs: Api<Service> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo_name = "e2e-srv-repo";
    let object_name = "e2e-srv-repo-kopia-ui";

    // 1. Repository with spec.server becomes Ready and pins status.server.endpoint.
    repos
        .create(
            &PostParams::default(),
            &cr(server_repository_json(repo_name)),
        )
        .await
        .expect("create server Repository");
    wait_phase(&repos, repo_name, "Ready")
        .await
        .expect("server Repository should reach Ready");
    wait_until(
        "status.server.endpoint is pinned",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repos, repo_name).await;
            let ep = s
                .get("server")
                .and_then(|sv| sv.get("endpoint"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Ok((!ep.is_empty()).then_some(()))
        },
    )
    .await
    .expect("controller should pin status.server.endpoint");

    // 2. The owned Service exists and the Deployment becomes Available (the kopia
    //    server's TCP readiness probe passing proves it bound the listen port).
    svcs.get(object_name)
        .await
        .expect("server Service should exist");
    wait_deployment_available(&deps, object_name)
        .await
        .expect("server Deployment should become Available");

    // 3. The embedded HTML UI actually serves: GET it through the apiserver Service
    //    proxy (no-auth `insecure` mode). A non-empty 200 with a kopia marker proves
    //    the mover image's kopia is the UI-embedded build (`server start --ui`).
    let ui = wait_until(
        "kopia UI responds via the service proxy",
        default_timeout(),
        poll_interval(),
        || async {
            let path =
                format!("/api/v1/namespaces/{E2E_NAMESPACE}/services/{object_name}:http/proxy/");
            let req = http::Request::get(path).body(Vec::new()).unwrap();
            match client.request_text(req).await {
                Ok(body) if !body.is_empty() => Ok(Some(body)),
                // Server may still be warming up; keep polling.
                Ok(_) => Ok(None),
                Err(_) => Ok(None),
            }
        },
    )
    .await
    .expect("kopia UI should serve over HTTP through the service proxy");
    let lower = ui.to_lowercase();
    assert!(
        lower.contains("kopia") || lower.contains("<!doctype") || lower.contains("<html"),
        "proxied response should be the kopia web UI, got: {}",
        &ui.chars().take(200).collect::<String>()
    );

    // 4. Disable the server (spec.server = null) → owned objects are torn down.
    repos
        .patch(
            repo_name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "server": serde_json::Value::Null } })),
        )
        .await
        .expect("disable server");
    wait_until(
        "server Deployment is removed after disable",
        default_timeout(),
        poll_interval(),
        || async {
            match deps.get_opt(object_name).await? {
                Some(_) => Ok(None),
                None => Ok(Some(())),
            }
        },
    )
    .await
    .expect("server Deployment should be deleted when spec.server is removed");

    // Cleanup.
    let _ = repos.delete(repo_name, &DeleteParams::default()).await;
}

/// Regression guard for `spec.server.readOnly`: the operator must connect the server
/// read-only end-to-end. Proves (a) the controller resolves+pins
/// `status.server.readOnly: true`, (b) it threads `readOnly: true` into the mover's
/// work-spec ConfigMap, and (c) the kopia server still **comes up and serves the UI**
/// against a read-only connection (the one behaviour that couldn't be unit-tested —
/// browse sessions connect read-only but never run `server start`). We assert the
/// operator-real signals rather than attempting an HTTP mutation through the proxy:
/// kopia's write API is CSRF/auth-gated, so a rejected write wouldn't isolate the
/// read-only cause. The read-only connect itself is unit-tested in the mover serve path.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn server_read_only_ui_connects_read_only() {
    use k8s_openapi::api::apps::v1::Deployment;
    use k8s_openapi::api::core::v1::{ConfigMap, Service};

    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio])
        .await
        .expect("provision MinIO fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let deps: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let svcs: Api<Service> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo_name = "e2e-srv-ro-repo";
    let object_name = "e2e-srv-ro-repo-kopia-ui";

    // 1. Repository with spec.server.readOnly becomes Ready.
    repos
        .create(
            &PostParams::default(),
            &cr(server_readonly_repository_json(repo_name)),
        )
        .await
        .expect("create read-only server Repository");
    wait_phase(&repos, repo_name, "Ready")
        .await
        .expect("read-only server Repository should reach Ready");

    // 2. The controller pins the EFFECTIVE read-only state to status.server.readOnly.
    wait_until(
        "status.server.readOnly is pinned true",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repos, repo_name).await;
            let ro = s
                .get("server")
                .and_then(|sv| sv.get("readOnly"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(ro.then_some(()))
        },
    )
    .await
    .expect("controller should pin status.server.readOnly: true");

    // 3. The mover work-spec ConfigMap carries readOnly: true (the contract that makes
    //    the serve entrypoint connect with --readonly).
    let cm = cms
        .get(object_name)
        .await
        .expect("server ConfigMap should exist");
    let spec_json = &cm.data.expect("ConfigMap has data")["server-spec.json"];
    let ws: serde_json::Value = serde_json::from_str(spec_json).expect("server-spec.json parses");
    assert_eq!(
        ws.get("readOnly").and_then(|v| v.as_bool()),
        Some(true),
        "work spec should instruct the mover to connect read-only: {ws}"
    );

    // 4. The server still serves the UI over a read-only connection (the real risk:
    //    kopia 0.23 `server start` had never been validated against a --readonly
    //    connect in our image). A non-empty 200 through the Service proxy proves it.
    svcs.get(object_name)
        .await
        .expect("server Service should exist");
    wait_deployment_available(&deps, object_name)
        .await
        .expect("read-only server Deployment should become Available");
    let ui = wait_until(
        "kopia UI responds via the service proxy (read-only)",
        default_timeout(),
        poll_interval(),
        || async {
            let path =
                format!("/api/v1/namespaces/{E2E_NAMESPACE}/services/{object_name}:http/proxy/");
            let req = http::Request::get(path).body(Vec::new()).unwrap();
            match client.request_text(req).await {
                Ok(body) if !body.is_empty() => Ok(Some(body)),
                Ok(_) | Err(_) => Ok(None),
            }
        },
    )
    .await
    .expect("read-only kopia UI should serve over HTTP through the service proxy");
    let lower = ui.to_lowercase();
    assert!(
        lower.contains("kopia") || lower.contains("<!doctype") || lower.contains("<html"),
        "proxied response should be the kopia web UI, got: {}",
        &ui.chars().take(200).collect::<String>()
    );

    // Cleanup.
    let _ = repos.delete(repo_name, &DeleteParams::default()).await;
}

/// Compile-time guard that `Client` is reachable from this crate even when the
/// `e2e` feature gates the bodies above — keeps the dependency graph honest.
#[allow(dead_code)]
fn _type_anchor(_c: Client) {}
