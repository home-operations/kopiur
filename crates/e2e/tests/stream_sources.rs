//! e2e: `SnapshotPolicy.spec.sources[].stream` — capture a command's stdout as one
//! virtual file, and stream it back into a pod on restore.
//!
//! These are the acceptance criteria for the feature, and they are written to fail
//! for the RIGHT reason. In particular:
//!
//! * `stream_backup_and_restore_is_byte_identical` proves the data path end-to-end
//!   with content, not just phases — a Snapshot that says `Succeeded` while storing
//!   nothing would pass a phase-only assertion.
//! * `failing_producer_leaves_no_usable_snapshot` is the one that matters most. The
//!   producer writes real bytes and THEN exits non-zero, which is exactly the shape
//!   that would silently store a truncated database dump. It asserts both that the
//!   Snapshot failed AND that the repository holds no snapshot for that identity.
//! * `stream_content_never_appears_in_logs_or_status` greps the operator and mover
//!   output for the payload. The payload is a distinctive sentinel precisely so a
//!   leak cannot hide in ordinary noise.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use common::{cr, ensure_repo, repository_json, wait_phase};
use kube::api::{DeleteParams, ListParams, LogParams, PostParams};
use kube::{Api, Client, ResourceExt};

use k8s_openapi::api::core::v1::{Namespace, Pod};

use kopiur_api::{Repository, Restore, Snapshot, SnapshotPolicy};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, builders, default_timeout, poll_interval, wait_until,
};

const SUBPATH: &str = "stream";
const REPO: &str = "e2e-stream-repo";
const FAIL_SUBPATH: &str = "stream-fail";
const FAIL_REPO: &str = "e2e-stream-fail-repo";

/// A distinctive payload: if any of it reaches a log, an Event, or a status field,
/// the leak test finds it. Deliberately unlike anything the operator prints itself.
const SENTINEL: &str = "KOPIUR-STREAM-SENTINEL-8f3a1c9e-payload-line";

/// Opt the e2e namespace in to stream-exec movers. Without this the Snapshot parks
/// at `MoverPermitted=False` — which is itself asserted by `gate_blocks_until_namespace_opts_in`.
async fn annotate_namespace(client: &Client, ns: &str, on: bool) {
    let api: Api<Namespace> = Api::all(client.clone());
    let patch = serde_json::json!({
        "metadata": { "annotations": {
            kopiur_api::consts::STREAM_EXEC_ANNOTATION: if on { Some("true") } else { None },
        }}
    });
    api.patch(
        ns,
        &kube::api::PatchParams::apply("kopiur-e2e").force(),
        &kube::api::Patch::Merge(&patch),
    )
    .await
    .expect("annotate namespace for stream-exec");
}

/// A long-lived pod that plays the part of the database: it just sleeps, and the
/// stream source execs a command inside it.
fn producer_pod(ns: &str, name: &str) -> Pod {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "namespace": ns, "labels": { "app": name } },
        "spec": {
            "restartPolicy": "Never",
            "containers": [{
                "name": "db",
                "image": kopiur_e2e::consts::BUSYBOX_IMAGE,
                "imagePullPolicy": "IfNotPresent",
                "command": ["sleep", "3600"],
            }],
        },
    }))
    .expect("producer pod")
}

fn stream_policy_json(
    name: &str,
    repo: &str,
    app_label: &str,
    command: serde_json::Value,
    file_name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": repo },
            "identity": { "username": name, "hostname": "e2e" },
            "sources": [{
                "stream": {
                    "fileName": file_name,
                    "workloadExec": {
                        "podSelector": { "matchLabels": { "app": app_label } },
                        "container": "db",
                        "command": command,
                        "timeout": "5m"
                    }
                }
            }]
        }
    })
}

fn snapshot_json(name: &str, policy: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": { "policyRef": { "name": policy } }
    })
}

async fn ensure_producer(client: &Client, name: &str) {
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    if pods.get_opt(name).await.ok().flatten().is_none() {
        let _ = pods
            .create(&PostParams::default(), &producer_pod(E2E_NAMESPACE, name))
            .await;
    }
    wait_until(
        &format!("{name} Running"),
        default_timeout(),
        poll_interval(),
        || async {
            Ok(pods
                .get_opt(name)
                .await?
                .and_then(|p| p.status.and_then(|s| s.phase))
                .filter(|ph| ph == "Running")
                .map(|_| ()))
        },
    )
    .await
    .expect("the producer pod should reach Running");
}

/// Acceptance 1 + 5: known bytes out, byte-identical bytes back in.
#[tokio::test]
#[cfg_attr(not(feature = "e2e"), ignore)]
async fn stream_backup_and_restore_is_byte_identical() {
    let Some(world) = World::connect().await else {
        eprintln!("no cluster; skipping");
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    ensure_repo(&client, SUBPATH).await;
    annotate_namespace(&client, E2E_NAMESPACE, true).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(REPO, SUBPATH, serde_json::json!({}))),
        )
        .await;
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("stream Repository should reach Ready");

    ensure_producer(&client, "stream-db").await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = policies
        .create(
            &PostParams::default(),
            &cr(stream_policy_json(
                "stream-policy",
                REPO,
                "stream-db",
                serde_json::json!(["sh", "-c", format!("printf '%s\\n' '{SENTINEL}'")]),
                "dump.sql",
            )),
        )
        .await;

    let snaps: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = snaps
        .create(
            &PostParams::default(),
            &cr(snapshot_json("stream-snap", "stream-policy")),
        )
        .await;
    wait_phase(&snaps, "stream-snap", "Succeeded")
        .await
        .expect("a stream Snapshot should succeed");

    // The snapshot must actually contain something — a Succeeded phase over an empty
    // capture is precisely the failure this test exists to catch.
    let got = snaps.get("stream-snap").await.expect("snapshot");
    let stats = got.status.as_ref().and_then(|s| s.stats.as_ref());
    assert_eq!(
        stats.and_then(|s| s.files_new),
        Some(1),
        "a stream snapshot stores exactly one virtual file (files_new): {:#?}",
        got.status
    );
    let identity = got
        .status
        .as_ref()
        .and_then(|s| s.snapshot.as_ref())
        .map(|s| s.identity.clone())
        .expect("recorded identity");
    assert_eq!(
        identity.source_path.as_deref(),
        Some("/stream/dump.sql"),
        "a stream source records /stream/<fileName>, distinct from /pvc/<name>"
    );

    // Restore it back into a second pod and read what arrived.
    ensure_producer(&client, "stream-target").await;
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restore = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "stream-restore", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": REPO },
            "source": { "snapshotRef": { "name": "stream-snap" } },
            "target": { "streamExec": {
                "fileName": "dump.sql",
                "workloadExec": {
                    "podSelector": { "matchLabels": { "app": "stream-target" } },
                    "container": "db",
                    // Write stdin to a file we can then read back out of the pod.
                    "command": ["sh", "-c", "cat > /tmp/restored.sql"],
                    "timeout": "5m"
                }
            }}
        }
    });
    let _ = restores.create(&PostParams::default(), &cr(restore)).await;
    wait_phase(&restores, "stream-restore", "Completed")
        .await
        .expect("a streamExec restore should complete");

    // Byte-identity, proved by reading the file the restore wrote INSIDE the pod.
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let mut buf = Vec::new();
    {
        use tokio::io::AsyncReadExt;
        let mut attached = pods
            .exec(
                "stream-target",
                vec!["cat", "/tmp/restored.sql"],
                &kube::api::AttachParams::default()
                    .container("db")
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .expect("exec into the restore target");
        if let Some(mut out) = attached.stdout() {
            let _ = out.read_to_end(&mut buf).await;
        }
    }
    let restored = String::from_utf8_lossy(&buf);
    assert_eq!(
        restored.trim(),
        SENTINEL,
        "the restored bytes must equal what the producer wrote"
    );
}

/// Acceptance 2 — THE correctness criterion. The producer emits real bytes and then
/// fails; the Snapshot must fail AND leave no usable snapshot behind.
#[tokio::test]
#[cfg_attr(not(feature = "e2e"), ignore)]
async fn failing_producer_leaves_no_usable_snapshot() {
    let Some(world) = World::connect().await else {
        eprintln!("no cluster; skipping");
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    ensure_repo(&client, FAIL_SUBPATH).await;
    annotate_namespace(&client, E2E_NAMESPACE, true).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                FAIL_REPO,
                FAIL_SUBPATH,
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&repos, FAIL_REPO, "Ready")
        .await
        .expect("repo Ready");

    ensure_producer(&client, "stream-fail-db").await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = policies
        .create(
            &PostParams::default(),
            &cr(stream_policy_json(
                "stream-fail-policy",
                FAIL_REPO,
                "stream-fail-db",
                // Bytes first, THEN failure: the shape that would otherwise be
                // stored as a complete-looking but truncated dump.
                serde_json::json!(["sh", "-c", "printf 'partial data\\n'; exit 7"]),
                "dump.sql",
            )),
        )
        .await;

    let snaps: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = snaps
        .create(
            &PostParams::default(),
            &cr(snapshot_json("stream-fail-snap", "stream-fail-policy")),
        )
        .await;
    wait_phase(&snaps, "stream-fail-snap", "Failed")
        .await
        .expect("a failing producer must fail the Snapshot");

    // And no snapshot may exist in the repository for that identity: a Restore that
    // asks for the latest must find nothing rather than a truncated dump.
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let probe = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "stream-fail-probe", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": FAIL_REPO },
            "source": { "identity": {
                "username": "stream-fail-policy",
                "hostname": "e2e",
                "sourcePath": "/stream/dump.sql"
            }},
            "target": { "pvc": { "name": "stream-fail-dst", "capacity": "1Gi" } },
            "policy": { "onMissingSnapshot": "Fail", "waitTimeout": "60s" }
        }
    });
    let _ = restores.create(&PostParams::default(), &cr(probe)).await;
    wait_phase(&restores, "stream-fail-probe", "Failed")
        .await
        .expect(
            "restoring the aborted identity must FAIL (nothing was stored); if this \
             completes, a truncated dump was retained as a usable backup",
        );
}

/// Acceptance 3 — selector mismatches fail clearly, and say what to do.
#[tokio::test]
#[cfg_attr(not(feature = "e2e"), ignore)]
async fn zero_pod_matches_fails_with_an_actionable_message() {
    let Some(world) = World::connect().await else {
        eprintln!("no cluster; skipping");
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    ensure_repo(&client, SUBPATH).await;
    annotate_namespace(&client, E2E_NAMESPACE, true).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(REPO, SUBPATH, serde_json::json!({}))),
        )
        .await;
    wait_phase(&repos, REPO, "Ready").await.expect("repo Ready");

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = policies
        .create(
            &PostParams::default(),
            &cr(stream_policy_json(
                "stream-nopod-policy",
                REPO,
                "no-such-workload",
                serde_json::json!(["sh", "-c", "echo hi"]),
                "dump.sql",
            )),
        )
        .await;

    let snaps: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = snaps
        .create(
            &PostParams::default(),
            &cr(snapshot_json("stream-nopod-snap", "stream-nopod-policy")),
        )
        .await;
    wait_phase(&snaps, "stream-nopod-snap", "Failed")
        .await
        .expect("a selector matching no pod must fail the Snapshot");

    let got = snaps.get("stream-nopod-snap").await.expect("snapshot");
    let text = serde_json::to_string(&got.status).unwrap_or_default();
    assert!(
        text.contains("no pod matches podSelector"),
        "the failure must name the selector problem, got: {text}"
    );
}

/// Acceptance 4 — the dump must not leak into logs, Events, or CR status.
#[tokio::test]
#[cfg_attr(not(feature = "e2e"), ignore)]
async fn stream_content_never_appears_in_logs_or_status() {
    let Some(world) = World::connect().await else {
        eprintln!("no cluster; skipping");
        return;
    };
    let client = world.client().clone();

    // Depends on the backup scenario having run in this shard.
    let snaps: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let Some(got) = snaps.get_opt("stream-snap").await.ok().flatten() else {
        eprintln!("stream-snap absent; the backup scenario owns it — skipping");
        return;
    };

    // 1. CR status.
    let status = serde_json::to_string(&got.status).unwrap_or_default();
    assert!(
        !status.contains(SENTINEL),
        "the dump payload leaked into Snapshot.status: {status}"
    );

    // 2. Events on the Snapshot.
    let events: Api<k8s_openapi::api::core::v1::Event> =
        Api::namespaced(client.clone(), E2E_NAMESPACE);
    let listed = events.list(&ListParams::default()).await.expect("events");
    for e in listed.items {
        let blob = serde_json::to_string(&e).unwrap_or_default();
        assert!(
            !blob.contains(SENTINEL),
            "the dump payload leaked into an Event"
        );
    }

    // 3. Mover pod logs. The mover copies the bytes pipe-to-pipe, so none of them
    //    should ever have been written to its stdout.
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let mover_pods = pods
        .list(&ListParams::default().labels("app.kubernetes.io/component=mover"))
        .await
        .expect("list mover pods");
    for p in mover_pods.items {
        let name = p.name_any();
        if let Ok(logs) = pods.logs(&name, &LogParams::default()).await {
            assert!(
                !logs.contains(SENTINEL),
                "the dump payload leaked into mover pod {name}'s logs"
            );
        }
    }
}

/// The namespace opt-in actually gates: without the annotation the run parks
/// instead of exec'ing. Uses its own policy so it cannot race the others.
#[tokio::test]
#[cfg_attr(not(feature = "e2e"), ignore)]
async fn gate_blocks_until_namespace_opts_in() {
    let Some(world) = World::connect().await else {
        eprintln!("no cluster; skipping");
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    ensure_repo(&client, SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(REPO, SUBPATH, serde_json::json!({}))),
        )
        .await;
    wait_phase(&repos, REPO, "Ready").await.expect("repo Ready");
    ensure_producer(&client, "stream-gate-db").await;

    // Opt OUT first, so the gate is the thing being observed.
    annotate_namespace(&client, E2E_NAMESPACE, false).await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = policies
        .create(
            &PostParams::default(),
            &cr(stream_policy_json(
                "stream-gate-policy",
                REPO,
                "stream-gate-db",
                serde_json::json!(["sh", "-c", "echo gated"]),
                "dump.sql",
            )),
        )
        .await;
    let snaps: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = snaps
        .create(
            &PostParams::default(),
            &cr(snapshot_json("stream-gate-snap", "stream-gate-policy")),
        )
        .await;

    // It must PARK with the actionable condition, not run and not fail outright.
    wait_until(
        "stream-gate-snap parked at StreamExecNotPermitted",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(snaps
                .get_opt("stream-gate-snap")
                .await?
                .map(|s| serde_json::to_string(&s.status).unwrap_or_default())
                .filter(|t| t.contains("StreamExecNotPermitted"))
                .map(|_| ()))
        },
    )
    .await
    .expect("a stream Snapshot must park at StreamExecNotPermitted without the opt-in");

    // Opting in releases it.
    annotate_namespace(&client, E2E_NAMESPACE, true).await;
    wait_phase(&snaps, "stream-gate-snap", "Succeeded")
        .await
        .expect("annotating the namespace should release the parked Snapshot");

    // Leave the shard's other scenarios their fixtures.
    let _ = snaps
        .delete("stream-gate-snap", &DeleteParams::default())
        .await;
}
