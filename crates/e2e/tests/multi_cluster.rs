//! e2e: **multi-cluster shared-repository support** (M7a) — the user-visible
//! proof that N clusters can safely share one kopia repository once
//! `identityDefaults.cluster` is set.
//!
//! Everything exercised here is implemented on this branch: M0a identity-aware
//! matchers; M0b kopia-retention neutralization; M1/M5 `identityDefaults.cluster`
//! on both repository kinds (`<ns>.<cluster>` default hostname); M2/M5 the
//! repository-edit identity guard; M3/M4 `catalog.foreignSnapshots`
//! Ignore/Fallback + identity-aware placement + `foreignSnapshotCount`; M6 the
//! cluster-qualified maintenance lease with `ownerAliases`/`RestampPolicy::
//! OwnFormatsOnly` + ReadOnly owner-stamp gating + `--readonly` bootstrap connect.
//!
//! The foreign writer is `builders::foreign_kopia_pod` — raw `kopia` (the mover
//! image's binary) run as sequential initContainers against the e2e MinIO,
//! simulating a PEER cluster (or a legacy pre-cluster-identity writer) that
//! kopiur never produced. Every scenario forces a DETERMINISTIC catalog
//! scan/bootstrap-recycle via a spec-change bump (never a `periodicRefresh`
//! timer): a spec edit always bumps `metadata.generation`, which
//! `bootstrap_recycle_due`/`scan_due` honor unconditionally (independent of
//! `periodicRefresh`), so the rescan is immediate and the exact-count
//! assertions below can never race a background timer.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::events::v1::Event;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, ResourceExt};
use serde::de::DeserializeOwned;

use kopiur_api::{ClusterRepository, Maintenance, Restore, Snapshot, SnapshotPolicy};
use kopiur_e2e::builders::{self, SeedStep};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, apply, apply_secret, consts, default_timeout, ensure_namespace,
    poll_interval, wait, wait_until,
};

/// Deserialize a CR from a JSON literal into its typed kube object.
fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

/// Poll a CR until `status.phase == want_phase`.
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

/// Run a foreign seeder pod to completion (the shared
/// [`kopiur_e2e::apply::run_foreign_seeder`], expect-wrapped for test flow).
async fn run_seeder(client: &Client, name: &str, steps: &[SeedStep<'_>]) {
    kopiur_e2e::apply::run_foreign_seeder(client, E2E_NAMESPACE, name, steps)
        .await
        .expect("foreign kopia seeder");
}

/// A `ClusterRepository` against the shared in-cluster MinIO, with a cluster
/// identity ([`kopiur_api::common::IdentityDefaults::cluster`]) and a tenancy
/// gate. `maintenance_enabled` is `false` for the placement/catalog scenarios
/// (a/b/c/e/f) — nothing there exercises maintenance, and disabling it keeps
/// the bootstrap from stamping/restamping a maintenance owner or reconciling a
/// managed `Maintenance` CR that no assertion reads (pure noise); it is left
/// default-on (`true`) for d1/d2, where the managed lease IS the point.
fn crepo_json(
    name: &str,
    bucket: &str,
    cluster: &str,
    allowed_namespaces: serde_json::Value,
    create: bool,
    maintenance_enabled: bool,
    catalog: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut spec = serde_json::json!({
        "backend": { "s3": {
            "bucket": bucket,
            "endpoint": consts::MINIO_ENDPOINT,
            "region": "us-east-1",
            "tls": { "disableTls": true },
            "auth": { "secretRef": { "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE } }
        }},
        "encryption": {
            "passwordSecretRef": {
                "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE, "key": "KOPIA_PASSWORD"
            }
        },
        "create": { "enabled": create },
        "allowedNamespaces": allowed_namespaces,
        "identityDefaults": { "cluster": cluster },
        "maintenance": { "enabled": maintenance_enabled }
    });
    if let Some(c) = catalog {
        spec["catalog"] = c;
    }
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "ClusterRepository",
        "metadata": { "name": name },
        "spec": spec
    })
}

/// Patch a harmless, always-DIFFERENT `catalog.retain.maxAgeDays` value onto a
/// `ClusterRepository` to bump `metadata.generation` deterministically — this
/// forces a bootstrap-Job recycle (`bootstrap_recycle_due`'s generation arm)
/// and a fresh catalog scan (`scan_due`'s generation arm) on the NEXT
/// reconcile, regardless of `catalog.periodicRefresh` (which stays off/unset
/// throughout this file — see the module docs for why a spec-change bump is
/// used instead of a timer everywhere in this file).
async fn bump_catalog(crepos: &Api<ClusterRepository>, name: &str, max_age_days: i64) {
    crepos
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "catalog": { "retain": { "maxAgeDays": max_age_days } } }
            })),
        )
        .await
        .expect("bump catalog.retain.maxAgeDays to force a deterministic rescan");
}

/// This repository CR's discovered rows in `ns`, via the dedup labels the
/// catalog scan stamps (`origin=discovered` + the repository UID).
async fn discovered_rows(client: &Client, ns: &str, repo_uid: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), ns);
    let selector = format!(
        "kopiur.home-operations.com/origin=discovered,\
         kopiur.home-operations.com/repository-uid={repo_uid}"
    );
    api.list(&ListParams::default().labels(&selector))
        .await
        .expect("list discovered Snapshots")
        .items
}

/// This repository CR's discovered rows across EVERY namespace — used to
/// assert a foreign-ignored snapshot materialized NOWHERE at all (scenario b).
async fn discovered_rows_any_ns(client: &Client, repo_uid: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::all(client.clone());
    let selector = format!(
        "kopiur.home-operations.com/origin=discovered,\
         kopiur.home-operations.com/repository-uid={repo_uid}"
    );
    api.list(&ListParams::default().labels(&selector))
        .await
        .expect("list discovered Snapshots (cluster-wide)")
        .items
}

/// Wait until this repository's `status.catalog.discoveredBackupCount` reaches
/// `want` exactly (counts can pass through intermediate values mid-scan).
async fn wait_discovered_count(crepos: &Api<ClusterRepository>, name: &str, want: i64) {
    wait_until(
        &format!("{name} discoveredBackupCount={want}"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(crepos, name).await;
            let n = s
                .pointer("/catalog/discoveredBackupCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            Ok((n == want).then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{name} should reach discoveredBackupCount={want}: {e}"));
}

/// The `username@hostname:path` identity recorded on a discovered Snapshot.
fn row_identity(s: &Snapshot) -> String {
    let v = serde_json::to_value(s).unwrap_or_default();
    let id = v
        .pointer("/status/snapshot/identity")
        .cloned()
        .unwrap_or_default();
    format!(
        "{}@{}:{}",
        id.get("username").and_then(|x| x.as_str()).unwrap_or(""),
        id.get("hostname").and_then(|x| x.as_str()).unwrap_or(""),
        id.get("sourcePath").and_then(|x| x.as_str()).unwrap_or(""),
    )
}

/// A discovered row's recorded kopia identity hostname.
fn row_hostname(s: &Snapshot) -> String {
    let v = serde_json::to_value(s).unwrap_or_default();
    v.pointer("/status/snapshot/identity/hostname")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// A `Maintenance`'s (or any CR's) `status.conditions[type=type_].(status,reason)`.
fn condition(status: &serde_json::Value, type_: &str) -> Option<(String, String)> {
    status
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some(type_))
        })
        .map(|c| {
            (
                c.get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                c.get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
}

/// Request a manual maintenance run via the run-requested/run-mode annotations
/// on the (operator-managed) `Maintenance` CR and wait until `status.manualRun`
/// pins THIS request with a terminal phase (`Succeeded` — the mover Job exits 0
/// whether it actually ran maintenance or yielded the lease; see
/// `crates/mover/src/main.rs`'s `LeaseAction::Yield` arm). Mirrors
/// `credential_projection.rs`'s `run_manual_maintenance`.
async fn run_manual_maintenance(maints: &Api<Maintenance>, name: &str) {
    let requested = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let patch = serde_json::json!({ "metadata": { "annotations": {
        kopiur_api::consts::RUN_REQUESTED_ANNOTATION: requested,
        kopiur_api::consts::RUN_MODE_ANNOTATION: "quick",
    }}});
    maints
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .expect("annotate Maintenance with a manual run request");
    wait_until(
        &format!("{name} manualRun {requested} terminal"),
        default_timeout(),
        poll_interval(),
        || async {
            let status = status_json(maints, name).await;
            let m = status.get("manualRun");
            let matches_request = m
                .and_then(|m| m.get("requestedAt"))
                .and_then(|v| v.as_str())
                == Some(requested.as_str());
            let phase = m.and_then(|m| m.get("phase")).and_then(|v| v.as_str());
            Ok((matches_request && phase == Some("Succeeded")).then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("manual maintenance run {requested} must succeed: {e}"));
}

/// (a) A `ClusterRepository` with `identityDefaults.cluster: east`: a snapshot
/// seeded by a foreign writer under the CLUSTER-QUALIFIED hostname convention
/// this cluster itself would produce (`<ns>.east`) — simulating aged-out/DR
/// history (kopia data survives; no `Snapshot` CR was ever created for it in
/// THIS cluster) — must still materialize as a discovered row IN that
/// namespace, and a live `SnapshotPolicy` referencing the same repository
/// must resolve THAT SAME cluster-qualified hostname as its default identity.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn cluster_qualified_own_snapshots_place_in_their_namespace() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::WorkloadNs])
        .await
        .expect("provision MinIO + workload namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), consts::WORKLOAD_NS);

    let name = "e2e-mc-a";
    let bucket = "kopiur-mc-a";
    let hostname = format!("{}.east", consts::WORKLOAD_NS);

    crepos
        .create(
            &PostParams::default(),
            &cr(crepo_json(
                name,
                bucket,
                "east",
                serde_json::json!({ "list": [consts::WORKLOAD_NS] }),
                true,
                false,
                None,
            )),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, name, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    // Simulate aged-out/DR history: a foreign writer connects OUT-OF-BAND and
    // snapshots under this cluster's OWN qualified hostname convention — no
    // Snapshot CR was ever created for it here. NO WipeBucket / CreateRepo here:
    // kopiur (create: true, above) already created the repository in this
    // bucket — wiping it or re-`create`ing over it would destroy/conflict with
    // that repository, so the seeder only CONNECTS to what already exists.
    run_seeder(
        &client,
        "e2e-mc-a-seed",
        &[
            SeedStep::WriteFile {
                dir: "app",
                file: "f.txt",
                content: "aged-out-dr-history",
            },
            SeedStep::ConnectRepo {
                bucket,
                username: "legacy",
                hostname: &hostname,
            },
            SeedStep::Snapshot { dir: "app" },
        ],
    )
    .await;

    // Spec-change bump: deterministic, immediate rescan (see module docs).
    bump_catalog(&crepos, name, 3650).await;
    wait_discovered_count(&crepos, name, 1).await;

    let repo_uid = crepos
        .get(name)
        .await
        .expect("get ClusterRepository")
        .uid()
        .expect("uid");
    let rows = discovered_rows(&client, consts::WORKLOAD_NS, &repo_uid).await;
    assert_eq!(
        rows.len(),
        1,
        "the cluster-qualified snapshot should land in its own namespace"
    );
    let v = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(
        v.pointer("/spec/deletionPolicy").and_then(|x| x.as_str()),
        Some("Retain"),
        "discovered rows are FORCED Retain: {v}"
    );
    assert_eq!(
        row_hostname(&rows[0]),
        hostname,
        "the discovered row's identity hostname must be the cluster-qualified one: {v}"
    );

    // A live SnapshotPolicy (no identity override) resolves the SAME default.
    policies
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "SnapshotPolicy",
                "metadata": { "name": "e2e-mc-a-policy", "namespace": consts::WORKLOAD_NS },
                "spec": {
                    "repository": { "kind": "ClusterRepository", "name": name },
                    "sources": [ { "pvc": { "name": consts::PVC_SRC } } ],
                    "copyMethod": "Direct"
                }
            })),
        )
        .await
        .expect("create SnapshotPolicy");
    wait_until(
        "policy resolves the cluster-qualified default hostname",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&policies, "e2e-mc-a-policy").await;
            let h = s
                .pointer("/resolved/identity/hostname")
                .and_then(|v| v.as_str());
            Ok((h == Some(hostname.as_str())).then_some(()))
        },
    )
    .await
    .expect("SnapshotPolicy.status.resolved.identity.hostname should be `<ns>.east`");

    let _ = policies
        .delete("e2e-mc-a-policy", &DeleteParams::default())
        .await;
    let _ = crepos.delete(name, &DeleteParams::default()).await;
}

/// (b) A snapshot seeded under ANOTHER cluster's qualified hostname
/// (`<ns>.west`, this repository being `east`) must be ignored entirely: no
/// discovered `Snapshot` CR anywhere, `status.catalog.foreignSnapshotCount`
/// counts it, and — critically — NO `DiscoveredSnapshotUnplaced` Warning event
/// is published for the repository (pre-M4 code warned here; a foreign
/// snapshot on a repository shared across clusters is expected and routine,
/// not a misconfiguration).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn foreign_cluster_snapshots_are_ignored_and_counted() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::WorkloadNs])
        .await
        .expect("provision MinIO + workload namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());

    let name = "e2e-mc-b";
    let bucket = "kopiur-mc-b";
    let hostname = format!("{}.west", consts::WORKLOAD_NS);

    crepos
        .create(
            &PostParams::default(),
            &cr(crepo_json(
                name,
                bucket,
                "east",
                serde_json::json!({ "list": [consts::WORKLOAD_NS] }),
                true,
                false,
                None,
            )),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, name, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    // NO WipeBucket / CreateRepo: kopiur (create: true, above) already created
    // the repository here — the seeder only CONNECTS to it.
    run_seeder(
        &client,
        "e2e-mc-b-seed",
        &[
            SeedStep::WriteFile {
                dir: "app",
                file: "f.txt",
                content: "another-clusters-data",
            },
            SeedStep::ConnectRepo {
                bucket,
                username: "peer",
                hostname: &hostname,
            },
            SeedStep::Snapshot { dir: "app" },
        ],
    )
    .await;

    bump_catalog(&crepos, name, 3650).await;
    wait_until(
        "foreign snapshot counted, never discovered",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&crepos, name).await;
            let foreign = s
                .pointer("/catalog/foreignSnapshotCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let discovered = s
                .pointer("/catalog/discoveredBackupCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            Ok((foreign >= 1 && discovered == 0).then_some(()))
        },
    )
    .await
    .expect("a foreign-cluster snapshot must be counted, never discovered");

    let repo_uid = crepos
        .get(name)
        .await
        .expect("get ClusterRepository")
        .uid()
        .expect("uid");
    let rows = discovered_rows_any_ns(&client, &repo_uid).await;
    assert!(
        rows.is_empty(),
        "a foreign-cluster snapshot must materialize NOWHERE: {rows:?}"
    );

    // Cluster-scoped objects publish Events into the "default" namespace (see
    // `kube::runtime::events::Recorder::new`'s doc: "Cluster scoped objects
    // will publish events in the default namespace").
    let events: Api<Event> = Api::namespaced(client.clone(), "default");
    let list = events
        .list(&ListParams::default())
        .await
        .expect("list events in the default namespace");
    let unplaced: Vec<&Event> = list
        .items
        .iter()
        .filter(|e| {
            e.reason.as_deref() == Some("DiscoveredSnapshotUnplaced")
                && e.regarding
                    .as_ref()
                    .is_some_and(|r| r.name.as_deref() == Some(name))
        })
        .collect();
    assert!(
        unplaced.is_empty(),
        "a foreign-cluster snapshot is EXPECTED and routine, never a Warning: {unplaced:?}"
    );

    let _ = crepos.delete(name, &DeleteParams::default()).await;
}

/// (c) A bare hostname (no `.<cluster>` suffix at all — written before cluster
/// identity existed, or by a bare-hostname legacy writer) still places
/// normally under cluster mode, as long as the namespace it names is allowed
/// (the home-cluster legacy rule: [`kopiur_api::identity::HostClass::Bare`]
/// with `ns_allowed` places exactly like [`kopiur_api::identity::HostClass::OwnCluster`]).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn legacy_bare_hostnames_still_place() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::WorkloadNs])
        .await
        .expect("provision MinIO + workload namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());

    let name = "e2e-mc-c";
    let bucket = "kopiur-mc-c";

    crepos
        .create(
            &PostParams::default(),
            &cr(crepo_json(
                name,
                bucket,
                "east",
                serde_json::json!({ "list": [consts::WORKLOAD_NS] }),
                true,
                false,
                None,
            )),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, name, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    // NO WipeBucket / CreateRepo: kopiur (create: true, above) already created
    // the repository here — the seeder only CONNECTS to it.
    run_seeder(
        &client,
        "e2e-mc-c-seed",
        &[
            SeedStep::WriteFile {
                dir: "app",
                file: "f.txt",
                content: "pre-cluster-identity-history",
            },
            // A BARE hostname (== the namespace, no `.east` suffix): legacy,
            // written before this repository ever had a cluster identity.
            SeedStep::ConnectRepo {
                bucket,
                username: "legacy",
                hostname: consts::WORKLOAD_NS,
            },
            SeedStep::Snapshot { dir: "app" },
        ],
    )
    .await;

    bump_catalog(&crepos, name, 3650).await;
    wait_discovered_count(&crepos, name, 1).await;

    let repo_uid = crepos
        .get(name)
        .await
        .expect("get ClusterRepository")
        .uid()
        .expect("uid");
    let rows = discovered_rows(&client, consts::WORKLOAD_NS, &repo_uid).await;
    assert_eq!(
        rows.len(),
        1,
        "the bare-hostname legacy row should still place"
    );
    assert_eq!(
        row_hostname(&rows[0]),
        consts::WORKLOAD_NS,
        "the bare hostname is preserved verbatim (never rewritten to `<ns>.east`)"
    );

    let _ = crepos.delete(name, &DeleteParams::default()).await;
}

/// (d1) A repository already claimed by a PEER cluster's cluster-qualified
/// maintenance lease: this cluster's managed `Maintenance` must YIELD
/// (`LeaseOwned=False` / `LeaseHeldByOther`), never run, and — the
/// anti-restamp / anti-ping-pong assertion — a forced bootstrap re-run (a spec
/// nudge) must NOT restamp the owner to itself: `RestampPolicy::OwnFormatsOnly`
/// only re-stamps an empty or ALIAS-recognized owner, never an unrecognized
/// foreign one (pre-M6 code used `AnyStale` here and would have clobbered it,
/// causing an infinite cross-cluster ping-pong on a shared repo).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn maintenance_yields_to_peer_cluster_owner() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Minio]).await.expect("provision MinIO");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let name = "e2e-mc-d1";
    let bucket = "kopiur-mc-d1";
    // The peer cluster's ("west") qualified lease identity for a ClusterRepository
    // named `name`: `kopiur/west/clusterrepository/<name>` dot-joined by
    // `kopia_lease_identity` to `kopiur.west.clusterrepository.<name>`.
    let peer_hostname = format!("kopiur.west.clusterrepository.{name}");
    let peer_owner = format!("kopiur@{peer_hostname}");

    // Seed the repository AND stamp the peer's maintenance ownership, entirely
    // out-of-band (raw kopia, never kopiur).
    run_seeder(
        &client,
        "e2e-mc-d1-seed",
        &[
            SeedStep::WipeBucket { bucket },
            SeedStep::WriteFile {
                dir: "seed",
                file: "f.txt",
                content: "peer-owned-repo",
            },
            SeedStep::CreateRepo {
                bucket,
                username: "seed",
                hostname: "seed-init",
            },
            SeedStep::Snapshot { dir: "seed" },
            SeedStep::ConnectRepo {
                bucket,
                username: "kopiur",
                hostname: &peer_hostname,
            },
            SeedStep::ClaimMaintenance,
        ],
    )
    .await;

    // Adopt (create: false) — the repository and its peer-claimed lease
    // already exist; this cluster ("east") must never clobber either.
    crepos
        .create(
            &PostParams::default(),
            &cr(crepo_json(
                name,
                bucket,
                "east",
                serde_json::json!({ "all": true }),
                false,
                true,
                None,
            )),
        )
        .await
        .expect("create adopting ClusterRepository");
    wait_phase(&crepos, name, "Ready")
        .await
        .expect("adopting ClusterRepository should reach Ready");

    wait_until(
        "the operator-managed Maintenance CR appears",
        default_timeout(),
        poll_interval(),
        || async { maints.get_opt(name).await },
    )
    .await
    .expect("the operator should manage a Maintenance CR for this repository");

    run_manual_maintenance(&maints, name).await;
    let s = status_json(&maints, name).await;
    assert_eq!(
        condition(&s, "LeaseOwned"),
        Some(("False".to_string(), "LeaseHeldByOther".to_string())),
        "the run must yield to the peer's lease: {s}"
    );
    // `status.full.lastHandledAt` DOES get stamped almost immediately (the
    // first-ever cron slot for a freshly-adopted Maintenance is already "due" —
    // `mode_after` falls back to a year ago — so the controller's own schedule
    // picks it up and records it "handled" whether the Job ran or yielded); the
    // load-bearing assertion is `lastRunAt`, which ONLY the mover's
    // `maintenance_ran_body` sets on an ACTUAL claimed run (never on a yield).
    assert!(
        s.pointer("/full/lastRunAt").is_none(),
        "a yielded run must never have run full maintenance: {s}"
    );
    assert_eq!(
        s.pointer("/ownership/owner").and_then(|v| v.as_str()),
        Some(peer_owner.as_str()),
        "the observed holder must be the peer's owner, verbatim: {s}"
    );

    // Force a bootstrap re-run (a spec nudge) and prove it does NOT restamp:
    // wait for a FRESH bootstrap to actually complete (catalog.lastRefreshAt
    // advances), then re-check the lease — it must STILL yield, to the SAME
    // peer owner, unchanged.
    let before = status_json(&crepos, name)
        .await
        .pointer("/catalog/lastRefreshAt")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    bump_catalog(&crepos, name, 3650).await;
    wait_until(
        "a fresh bootstrap ran after the spec nudge",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&crepos, name).await;
            let refreshed = s
                .pointer("/catalog/lastRefreshAt")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Ok((refreshed.is_some() && refreshed != before).then_some(()))
        },
    )
    .await
    .expect("the spec nudge should recycle the bootstrap Job for a fresh connect");

    run_manual_maintenance(&maints, name).await;
    let s = status_json(&maints, name).await;
    assert_eq!(
        condition(&s, "LeaseOwned"),
        Some(("False".to_string(), "LeaseHeldByOther".to_string())),
        "the run must STILL yield after the bootstrap re-run (no restamp): {s}"
    );
    assert_eq!(
        s.pointer("/ownership/owner").and_then(|v| v.as_str()),
        Some(peer_owner.as_str()),
        "the bootstrap re-run must NEVER have restamped the peer's owner: {s}"
    );
    assert!(
        s.pointer("/full/lastRunAt").is_none(),
        "still no full maintenance run: {s}"
    );

    let _ = crepos.delete(name, &DeleteParams::default()).await;
}

/// (d2) A repository stamped with the LEGACY (pre-M6, pre-cluster-identity)
/// maintenance-owner format upgrades on first claim: the managed
/// `Maintenance`'s `ownership.owner`/`ownerAliases` are the NEW cluster-qualified
/// lease + the recognized legacy alias, the bootstrap's connect-to-existing
/// self-heal restamps kopia's recorded owner to the new format (recognized via
/// the alias), and the very next run claims the lease cleanly.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn legacy_owner_upgrades_on_first_claim() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Minio]).await.expect("provision MinIO");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let name = "e2e-mc-d2";
    let bucket = "kopiur-mc-d2";
    // The LEGACY (pre-M6) lease format for a ClusterRepository named `name`:
    // `kopiur/clusterrepository/<name>` sanitized whole-string to
    // `kopiur-clusterrepository-<name>`.
    let legacy_hostname = format!("kopiur-clusterrepository-{name}");

    run_seeder(
        &client,
        "e2e-mc-d2-seed",
        &[
            SeedStep::WipeBucket { bucket },
            SeedStep::WriteFile {
                dir: "seed",
                file: "f.txt",
                content: "legacy-owned-repo",
            },
            SeedStep::CreateRepo {
                bucket,
                username: "seed",
                hostname: "seed-init",
            },
            SeedStep::Snapshot { dir: "seed" },
            SeedStep::ConnectRepo {
                bucket,
                username: "kopiur",
                hostname: &legacy_hostname,
            },
            SeedStep::ClaimMaintenance,
        ],
    )
    .await;

    crepos
        .create(
            &PostParams::default(),
            &cr(crepo_json(
                name,
                bucket,
                "east",
                serde_json::json!({ "all": true }),
                false,
                true,
                None,
            )),
        )
        .await
        .expect("create adopting ClusterRepository");
    wait_phase(&crepos, name, "Ready")
        .await
        .expect("adopting ClusterRepository should reach Ready");

    let maint = wait_until(
        "the operator-managed Maintenance CR appears",
        default_timeout(),
        poll_interval(),
        || async { maints.get_opt(name).await },
    )
    .await
    .expect("the operator should manage a Maintenance CR for this repository");

    let v = serde_json::to_value(&maint).unwrap();
    assert_eq!(
        v.pointer("/spec/ownership/owner").and_then(|x| x.as_str()),
        Some(format!("kopiur/east/clusterrepository/{name}").as_str()),
        "the managed lease must be cluster-qualified: {v}"
    );
    assert_eq!(
        v.pointer("/spec/ownership/ownerAliases"),
        Some(&serde_json::json!([format!(
            "kopiur/clusterrepository/{name}"
        )])),
        "the PRE-cluster lease must be recorded as the migration alias: {v}"
    );

    run_manual_maintenance(&maints, name).await;
    let s = status_json(&maints, name).await;
    assert_eq!(
        condition(&s, "LeaseOwned"),
        Some(("True".to_string(), "LeaseClaimed".to_string())),
        "the alias-recognized legacy owner should be upgraded and claimed cleanly: {s}"
    );

    let _ = crepos.delete(name, &DeleteParams::default()).await;
}

/// (e) `catalog.foreignSnapshots: Ignore` (+ a cluster identity) drops foreign
/// rows; flipping to `fallbackNamespace` + `Fallback` on a spec change
/// materializes them retroactively into the fallback namespace on that SAME
/// deterministic rescan — and a foreign row, once visible, restores byte-exact
/// via `Restore.spec.source.identity` (the raw `username@hostname` — no
/// `Snapshot` CR needed).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn foreign_fallback_flip_materializes_on_spec_change() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::WorkloadNs])
        .await
        .expect("provision MinIO + workload namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());

    let name = "e2e-mc-e";
    let bucket = "kopiur-mc-e";
    let foreign_username = "drifter";
    let foreign_hostname = format!("{}.west", consts::WORKLOAD_NS);

    crepos
        .create(
            &PostParams::default(),
            &cr(crepo_json(
                name,
                bucket,
                "east",
                // BOTH namespaces: the workload ns (the foreign hostname's
                // namespace part — irrelevant to tenancy, ForeignCluster
                // classification precedes it, but kept for realism) AND the
                // operator ns — the fallback namespace, where the Restore
                // below lives. Placement INTO fallbackNamespace needs no
                // tenancy, but a consumer CR (the Restore) referencing this
                // ClusterRepository from there does.
                serde_json::json!({ "list": [consts::WORKLOAD_NS, E2E_NAMESPACE] }),
                true,
                false,
                Some(serde_json::json!({ "foreignSnapshots": "Ignore" })),
            )),
        )
        .await
        .expect("create ClusterRepository (foreignSnapshots: Ignore)");
    wait_phase(&crepos, name, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    // NO WipeBucket: kopiur (create: true, above) already created the
    // repository here — the seeder only CONNECTS to it.
    run_seeder(
        &client,
        "e2e-mc-e-seed",
        &[
            SeedStep::WriteFile {
                dir: "stray",
                file: "g.txt",
                content: "foreign-fallback-data",
            },
            SeedStep::ConnectRepo {
                bucket,
                username: foreign_username,
                hostname: &foreign_hostname,
            },
            SeedStep::Snapshot { dir: "stray" },
        ],
    )
    .await;

    // Scan #1, under `Ignore`: the foreign row is counted, never materialized.
    bump_catalog(&crepos, name, 3650).await;
    wait_until(
        "the foreign snapshot is ignored under foreignSnapshots: Ignore",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&crepos, name).await;
            let foreign = s
                .pointer("/catalog/foreignSnapshotCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let discovered = s
                .pointer("/catalog/discoveredBackupCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            Ok((foreign >= 1 && discovered == 0).then_some(()))
        },
    )
    .await
    .expect("the foreign snapshot must be counted, never discovered, under Ignore");

    // Flip to Fallback + set fallbackNamespace in ONE patch (both are required
    // together — adopting a cluster identity while a fallback namespace is
    // already configured must never silently change what it does) — this
    // spec change IS scan #2's deterministic trigger.
    crepos
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "catalog": {
                    "fallbackNamespace": E2E_NAMESPACE,
                    "foreignSnapshots": "Fallback"
                } }
            })),
        )
        .await
        .expect("flip to Fallback + set fallbackNamespace");
    wait_discovered_count(&crepos, name, 1).await;

    let repo_uid = crepos
        .get(name)
        .await
        .expect("get ClusterRepository")
        .uid()
        .expect("uid");
    let rows = discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await;
    assert_eq!(
        rows.len(),
        1,
        "the foreign row must materialize into catalog.fallbackNamespace"
    );
    assert_eq!(
        row_identity(&rows[0]),
        format!("{foreign_username}@{foreign_hostname}:/data/stray")
    );

    // Restore it via raw identity — no Snapshot CR exists for this row's
    // origin, only the discovered CR the scan above just created.
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": "e2e-mc-e-restore", "namespace": E2E_NAMESPACE },
                "spec": {
                    "repository": { "kind": "ClusterRepository", "name": name },
                    "source": { "identity": {
                        "username": foreign_username,
                        "hostname": foreign_hostname,
                        "sourcePath": "/data/stray"
                    } },
                    "target": { "pvc": { "name": "e2e-mc-e-restored", "capacity": "100Mi" } }
                }
            })),
        )
        .await
        .expect("create identity-sourced Restore");
    wait_phase(&restores, "e2e-mc-e-restore", "Completed")
        .await
        .expect("the identity-sourced Restore should complete");

    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let reader = builders::one_shot_pod(
        E2E_NAMESPACE,
        "e2e-mc-e-reader",
        &[
            "sh",
            "-c",
            "test \"$(cat /restore/g.txt)\" = foreign-fallback-data",
        ],
        &[("e2e-mc-e-restored", "/restore")],
    );
    pods.create(&PostParams::default(), &reader)
        .await
        .expect("create reader pod");
    wait::pod_succeeded(&client, E2E_NAMESPACE, "e2e-mc-e-reader")
        .await
        .expect("the restored PVC must hold the foreign snapshot's exact bytes");

    let _ = restores
        .delete("e2e-mc-e-restore", &DeleteParams::default())
        .await;
    let _ = crepos.delete(name, &DeleteParams::default()).await;
}

/// (f) M0a regression at the e2e tier: two namespaces, each with a
/// SAME-NAMED PVC backed up to the SAME shared repository under distinct
/// cluster-qualified identities (`<nsA>.east` / `<nsB>.east` — same
/// `/pvc/<name>` kopia source path). Deleting nsA's `Snapshot` CR
/// (`deletionPolicy: Delete`) must delete ONLY nsA's kopia snapshot: nsB's
/// snapshot — sharing the exact same path, differing only by identity
/// hostname — must survive and still restore.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn same_path_different_identity_is_never_matched() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Minio]).await.expect("provision MinIO");
    let client = world.client().clone();

    const NSA: &str = "kopiur-e2e-mc-f-a";
    const NSB: &str = "kopiur-e2e-mc-f-b";
    ensure_namespace(&client, NSA)
        .await
        .expect("create namespace A");
    ensure_namespace(&client, NSB)
        .await
        .expect("create namespace B");

    // Each namespace gets its OWN hostPath PV (a PV binds exactly one PVC at a
    // time) over the SAME seeded source dir, bound to a PVC of the SAME NAME
    // in both namespaces — the "same path" half of the regression: both
    // resolve to the identical kopia sourcePath `/pvc/<PVC_SRC>`.
    apply::apply_all(
        &client,
        &[
            builders::hostpath_pv("kopiur-e2e-mc-f-a-src", consts::HOSTPATH_SRC, "1Gi").into(),
            builders::static_pvc(NSA, consts::PVC_SRC, "kopiur-e2e-mc-f-a-src", "1Gi").into(),
            builders::hostpath_pv("kopiur-e2e-mc-f-b-src", consts::HOSTPATH_SRC, "1Gi").into(),
            builders::static_pvc(NSB, consts::PVC_SRC, "kopiur-e2e-mc-f-b-src", "1Gi").into(),
        ],
    )
    .await
    .expect("provision two namespaces with a same-named source PVC");
    let s3_creds = [
        (consts::KEY_KOPIA_PASSWORD, consts::KOPIA_PASSWORD),
        (consts::KEY_AWS_ACCESS_KEY_ID, consts::MINIO_USER),
        (consts::KEY_AWS_SECRET_ACCESS_KEY, consts::MINIO_PASS),
    ];
    apply_secret(&client, NSA, consts::SECRET_S3_CREDS, &s3_creds)
        .await
        .expect("seed S3 creds in namespace A");
    apply_secret(&client, NSB, consts::SECRET_S3_CREDS, &s3_creds)
        .await
        .expect("seed S3 creds in namespace B");

    let name = "e2e-mc-f";
    let bucket = "kopiur-mc-f";
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    crepos
        .create(
            &PostParams::default(),
            &cr(crepo_json(
                name,
                bucket,
                "east",
                serde_json::json!({ "list": [NSA, NSB] }),
                true,
                false,
                None,
            )),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, name, "Ready")
        .await
        .expect("ClusterRepository should bootstrap to Ready");

    let policy_json = |ns: &str, pname: &str| {
        serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": pname, "namespace": ns },
            "spec": {
                "repository": { "kind": "ClusterRepository", "name": name },
                "sources": [ { "pvc": { "name": consts::PVC_SRC } } ],
                "copyMethod": "Direct",
                "retention": { "keepLatest": 5 }
            }
        })
    };
    let policies_a: Api<SnapshotPolicy> = Api::namespaced(client.clone(), NSA);
    let policies_b: Api<SnapshotPolicy> = Api::namespaced(client.clone(), NSB);
    policies_a
        .create(&PostParams::default(), &cr(policy_json(NSA, "pol-a")))
        .await
        .expect("create SnapshotPolicy in namespace A");
    policies_b
        .create(&PostParams::default(), &cr(policy_json(NSB, "pol-b")))
        .await
        .expect("create SnapshotPolicy in namespace B");

    let backup_json = |ns: &str, bname: &str, policy: &str| {
        serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": bname, "namespace": ns },
            "spec": { "policyRef": { "name": policy }, "deletionPolicy": "Delete" }
        })
    };
    let backups_a: Api<Snapshot> = Api::namespaced(client.clone(), NSA);
    let backups_b: Api<Snapshot> = Api::namespaced(client.clone(), NSB);
    backups_a
        .create(
            &PostParams::default(),
            &cr(backup_json(NSA, "snap-a", "pol-a")),
        )
        .await
        .expect("create Snapshot in namespace A");
    wait_phase(&backups_a, "snap-a", "Succeeded")
        .await
        .expect("namespace A's Snapshot should succeed");
    backups_b
        .create(
            &PostParams::default(),
            &cr(backup_json(NSB, "snap-b", "pol-b")),
        )
        .await
        .expect("create Snapshot in namespace B");
    wait_phase(&backups_b, "snap-b", "Succeeded")
        .await
        .expect("namespace B's Snapshot should succeed");

    // Delete nsA's Snapshot CR (deletionPolicy: Delete) — its finalizer
    // deletes ONLY nsA's kopia snapshot (by its pinned id).
    backups_a
        .delete("snap-a", &DeleteParams::default())
        .await
        .expect("delete namespace A's Snapshot CR");
    wait_until(
        "namespace A's Snapshot CR is fully deleted",
        default_timeout(),
        poll_interval(),
        || async { Ok(backups_a.get_opt("snap-a").await?.is_none().then_some(())) },
    )
    .await
    .expect("namespace A's Snapshot CR should delete cleanly");

    // nsB's kopia snapshot — sharing the EXACT same path, distinguished only
    // by identity hostname — must have survived: its Snapshot CR still
    // restores.
    let restores: Api<Restore> = Api::namespaced(client.clone(), NSB);
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": "e2e-mc-f-restore", "namespace": NSB },
                "spec": {
                    "repository": { "kind": "ClusterRepository", "name": name },
                    "source": { "snapshotRef": { "name": "snap-b" } },
                    "target": { "pvc": { "name": "e2e-mc-f-restored", "capacity": "1Gi" } }
                }
            })),
        )
        .await
        .expect("create Restore from namespace B's surviving Snapshot");
    wait_phase(&restores, "e2e-mc-f-restore", "Completed")
        .await
        .expect(
            "namespace B's kopia snapshot must SURVIVE namespace A's deletion and still restore",
        );

    let _ = restores
        .delete("e2e-mc-f-restore", &DeleteParams::default())
        .await;
    let _ = backups_b.delete("snap-b", &DeleteParams::default()).await;
    let _ = crepos.delete(name, &DeleteParams::default()).await;
    let nss: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client.clone());
    let _ = nss.delete(NSA, &DeleteParams::default()).await;
    let _ = nss.delete(NSB, &DeleteParams::default()).await;
}
