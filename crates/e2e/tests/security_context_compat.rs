//! End-to-end scenarios for the securityContext-compatibility pipeline (the "validate that
//! the mover can read the PVC the workload mounts" feature), against a Helm-deployed operator
//! in kind. Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; skips gracefully without a
//! cluster. Run via `mise run //crates/e2e:test`.
//!
//! Covers the two surfaces that are deterministic end-to-end:
//!   1. `inheritSecurityContextFrom.pvcConsumer` — the operator auto-derives the workload
//!      pod from the source PVC and the mover Job inherits its UID/GID (no selector), with
//!      `SecurityContextCompatible=True`.
//!   2. The reconcile-time `SecurityContextCompatible=False` condition when an explicit mover
//!      UID shares neither UID nor group with the workload that mounts the source PVC.
//!
//! The mover's runtime readability preflight, the restore-direction predicate, the admission
//! warning, and the full compatibility truth table are covered by unit tests in
//! `kopiur-api`/`kopiur-mover`/`kopiur-controller`/`kopiur-webhook`; this shard proves the
//! controller wiring resolves real workload pods and stamps the real conditions.

#![cfg(all(unix, feature = "e2e"))]

use kube::api::{DeleteParams, PostParams};
use kube::{Api, Client};
use serde::de::DeserializeOwned;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;

use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

const CREDS_SECRET: &str = "kopia-creds";
const SECURITY_CONTEXT_COMPATIBLE: &str = "SecurityContextCompatible";

fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

fn repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": { "passwordSecretRef": { "name": CREDS_SECRET, "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    })
}

/// A labeled workload pod that mounts the shared source PVC `e2e-src`, running as `uid`
/// (with the pod `fsGroup`), so the operator's pvcConsumer/compat logic can read its identity.
fn workload_pod_json(name: &str, uid: i64, fs_group: i64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE, "labels": { "app": name } },
        "spec": {
            "securityContext": { "fsGroup": fs_group },
            "containers": [{
                "name": "app",
                "image": "registry.k8s.io/pause:3.9",
                "securityContext": { "runAsUser": uid, "runAsGroup": uid, "runAsNonRoot": true },
                "volumeMounts": [{ "name": "src", "mountPath": "/data" }]
            }],
            "volumes": [{ "name": "src", "persistentVolumeClaim": { "claimName": "e2e-src" } }]
        }
    })
}

async fn wait_pod_running(pods: &Api<Pod>, name: &str) {
    wait_until(
        "workload pod Running",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(pods.get_opt(name).await?.filter(|p| {
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
}

/// Read a Snapshot's `SecurityContextCompatible` condition (status + reason), waiting for it
/// to appear.
async fn wait_compat_condition(backups: &Api<Snapshot>, name: &str) -> (String, String) {
    wait_until(
        "SecurityContextCompatible condition present",
        default_timeout(),
        poll_interval(),
        || async {
            let Some(b) = backups.get_opt(name).await? else {
                return Ok(None);
            };
            let cond = b
                .status
                .as_ref()
                .and_then(|s| {
                    s.conditions
                        .iter()
                        .find(|c| c.type_ == SECURITY_CONTEXT_COMPATIBLE)
                })
                .map(|c| (c.status.clone(), c.reason.clone()));
            Ok(cond)
        },
    )
    .await
    .expect("the SecurityContextCompatible condition should be stamped on the Snapshot")
}

fn backup_json(name: &str, policy: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": { "policyRef": { "name": policy }, "deletionPolicy": "Retain" }
    })
}

async fn cleanup(client: &Client, repo: &str, policy: &str, backup: &str, pod: &str) {
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
    // Delete the workload pod with NO grace period and WAIT for it to actually be gone.
    // Every scenario here parks a pod on the SAME source claim (`e2e-src`), and the compat
    // assessment unions the writer UIDs of every pod mounting it — a still-Terminating pod is
    // returned by that LIST, so its UID would leak into the next scenario's verdict and flip a
    // `Compatible` to `Unknown`. The default 30s grace made these scenarios order-dependent.
    let _ = pods
        .delete(pod, &DeleteParams::default().grace_period(0))
        .await;
    let _ = wait_until(
        "workload pod fully gone (not just Terminating)",
        default_timeout(),
        poll_interval(),
        || async { Ok(pods.get_opt(pod).await?.is_none().then_some(())) },
    )
    .await;
}

/// Scenario (a): `pvcConsumer: {}` auto-derives the workload pod that mounts the source PVC —
/// the mover Job inherits its UID/GID with NO selector, and `SecurityContextCompatible=True`.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn pvc_consumer_auto_derives_security_context() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();

    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // A workload running as uid 2500 that mounts the source PVC.
    pods.create(
        &PostParams::default(),
        &cr(workload_pod_json("e2e-scc-consumer", 2500, 2500)),
    )
    .await
    .expect("create workload pod");
    wait_pod_running(&pods, "e2e-scc-consumer").await;

    repos
        .create(&PostParams::default(), &cr(repository_json("e2e-scc-repo")))
        .await
        .expect("create Repository");

    // pvcConsumer: NO selector — the operator derives the pod from the source PVC.
    let policy = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-scc-policy", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-scc-repo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "mover": { "inheritSecurityContextFrom": { "pvcConsumer": {} } }
        }
    });
    policies
        .create(&PostParams::default(), &cr(policy))
        .await
        .expect("create SnapshotPolicy with pvcConsumer");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-scc-backup", "e2e-scc-policy")),
        )
        .await
        .expect("create Snapshot");

    // The mover Job's pod template must carry the AUTO-DERIVED workload UID (2500).
    let job = wait_until(
        "mover Job created with pvcConsumer-derived securityContext",
        default_timeout(),
        poll_interval(),
        || async {
            let Some(job) = jobs.get_opt("e2e-scc-backup").await? else {
                return Ok(None);
            };
            let has_uid = job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|p| p.containers.first())
                .and_then(|c| c.security_context.as_ref())
                .and_then(|sc| sc.run_as_user)
                .is_some();
            Ok(has_uid.then_some(job))
        },
    )
    .await
    .expect("mover Job should be created carrying the pvcConsumer-derived securityContext");

    let pod_spec = job.spec.and_then(|s| s.template.spec).expect("pod spec");
    let uid = pod_spec
        .containers
        .first()
        .and_then(|c| c.security_context.as_ref())
        .and_then(|sc| sc.run_as_user);
    assert_eq!(
        uid,
        Some(2500),
        "pvcConsumer must auto-derive the workload's runAsUser (2500), got {uid:?}"
    );

    // And the compatibility condition is True (the mover UID exactly matches the workload).
    let (status, reason) = wait_compat_condition(&backups, "e2e-scc-backup").await;
    assert_eq!(
        status, "True",
        "an auto-matched mover must be SecurityContextCompatible=True (reason {reason})"
    );

    cleanup(
        &client,
        "e2e-scc-repo",
        "e2e-scc-policy",
        "e2e-scc-backup",
        "e2e-scc-consumer",
    )
    .await;
}

/// Scenario (b): an explicit mover UID that doesn't match the workload mounting the source
/// must NOT be flagged `SecurityContextCompatible=False` up front. A securityContext-only
/// heuristic can't see file modes (the data may be world-readable), so the reconcile path is
/// positive-only — a `False` only ever comes from kopia *actually* excluding entries at
/// runtime (`assess_completed_backup`). This is the regression test for the launch-time
/// false-alarm the review caught. We assert the reconcile ran (the mover Job was created) and
/// never wrote `False`; we deliberately do NOT require the backup to complete, since the mover
/// UID also governs (filesystem) repo access, which is orthogonal to source readability.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn mismatched_mover_is_not_flagged_false_up_front() {
    use k8s_openapi::api::batch::v1::Job;

    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();

    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // A workload running as uid 2600 mounts the source PVC — so the up-front check has a real
    // consumer to compare against (mover uid 7777 vs workload 2600: no shared UID or group).
    pods.create(
        &PostParams::default(),
        &cr(workload_pod_json("e2e-scc-mismatch-pod", 2600, 2600)),
    )
    .await
    .expect("create workload pod");
    wait_pod_running(&pods, "e2e-scc-mismatch-pod").await;

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json("e2e-scc-mm-repo")),
        )
        .await
        .expect("create Repository");

    let policy = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-scc-mm-policy", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-scc-mm-repo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
            // now defaults to Snapshot, which would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "mover": { "securityContext": { "runAsUser": 7777, "runAsGroup": 7777, "runAsNonRoot": true } }
        }
    });
    policies
        .create(&PostParams::default(), &cr(policy))
        .await
        .expect("create SnapshotPolicy with a mismatched explicit mover UID");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-scc-mm-backup", "e2e-scc-mm-policy")),
        )
        .await
        .expect("create Snapshot");

    // Wait until the reconcile has launched the mover Job — the up-front compat check runs
    // immediately before this, so by now it has had its say.
    wait_until(
        "mover Job created",
        default_timeout(),
        poll_interval(),
        || async { jobs.get_opt("e2e-scc-mm-backup").await },
    )
    .await
    .expect("the mover Job should be created (the reconcile, incl. the compat check, has run)");

    // The up-front heuristic must NOT have flagged it `False` (it can't see file modes). Give a
    // couple of reconcile cycles to be sure nothing writes a late `False`, then assert.
    for _ in 0..5 {
        let b = backups
            .get("e2e-scc-mm-backup")
            .await
            .expect("read the Snapshot");
        let false_flag = b.status.as_ref().is_some_and(|s| {
            s.conditions
                .iter()
                .any(|c| c.type_ == SECURITY_CONTEXT_COMPATIBLE && c.status == "False")
        });
        assert!(
            !false_flag,
            "a UID mismatch must not be flagged SecurityContextCompatible=False up front \
             (the data may be world-readable; only kopia's runtime output sets False)"
        );
        tokio::time::sleep(poll_interval()).await;
    }

    cleanup(
        &client,
        "e2e-scc-mm-repo",
        "e2e-scc-mm-policy",
        "e2e-scc-mm-backup",
        "e2e-scc-mismatch-pod",
    )
    .await;
}

/// A workload pod that mounts `e2e-src` but pins **no** `runAsUser` at either level — its
/// identity would come from its image's `USER` line, which the operator cannot read from the
/// spec. The container securityContext is present but hardened-only (the restricted-PSA /
/// bjw-s house style), which is exactly what made the old code accept it.
fn uidless_workload_pod_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE, "labels": { "app": name } },
        "spec": {
            "containers": [{
                "name": "app",
                "image": "registry.k8s.io/pause:3.9",
                // Present, hardened — but pins no identity whatsoever.
                "securityContext": { "allowPrivilegeEscalation": false },
                "volumeMounts": [{ "name": "src", "mountPath": "/data" }]
            }],
            "volumes": [{ "name": "src", "persistentVolumeClaim": { "claimName": "e2e-src" } }]
        }
    })
}

/// Scenario (c) — **the regression guard for the reported bug.** `pvcConsumer` against a
/// workload that pins no UID must NEVER report `SecurityContextCompatible=True`.
///
/// On `main` this fails: the reconciler short-circuited `pvcConsumer` straight to `True`
/// ("the mover inherited the source PVC consumer's securityContext (pvcConsumer), so its
/// UID/GID matches the workload by construction") without consulting the compat engine at
/// all. Inheriting a UID-less workload copies no UID, so the mover silently ran as its own
/// image's 65532 and the backup then failed with `permission denied` — while the CR claimed
/// the identities matched.
///
/// The assertion is deliberately about the CONDITION, not the backup outcome: whether the
/// mover can read `e2e-src` depends on file modes the harness doesn't pin. What must hold is
/// that the operator never *claims* a match it did not verify.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn pvc_consumer_never_claims_compatible_for_a_uidless_workload() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();

    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    pods.create(
        &PostParams::default(),
        &cr(uidless_workload_pod_json("e2e-scc-uidless")),
    )
    .await
    .expect("create UID-less workload pod");
    wait_pod_running(&pods, "e2e-scc-uidless").await;

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json("e2e-scc-nouid-repo")),
        )
        .await
        .expect("create Repository");

    let policy = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "e2e-scc-nouid-policy", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-scc-nouid-repo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 },
            "mover": { "inheritSecurityContextFrom": { "pvcConsumer": {} } }
        }
    });
    policies
        .create(&PostParams::default(), &cr(policy))
        .await
        .expect("create SnapshotPolicy with pvcConsumer");
    backups
        .create(
            &PostParams::default(),
            &cr(backup_json("e2e-scc-nouid-backup", "e2e-scc-nouid-policy")),
        )
        .await
        .expect("create Snapshot");

    // Wait for the reconcile to have actually run and made its compatibility decision — the
    // mover Job's existence is the proof it got past the resolve/gate path.
    wait_until(
        "mover Job created (the reconcile reached its compat decision)",
        default_timeout(),
        poll_interval(),
        || async { jobs.get_opt("e2e-scc-nouid-backup").await },
    )
    .await
    .expect("mover Job should be created");

    // The mover carries NO inherited runAsUser: there was none to inherit. It will run as the
    // mover image's own 65532 — which is precisely why claiming a match would be a lie.
    let job = jobs
        .get("e2e-scc-nouid-backup")
        .await
        .expect("get mover Job");
    let inherited_uid = job
        .spec
        .and_then(|s| s.template.spec)
        .and_then(|p| p.containers.first().cloned())
        .and_then(|c| c.security_context)
        .and_then(|sc| sc.run_as_user);
    assert_eq!(
        inherited_uid, None,
        "inheriting a UID-less workload cannot pin a mover UID; got {inherited_uid:?}"
    );

    // THE ASSERTION — a direct read, not an inverted timeout. The compat decision is made and
    // patched BEFORE the mover Job is applied, so the Job existing above already proves this
    // reconcile reached and answered the question. Asserting on a `wait_until(...).is_err()`
    // would instead pass for any reason the poll failed (RBAC, dead apiserver) — a test that
    // cannot fail for the right reason is worse than no test.
    let backup = backups
        .get("e2e-scc-nouid-backup")
        .await
        .expect("get Snapshot after its reconcile stamped the compat decision");
    let compat = backup.status.as_ref().and_then(|s| {
        s.conditions
            .iter()
            .find(|c| c.type_ == SECURITY_CONTEXT_COMPATIBLE)
    });
    assert!(
        !matches!(compat, Some(c) if c.status == "True"),
        "regression: the operator claimed SecurityContextCompatible=True for a mover that \
         inherited no UID from a UID-less workload — condition: {compat:?}"
    );

    cleanup(
        &client,
        "e2e-scc-nouid-repo",
        "e2e-scc-nouid-policy",
        "e2e-scc-nouid-backup",
        "e2e-scc-uidless",
    )
    .await;
}
