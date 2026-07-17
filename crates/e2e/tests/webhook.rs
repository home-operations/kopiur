//! End-to-end: self-managed webhook TLS (`webhook.tls.mode: self`).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`, skipping gracefully without
//! a cluster. The chart installs the webhook with self-managed TLS (no
//! cert-manager): the controller mints its own CA + serving cert into the
//! `kopiur-webhook-tls` Secret and injects the CA into both webhook
//! configurations' `caBundle`. This scenario asserts that whole bootstrap chain
//! against a real API server:
//!
//! 1. the serving Secret is minted (carries `tls.crt`/`tls.key`/`ca.crt`),
//! 2. `caBundle` is populated on the Validating + Mutating configs,
//! 3. a VALID CR is admitted — which, under `failurePolicy: Fail`, can only
//!    happen if the API server reached the webhook over TLS and trusted its cert
//!    (i.e. mint → Secret → pod TLS → caBundle → trust all worked), and
//! 4. an INVALID CR is rejected by the webhook (admission actually runs).
//!
//! Run: `mise run //crates/e2e:test`.

#![cfg(all(unix, feature = "e2e"))]

use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};

use k8s_openapi::api::admissionregistration::v1::{
    MutatingWebhookConfiguration, ValidatingWebhookConfiguration,
};
use k8s_openapi::api::core::v1::Secret;

use kopiur_api::consts::ALLOW_IDENTITY_CHANGE_ANNOTATION;
use kopiur_api::{ClusterRepository, Repository, RepositoryReplication, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, World, default_timeout, poll_interval, wait_until};

/// Names the chart renders for release "kopiur" (the e2e release).
const WEBHOOK_SECRET: &str = "kopiur-webhook-tls";
const VALIDATING_CONFIG: &str = "kopiur-validating";
const MUTATING_CONFIG: &str = "kopiur-mutating";

/// A spec-valid Repository (filesystem backend) — admission validates the spec
/// shape only, so the referenced Secret/PVC need not exist for it to be admitted.
fn valid_repository(name: &str) -> Repository {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": { "passwordSecretRef": { "name": "kopia-creds", "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    }))
    .expect("valid Repository JSON deserializes")
}

/// A SnapshotPolicy whose single source names NEITHER a pvc NOR a selector. This
/// passes the CRD structural schema (both are optional) but the shared
/// `api::validate` validator the webhook runs rejects it — so it exercises the
/// admission *logic*, not just the cert plumbing.
fn invalid_backup_config(name: &str) -> SnapshotPolicy {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "any" },
            "sources": [ {} ],
            "retention": { "keepLatest": 5 }
        }
    }))
    .expect("SnapshotPolicy JSON deserializes")
}

/// A SnapshotPolicy whose mover sets BOTH `securityContext` and
/// `inheritSecurityContextFrom`. This used to be webhook-rejected as mutually exclusive; the
/// two are adjacent merge layers (`inherited ⊂ explicit`), so admission must now ACCEPT it.
fn mover_inherit_plus_explicit_backup_config(name: &str) -> SnapshotPolicy {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "any" },
            "sources": [ { "pvc": { "name": "data" } } ],
            "retention": { "keepLatest": 5 },
            "mover": {
                "securityContext": { "runAsUser": 1000 },
                "inheritSecurityContextFrom": { "workloadSelector": { "podSelector": { "matchLabels": { "app": "x" } } } }
            }
        }
    }))
    .expect("SnapshotPolicy JSON deserializes")
}

#[tokio::test]
#[ignore = "requires a kind cluster with the operator installed (mise //crates/e2e:test)"]
async fn self_managed_webhook_tls_bootstraps_and_gates_admission() {
    let Some(world) = World::connect().await else {
        return; // no cluster: graceful no-op
    };
    let client = world.client().clone();

    // 1. The controller mints the serving Secret (tls + ca material).
    let secrets: Api<Secret> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    wait_until(
        "webhook serving Secret minted with tls + ca material",
        default_timeout(),
        poll_interval(),
        || async {
            let Some(s) = secrets.get_opt(WEBHOOK_SECRET).await? else {
                return Ok(None);
            };
            let data = s.data.unwrap_or_default();
            let has = |k: &str| data.get(k).is_some_and(|b| !b.0.is_empty());
            Ok((has("tls.crt") && has("tls.key") && has("ca.crt")).then_some(()))
        },
    )
    .await
    .expect("webhook TLS Secret should be minted by the controller");

    // 2. The caBundle is injected into BOTH webhook configurations.
    wait_until(
        "caBundle injected into the validating + mutating webhook configs",
        default_timeout(),
        poll_interval(),
        || async {
            let ok_v = validating_has_ca_bundle(&client).await?;
            let ok_m = mutating_has_ca_bundle(&client).await?;
            Ok((ok_v && ok_m).then_some(()))
        },
    )
    .await
    .expect("caBundle should be injected into both webhook configurations");

    // 3. A valid CR is admitted. Under failurePolicy=Fail this proves the API
    //    server reached the webhook over TLS and trusted its cert.
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = "webhook-admit-ok";
    let _ = repos.delete(name, &DeleteParams::default()).await; // clean any leftover
    repos
        .create(&PostParams::default(), &valid_repository(name))
        .await
        .expect("a valid Repository must be ADMITTED — failure here means the API server could not reach/trust the self-managed webhook");
    // Don't let it linger reconciling against absent infra.
    let _ = repos.delete(name, &DeleteParams::default()).await;

    // 4. An invalid CR is rejected BY THE WEBHOOK (admission logic runs).
    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let bad = "webhook-deny";
    let _ = configs.delete(bad, &DeleteParams::default()).await;
    let err = configs
        .create(&PostParams::default(), &invalid_backup_config(bad))
        .await
        .expect_err("a source with neither pvc nor selector must be DENIED by the webhook");
    let msg = err.to_string();
    assert!(
        msg.contains("denied the request") || msg.to_lowercase().contains("admission"),
        "rejection should come from the admission webhook, got: {msg}"
    );
    let _ = configs.delete(bad, &DeleteParams::default()).await;

    // 5. A mover that sets BOTH securityContext and inheritSecurityContextFrom is ACCEPTED:
    //    they are adjacent merge layers (explicit overrides inherited field-wise, and stands
    //    in alone when inheritance cannot resolve a pod), not competing sources. This asserts
    //    the relaxation reaches a live cluster — a unit test on `validate_mover` alone cannot
    //    prove the deployed webhook stopped rejecting it.
    let merged_mover = "webhook-allow-inherit-plus-explicit";
    let _ = configs.delete(merged_mover, &DeleteParams::default()).await;
    configs
        .create(
            &PostParams::default(),
            &mover_inherit_plus_explicit_backup_config(merged_mover),
        )
        .await
        .expect("securityContext + inheritSecurityContextFrom must be ACCEPTED by the webhook");
    let _ = configs.delete(merged_mover, &DeleteParams::default()).await;

    // 6. A RepositoryReplication mover that sets inheritSecurityContextFrom is DENIED. A
    //    replication mover copies blobs repo->repo and never reads a workload's files, so
    //    there is no workload identity to take — and the reconciler never resolves the field.
    //    It used to be accepted and silently dropped: the manifest claimed the mover ran as
    //    the workload, and it did not.
    let repls: Api<RepositoryReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let bad_repl = "webhook-deny-repl-inherit";
    let _ = repls.delete(bad_repl, &DeleteParams::default()).await;
    let err = repls
        .create(&PostParams::default(), &replication_with_inherit(bad_repl))
        .await
        .expect_err("inheritSecurityContextFrom on a replication mover must be DENIED");
    let msg = err.to_string();
    assert!(
        msg.contains("denied the request") || msg.to_lowercase().contains("admission"),
        "the rejection should come from the admission webhook, got: {msg}"
    );
    let _ = repls.delete(bad_repl, &DeleteParams::default()).await;
}

/// A `RepositoryReplication` whose mover sets `inheritSecurityContextFrom` — structurally
/// valid, but unhonorable: replication never reads workload files, so the reconciler would
/// silently drop it. Admission rejects it instead.
fn replication_with_inherit(name: &str) -> RepositoryReplication {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "RepositoryReplication",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "sourceRef": { "kind": "Repository", "name": "any" },
            "destination": { "s3": { "bucket": "mirror", "region": "us-east-1" } },
            "schedule": { "cron": "0 5 * * *" },
            "mover": {
                "inheritSecurityContextFrom": { "pvcConsumer": {} }
            }
        }
    }))
    .expect("RepositoryReplication JSON deserializes")
}

/// A SnapshotPolicy whose explicit `spec.identity.username` carries kopia's `@`
/// delimiter — structurally a valid string, but the shared `validate_identity_component`
/// rejects it (it would misparse `username@hostname`). Exercises the identity shape
/// validator through admission.
fn bad_identity_backup_config(name: &str) -> SnapshotPolicy {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "any" },
            "identity": { "username": "bad@user" },
            "sources": [ { "pvc": { "name": "data" } } ],
            "retention": { "keepLatest": 5 }
        }
    }))
    .expect("SnapshotPolicy JSON deserializes")
}

/// A spec-valid SnapshotPolicy with an explicit pinned identity, suspended so the
/// reconciler doesn't churn it while we drive admission directly.
fn identity_backup_config(name: &str, username: &str, hostname: &str) -> SnapshotPolicy {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "any" },
            "identity": { "username": username, "hostname": hostname },
            "sources": [ { "pvc": { "name": "data" } } ],
            "retention": { "keepLatest": 5 },
            "suspend": true
        }
    }))
    .expect("SnapshotPolicy JSON deserializes")
}

/// The identity shape validator and the fork-on-edit guard run through the real
/// admission webhook against a live API server. The fork case is the one thing unit
/// tests can't prove: that the API server delivers `oldObject.status` (the pinned
/// identity + history) to the webhook on UPDATE.
#[tokio::test]
#[ignore = "requires a kind cluster with the operator installed (mise //crates/e2e:test)"]
async fn identity_shape_and_fork_guard_are_enforced_at_admission() {
    let Some(world) = World::connect().await else {
        return; // no cluster: graceful no-op
    };
    let client = world.client().clone();
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // 1. An identity component carrying kopia's '@' delimiter is rejected.
    let bad = "webhook-bad-identity";
    let _ = policies.delete(bad, &DeleteParams::default()).await;
    let err = policies
        .create(&PostParams::default(), &bad_identity_backup_config(bad))
        .await
        .expect_err("an identity username with '@' must be DENIED by the webhook");
    let msg = err.to_string();
    assert!(
        msg.contains("denied the request") || msg.to_lowercase().contains("admission"),
        "identity shape rejection should come from the webhook, got: {msg}"
    );
    let _ = policies.delete(bad, &DeleteParams::default()).await;

    // 2. Fork-on-edit guard. Create a pinned-identity policy, simulate history by
    //    patching status (resolved identity + lastSuccessfulSnapshot), then attempt to
    //    change the identity.
    let name = "webhook-identity-fork";
    let _ = policies.delete(name, &DeleteParams::default()).await;
    policies
        .create(
            &PostParams::default(),
            &identity_backup_config(name, "pg", "billing"),
        )
        .await
        .expect("a valid pinned-identity policy is admitted");

    let status = serde_json::json!({
        "status": {
            "resolved": { "identity": { "username": "pg", "hostname": "billing" } },
            "lastSuccessfulSnapshot": "2026-01-01T00:00:00Z"
        }
    });
    policies
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&status))
        .await
        .expect("status patch (simulated history) applies");

    // Guard against a false pass: the simulated history must actually be present before
    // we test the UPDATE (the reconciler is suspended, but assert it didn't clear it).
    let got = policies.get_status(name).await.expect("get status");
    assert!(
        got.status
            .as_ref()
            .and_then(|s| s.last_successful_snapshot.as_ref())
            .is_some(),
        "simulated history must be pinned before the UPDATE test"
    );

    // Changing the resolved identity (hostname) on a policy with history → DENIED.
    let change = serde_json::json!({ "spec": { "identity": { "hostname": "payments" } } });
    let err = policies
        .patch(name, &PatchParams::default(), &Patch::Merge(&change))
        .await
        .expect_err("re-identifying a policy with history must be DENIED");
    let msg = err.to_string();
    assert!(
        msg.contains("denied the request") || msg.to_lowercase().contains("admission"),
        "fork rejection should come from the webhook, got: {msg}"
    );

    // The same change WITH the acknowledgment annotation → ADMITTED.
    let acked = serde_json::json!({
        "metadata": { "annotations": { ALLOW_IDENTITY_CHANGE_ANNOTATION: "intentional" } },
        "spec": { "identity": { "hostname": "payments" } }
    });
    policies
        .patch(name, &PatchParams::default(), &Patch::Merge(&acked))
        .await
        .expect("an acknowledged re-identification must be ADMITTED");

    let _ = policies.delete(name, &DeleteParams::default()).await;
}

/// A cluster-scoped, filesystem-backed `ClusterRepository` (admission validates
/// spec shape only — the backend need not actually be reachable for these
/// admission-only assertions, mirroring `valid_repository`/`self_managed_webhook_tls_
/// bootstraps_and_gates_admission`'s own posture above).
fn cluster_repository_json(name: &str) -> ClusterRepository {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "ClusterRepository",
        "metadata": { "name": name },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": {
                "passwordSecretRef": { "name": "kopia-creds", "namespace": E2E_NAMESPACE, "key": "KOPIA_PASSWORD" }
            },
            "create": { "enabled": false },
            "allowedNamespaces": { "all": true }
        }
    }))
    .expect("ClusterRepository JSON deserializes")
}

/// A `SnapshotPolicy` referencing `crepo` by `ClusterRepository`, suspended so
/// the reconciler doesn't churn it while we drive admission directly (mirrors
/// `identity_backup_config` above).
fn cluster_repo_backup_config(name: &str, crepo: &str) -> SnapshotPolicy {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "ClusterRepository", "name": crepo },
            "sources": [ { "pvc": { "name": "data" } } ],
            "retention": { "keepLatest": 5 },
            "suspend": true
        }
    }))
    .expect("SnapshotPolicy JSON deserializes")
}

/// The repository-edit identity guard (M2/M5), exercised for a
/// `ClusterRepository` at the REAL admission webhook (unit tests already cover
/// the pure decision — `detect_repository_identity_change` — and the thin IO
/// caller — `check_repository_identity_change`'s cluster-wide LIST; this
/// proves the API server actually delivers `oldObject` on UPDATE the same way
/// `identity_shape_and_fork_guard_are_enforced_at_admission` proves it for the
/// per-policy fork guard).
///
/// A `SnapshotPolicy` with one successful snapshot references the
/// `ClusterRepository`; adding `identityDefaults.cluster` (unset → `east`) would
/// silently re-identify it on its very next backup (ADR-0004 §5: the default
/// hostname becomes `<namespace>.<cluster>` instead of bare `<namespace>`) with
/// no per-policy edit to acknowledge it — denied unless the repository carries
/// `kopiur.home-operations.com/allow-identity-change`.
#[tokio::test]
#[ignore = "requires a kind cluster with the operator installed (mise //crates/e2e:test)"]
async fn setting_cluster_on_repo_with_history_requires_acknowledgment() {
    let Some(world) = World::connect().await else {
        return; // no cluster: graceful no-op
    };
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let crepo = "webhook-crepo-history";
    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
    crepos
        .create(&PostParams::default(), &cluster_repository_json(crepo))
        .await
        .expect("create ClusterRepository (admission-shape only)");

    let policy = "webhook-crepo-history-policy";
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    policies
        .create(
            &PostParams::default(),
            &cluster_repo_backup_config(policy, crepo),
        )
        .await
        .expect("a valid policy referencing the ClusterRepository is admitted");

    // Simulate history: one successful snapshot (no identity override — the
    // policy consults the repository's defaults, so it IS affected by an edit).
    let status = serde_json::json!({
        "status": { "lastSuccessfulSnapshot": "2026-01-01T00:00:00Z" }
    });
    policies
        .patch_status(policy, &PatchParams::default(), &Patch::Merge(&status))
        .await
        .expect("status patch (simulated history) applies");
    let got = policies.get_status(policy).await.expect("get status");
    assert!(
        got.status
            .as_ref()
            .and_then(|s| s.last_successful_snapshot.as_ref())
            .is_some(),
        "simulated history must be pinned before the UPDATE test"
    );

    // Adding identityDefaults.cluster (None -> Some) with a consumer that has
    // history and no acknowledgment -> DENIED, naming the policy.
    let change = serde_json::json!({ "spec": { "identityDefaults": { "cluster": "east" } } });
    let err = crepos
        .patch(crepo, &PatchParams::default(), &Patch::Merge(&change))
        .await
        .expect_err(
            "adding identityDefaults.cluster on a repo with consumer history must be DENIED",
        );
    let msg = err.to_string();
    assert!(
        msg.contains("denied the request") || msg.to_lowercase().contains("admission"),
        "rejection should come from the admission webhook, got: {msg}"
    );
    assert!(
        msg.contains(&format!("{E2E_NAMESPACE}/{policy}")),
        "the deny message must name the affected policy, got: {msg}"
    );

    // The SAME change WITH the acknowledgment annotation -> ADMITTED.
    let acked = serde_json::json!({
        "metadata": { "annotations": { ALLOW_IDENTITY_CHANGE_ANNOTATION: "intentional" } },
        "spec": { "identityDefaults": { "cluster": "east" } }
    });
    crepos
        .patch(crepo, &PatchParams::default(), &Patch::Merge(&acked))
        .await
        .expect("an acknowledged identityDefaults change must be ADMITTED");
    // The ack path also attaches an admission WARNING naming the same policy
    // (`check_repository_identity_change`'s `outcome.consumers`, rendered via
    // `describe_identity_change_consumers`) — but kube-rs's `Api::patch` only
    // returns the persisted object, not the HTTP response's `Warning:` headers,
    // so that half of the behavior has no client-observable surface here; it is
    // asserted at the unit tier instead (`identity_repo_edit.rs`'s tests and
    // `handlers.rs`'s warning-attachment call site).

    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = crepos.delete(crepo, &DeleteParams::default()).await;
}

/// A `Snapshot` carrying the `origin: discovered` label AND `spec.onScheduleDelete`
/// — structurally valid (both fields are optional in the CRD), but the shared
/// `validate_backup` validator the webhook runs rejects it: a discovered snapshot has
/// no owning `SnapshotSchedule` for a cascade policy to apply to (mirrors the
/// discovered-must-Retain rule). deletionPolicy is set to Retain so the rejection can
/// ONLY be the onScheduleDelete rule, not the deletionPolicy one.
fn discovered_snapshot_with_on_schedule_delete(name: &str) -> Snapshot {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": {
            "name": name,
            "namespace": E2E_NAMESPACE,
            "labels": { "kopiur.home-operations.com/origin": "discovered" }
        },
        "spec": { "deletionPolicy": "Retain", "onScheduleDelete": "Retain" }
    }))
    .expect("Snapshot JSON deserializes")
}

/// A MANUAL `Snapshot` (no origin marker → origin `manual`) that sets
/// `spec.onScheduleDelete`. The field is inert for a manual snapshot but NOT
/// forbidden, so admission accepts it — the positive counterpart proving the
/// rejection above is origin-scoped, not a blanket ban on the field.
fn manual_snapshot_with_on_schedule_delete(name: &str) -> Snapshot {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": { "policyRef": { "name": "any" }, "deletionPolicy": "Delete", "onScheduleDelete": "Retain" }
    }))
    .expect("Snapshot JSON deserializes")
}

/// The mass-deletion cascade validator through the REAL admission webhook: a
/// `origin: discovered` Snapshot setting `spec.onScheduleDelete` is DENIED (the new
/// `DiscoveredCannotSetOnScheduleDelete` rule), while a manual Snapshot setting the
/// same field is ACCEPTED. A unit test on `validate_backup` alone can't prove the
/// deployed webhook enforces this against a live API server.
#[tokio::test]
#[ignore = "requires a kind cluster with the operator installed (mise //crates/e2e:test)"]
async fn discovered_snapshot_on_schedule_delete_is_rejected_but_manual_is_accepted() {
    let Some(world) = World::connect().await else {
        return; // no cluster: graceful no-op
    };
    let client = world.client().clone();
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // 1. discovered + onScheduleDelete → DENIED, and the message names the field.
    let bad = "webhook-discovered-osd";
    let _ = backups.delete(bad, &DeleteParams::default()).await;
    let err = backups
        .create(
            &PostParams::default(),
            &discovered_snapshot_with_on_schedule_delete(bad),
        )
        .await
        .expect_err("a discovered Snapshot setting onScheduleDelete must be DENIED");
    let msg = err.to_string();
    assert!(
        msg.contains("denied the request") || msg.to_lowercase().contains("admission"),
        "rejection should come from the admission webhook, got: {msg}"
    );
    assert!(
        msg.contains("onScheduleDelete"),
        "the deny message must name onScheduleDelete, got: {msg}"
    );
    let _ = backups.delete(bad, &DeleteParams::default()).await;

    // 2. manual + onScheduleDelete → ACCEPTED (the field is inert but allowed).
    let ok = "webhook-manual-osd";
    let _ = backups.delete(ok, &DeleteParams::default()).await;
    backups
        .create(
            &PostParams::default(),
            &manual_snapshot_with_on_schedule_delete(ok),
        )
        .await
        .expect("a manual Snapshot may set onScheduleDelete (the field is origin-scoped)");
    let _ = backups.delete(ok, &DeleteParams::default()).await;
}

/// True when every webhook in the ValidatingWebhookConfiguration carries a
/// non-empty caBundle. Returns `kube::Error` so it composes with the
/// `wait_until` polling closures (whose error type is `kube::Error`).
async fn validating_has_ca_bundle(client: &Client) -> Result<bool, kube::Error> {
    let api: Api<ValidatingWebhookConfiguration> = Api::all(client.clone());
    let Some(cfg) = api.get_opt(VALIDATING_CONFIG).await? else {
        return Ok(false);
    };
    let webhooks = cfg.webhooks.unwrap_or_default();
    Ok(!webhooks.is_empty()
        && webhooks.iter().all(|w| {
            w.client_config
                .ca_bundle
                .as_ref()
                .is_some_and(|b| !b.0.is_empty())
        }))
}

/// True when every webhook in the MutatingWebhookConfiguration carries a
/// non-empty caBundle. Returns `kube::Error` (see [`validating_has_ca_bundle`]).
async fn mutating_has_ca_bundle(client: &Client) -> Result<bool, kube::Error> {
    let api: Api<MutatingWebhookConfiguration> = Api::all(client.clone());
    let Some(cfg) = api.get_opt(MUTATING_CONFIG).await? else {
        return Ok(false);
    };
    let webhooks = cfg.webhooks.unwrap_or_default();
    Ok(!webhooks.is_empty()
        && webhooks.iter().all(|w| {
            w.client_config
                .ca_bundle
                .as_ref()
                .is_some_and(|b| !b.0.is_empty())
        }))
}
