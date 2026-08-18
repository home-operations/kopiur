//! e2e: `Repository.spec.seed` (issue #380) — the disaster-recovery drill.
//!
//! #380's symptom: after losing a cluster you point a fresh `Repository` at a
//! surviving mirror, kopiur bootstraps it, reports `Ready`, and every restore
//! then resolves NOTHING — because "bootstrap" meant `repository create`, which
//! initializes an EMPTY repository over a backend that already holds years of
//! history (or, worse, next to it). `spec.seed` makes the rebuild copy the
//! history in first, and these scenarios drive the whole thing against a live
//! operator:
//!
//! 1. **Blob mode** — primary + policy + a real snapshot, mirrored by a
//!    `RepositoryReplication`, then a fresh repository seeded from the mirror
//!    with `seed.from.backend`. The proof is the last step: a `fromPolicy`
//!    `Restore` against the rebuilt repository resolves the ORIGINAL kopia
//!    snapshot id and Completes — the exact thing that returned nothing before
//!    the fix. Plus the two standing-field no-ops (`spec.seed` added to a live
//!    repository, and a seed over an already-initialized backend).
//! 2. **Migrate mode** — `seed.from.repository` against another repository CR
//!    with its OWN password, then a restore BY IDENTITY off the seeded copy.
//! 3. **Empty source** — a real kopia repository holding zero snapshots must
//!    NOT produce a `Ready` repository; asserted on the `Seeded=False` REASON
//!    and its remediation text, never on a phase.
//!
//! ## Mount paths (load-bearing)
//!
//! A seeding bootstrap mounts TWO filesystem repositories in ONE pod: this
//! repository at its own `backend.path`, and the seed source in the Job
//! builder's spare volume slot at the source's path. Two repositories both at
//! the default `/repo` would put two volumes on one `mountPath`, so every
//! scenario here pins distinct paths. kopia writes the repository at the PVC
//! *root* regardless of the mount path (the same fact tests/replication.rs and
//! tests/snapshot_replication.rs rely on), so a verifier over the same subpath
//! still reads it at the default `/repo`.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use std::time::{Duration, Instant};

use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::{Api, Client, ResourceExt};

use kopiur_api::consts::{ORIGIN_LABEL, REPOSITORY_UID_LABEL};
use kopiur_api::{Repository, RepositoryReplication, Restore, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait_until};

/// In-pod mount path for a SEED SOURCE mounted beside this repository's own
/// backend. Distinct from [`consts::ISOLATED_REPO_PATH`] on purpose — see the
/// module docs.
const SEED_SOURCE_PATH: &str = "/seed-src";
/// In-pod mount path for a repository that is mounted beside another one as the
/// read-write side (the migrate-seeded repository, and the replication mirror).
const SECOND_REPO_PATH: &str = "/repo-dst";

/// A filesystem `spec.seed.from.backend` over the isolated repo dir `subpath`,
/// mounted at [`SEED_SOURCE_PATH`].
fn seed_from_backend(subpath: &str) -> serde_json::Value {
    serde_json::json!({
        "backend": { "filesystem": {
            "path": SEED_SOURCE_PATH,
            "volume": { "pvc": { "name": consts::isolated_repo_pvc(subpath) } }
        } }
    })
}

/// Poll until the `Seeded` condition reaches `want_status`/`want_reason`,
/// returning it.
///
/// Asserting on the REASON rather than the phase is deliberate and is what the
/// #380 plan calls for: `Seeded=False` spans a healthy in-progress copy, three
/// different parks and five failure classes, all of which share a phase at one
/// time or another (`Initializing` while copying, `Degraded` between retries,
/// `Pending` while parked). The reason is the only stable answer, and it is the
/// same string `kubectl kopiur doctor` matches through the structural-gate
/// registry.
async fn wait_seeded<K>(
    api: &Api<K>,
    name: &str,
    want_status: &str,
    want_reason: &str,
) -> serde_json::Value
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    wait_until(
        &format!("{name} Seeded={want_status} ({want_reason})"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(api, name).await;
            let cond = s
                .get("conditions")
                .and_then(|c| c.as_array())
                .and_then(|a| {
                    a.iter().find(|c| {
                        c.get("type").and_then(|t| t.as_str()) == Some("Seeded")
                            && c.get("status").and_then(|t| t.as_str()) == Some(want_status)
                            && c.get("reason").and_then(|t| t.as_str()) == Some(want_reason)
                    })
                })
                .cloned();
            Ok(cond)
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{name} should report Seeded={want_status}/{want_reason}: {e}"))
}

/// A repository's `origin: discovered` catalog rows — the `Snapshot` CRs the
/// in-bootstrap catalog scan materializes for history it did not produce (D8:
/// a seed mints no copy CRs of its own; the ordinary scan path adopts them).
async fn discovered_rows(client: &Client, repo_uid: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let selector = format!("{ORIGIN_LABEL}=discovered,{REPOSITORY_UID_LABEL}={repo_uid}");
    api.list(&ListParams::default().labels(&selector))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// The kopia snapshot id recorded on a `Snapshot` (`status.snapshot.kopiaSnapshotID`).
async fn snapshot_kopia_id(api: &Api<Snapshot>, name: &str) -> String {
    status_json(api, name)
        .await
        .pointer("/snapshot/kopiaSnapshotID")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The kopia identity recorded on a `Snapshot` (`status.snapshot.identity`) —
/// real operator output, which is the only honest source for "what would a
/// rebuilt cluster have to reproduce".
async fn snapshot_identity(api: &Api<Snapshot>, name: &str) -> serde_json::Value {
    status_json(api, name)
        .await
        .pointer("/snapshot/identity")
        .cloned()
        .unwrap_or_else(|| panic!("Snapshot {name} should pin status.snapshot.identity"))
}

/// A `Restore` into a fresh dynamically-provisioned PVC, reading `source` from
/// `repo`.
///
/// `onMissingSnapshot: Fail` is pinned deliberately. The `fromPolicy` DEFAULT is
/// `Continue` (deploy-or-restore), which turns "the repository holds nothing"
/// into a **Completed** restore of an empty volume — i.e. #380's symptom
/// wearing a green phase. Here the whole question is whether the seeded
/// repository really holds the history, so a resolution that finds nothing must
/// be loud.
fn restore_json(name: &str, repo: &str, source: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": repo },
            "source": source,
            "policy": { "onMissingSnapshot": "Fail" },
            "target": { "pvc": { "name": format!("{name}-dst"), "capacity": "1Gi", "accessModes": ["ReadWriteOnce"] } }
        }
    })
}

/// Scenario 1 — the blob-mode disaster-recovery drill, end to end.
///
/// This is #380's acceptance test. Every step is a real operator action:
///
/// 1. the pre-disaster primary: `Repository` + `SnapshotPolicy` + a Snapshot
///    that actually Succeeded (so the repository holds REAL data, not a
///    fixture);
/// 2. a `RepositoryReplication` mirrors it blob-for-blob to a second filesystem
///    repository — the DR survivor;
/// 3. "the cluster is gone": a brand-new `Repository` over an EMPTY dir, with
///    `seed.from.backend` pointed at the mirror;
/// 4. it must reach `Ready` **and** `Seeded=True/Seeded` with a
///    `status.seed.snapshotCount >= 1`, and the catalog scan that runs inside
///    the same bootstrap must materialize the history as `origin: discovered`
///    `Snapshot` CRs;
/// 5. **the proof**: a `fromPolicy` `Restore` against the rebuilt repository,
///    through a policy carrying the pre-disaster identity, resolves the
///    ORIGINAL kopia snapshot id and Completes. Before the fix this repository
///    would have been `Ready` and empty, and this restore would have resolved
///    nothing at all.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn blob_seed_rebuilds_a_repository_from_a_mirror_and_restores_its_history() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const PRIMARY: &str = "e2e-seed-primary";
    const POLICY: &str = "e2e-seed-pol";
    const BACKUP: &str = "e2e-seed-b1";
    const MIRROR_REPL: &str = "e2e-seed-mirror";
    const REBUILT: &str = "e2e-seed-blob";
    const RECOVERY_POLICY: &str = "e2e-seed-recovery-pol";

    // 1. The pre-disaster primary, with real data in it.
    ensure_seed(&client, PRIMARY, POLICY, BACKUP, "seed-primary").await;
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let original_id = snapshot_kopia_id(&backups, BACKUP).await;
    assert!(
        !original_id.is_empty(),
        "the pre-disaster Snapshot must own a real kopia snapshot id"
    );
    let original_identity = snapshot_identity(&backups, BACKUP).await;
    // The recovery policy below reproduces this identity by hand. If either
    // half were missing the override would silently fall back to the DEFAULTS
    // (username <- object name, hostname <- namespace), the recovery policy
    // would resolve a DIFFERENT identity, and the restore below would find
    // nothing — a false failure that looks exactly like the bug under test.
    let (orig_user, orig_host) = (
        original_identity
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        original_identity
            .get("hostname")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    );
    assert!(
        !orig_user.is_empty() && !orig_host.is_empty(),
        "the pre-disaster identity must be fully pinned; got {original_identity}"
    );

    // 2. The surviving mirror: a blob-for-blob `RepositoryReplication` copy.
    ensure_repo(&client, "seed-mirror").await;
    let repls: Api<RepositoryReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repls,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "RepositoryReplication",
            "metadata": { "name": MIRROR_REPL, "namespace": E2E_NAMESPACE },
            "spec": {
                "sourceRef": { "kind": "Repository", "name": PRIMARY },
                "destination": { "filesystem": { "path": SECOND_REPO_PATH, "volume": { "pvc": { "name": consts::isolated_repo_pvc("seed-mirror") } } } },
                "schedule": { "cron": "* * * * *" }
            }
        })),
        "create the mirror RepositoryReplication",
    )
    .await;
    wait_until(
        "the mirror records a successful run (status.lastReplicated)",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repls, MIRROR_REPL).await;
            Ok(s.get("lastReplicated")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .map(str::to_string))
        },
    )
    .await
    .expect("the mirror should replicate before the disaster");
    // The mirror's CONTENT is what the drill needs, not an ongoing mirror — and
    // "the source cluster is gone" is the premise. Deleting the replication now
    // also keeps a minute-by-minute writer off the PVC the seeding Job is about
    // to read, so nothing in this scenario depends on that being safe.
    let _ = repls.delete(MIRROR_REPL, &DeleteParams::default()).await;

    // 3. The disaster: a brand-new Repository over an EMPTY dir, seeded from
    //    the mirror. `create.enabled` stays on precisely to prove the seed
    //    WINS — a seed-armed bootstrap never falls back to `repository create`,
    //    which is the whole of #380.
    ensure_repo(&client, "seed-blob").await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repos,
        &cr(repository_json(
            REBUILT,
            "seed-blob",
            serde_json::json!({ "seed": { "from": seed_from_backend("seed-mirror") } }),
        )),
        "create the rebuilt Repository with spec.seed",
    )
    .await;
    // Read the seed back off the CR: a mistyped key would be dropped silently
    // by serde AND pruned by the apiserver, and the repository would then
    // bootstrap EMPTY and call itself Ready — #380 re-entered through the test.
    let live = repos
        .get(REBUILT)
        .await
        .expect("get the rebuilt Repository");
    assert!(
        serde_json::to_value(&live.spec)
            .unwrap_or_default()
            .pointer("/seed/from/backend/filesystem/path")
            .is_some(),
        "spec.seed must have survived admission; a dropped seed makes this test vacuous"
    );

    // 4. Seeded, then Ready — in that order, because `Ready` is only flipped by
    //    a bootstrap result written AFTER the seed, the connect and the catalog
    //    listing all succeeded.
    let seeded = wait_seeded(&repos, REBUILT, "True", "Seeded").await;
    assert!(
        seeded
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| !m.is_empty()),
        "the Seeded condition must carry a message; got {seeded}"
    );
    wait_phase(&repos, REBUILT, "Ready")
        .await
        .expect("a seeded repository must reach Ready");
    let status = status_json(&repos, REBUILT).await;
    let seed_status = status
        .get("seed")
        .unwrap_or_else(|| panic!("a seeded repository must record status.seed; got {status}"));
    assert_eq!(
        seed_status.get("mode").and_then(|v| v.as_str()),
        Some("blob"),
        "status.seed: {seed_status}"
    );
    assert!(
        seed_status
            .get("snapshotCount")
            .and_then(|v| v.as_i64())
            .is_some_and(|n| n >= 1),
        "the seed must report the history it copied; status.seed: {seed_status}"
    );
    assert!(
        seed_status
            .get("seededAt")
            .and_then(|v| v.as_str())
            .is_some_and(|t| !t.is_empty()),
        "status.seed: {seed_status}"
    );
    let rebuilt_uid = live.uid().expect("the rebuilt Repository has a uid");

    // The in-bootstrap catalog scan must have materialized the seeded history
    // as discovered rows — otherwise the data is there but nothing in the
    // cluster can see it.
    let rows = wait_until(
        "the seeded history materializes as discovered Snapshot CRs",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let uid = rebuilt_uid.clone();
            async move {
                let rows = discovered_rows(&client, &uid).await;
                Ok((!rows.is_empty()).then_some(rows))
            }
        },
    )
    .await
    .expect("a seeded repository's catalog scan should materialize its history");
    assert!(
        !rows.is_empty(),
        "expected at least one discovered Snapshot for the rebuilt repository"
    );

    // 5. THE PROOF. A policy carrying the pre-disaster identity (the docs'
    //    "identity config must match byte-for-byte" rule, expressed as an
    //    explicit override so the test states what it depends on) plus a
    //    `fromPolicy` Restore against the REBUILT repository.
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            RECOVERY_POLICY,
            "Repository",
            REBUILT,
            serde_json::json!({
                "identity": { "username": orig_user, "hostname": orig_host }
            }),
        )),
        "create the recovery SnapshotPolicy",
    )
    .await;

    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restore = "e2e-seed-blob-restore";
    let _ = restores.delete(restore, &DeleteParams::default()).await;
    create_idempotent(
        &restores,
        &cr(restore_json(
            restore,
            REBUILT,
            serde_json::json!({ "fromPolicy": { "name": RECOVERY_POLICY } }),
        )),
        "create the fromPolicy Restore against the rebuilt repository",
    )
    .await;
    wait_phase(&restores, restore, "Completed")
        .await
        .expect("a fromPolicy restore off a seeded repository must Complete");
    let rs = status_json(&restores, restore).await;
    assert_eq!(
        rs.pointer("/resolved/kopiaSnapshotID")
            .and_then(|v| v.as_str()),
        Some(original_id.as_str()),
        "the restore must resolve the ORIGINAL pre-disaster snapshot — this is the #380 \
         symptom: before the fix the rebuilt repository was Ready and empty, and this \
         resolved nothing. status: {rs}"
    );

    // --- Standing-field no-op (a): `spec.seed` added to a LIVE repository -----
    // A seed is armed only while `status.uniqueId` is unset, so a seed block
    // added to (or left standing on) a repository that is already initialized
    // must change nothing at all: no re-seed, no marker, no new uniqueId. This
    // is what makes `spec.seed` safe to commit unconditionally in GitOps.
    let before = status_json(&repos, PRIMARY).await;
    let unique_id = before
        .get("uniqueId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        !unique_id.is_empty(),
        "the live primary must already have a pinned uniqueId; status: {before}"
    );
    repos
        .patch(
            PRIMARY,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "seed": { "from": seed_from_backend("seed-mirror") } }
            })),
        )
        .await
        .expect("patch spec.seed onto the live primary");
    // Hold the invariant over a window rather than sampling once: the failure
    // this guards against (a re-seed) takes a reconcile or two to appear.
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        let s = status_json(&repos, PRIMARY).await;
        assert_eq!(
            s.get("uniqueId").and_then(|v| v.as_str()),
            Some(unique_id.as_str()),
            "a standing spec.seed must never re-point a live repository; status: {s}"
        );
        assert!(
            s.pointer("/seed/startedAt").is_none(),
            "a standing spec.seed on a live repository must not arm a seed attempt; status: {s}"
        );
        tokio::time::sleep(poll_interval()).await;
    }
    wait_phase(&repos, PRIMARY, "Ready")
        .await
        .expect("the live primary must stay Ready with a standing spec.seed");

    // --- Standing-field no-op (b): a seed over an ALREADY-INITIALIZED backend --
    // The other half of "never clobbers anything": a FRESH CR (so the seed IS
    // armed) over a backend that already holds a repository. The mover must
    // report the no-op rather than copying over it, and the reason is how a
    // GitOps re-apply of the whole DR bundle stays idempotent.
    const REAPPLIED: &str = "e2e-seed-blob-again";
    create_idempotent(
        &repos,
        &cr(repository_json(
            REAPPLIED,
            "seed-blob",
            serde_json::json!({
                "seed": { "from": seed_from_backend("seed-mirror") },
                // The first CR owns this repository's maintenance; a second
                // manager would only add noise to the assertion below.
                "maintenance": { "enabled": false }
            }),
        )),
        "re-apply the seeded Repository as a fresh CR",
    )
    .await;
    wait_seeded(&repos, REAPPLIED, "True", "AlreadyInitialized").await;
    wait_phase(&repos, REAPPLIED, "Ready")
        .await
        .expect("an already-initialized seed target must still reach Ready");

    let _ = restores.delete(restore, &DeleteParams::default()).await;
}

/// Scenario 2 — the migrate-mode drill: `seed.from.repository`.
///
/// Where blob mode copies storage, migrate mode copies SNAPSHOTS between two
/// independently-encrypted repositories with `kopia snapshot migrate`. Three
/// things only a live run can prove, and all three are asserted:
///
/// * the source repository's OWN kopia password reached the mover (the seeded
///   repository has a different one, so a source connect that fell back to the
///   local password could not have opened it);
/// * migrate mode creates the local repository ITSELF — `create.enabled` is
///   `false` here, and a seed-armed bootstrap never takes the create fallback;
/// * `kopia snapshot migrate` preserves identity, so a restore BY IDENTITY off
///   the seeded copy resolves the source's history.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn migrate_seed_copies_history_from_another_repository_and_keeps_its_identity() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const SOURCE: &str = "e2e-seed-mig-src";
    const POLICY: &str = "e2e-seed-mig-pol";
    const BACKUP: &str = "e2e-seed-mig-b1";
    const SEEDED: &str = "e2e-seed-mig-dst";
    const SEEDED_SECRET: &str = "kopia-seed-mig-creds";
    // DIFFERENT from the shared `kopia-creds` password — the whole point.
    const SEEDED_PASSWORD: &str = "e2e-seed-migrate-password-789";

    // The source repository, with real history under a real identity.
    ensure_seed(&client, SOURCE, POLICY, BACKUP, "seed-mig-src").await;
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let source_identity = snapshot_identity(&backups, BACKUP).await;

    // The seeded repository's own password.
    {
        use kopiur_e2e::apply::{Fixture, apply_all};
        use kopiur_e2e::builders;
        let fixtures: Vec<Fixture> = vec![
            builders::opaque_secret(
                E2E_NAMESPACE,
                SEEDED_SECRET,
                &[(consts::KEY_KOPIA_PASSWORD, SEEDED_PASSWORD)],
            )
            .into(),
        ];
        apply_all(&client, &fixtures)
            .await
            .expect("provision the seeded repository's password Secret");
    }

    ensure_repo(&client, "seed-mig-dst").await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repos,
        &cr(repository_json_with(
            SEEDED,
            "seed-mig-dst",
            // Beside the source's `/repo` in the same pod — see the module docs.
            SECOND_REPO_PATH,
            SEEDED_SECRET,
            serde_json::json!({
                // Deliberately OFF: migrate mode runs `repository create`
                // itself, and a seed-armed bootstrap never takes the create
                // fallback — so reaching Ready here proves both at once.
                "create": { "enabled": false },
                "seed": {
                    "from": { "repository": { "kind": "Repository", "name": SOURCE } },
                    "migrate": { "parallel": 2 }
                }
            }),
        )),
        "create the migrate-seeded Repository",
    )
    .await;
    let live = repos.get(SEEDED).await.expect("get the seeded Repository");
    assert_eq!(
        serde_json::to_value(&live.spec)
            .unwrap_or_default()
            .pointer("/seed/from/repository/name")
            .and_then(|v| v.as_str()),
        Some(SOURCE),
        "spec.seed.from.repository must have survived admission"
    );

    wait_seeded(&repos, SEEDED, "True", "Seeded").await;
    wait_phase(&repos, SEEDED, "Ready")
        .await
        .expect("a migrate-seeded repository must reach Ready");
    let status = status_json(&repos, SEEDED).await;
    let seed_status = status
        .get("seed")
        .unwrap_or_else(|| panic!("a seeded repository must record status.seed; got {status}"));
    assert_eq!(
        seed_status.get("mode").and_then(|v| v.as_str()),
        Some("migrate"),
        "status.seed: {seed_status}"
    );
    assert!(
        seed_status
            .get("snapshotsCopied")
            .and_then(|v| v.as_i64())
            .is_some_and(|n| n >= 1),
        "a migrate seed must report the manifests it copied; status.seed: {seed_status}"
    );

    // Identity survives the migrate, so a restore keyed on the SOURCE's
    // identity finds the history in the SEEDED repository.
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restore = "e2e-seed-mig-restore";
    let _ = restores.delete(restore, &DeleteParams::default()).await;
    create_idempotent(
        &restores,
        &cr(restore_json(
            restore,
            SEEDED,
            serde_json::json!({ "identity": source_identity }),
        )),
        "create the identity Restore against the migrate-seeded repository",
    )
    .await;
    wait_phase(&restores, restore, "Completed")
        .await
        .expect("an identity restore off a migrate-seeded repository must Complete");
    let rs = status_json(&restores, restore).await;
    assert_eq!(
        rs.get("sourceKind").and_then(|v| v.as_str()),
        Some("Identity"),
        "status: {rs}"
    );
    assert!(
        rs.pointer("/resolved/kopiaSnapshotID")
            .and_then(|v| v.as_str())
            .is_some_and(|id| !id.is_empty()),
        "the restore must resolve a real migrated snapshot (migrate mints NEW manifest ids in \
         the destination repository, so the id differs from the source's — what must survive \
         is the identity it was recorded under). status: {rs}"
    );

    let _ = restores.delete(restore, &DeleteParams::default()).await;
}

/// Scenario 3 — a valid but EMPTY seed source must never produce a `Ready`
/// repository.
///
/// The source here is a real, initialized kopia repository that simply holds
/// zero snapshots — a mirror whose replication never ran, which is exactly what
/// a mis-pointed DR manifest looks like. Blocking `Ready` is the intended
/// behaviour (`allowEmptySource` is the explicit opt-out), because the
/// alternative is #380 again: a `Ready` repository with nothing in it.
///
/// The assertion is on the `Seeded=False` **reason** and its remediation text,
/// never on the phase: this state cycles `Initializing` -> `Degraded` as kopiur
/// recycles the Job every ~2 minutes, so a phase assertion would be a coin
/// flip. The reason is stable, and it is the same string the structural-gate
/// registry (and so `kubectl kopiur doctor`) matches on.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn an_empty_seed_source_blocks_ready_and_says_how_to_override_it() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const EMPTY_SOURCE: &str = "e2e-seed-empty-src";
    const TARGET: &str = "e2e-seed-empty-dst";

    // A real kopia repository with no snapshots in it.
    ensure_repo(&client, "seed-empty-src").await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repos,
        &cr(repository_json(
            EMPTY_SOURCE,
            "seed-empty-src",
            serde_json::json!({}),
        )),
        "create the empty source Repository",
    )
    .await;
    wait_phase(&repos, EMPTY_SOURCE, "Ready")
        .await
        .expect("the empty source repository should bootstrap (it is valid, just empty)");

    // Seeding from it must park, loudly.
    ensure_repo(&client, "seed-empty-dst").await;
    create_idempotent(
        &repos,
        &cr(repository_json(
            TARGET,
            "seed-empty-dst",
            serde_json::json!({ "seed": { "from": seed_from_backend("seed-empty-src") } }),
        )),
        "create the Repository seeded from an empty source",
    )
    .await;

    let cond = wait_seeded(&repos, TARGET, "False", "SeedSourceEmpty").await;
    let message = cond
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("allowEmptySource"),
        "the condition must name the override that unblocks it, or the operator has nowhere \
         to go; message: {message}"
    );
    // ...and the repository must NOT be Ready behind it. Asserted at the moment
    // the reason is observed AND held over a window, because "Ready arrives a
    // beat later" is precisely the bug (#380: Ready with an empty repository).
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let s = status_json(&repos, TARGET).await;
        assert_ne!(
            s.get("phase").and_then(|v| v.as_str()),
            Some("Ready"),
            "a repository whose seed source is empty must never report Ready; status: {s}"
        );
        tokio::time::sleep(poll_interval()).await;
    }

    let _ = repos.delete(TARGET, &DeleteParams::default()).await;
}
