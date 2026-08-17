//! e2e: `RepositoryReplication` mirrors a source filesystem repo to a second
//! filesystem repo on a schedule via `kopia repository sync-to` (ADR-0005 §13(d)).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;

use kopiur_api::{Repository, RepositoryReplication};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait_until};

/// `RepositoryReplication` (ADR-0005 §13(d)): mirror a source filesystem repo to a
/// SECOND filesystem repo on a schedule (`kopia repository sync-to`). An every-minute
/// schedule drives a replication mover Job; on success the controller records
/// `status.lastReplicated`. We then verify the destination actually received the
/// snapshot by connecting a verifier Repository to it.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn repository_replication_mirrors_to_second_filesystem_repo() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    // A source repo with a real snapshot to mirror.
    ensure_seed(
        &client,
        "e2e-repl-src",
        "e2e-repl-policy",
        "e2e-repl-seed",
        "repl-src",
    )
    .await;
    // The destination is its OWN isolated repo dir (PVC bound 1:1 to its own
    // hostPath) — a distinct kopia repo from the source, so the mirror is observable.
    ensure_repo(&client, "repl-dst").await;

    let repls: Api<RepositoryReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = "e2e-repl";
    // Mirror to a second filesystem repo (a filesystem destination needs no access
    // credentials; the mirror reuses the source password verbatim, since sync-to is a
    // blob copy).
    repls
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "RepositoryReplication",
                "metadata": { "name": name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "sourceRef": { "kind": "Repository", "name": "e2e-repl-src" },
                    // The destination uses a DISTINCT in-pod path from the source's
                    // `/repo`: a replication mover mounts BOTH source and destination in
                    // one Job, so they cannot share a mount path (the operator rejects a
                    // same-path/same-target destination). kopia writes the repo at the
                    // PVC root regardless of the mount path, so the `repl-dst` verifier
                    // (which mounts the same PVC at `/repo`) still reads the mirror.
                    "destination": { "filesystem": { "path": "/repo-dst", "volume": { "pvc": { "name": consts::isolated_repo_pvc("repl-dst") } } } },
                    "schedule": { "cron": "* * * * *" },
                    // #216: sync-to tuning — asserted both at the mover's actual
                    // behavior (the run still succeeds/records lastReplicated with a
                    // real `--parallel`/`--delete` on argv) AND at the work-spec
                    // ConfigMap contract below (the seam where a regression would
                    // silently drop the knobs while the run still succeeded).
                    "sync": { "parallel": 2, "deleteExtra": true }
                }
            })),
        )
        .await
        .expect("create RepositoryReplication to a second filesystem repo");

    // The per-slot mover Job's inline work-spec env carries the #216 sync
    // knobs — the controller→mover contract, mirroring the `policy_knobs`
    // compression/upload-knobs pattern. The spec rides the Job itself (#224 —
    // no per-run ConfigMap), and the Job outlives the slot by its TTL.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let selector = format!(
        "app.kubernetes.io/component=replication,kopiur.home-operations.com/replication={name}"
    );
    let job = wait_until(
        "a replication mover Job is created",
        default_timeout(),
        poll_interval(),
        || async {
            let list = jobs.list(&ListParams::default().labels(&selector)).await?;
            Ok(list.items.into_iter().next())
        },
    )
    .await
    .expect("a replication mover Job should be created");
    let raw = job
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.containers.first())
        .and_then(|c| c.env.as_ref())
        .and_then(|env| env.iter().find(|e| e.name == "KOPIUR_WORK_SPEC"))
        .and_then(|e| e.value.clone())
        .expect("replication Job carries the inline work-spec env");
    let spec: serde_json::Value = serde_json::from_str(&raw).expect("work-spec env parses as JSON");
    let replicate = spec
        .pointer("/operation/replicate")
        .unwrap_or(&serde_json::Value::Null);
    assert_eq!(
        replicate.get("parallel").and_then(|v| v.as_i64()),
        Some(2),
        "sync.parallel must reach the mover contract; replicate: {replicate}"
    );
    assert_eq!(
        replicate.get("deleteExtra").and_then(|v| v.as_bool()),
        Some(true),
        "sync.deleteExtra must reach the mover contract; replicate: {replicate}"
    );

    // Within a couple of minutes a replication runs and records lastReplicated —
    // proving kopia actually accepted `--parallel 2 --delete` on argv.
    wait_until(
        "RepositoryReplication records a successful run (status.lastReplicated)",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repls, name).await;
            Ok(s.get("lastReplicated")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect("the replication should run and stamp status.lastReplicated");

    // kstatus consistency guard (regression: the same two-pass heal bug as
    // restore/snapshot). The mover stamps `phase: Succeeded` + `lastReplicated`;
    // the controller heals `Ready=True` in a FOLLOW-UP reconcile. Gate on the
    // healed condition — `wait_ready`, not the mover-written `lastReplicated` —
    // so `kubectl wait --for=condition=Ready` / Flux health gates work and a
    // future heal-latency regression is caught here, not just in CI flake.
    wait_ready(&repls, name)
        .await
        .expect("a replication that recorded a successful run must heal kstatus Ready=True");

    // The destination repo must now hold the mirrored snapshot: connect a verifier.
    let count = observed_snapshot_count(&client, "e2e-repl-verify", "repl-dst").await;
    assert!(
        count >= 1,
        "the destination repository must hold the mirrored snapshot, got {count}"
    );

    // Leak guard (the "605 ConfigMaps" fix, #224): a replication slot creates
    // NO per-run ConfigMap at all — the spec rides the Job env. Per-slot names
    // used to accumulate one ConfigMap per slot on the long-lived
    // RepositoryReplication CR, forever.
    for job in jobs
        .list(&ListParams::default().labels(&selector))
        .await
        .expect("list replication Jobs")
        .items
    {
        let Some(job_name) = job.metadata.name else {
            continue;
        };
        assert!(
            cms.get_opt(&job_name)
                .await
                .expect("query ConfigMap")
                .is_none(),
            "replication slot {job_name} must not create a per-run work-spec ConfigMap"
        );
    }

    let _ = repls.delete(name, &DeleteParams::default()).await;
}

/// Regression guard for #200: a `RepositoryReplication` from a **filesystem** source
/// to an **S3** destination. The destination bucket needs its OWN access credentials,
/// which the mover must inject (under the `KOPIUR_DEST_` env prefix) and remap for the
/// `sync-to` subprocess. Before the fix the destination Secret was never wired in, so
/// `kopia repository sync-to s3` ran with no `--access-key`/`--secret-access-key` and
/// failed — the replication never recorded a successful run. Reaching
/// `status.lastReplicated` + `Ready=True` therefore proves the destination credentials
/// reached the mover. (The source is filesystem — it carries no AWS_* of its own — so
/// the S3 auth here can ONLY have come from the injected destination Secret.)
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn repository_replication_mirrors_filesystem_source_to_s3_destination() {
    let Some(world) = World::connect().await else {
        return;
    };
    // Filesystem source PVC + credential Secret, and MinIO with the S3 credential
    // Secret placed in the operator namespace (== E2E_NAMESPACE), so the destination
    // Secret co-resides with the CR as the admission webhook requires.
    world
        .ensure(&[Need::Filesystem, Need::Minio])
        .await
        .expect("fixtures");
    let client = world.client().clone();
    ensure_seed(
        &client,
        "e2e-repl-s3-src",
        "e2e-repl-s3-policy",
        "e2e-repl-s3-seed",
        "repl-s3-src",
    )
    .await;

    let repls: Api<RepositoryReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = "e2e-repl-s3";
    repls
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "RepositoryReplication",
                "metadata": { "name": name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "sourceRef": { "kind": "Repository", "name": "e2e-repl-s3-src" },
                    // The destination's OWN S3 keys ride auth.secretRef. No namespace →
                    // same namespace as the CR (co-resident, as the webhook enforces).
                    "destination": { "s3": {
                        "bucket": "kopiur-repl-dst",
                        "endpoint": consts::MINIO_ENDPOINT,
                        "region": "us-east-1",
                        "tls": { "disableTls": true },
                        "auth": { "secretRef": { "name": consts::SECRET_S3_CREDS } }
                    }},
                    "schedule": { "cron": "* * * * *" }
                }
            })),
        )
        .await
        .expect("create RepositoryReplication to an S3 destination");

    // sync-to to S3 only succeeds with the destination credentials — this is the guard.
    wait_until(
        "RepositoryReplication to S3 records a successful run (status.lastReplicated)",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repls, name).await;
            Ok(s.get("lastReplicated")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect("the S3-destination replication should run and stamp status.lastReplicated");

    wait_ready(&repls, name).await.expect(
        "an S3-destination replication that recorded a successful run must heal Ready=True",
    );

    let _ = repls.delete(name, &DeleteParams::default()).await;
}

/// The definitive #200 guard: an **S3 source → S3 destination** replication where the
/// two backends use DISJOINT, bucket-scoped MinIO users. The source user can write
/// only the source bucket; the destination user only the destination bucket. So
/// `kopia repository sync-to` — which reads the source and writes the destination in
/// one process — succeeds ONLY if the mover authenticates the destination write with
/// the DESTINATION user's keys (the `KOPIUR_DEST_` remap) while the source read keeps
/// using the source's. If the remap were broken (destination fell back to the source's
/// `AWS_*`), the write to the destination bucket would be denied and the run would
/// never stamp `lastReplicated`. Passing therefore proves BOTH the collision-free
/// separation AND that remapping `AWS_*` for the sync-to child does not break the
/// source read (kopia persists the source storage creds into repository.config at
/// connect).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn repository_replication_s3_to_s3_uses_destination_scoped_credentials() {
    let Some(world) = World::connect().await else {
        return;
    };
    // MinIO with the two bucket-scoped users + their Secrets (in the operator ns,
    // which is E2E_NAMESPACE, so the destination Secret co-resides with the CR).
    world.ensure(&[Need::Minio]).await.expect("fixtures");
    let client = world.client().clone();

    // Source S3 repository, writable ONLY by the source-scoped user. `create.enabled`
    // bootstraps it — proving the source keys can write the source bucket.
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let src = "e2e-repl-s2s-src";
    repos
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Repository",
                "metadata": { "name": src, "namespace": E2E_NAMESPACE },
                "spec": {
                    "backend": { "s3": {
                        "bucket": consts::S3_REPL_SRC_BUCKET,
                        "endpoint": consts::MINIO_ENDPOINT,
                        "region": "us-east-1",
                        "tls": { "disableTls": true },
                        "auth": { "secretRef": { "name": consts::SECRET_S3_REPL_SRC } }
                    }},
                    "encryption": { "passwordSecretRef": { "name": consts::SECRET_S3_REPL_SRC, "key": consts::KEY_KOPIA_PASSWORD } },
                    "create": { "enabled": true }
                }
            })),
        )
        .await
        .expect("create source S3 repository");
    // Gate on phase, not a `Ready` condition: a namespaced `Repository`'s bootstrap
    // success writes `phase: Ready` + a `Bootstrapped` condition — it never emits a
    // kstatus `Ready` condition, so `wait_ready` would time out every run.
    wait_phase(&repos, src, "Ready")
        .await
        .expect("source S3 repository bootstraps (source-scoped keys can write the source bucket)");

    // Replicate to a DIFFERENT bucket writable ONLY by the destination-scoped user.
    let repls: Api<RepositoryReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = "e2e-repl-s2s";
    repls
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "RepositoryReplication",
                "metadata": { "name": name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "sourceRef": { "kind": "Repository", "name": src },
                    "destination": { "s3": {
                        "bucket": consts::S3_REPL_DST_BUCKET,
                        "endpoint": consts::MINIO_ENDPOINT,
                        "region": "us-east-1",
                        "tls": { "disableTls": true },
                        "auth": { "secretRef": { "name": consts::SECRET_S3_REPL_DST } }
                    }},
                    "schedule": { "cron": "* * * * *" }
                }
            })),
        )
        .await
        .expect("create S3→S3 RepositoryReplication");

    wait_until(
        "S3→S3 replication records a successful run (status.lastReplicated)",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repls, name).await;
            Ok(s.get("lastReplicated")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect("S3→S3 replication should succeed using the destination-scoped credentials");

    wait_ready(&repls, name)
        .await
        .expect("the S3→S3 replication must heal Ready=True");

    let _ = repls.delete(name, &DeleteParams::default()).await;
    let _ = repos.delete(src, &DeleteParams::default()).await;
}

/// Issue #380: an ON-DEMAND run, requested with the `run-requested`
/// annotation, on a replication whose cron will not fire for months.
///
/// The far-future schedule (`0 5 1 1 *` — 05:00 on 1 January) is the whole
/// point: nothing but the annotation can produce a run, so `lastReplicated`
/// stamping proves the REQUEST drove it rather than a slot that happened to
/// come due. It also pins the two halves of the contract that a unit test
/// cannot reach — that the requested run rides the ordinary mover path (a real
/// `sync-to` into a real destination), and that `status.manualRun` answers the
/// exact timestamp that was asked for.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn repository_replication_runs_on_demand_from_the_run_requested_annotation() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_seed(
        &client,
        "e2e-repl-run-src",
        "e2e-repl-run-policy",
        "e2e-repl-run-seed",
        "repl-run-src",
    )
    .await;
    ensure_repo(&client, "repl-run-dst").await;

    let repls: Api<RepositoryReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = "e2e-repl-run";
    repls
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "RepositoryReplication",
                "metadata": { "name": name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "sourceRef": { "kind": "Repository", "name": "e2e-repl-run-src" },
                    "destination": { "filesystem": { "path": "/repo-dst", "volume": { "pvc": { "name": consts::isolated_repo_pvc("repl-run-dst") } } } },
                    // Months away: no cron slot can fire during this test.
                    "schedule": { "cron": "0 5 1 1 *" }
                }
            })),
        )
        .await
        .expect("create RepositoryReplication with a far-future schedule");

    // Nothing should have run yet — otherwise the assertion below would pass
    // on a cron slot and prove nothing about the annotation.
    let before = status_json(&repls, name).await;
    assert!(
        before.get("lastReplicated").is_none(),
        "a far-future schedule must not have replicated yet; status: {before}"
    );

    let requested_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    repls
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "annotations": {
                    kopiur_api::consts::RUN_REQUESTED_ANNOTATION: requested_at
                } }
            })),
        )
        .await
        .expect("annotate the replication with a run request");

    // The requested run goes through the ordinary mover path, so success means
    // a real `sync-to` completed: the mover stamps `lastReplicated`…
    wait_until(
        "an annotation-requested replication run stamps status.lastReplicated",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repls, name).await;
            Ok(s.get("lastReplicated")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect("the requested run should execute and stamp status.lastReplicated");

    // …and the controller answers the EXACT request in status.manualRun.
    let manual = wait_until(
        "status.manualRun reaches a terminal phase",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repls, name).await;
            Ok(s.get("manualRun")
                .and_then(|m| m.get("phase").and_then(|p| p.as_str()).map(|_| m.clone())))
        },
    )
    .await
    .expect("the controller should record status.manualRun");
    assert_eq!(
        manual.get("phase").and_then(|v| v.as_str()),
        Some("Succeeded"),
        "manualRun: {manual}"
    );
    assert_eq!(
        manual.get("requestedAt").and_then(|v| v.as_str()),
        Some(requested_at.as_str()),
        "manualRun must pin the timestamp that was requested; got {manual}"
    );
    assert!(
        manual
            .get("completedAt")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "a terminal manual run must record completedAt; got {manual}"
    );

    // The destination really received the snapshot — the requested run did the
    // same work a scheduled one would.
    let count = observed_snapshot_count(&client, "e2e-repl-run-verify", "repl-run-dst").await;
    assert!(
        count >= 1,
        "the destination repository must hold the mirrored snapshot, got {count}"
    );

    let _ = repls.delete(name, &DeleteParams::default()).await;
}
