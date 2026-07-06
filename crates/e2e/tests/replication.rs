//! e2e: `RepositoryReplication` mirrors a source filesystem repo to a second
//! filesystem repo on a schedule via `kopia repository sync-to` (ADR-0005 §13(d)).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use kube::Api;
use kube::api::{DeleteParams, PostParams};

use kopiur_api::RepositoryReplication;
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
                    "schedule": { "cron": "* * * * *" }
                }
            })),
        )
        .await
        .expect("create RepositoryReplication to a second filesystem repo");

    // Within a couple of minutes a replication runs and records lastReplicated.
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
