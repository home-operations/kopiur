//! e2e: `moverDefaults` inheritance, the bootstrap-gap fix, and the field-wise
//! security-context merge (ADR-0004 §1/§2).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::events::v1::Event;
use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// THE HEADLINE. A `Repository.spec.moverDefaults` security/pod context must reach
/// (1) the BOOTSTRAP Job's pod (the bootstrap-gap fix — before ADR-0004 the
/// connect/create Job ignored moverDefaults, so a filesystem repo on a
/// non-65532-owned dir was un-bootstrappable), and (2) the backup mover's pod. A
/// per-recipe `mover.securityContext.runAsUser` then merges OVER moverDefaults, and
/// in every rendered container the hardened `drop:[ALL]`/seccomp SURVIVES.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn mover_defaults_inherited_by_bootstrap_and_backup_with_recipe_override() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "moverdefaults").await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-md-repo";
    // moverDefaults inherited by EVERY mover (bootstrap + backup): pod fsGroup +
    // container runAsUser/runAsGroup. The mover image runs as 65532, so keep the repo
    // dir accessible — these values just prove inheritance, not a UID the kopia run
    // must match for a hostPath repo (kopia creates the subdir as the running UID).
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "moverdefaults",
                serde_json::json!({
                    "moverDefaults": {
                        "podSecurityContext": { "fsGroup": 65532 },
                        "securityContext": { "runAsUser": 65532, "runAsGroup": 65532, "runAsNonRoot": true }
                    }
                }),
            )),
        )
        .await
        .expect("create Repository with moverDefaults");

    // (1) BOOTSTRAP GAP FIX: the connect/create Job (`<repo>-discovery`) inherits the
    //     repository's moverDefaults. Before ADR-0004 the bootstrap path used a bare
    //     hardened context and ignored moverDefaults entirely.
    let boot = wait_for_job(&jobs, &format!("{repo}-discovery")).await;
    assert_eq!(
        job_pod_sc(&boot).and_then(|sc| sc.fs_group),
        Some(65532),
        "bootstrap Job pod must inherit moverDefaults.podSecurityContext.fsGroup (the gap fix)"
    );
    let boot_sc = job_container_sc(&boot).expect("bootstrap container securityContext");
    assert_eq!(
        boot_sc.run_as_user,
        Some(65532),
        "bootstrap Job container must inherit moverDefaults.securityContext.runAsUser"
    );
    assert_hardening_survives(&boot_sc, "bootstrap");

    wait_phase(&repos, repo, "Ready").await.expect(
        "Repository should bootstrap to Ready (proving moverDefaults didn't break bootstrap)",
    );

    // (2) A backup mover whose recipe `mover.securityContext.runAsUser` OVERRIDES
    //     moverDefaults (3000 wins over 65532), while the pod fsGroup still inherits
    //     from moverDefaults (the recipe set no podSecurityContext) and the hardened
    //     drop:[ALL]/seccomp survive the field-wise merge.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                "e2e-md-policy",
                "Repository",
                repo,
                serde_json::json!({
                    "mover": { "securityContext": { "runAsUser": 3000, "runAsNonRoot": true } }
                }),
            )),
        )
        .await
        .expect("create SnapshotPolicy with a recipe mover override");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-md-backup",
                "e2e-md-policy",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    let bjob = wait_for_job(&jobs, "e2e-md-backup").await;
    let bsc = job_container_sc(&bjob).expect("backup container securityContext");
    assert_eq!(
        bsc.run_as_user,
        Some(3000),
        "recipe mover.securityContext.runAsUser (3000) must win over moverDefaults (65532)"
    );
    assert_eq!(
        job_pod_sc(&bjob).and_then(|sc| sc.fs_group),
        Some(65532),
        "backup mover pod must inherit moverDefaults.podSecurityContext.fsGroup (recipe set none)"
    );
    assert_hardening_survives(&bsc, "backup (recipe override)");

    // Cleanup.
    let _ = backups
        .delete("e2e-md-backup", &DeleteParams::default())
        .await;
    let _ = policies
        .delete("e2e-md-policy", &DeleteParams::default())
        .await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}

// --- #464: the dedicated hold for an unresolvable live-pod inherit ------------
//
// Both scenarios reuse the `moverdefaults` repo subpath this file already owns, deliberately:
// a new subpath must be added to `consts::REPO_SUBPATHS` AND the `node-seed` mise task in
// lockstep, and nothing here needs repo isolation. What DOES matter is that every mover
// touching that repo runs as the SAME uid — kopia writes its control files 0600, so a repo
// bootstrapped by 65532 is unreadable to a 1000 sibling (the reason
// `security_context_compat`'s scenario (g) needed its own). So the workload pins uid 65532 and
// proves inheritance through `runAsGroup` instead, which kopia does not care about.

/// The workload whose securityContext is inherited: a scalable `Deployment` (NOT a bare Pod —
/// scaling it to zero and back is the scenario), labelled for the policy's `workloadSelector`.
///
/// `runAsUser: 65532` keeps the mover able to read the shared repo (see the module note);
/// `runAsGroup` is the tell-tale, because the hardened default is 65532 for both, so a UID
/// assertion alone could not distinguish "inherited" from "defaulted".
fn inherit_workload_deployment(name: &str, replicas: i32, run_as_group: i64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "replicas": replicas,
            "selector": { "matchLabels": { "app": name } },
            "template": {
                "metadata": { "labels": { "app": name } },
                "spec": {
                    "containers": [{
                        "name": "app",
                        "image": "registry.k8s.io/pause:3.9",
                        "securityContext": {
                            "runAsUser": 65532,
                            "runAsGroup": run_as_group,
                            "runAsNonRoot": true
                        }
                    }]
                }
            }
        }
    })
}

/// Scale a `Deployment` to `replicas` and wait until that many pods are `readyReplicas`.
async fn scale_workload(client: &Client, name: &str, replicas: i32) {
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    deploys
        .patch_scale(
            name,
            &PatchParams::apply("kopiur-e2e").force(),
            &Patch::Apply(serde_json::json!({
                "apiVersion": "autoscaling/v1",
                "kind": "Scale",
                "metadata": { "name": name, "namespace": E2E_NAMESPACE },
                "spec": { "replicas": replicas }
            })),
        )
        .await
        .unwrap_or_else(|e| panic!("scale {name} to {replicas}: {e}"));
    wait_until(
        &format!("{name} at {replicas} ready replica(s)"),
        default_timeout(),
        poll_interval(),
        || async {
            let d = deploys.get(name).await?;
            let ready = d
                .status
                .as_ref()
                .and_then(|s| s.ready_replicas)
                .unwrap_or(0);
            Ok((ready == replicas).then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{name} never settled at {replicas} ready replica(s): {e}"));
}

/// This Snapshot's `SecurityContextResolved` condition (status, reason, message), or `None`
/// when it is absent — the absence IS the assertion for the fallback scenario.
async fn security_context_resolved(
    backups: &Api<Snapshot>,
    name: &str,
) -> Option<(String, String, String)> {
    let b = backups.get_opt(name).await.ok().flatten()?;
    b.status.as_ref().and_then(|s| {
        s.conditions
            .iter()
            .find(|c| c.type_ == kopiur_api::consts::SECURITY_CONTEXT_RESOLVED_CONDITION)
            .map(|c| (c.status.clone(), c.reason.clone(), c.message.clone()))
    })
}

/// Wait until `SecurityContextResolved` reaches `(want_status, want_reason)`, returning its
/// message.
async fn wait_security_context_resolved(
    backups: &Api<Snapshot>,
    name: &str,
    want_status: &str,
    want_reason: &str,
) -> String {
    wait_until(
        &format!("SecurityContextResolved={want_status} ({want_reason}) on {name}"),
        default_timeout(),
        poll_interval(),
        || async {
            Ok(security_context_resolved(backups, name)
                .await
                .filter(|(status, reason, _)| status == want_status && reason == want_reason)
                .map(|(_, _, message)| message))
        },
    )
    .await
    .unwrap_or_else(|e| {
        panic!("{name} never reported SecurityContextResolved={want_status} ({want_reason}): {e}")
    })
}

/// #464 — THE HEADLINE for the hold. A `workloadSelector` inherit whose workload is scaled to
/// ZERO, with no explicit `mover.securityContext` to fall back on, must PARK the run
/// (`phase: Pending`, `SecurityContextResolved=False`/`InheritSourceMissing`, a Warning Event
/// naming the selector, and NO mover Job) instead of failing with an opaque generic
/// `MissingDependency`.
///
/// Then — the half that only an e2e can prove — scaling the workload back UP must let the SAME
/// Snapshot proceed with no re-apply, **and must CLEAR the gate**. That clear is the
/// regression guard for the inverse bug: `kubectl kopiur doctor` suppresses a stale structural
/// gate only for a TERMINAL phase, so a `Running`/`Succeeded` Snapshot still carrying the
/// `False` would be reported as "blocked … it will wait forever", for the whole mover run.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn inherit_holds_when_the_workload_is_scaled_to_zero_then_heals_on_scale_up() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "moverdefaults").await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let events: Api<Event> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-hold-repo";
    let policy = "e2e-hold-policy";
    let backup = "e2e-hold-backup";
    let workload = "e2e-hold-wl";

    // Create the workload FIRST and let it settle, so the scale-to-zero below is a real
    // transition from a known-good state rather than a never-existed one.
    create_idempotent(
        &deploys,
        &cr(inherit_workload_deployment(workload, 1, 4321)),
        "create the inherit-source Deployment",
    )
    .await;
    scale_workload(&client, workload, 1).await;

    create_idempotent(
        &repos,
        &cr(repository_json(
            repo,
            "moverdefaults",
            serde_json::json!({}),
        )),
        "create Repository",
    )
    .await;
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("Repository should bootstrap to Ready");

    // The recipe: inherit from the labelled workload, and pin NOTHING else. The absent
    // `mover.securityContext` is the scenario — with one that pins a runAsUser this run would
    // take the fallback path instead (the second scenario below).
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            policy,
            "Repository",
            repo,
            serde_json::json!({
                "mover": {
                    "inheritSecurityContextFrom": {
                        "workloadSelector": { "podSelector": { "matchLabels": { "app": workload } } }
                    }
                }
            }),
        )),
        "create SnapshotPolicy with a workloadSelector inherit and no fallback",
    )
    .await;

    // Quiesce the workload — the case the whole feature is for.
    scale_workload(&client, workload, 0).await;

    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            backup,
            policy,
            serde_json::json!({}),
        )),
        "create Snapshot",
    )
    .await;

    // (1) HELD, with the registered gate and a message that NAMES the selector — the
    //     reporter's actual ask in #464 ("a warning naming the selector … is the thing I'd
    //     have wanted most").
    let message =
        wait_security_context_resolved(&backups, backup, "False", "InheritSourceMissing").await;
    assert!(
        message.contains(&format!("app={workload}")),
        "the hold message must name the selector that matched nothing: {message}"
    );
    assert!(
        message.contains("HELD"),
        "the message must say the run is held, not merely that something failed: {message}"
    );
    wait_phase(&backups, backup, "Pending")
        .await
        .expect("the held Snapshot must park at phase Pending, not Failed");

    // (2) NO mover Job. Launching one would run the backup at the mover image's own uid —
    //     the wrong-UID backup the hold exists to prevent.
    assert!(
        jobs.get_opt(backup)
            .await
            .expect("list mover Jobs")
            .is_none(),
        "a held run must not launch a mover Job"
    );

    // (3) A Warning Event carrying the gate's reason and the remediation action, so the hold
    //     is visible to `kubectl describe` and not only in the condition.
    let ev = wait_until(
        "an InheritSourceMissing Warning Event regarding the Snapshot",
        default_timeout(),
        poll_interval(),
        || async {
            let list = events.list(&ListParams::default()).await?;
            Ok(list.items.into_iter().find(|e| {
                e.type_.as_deref() == Some("Warning")
                    && e.reason.as_deref() == Some("InheritSourceMissing")
                    && e.regarding.as_ref().is_some_and(|r| {
                        r.kind.as_deref() == Some("Snapshot") && r.name.as_deref() == Some(backup)
                    })
            }))
        },
    )
    .await
    .expect("the hold must publish a Warning Event, not park silently");
    assert_eq!(
        ev.action.as_deref(),
        Some("ScaleWorkloadOrPinMoverRunAsUser"),
        "the Event action is the remediation hint `kubectl describe` surfaces"
    );
    assert!(
        ev.note
            .unwrap_or_default()
            .contains(&format!("app={workload}")),
        "the Event note must name the selector too"
    );

    // (4) SCALE BACK UP: the same Snapshot proceeds with no re-apply…
    scale_workload(&client, workload, 1).await;
    let job = wait_for_job(&jobs, backup).await;
    assert_eq!(
        job_container_sc(&job).and_then(|sc| sc.run_as_group),
        Some(4321),
        "the mover must carry the workload's INHERITED runAsGroup — proving the run resumed \
         through the inherit path, not by defaulting"
    );
    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("the previously-held Snapshot must complete once the workload is back");

    // (5) …and the gate is CLEARED. THE Important-1 guard: without the heal this condition
    //     stays `False` through `Running` and `Succeeded`, and `doctor` — which suppresses a
    //     stale gate only on a terminal phase — reports a healthy, finished backup as
    //     "blocked … it will wait forever".
    let (status, reason, _) = security_context_resolved(&backups, backup)
        .await
        .expect("the condition must still be present, healed rather than removed");
    assert_eq!(
        (status.as_str(), reason.as_str()),
        ("True", "InheritSourceResolved"),
        "the hold must heal once resolution succeeds, or doctor keeps reporting a phantom block"
    );

    // Cleanup.
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
    let _ = deploys.delete(workload, &DeleteParams::default()).await;
}

/// #464 — the negative half, and the guard that the fix did not break the working case. The
/// SAME scaled-to-zero workload, but the recipe ALSO pins `mover.securityContext.runAsUser`:
/// that context is the deliberate fallback, so the run must PROCEED and must carry NO
/// `SecurityContextResolved` condition at all — only the advisory
/// `SecurityContextInherited=False`/`InheritFallback`.
///
/// This is the arm a careless fix breaks: parking here would turn every working
/// inherit-with-fallback recipe into a wedged backup the moment its workload restarted.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn inherit_with_a_pinned_fallback_uid_proceeds_and_never_writes_the_hold() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "moverdefaults").await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-fbhold-repo";
    let policy = "e2e-fbhold-policy";
    let backup = "e2e-fbhold-backup";
    let workload = "e2e-fbhold-wl";

    // A Deployment that exists but is scaled to zero: the selector is right, there is simply
    // no live pod to read — byte-identical circumstances to the scenario above.
    create_idempotent(
        &deploys,
        &cr(inherit_workload_deployment(workload, 0, 4321)),
        "create the (quiesced) inherit-source Deployment",
    )
    .await;

    create_idempotent(
        &repos,
        &cr(repository_json(
            repo,
            "moverdefaults",
            serde_json::json!({}),
        )),
        "create Repository",
    )
    .await;
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("Repository should bootstrap to Ready");

    // The difference from the scenario above, and the only one: an explicit context that PINS
    // an identity. `runAsGroup: 1234` is the tell-tale — `runAsUser` stays 65532 so the mover
    // can read the shared repo, but 65532 is also the hardened default, so the group is what
    // proves the explicit context (not a default) reached the Job.
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            policy,
            "Repository",
            repo,
            serde_json::json!({
                "mover": {
                    "inheritSecurityContextFrom": {
                        "workloadSelector": { "podSelector": { "matchLabels": { "app": workload } } }
                    },
                    "securityContext": { "runAsUser": 65532, "runAsGroup": 1234, "runAsNonRoot": true }
                }
            }),
        )),
        "create SnapshotPolicy with a workloadSelector inherit AND a pinned fallback",
    )
    .await;
    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            backup,
            policy,
            serde_json::json!({}),
        )),
        "create Snapshot",
    )
    .await;

    // A Job at all is the point: the fallback keeps backups running while the app is down.
    let job = wait_for_job(&jobs, backup).await;
    assert_eq!(
        job_container_sc(&job).and_then(|sc| sc.run_as_group),
        Some(1234),
        "the mover must run on the EXPLICIT fallback context, not a default"
    );
    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("a fallback run must complete, not be held");

    // The hold condition must never have been written — this run was never blocked, so
    // `doctor` must see nothing here at all.
    assert!(
        security_context_resolved(&backups, backup).await.is_none(),
        "a run that took the fallback must carry NO SecurityContextResolved condition; writing \
         one (even True) would make every fallback run look like it had been parked"
    );

    // …and the advisory report still fires, so the fallback is not silent.
    let inherited = backups
        .get(backup)
        .await
        .expect("get Snapshot")
        .status
        .and_then(|s| {
            s.conditions
                .iter()
                .find(|c| c.type_ == "SecurityContextInherited")
                .map(|c| (c.status.clone(), c.reason.clone()))
        });
    assert_eq!(
        inherited,
        Some(("False".to_string(), "InheritFallback".to_string())),
        "the fallback must still be REPORTED as advisory — the run is not tracking the workload"
    );

    // Cleanup.
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
    let _ = deploys.delete(workload, &DeleteParams::default()).await;
}

/// The storage class the `snapshot-stack` mise task installs (`csi: true` shards only —
/// `reshape-core`, which runs this suite, is one). Mirrors `copy_methods.rs`.
const CSI_STORAGE_CLASS: &str = "csi-hostpath-sc";

/// A CSI-provisioned, bound, seeded source PVC — the only kind `copyMethod: Snapshot` can
/// stage. Deliberately NOT the shared static hostPath `e2e-src`, which has no provisioner.
async fn csi_source_pvc(client: &Client, pvc: &str, seed_pod: &str) {
    use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = pvcs
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                "metadata": { "name": pvc, "namespace": E2E_NAMESPACE },
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "storageClassName": CSI_STORAGE_CLASS,
                    "resources": { "requests": { "storage": "64Mi" } },
                },
            })),
        )
        .await;
    let _ = pods
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": { "name": seed_pod, "namespace": E2E_NAMESPACE },
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "seed", "image": kopiur_e2e::consts::BUSYBOX_IMAGE,
                        "imagePullPolicy": "IfNotPresent",
                        "command": ["sh", "-c", "echo kopiur-464 > /data/marker.txt"],
                        "volumeMounts": [{ "name": "d", "mountPath": "/data" }],
                    }],
                    "volumes": [{ "name": "d", "persistentVolumeClaim": { "claimName": pvc } }],
                },
            })),
        )
        .await;
    wait_until(
        &format!("CSI source PVC {pvc} Bound"),
        default_timeout(),
        poll_interval(),
        || async {
            let bound = pvcs
                .get_opt(pvc)
                .await?
                .and_then(|p| p.status.and_then(|s| s.phase))
                .as_deref()
                == Some("Bound");
            Ok(bound.then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("the CSI source PVC {pvc} must bind: {e}"));
}

/// #464 round 2 — the heal must SURVIVE the rest of the launch pass, on the copyMethod that
/// actually reaches the writers that used to erase it.
///
/// `inherit_holds_when_the_workload_is_scaled_to_zero_then_heals_on_scale_up` covers the same
/// hold/heal cycle, but on `copyMethod: Direct` (the base policy JSON's default), where
/// `resolve_staging` returns `NotApplicable` and writes nothing. Under **`Snapshot`** (and
/// `Clone`) it writes `SourceStaged` TWICE — once waiting on the VolumeSnapshot, once when
/// the stage is ready — both LATER in the same pass than the heal. Those writers used to
/// build their array from the reconcile-START copy, and a `conditions` merge patch REPLACES
/// the array, so they wrote the `False` straight back; the next pass healed from a
/// start-of-pass copy that said `False` again, forever. The Snapshot below therefore
/// SUCCEEDS while `doctor` calls it "blocked … it will wait forever" — the exact false
/// diagnosis this condition was added to remove, just narrowed to non-`Direct` copyMethods.
///
/// Requires the CSI snapshot stack, which the `reshape-core` shard (`csi: true`) installs.
#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:snapshot-stack)"]
async fn the_inherit_heal_survives_a_staged_copymethod_launch_pass() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    ensure_repo(&client, "moverdefaults").await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-stghold-repo";
    let policy = "e2e-stghold-policy";
    let backup = "e2e-stghold-backup";
    let workload = "e2e-stghold-wl";
    let src = "e2e-stghold-src";

    csi_source_pvc(&client, src, "e2e-stghold-seed").await;
    create_idempotent(
        &deploys,
        &cr(inherit_workload_deployment(workload, 1, 4322)),
        "create the inherit-source Deployment",
    )
    .await;
    scale_workload(&client, workload, 1).await;

    create_idempotent(
        &repos,
        &cr(repository_json(
            repo,
            "moverdefaults",
            serde_json::json!({}),
        )),
        "create Repository",
    )
    .await;
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("Repository should bootstrap to Ready");

    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            policy,
            "Repository",
            repo,
            serde_json::json!({
                // The whole point of this sibling. `merge_spec` takes BARE spec fields, and
                // the base JSON hardcodes `Direct` — asserted below, because a wrapper or a
                // pruned field would silently re-run the Direct scenario and pass green.
                "copyMethod": "Snapshot",
                "sources": [ { "pvc": { "name": src } } ],
                "mover": {
                    "inheritSecurityContextFrom": {
                        "workloadSelector": { "podSelector": { "matchLabels": { "app": workload } } }
                    }
                }
            }),
        )),
        "create SnapshotPolicy with copyMethod: Snapshot and a workloadSelector inherit",
    )
    .await;
    // Read it back: the overlay must have LANDED. (The e2e-overlay gotcha — a `{"spec": ...}`
    // wrapper is dropped by serde AND pruned by the apiserver.)
    let landed = policies.get(policy).await.expect("read the policy back");
    assert_eq!(
        landed.spec.copy_method,
        kopiur_api::CopyMethod::Snapshot,
        "the copyMethod overlay must land, or this test silently re-runs the Direct scenario"
    );

    scale_workload(&client, workload, 0).await;
    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            backup,
            policy,
            serde_json::json!({}),
        )),
        "create Snapshot",
    )
    .await;

    // (1) HELD, exactly as on the Direct path.
    wait_security_context_resolved(&backups, backup, "False", "InheritSourceMissing").await;
    assert!(
        jobs.get_opt(backup)
            .await
            .expect("list mover Jobs")
            .is_none(),
        "a held run must not launch a mover Job"
    );

    // (2) Scale back up: the SAME Snapshot stages a CSI VolumeSnapshot and runs.
    scale_workload(&client, workload, 1).await;
    let job = wait_for_job(&jobs, backup).await;
    assert_eq!(
        job_container_sc(&job).and_then(|sc| sc.run_as_group),
        Some(4322),
        "the mover must carry the workload's INHERITED runAsGroup"
    );
    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("the previously-held staged Snapshot must complete once the workload is back");

    // (3) THE ROUND-2 GUARD. The run staged (so both `SourceStaged` writers fired after the
    //     heal) and finished — and the gate must read `True`. Before the fix this is `False`,
    //     durably, on a Snapshot that Succeeded.
    let done = backups.get(backup).await.expect("read the Snapshot back");
    let staged_ready = done
        .status
        .as_ref()
        .and_then(|s| s.staged.as_ref())
        .and_then(|s| s.ready);
    assert_eq!(
        staged_ready,
        Some(true),
        "the run must actually have STAGED, or it never reached the clobbering writers: {:#?}",
        done.status
    );
    let (status, reason, _) = security_context_resolved(&backups, backup)
        .await
        .expect("the condition must still be present, healed rather than removed");
    assert_eq!(
        (status.as_str(), reason.as_str()),
        ("True", "InheritSourceResolved"),
        "the heal must survive staging's status writes, or doctor reports a succeeded backup \
         as blocked forever"
    );

    // Cleanup.
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
    let _ = deploys.delete(workload, &DeleteParams::default()).await;
}
