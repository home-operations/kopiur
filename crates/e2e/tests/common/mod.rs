//! Shared helpers for the ADR-0004 / ADR-0005 reshape e2e scenarios.
//!
//! Split across several purpose-named test files (`mover_config`, `data_integrity`,
//! `repository_lifecycle`, `tenancy`, `verification`, `replication`); each includes
//! this module with `mod common;`. As a subdirectory module (`tests/common/mod.rs`)
//! cargo does NOT compile it as its own test binary.
//!
//! ## Repo isolation (load-bearing)
//!
//! The operator mounts a filesystem repo's PVC at `backend.path` AND runs
//! `kopia --path=backend.path`, so the PVC *root* IS the kopia repo. A `path` subdir
//! under one shared PVC therefore does NOT isolate repos — every scenario would
//! collide on the same kopia repo at the PVC root. So each scenario binds its OWN
//! per-`subpath` PV/PVC (over `/kopiur-e2e/repos/<subpath>`, seeded 0777 by the mise
//! `e2e-node-seed` task) via [`ensure_repo`], using the fixed in-pod mount path
//! [`consts::ISOLATED_REPO_PATH`]. Keep `REPO_SUBPATHS` (consts) and the node-seed
//! list in lockstep.

#![allow(dead_code)]

use kube::api::{DeleteParams, PostParams};
use kube::{Api, Client};
use serde::Serialize;
use serde::de::DeserializeOwned;

use k8s_openapi::api::batch::v1::Job;

use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, consts, default_timeout, poll_interval, wait_until};

/// The repository password Secret the chart-installed operator reads.
pub const CREDS_SECRET: &str = "kopia-creds";

/// Deserialize a CR from a JSON literal into its typed kube object.
pub fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

/// Create `obj`, tolerating one left behind by a previous attempt at this test.
///
/// The `e2e` nextest profile retries a failed test IN PLACE
/// (`.config/nextest.toml`), and a failed attempt leaves its CRs in the cluster.
/// A bare `.expect("create")` therefore turns every retry into an instant
/// `AlreadyExists` panic whose message buries the real first failure. Every
/// other API error still panics — this tolerates exactly one reason, not "any
/// create problem".
pub async fn create_idempotent<K>(api: &Api<K>, obj: &K, what: &str)
where
    K: Clone + std::fmt::Debug + DeserializeOwned + Serialize,
{
    match api.create(&PostParams::default(), obj).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.reason == "AlreadyExists" => {}
        Err(e) => panic!("{what}: {e}"),
    }
}

/// Provision the isolated per-`subpath` repo PV/PVC (idempotent), so a filesystem
/// `Repository`/`ClusterRepository` over `subpath` gets its OWN kopia repo (the PVC
/// root). See the module docs for why a shared PVC + `path` subdir does not isolate.
pub async fn ensure_repo(client: &Client, subpath: &str) {
    use kopiur_e2e::apply::{Fixture, apply_all};
    use kopiur_e2e::builders;
    let pv = consts::isolated_repo_pv(subpath);
    let pvc = consts::isolated_repo_pvc(subpath);
    let host = consts::isolated_repo_hostpath(subpath);
    let fixtures: Vec<Fixture> = vec![
        builders::hostpath_pv(&pv, &host, "1Gi").into(),
        builders::static_pvc(E2E_NAMESPACE, &pvc, &pv, "1Gi").into(),
    ];
    apply_all(client, &fixtures)
        .await
        .unwrap_or_else(|e| panic!("provision isolated repo PV/PVC for subpath {subpath}: {e}"));
}

/// Provision the isolated per-`subpath` repo PVC in an arbitrary namespace `ns` (over
/// the same hostPath repo dir as [`ensure_repo`]), so a consumer mover that runs in
/// `ns` — a `ClusterRepository` backup runs in the CONSUMER namespace, where it mounts
/// the repository's filesystem PVC by name — can mount the repo. The PV name is keyed
/// by `ns` so multiple namespaces can each bind the shared hostPath (hostPath PVs bind
/// 1:1 to a PVC). Idempotent.
pub async fn ensure_repo_in_ns(client: &Client, subpath: &str, ns: &str) {
    use kopiur_e2e::apply::{Fixture, apply_all};
    use kopiur_e2e::builders;
    let pv = format!("{}-{ns}", consts::isolated_repo_pv(subpath));
    let pvc = consts::isolated_repo_pvc(subpath);
    let host = consts::isolated_repo_hostpath(subpath);
    let fixtures: Vec<Fixture> = vec![
        builders::hostpath_pv(&pv, &host, "1Gi").into(),
        builders::static_pvc(ns, &pvc, &pv, "1Gi").into(),
    ];
    apply_all(client, &fixtures)
        .await
        .unwrap_or_else(|e| panic!("provision repo PVC in ns {ns} for subpath {subpath}: {e}"));
}

/// A namespaced filesystem `Repository` over its OWN isolated repo hostPath (keyed by
/// `subpath`), with the given extra `spec` fields merged in. Callers must
/// [`ensure_repo`]`(subpath)` first.
pub fn repository_json(
    name: &str,
    subpath: &str,
    extra_spec: serde_json::Value,
) -> serde_json::Value {
    repository_json_with(
        name,
        subpath,
        consts::ISOLATED_REPO_PATH,
        CREDS_SECRET,
        extra_spec,
    )
}

/// Like [`repository_json`], but with an explicit in-pod mount `path` and
/// credentials Secret (key `KOPIA_PASSWORD`), so a scenario can give a
/// repository its OWN password and/or a non-default mount path.
///
/// The `path` knob matters when TWO filesystem repositories meet in ONE mover
/// pod (a `SnapshotReplication` fs→fs run mounts the source read-only AND the
/// destination read-write): both defaulting to `/repo` would put two volumes at
/// one `mountPath`. kopia writes the repo at the PVC *root* regardless of the
/// mount path (same fact the `RepositoryReplication` e2e relies on), so a
/// verifier over the same `subpath` at the default `/repo` still reads a repo
/// written under a different `path`.
pub fn repository_json_with(
    name: &str,
    subpath: &str,
    path: &str,
    creds_secret: &str,
    extra_spec: serde_json::Value,
) -> serde_json::Value {
    merge_spec(
        serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Repository",
            "metadata": { "name": name, "namespace": E2E_NAMESPACE },
            "spec": {
                "backend": { "filesystem": { "path": path, "volume": { "pvc": { "name": consts::isolated_repo_pvc(subpath) } } } },
                "encryption": { "passwordSecretRef": { "name": creds_secret, "key": "KOPIA_PASSWORD" } },
                "create": { "enabled": true }
            }
        }),
        extra_spec,
    )
}

/// A cluster-scoped filesystem `ClusterRepository` over its OWN isolated repo hostPath
/// (keyed by `subpath`), opened to all namespaces, with extra `spec` fields merged in.
/// Callers must [`ensure_repo`]`(subpath)` first.
pub fn cluster_repository_json(
    name: &str,
    subpath: &str,
    extra_spec: serde_json::Value,
) -> serde_json::Value {
    merge_spec(
        serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "ClusterRepository",
            "metadata": { "name": name },
            "spec": {
                "backend": { "filesystem": { "path": consts::ISOLATED_REPO_PATH, "volume": { "pvc": { "name": consts::isolated_repo_pvc(subpath) } } } },
                "encryption": { "passwordSecretRef": { "name": CREDS_SECRET, "namespace": E2E_NAMESPACE, "key": "KOPIA_PASSWORD" } },
                "create": { "enabled": true },
                "allowedNamespaces": { "all": true }
            }
        }),
        extra_spec,
    )
}

/// Merge `extra` (an object of **bare spec fields**) into `base["spec"]`.
///
/// `extra` is the CONTENTS of `spec`, not a `{"spec": {...}}` wrapper. Passing
/// the wrapper is silently catastrophic and is rejected here on purpose: it
/// lands as `spec.spec`, which serde drops on the way into the typed object
/// (unknown fields are ignored) and which the apiserver would prune from a
/// structural schema anyway. The CR is then created successfully carrying the
/// BASE spec, so the test exercises the default policy while believing it
/// configured something — the failure surfaces much later as "the feature does
/// not work", with nothing in any log pointing at the overlay. That is exactly
/// how the #351 e2e first failed in CI.
pub fn merge_spec(mut base: serde_json::Value, extra: serde_json::Value) -> serde_json::Value {
    if let (Some(spec), serde_json::Value::Object(more)) = (base.get_mut("spec"), extra) {
        assert!(
            !more.contains_key("spec"),
            "merge_spec takes the CONTENTS of spec, not a {{\"spec\": ...}} wrapper — \
             a wrapped overlay is silently dropped and the test would run against the \
             default spec"
        );
        let serde_json::Value::Object(s) = spec else {
            panic!("spec must be an object");
        };
        s.extend(more);
    }
    base
}

/// A `SnapshotPolicy` over the shared `e2e-src` source PVC, referencing `repo`.
///
/// Pins `copyMethod: Direct` explicitly — `e2e-src` is a statically-provisioned
/// (hostPath-backed, non-CSI) PVC, and `copyMethod` now defaults to `Snapshot`, which
/// would fail preflight (`SourceNotCSIProvisioned`) against it. Callers that need CSI
/// staging (e.g. `copy_methods.rs`) override `copyMethod` via `extra_spec`, which
/// [`merge_spec`] applies on top of this base.
pub fn snapshot_policy_json(
    ns: &str,
    name: &str,
    repo_kind: &str,
    repo: &str,
    extra_spec: serde_json::Value,
) -> serde_json::Value {
    merge_spec(
        serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": name, "namespace": ns },
            "spec": {
                "repository": { "kind": repo_kind, "name": repo },
                "sources": [ { "pvc": { "name": "e2e-src" } } ],
                "copyMethod": "Direct",
                "retention": { "keepLatest": 5 }
            }
        }),
        extra_spec,
    )
}

/// A MULTI-repository `SnapshotPolicy` (#368 Feature B) over the shared
/// `e2e-src` source: `spec.repositories` lists the given namespaced
/// `Repository` names in order. Same defaults as [`snapshot_policy_json`]
/// (`copyMethod: Direct` for the non-CSI `e2e-src` PVC, `keepLatest: 5`),
/// with `extra_spec` merged on top via [`merge_spec`] (bare spec fields, no
/// `{"spec": ...}` wrapper — see the merge_spec docs for why the wrapper is
/// silently catastrophic).
///
/// Callers should read the created CR back and assert
/// `spec.repositories.len()` — the same discipline as
/// `multi_pvc_group::assert_selector_landed`: a policy that quietly kept a
/// single-repo shape (e.g. the field pruned by a stale CRD schema) produces
/// one child and fails far away from the real fault.
pub fn multi_repo_policy_json(
    ns: &str,
    name: &str,
    repos: &[&str],
    extra_spec: serde_json::Value,
) -> serde_json::Value {
    let repositories: Vec<serde_json::Value> = repos
        .iter()
        .map(|r| serde_json::json!({ "kind": "Repository", "name": r }))
        .collect();
    merge_spec(
        serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": name, "namespace": ns },
            "spec": {
                "repositories": repositories,
                "sources": [ { "pvc": { "name": "e2e-src" } } ],
                "copyMethod": "Direct",
                "retention": { "keepLatest": 5 }
            }
        }),
        extra_spec,
    )
}

/// A `Snapshot` referencing `policy` (default `deletionPolicy: Retain`).
///
/// Carries the `kopiur.home-operations.com/config=<policy>` label — the same label a
/// `SnapshotSchedule` puts on the snapshots it creates — so the snapshot is visible to
/// its policy's GFS retention (which lists by that label). A manually-created snapshot
/// without it is outside retention by design.
pub fn snapshot_json(
    ns: &str,
    name: &str,
    policy: &str,
    extra_spec: serde_json::Value,
) -> serde_json::Value {
    merge_spec(
        serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": {
                "name": name,
                "namespace": ns,
                "labels": { "kopiur.home-operations.com/config": policy }
            },
            "spec": { "policyRef": { "name": policy }, "deletionPolicy": "Retain" }
        }),
        extra_spec,
    )
}

/// Poll a CR until `status.phase == want_phase`.
pub async fn wait_phase<K>(api: &Api<K>, name: &str, want_phase: &str) -> anyhow::Result<()>
where
    K: kube::Resource + Clone + DeserializeOwned + Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    wait_until(
        &format!("{name} phase={want_phase}"),
        default_timeout(),
        poll_interval(),
        || async {
            let Some(obj) = api.get_opt(name).await? else {
                return Ok(None);
            };
            let v = serde_json::to_value(&obj).unwrap_or_default();
            let phase = v
                .get("status")
                .and_then(|s| s.get("phase"))
                .and_then(|p| p.as_str())
                .unwrap_or("");
            Ok((phase == want_phase).then_some(()))
        },
    )
    .await
}

/// Read a CR's `status` as JSON (or `null`).
pub async fn status_json<K>(api: &Api<K>, name: &str) -> serde_json::Value
where
    K: kube::Resource + Clone + DeserializeOwned + Serialize + std::fmt::Debug,
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

/// Poll a CR until its `status.conditions[type=type_].status` equals `want`,
/// returning the matching condition object.
pub async fn wait_condition<K>(
    api: &Api<K>,
    name: &str,
    type_: &str,
    want: &str,
) -> anyhow::Result<serde_json::Value>
where
    K: kube::Resource + Clone + DeserializeOwned + Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    wait_until(
        &format!("{name} {type_}={want}"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(api, name).await;
            let cond = s
                .get("conditions")
                .and_then(|c| c.as_array())
                .and_then(|a| {
                    a.iter()
                        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some(type_))
                })
                .cloned();
            Ok(cond.filter(|c| c.get("status").and_then(|s| s.as_str()) == Some(want)))
        },
    )
    .await
}

/// Wait until a CR's kstatus `Ready` condition is `True`, returning the condition.
///
/// Prefer this over a bare [`wait_phase`] whenever the next thing you assert is a
/// controller-HEALED field — a kstatus condition, `status.hooks.*`, a
/// `kopiur_resource_phase` metric, or anything else written *after* the terminal
/// phase. Kopiur writes terminal state in two passes: the mover stamps the
/// terminal `status.phase`, then the controller's FOLLOW-UP reconcile heals the
/// derived fields a beat later (that reconcile is debounced). Gating on the phase
/// and reading a healed field immediately races that heal and reads a stale value
/// — the exact bug behind `restore_completed_reports_kstatus_ready`,
/// `http_request_post_hook_hits_in_cluster_receiver`, and
/// `metrics_reflect_backup_lifecycle`. Gating on `Ready=True` waits for the heal.
/// See `docs/dev/watch-and-reconcile.md` ("Two-pass terminal heal").
pub async fn wait_ready<K>(api: &Api<K>, name: &str) -> anyhow::Result<serde_json::Value>
where
    K: kube::Resource + Clone + DeserializeOwned + Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    wait_condition(api, name, "Ready", "True").await
}

/// Wait until the mover `Job` named `name` (in `ns`) exists, returning it.
pub async fn wait_for_job(jobs: &Api<Job>, name: &str) -> Job {
    wait_until(
        &format!("mover Job {name} created"),
        default_timeout(),
        poll_interval(),
        || async { jobs.get_opt(name).await },
    )
    .await
    .unwrap_or_else(|_| panic!("mover Job {name} should be created"))
}

/// The env var carrying a mover run's inline work-spec JSON — the controller↔
/// mover contract these tests assert at the wire level, so it is a deliberate
/// LITERAL here (an accidental rename in either crate must fail this suite).
#[allow(dead_code)] // each test binary compiles `common` separately; not all use it
pub const WORK_SPEC_ENV: &str = "KOPIUR_WORK_SPEC";

/// Extract + parse the inline work-spec JSON from a mover Job's pod env. The
/// spec rides the Job itself (#224 — no per-run ConfigMap exists), so this
/// reads the full controller→mover contract from the one run object, which
/// outlives the run by its `ttlSecondsAfterFinished` — no race with completion.
#[allow(dead_code)]
pub fn work_spec_json_from_job(job: &Job) -> serde_json::Value {
    let raw = job
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.containers.first())
        .and_then(|c| c.env.as_ref())
        .and_then(|env| env.iter().find(|e| e.name == WORK_SPEC_ENV))
        .and_then(|e| e.value.clone())
        .expect("mover Job carries the inline work-spec env");
    serde_json::from_str(&raw).expect("work-spec env parses as JSON")
}

/// Wait for the mover Job named `name` and return its parsed work-spec JSON.
#[allow(dead_code)]
pub async fn wait_for_work_spec_json(jobs: &Api<Job>, name: &str) -> serde_json::Value {
    let job = wait_for_job(jobs, name).await;
    work_spec_json_from_job(&job)
}

/// The mover container's container-level `securityContext` from a Job's pod template.
pub fn job_container_sc(job: &Job) -> Option<k8s_openapi::api::core::v1::SecurityContext> {
    job.spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.containers.first())
        .and_then(|c| c.security_context.clone())
}

/// The mover pod-level `securityContext` from a Job's pod template.
pub fn job_pod_sc(job: &Job) -> Option<k8s_openapi::api::core::v1::PodSecurityContext> {
    job.spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.security_context.clone())
}

/// The named `Volume` from a Job's pod template, or `None` if absent. Used to assert
/// how a mover volume was provisioned (emptyDir vs sized ephemeral PVC + class).
pub fn job_named_volume(job: &Job, name: &str) -> Option<k8s_openapi::api::core::v1::Volume> {
    job.spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.volumes.as_ref())
        .and_then(|vols| vols.iter().find(|v| v.name == name))
        .cloned()
}

/// The deep-verify scratch `Volume` (named `scratch`) — see [`job_named_volume`].
pub fn job_scratch_volume(job: &Job) -> Option<k8s_openapi::api::core::v1::Volume> {
    job_named_volume(job, "scratch")
}

/// The kopia cache `Volume` (named `kopia-cache`) — see [`job_named_volume`].
pub fn job_cache_volume(job: &Job) -> Option<k8s_openapi::api::core::v1::Volume> {
    job_named_volume(job, "kopia-cache")
}

/// The `(storageClassName, capacity)` of a `Volume`'s ephemeral `volumeClaimTemplate`,
/// or `None` if the volume is not a sized ephemeral PVC (e.g. an `emptyDir`).
pub fn ephemeral_class_and_capacity(
    vol: &k8s_openapi::api::core::v1::Volume,
) -> Option<(Option<String>, Option<String>)> {
    let spec = &vol.ephemeral.as_ref()?.volume_claim_template.as_ref()?.spec;
    let capacity = spec
        .resources
        .as_ref()
        .and_then(|r| r.requests.as_ref())
        .and_then(|m| m.get("storage"))
        .map(|q| q.0.clone());
    Some((spec.storage_class_name.clone(), capacity))
}

/// Assert a rendered mover container securityContext STILL carries the hardened
/// `capabilities.drop:[ALL]` + `seccompProfile: RuntimeDefault` — the de-hardening
/// regression guard (ADR-0004 §2): a partial `moverDefaults`/`mover` override must
/// never wipe the hardened base.
pub fn assert_hardening_survives(sc: &k8s_openapi::api::core::v1::SecurityContext, ctx: &str) {
    let drops = sc
        .capabilities
        .as_ref()
        .and_then(|c| c.drop.clone())
        .unwrap_or_default();
    assert!(
        drops.iter().any(|d| d == "ALL"),
        "{ctx}: hardened capabilities.drop:[ALL] must survive the merge, got {drops:?}"
    );
    assert_eq!(
        sc.seccomp_profile.as_ref().map(|s| s.type_.as_str()),
        Some("RuntimeDefault"),
        "{ctx}: hardened seccompProfile: RuntimeDefault must survive the merge"
    );
}

/// Bootstrap a fresh ReadOnly verifier `Repository` over `subpath` and return its
/// observed `status.storageStats.snapshotCount` (the catalog-scan count). Used to
/// prove whether a kopia snapshot exists in the repo.
pub async fn observed_snapshot_count(client: &Client, verifier: &str, subpath: &str) -> i64 {
    observed_snapshot_count_with_creds(client, verifier, subpath, CREDS_SECRET).await
}

/// Like [`observed_snapshot_count`], but connecting with a scenario-owned
/// credentials Secret — for repositories whose password is NOT the shared
/// `kopia-creds` one (e.g. the different-password `SnapshotReplication`
/// destination).
pub async fn observed_snapshot_count_with_creds(
    client: &Client,
    verifier: &str,
    subpath: &str,
    creds_secret: &str,
) -> i64 {
    // The verifier connects to the SAME isolated repo dir as the writer (same subpath
    // ⇒ same PVC); ensure it exists (idempotent) in case the verifier runs first.
    ensure_repo(client, subpath).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = repos
        .create(
            &PostParams::default(),
            // ReadOnly + create disabled: only ever CONNECTS to the existing repo and
            // scans its catalog (never writes/maintains).
            &cr(repository_json_with(
                verifier,
                subpath,
                consts::ISOLATED_REPO_PATH,
                creds_secret,
                serde_json::json!({ "mode": "ReadOnly", "create": { "enabled": false } }),
            )),
        )
        .await;
    wait_phase(&repos, verifier, "Ready")
        .await
        .expect("verifier Repository should connect+scan to Ready");
    let count = wait_until(
        &format!("verifier {verifier} reports a snapshotCount"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repos, verifier).await;
            Ok(s.pointer("/storageStats/snapshotCount")
                .and_then(|v| v.as_i64()))
        },
    )
    .await
    .unwrap_or_else(|_| panic!("verifier {verifier} should report storageStats.snapshotCount"));
    let _ = repos.delete(verifier, &DeleteParams::default()).await;
    count
}

/// Create a workload-namespace source PVC bound to a fresh hostPath PV over the shared
/// `/kopiur-e2e/src` dir (a hostPath PV binds 1:1 to a PVC).
pub async fn ensure_workload_source(client: &Client, ns: &str, label: &str) {
    use kopiur_e2e::apply::{Fixture, apply_all};
    use kopiur_e2e::builders;
    let pv = format!("kopiur-e2e-src-{label}");
    let fixtures: Vec<Fixture> = vec![
        builders::hostpath_pv(&pv, consts::HOSTPATH_SRC, "1Gi").into(),
        builders::static_pvc(ns, consts::PVC_SRC, &pv, "1Gi").into(),
    ];
    apply_all(client, &fixtures)
        .await
        .expect("provision workload-namespace source PV/PVC");
}

/// Seed a `Repository` + `SnapshotPolicy` + `Snapshot` over `subpath`, returning once
/// the Snapshot has Succeeded (a real snapshot to restore/operate on).
pub async fn ensure_seed(client: &Client, repo: &str, policy: &str, backup: &str, subpath: &str) {
    ensure_repo(client, subpath).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    if repos.get_opt(repo).await.ok().flatten().is_none() {
        let _ = repos
            .create(
                &PostParams::default(),
                &cr(repository_json(repo, subpath, serde_json::json!({}))),
            )
            .await;
    }
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("seed repo Ready");
    if policies.get_opt(policy).await.ok().flatten().is_none() {
        let _ = policies
            .create(
                &PostParams::default(),
                &cr(snapshot_policy_json(
                    E2E_NAMESPACE,
                    policy,
                    "Repository",
                    repo,
                    serde_json::json!({}),
                )),
            )
            .await;
    }
    if backups.get_opt(backup).await.ok().flatten().is_none() {
        let _ = backups
            .create(
                &PostParams::default(),
                &cr(snapshot_json(
                    E2E_NAMESPACE,
                    backup,
                    policy,
                    serde_json::json!({}),
                )),
            )
            .await;
    }
    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("seed Snapshot Succeeded");
}

/// Like [`ensure_seed`] but with NO snapshot: a Ready filesystem `Repository` over an
/// isolated, never-snapshotted `subpath` plus a `SnapshotPolicy`. A `fromPolicy` restore
/// against `policy` therefore resolves to NO snapshot — the deploy-or-restore
/// (`onMissingSnapshot: Continue`) case. Use a dedicated `subpath` so no other scenario's
/// snapshot leaks into this policy's identity. Idempotent.
pub async fn ensure_empty_policy(client: &Client, repo: &str, policy: &str, subpath: &str) {
    ensure_repo(client, subpath).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    if repos.get_opt(repo).await.ok().flatten().is_none() {
        let _ = repos
            .create(
                &PostParams::default(),
                &cr(repository_json(repo, subpath, serde_json::json!({}))),
            )
            .await;
    }
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("empty-policy repo Ready");
    if policies.get_opt(policy).await.ok().flatten().is_none() {
        let _ = policies
            .create(
                &PostParams::default(),
                &cr(snapshot_policy_json(
                    E2E_NAMESPACE,
                    policy,
                    "Repository",
                    repo,
                    serde_json::json!({}),
                )),
            )
            .await;
    }
}

// --- CSI populator helpers (shared by repository_lifecycle, multi_pvc_group,
//     populator_fanout, copy_methods, staging_*) ------------------------------
//
// Hoisted here by #443: the fan-out scenario needs the same claim/reader
// machinery the single-claim populator tests use, and a second private copy in
// a second test binary is exactly how two copies drift.

/// CSI hostpath StorageClass installed by the `snapshot-stack` harness step — a
/// populator-aware provisioner (its external-provisioner defers to `dataSourceRef`).
/// `Immediate` binding (provisions the prime PVC as soon as it's created).
pub const CSI_STORAGE_CLASS: &str = "csi-hostpath-sc";
/// The `WaitForFirstConsumer` variant over the same hostpath provisioner (also installed
/// by `snapshot-stack`). Exercises the populator handshake's late-binding path: the claim
/// only gets a `selected-node` once a pod schedules it, which the controller pins the
/// prime PVC to.
pub const CSI_STORAGE_CLASS_WFFC: &str = "csi-hostpath-sc-wffc";

/// The label KEY a `pvcSelector` matches on, mirroring `deploy/examples/04-multi-pvc-selector.yaml`.
pub const BACKUP_LABEL_KEY: &str = "backup";

/// Whether `storage_class` is present (proceed with the test). If it's absent we either
/// HARD-FAIL or skip: a `csi: true` CI shard installs the snapshot stack and sets
/// `KOPIUR_E2E_REQUIRE_CSI=1`, so there an absent class is a real setup failure and must
/// NOT silently pass — a silent skip once let a populator regression ship green (#121).
/// Without that env (local dev with no snapshot stack) we skip gracefully.
pub async fn csi_class_present_or_skip(client: &Client, storage_class: &str) -> bool {
    use k8s_openapi::api::storage::v1::StorageClass;
    let scs: Api<StorageClass> = Api::all(client.clone());
    if scs
        .get_opt(storage_class)
        .await
        .expect("list storageclasses")
        .is_some()
    {
        return true;
    }
    let require = std::env::var("KOPIUR_E2E_REQUIRE_CSI").is_ok_and(|v| v == "1");
    assert!(
        !require,
        "storageclass {storage_class} absent but KOPIUR_E2E_REQUIRE_CSI=1 — this shard must \
         install the CSI snapshot stack (mise run //crates/e2e:snapshot-stack) before the \
         populator/copyMethod tests; refusing to silently skip (cf. #121)"
    );
    eprintln!(
        "skipping populator test: storageclass {storage_class} absent \
         (run `mise run //crates/e2e:snapshot-stack`)"
    );
    false
}

/// Create a CSI PVC on [`CSI_STORAGE_CLASS`] carrying `BACKUP_LABEL_KEY: label_value`,
/// and seed `/data/marker.txt` with `marker` so it binds and carries data unique to it.
///
/// `label_value` is a PARAMETER, not a constant: several scenarios share one cluster and
/// one namespace and none of them deletes its PVCs, so a shared label value would make
/// one scenario's `pvcSelector` match another's volumes — failing on a count assertion
/// that has nothing to do with what it tests, and only in whichever order nextest picked.
/// The distinct MARKER is what proves cross-volume isolation: each restored PVC must come
/// back holding ITS OWN marker, never a sibling's.
pub async fn csi_pvc_with_data(client: &Client, name: &str, label_value: &str, marker: &str) {
    use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    // A seed Pod left `Succeeded` by a previous call makes the next
    // `pods.create` fail `AlreadyExists` behind the `let _ =` below — so a fresh
    // PVC would bind EMPTY and every marker assertion downstream would fail for
    // a reason that has nothing to do with what the test covers. Clear it, and
    // wait: `create` on a still-terminating name is rejected too.
    let seed = format!("{name}-seed");
    let _ = pods.delete(&seed, &DeleteParams::default()).await;
    wait_until(
        &format!("seed pod {seed} is gone"),
        default_timeout(),
        poll_interval(),
        || async { Ok(pods.get_opt(&seed).await?.is_none().then_some(())) },
    )
    .await
    .unwrap_or_else(|e| panic!("a previous seed pod for {name} must clear: {e}"));
    let _ = pvcs
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                "metadata": {
                    "name": name, "namespace": E2E_NAMESPACE,
                    "labels": { BACKUP_LABEL_KEY: label_value },
                },
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
                "metadata": { "name": seed, "namespace": E2E_NAMESPACE },
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "seed", "image": kopiur_e2e::consts::BUSYBOX_IMAGE,
                        "imagePullPolicy": "IfNotPresent",
                        "command": ["sh", "-c", format!("echo {marker} > /data/marker.txt")],
                        "volumeMounts": [{ "name": "d", "mountPath": "/data" }],
                    }],
                    "volumes": [{ "name": "d", "persistentVolumeClaim": { "claimName": name } }],
                },
            })),
        )
        .await;
    wait_until(
        &format!("PVC {name} Bound"),
        default_timeout(),
        poll_interval(),
        || async {
            let bound = pvcs
                .get_opt(name)
                .await?
                .and_then(|p| p.status.and_then(|s| s.phase))
                .as_deref()
                == Some("Bound");
            Ok(bound.then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("PVC {name} should bind: {e}"));
    // Bound is NOT the same as seeded: an `Immediate` class binds the PVC before
    // the seed pod has written anything. Wait for the WRITE, or a backup taken
    // straight after this call can capture an empty volume.
    kopiur_e2e::wait::pod_succeeded(client, E2E_NAMESPACE, &seed)
        .await
        .unwrap_or_else(|e| panic!("the seed pod for {name} must write {marker:?}: {e}"));
}

/// The `Snapshot` CRs a policy produced, by its config label.
pub async fn children_of(client: &Client, policy: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    api.list(
        &kube::api::ListParams::default()
            .labels(&format!("kopiur.home-operations.com/config={policy}")),
    )
    .await
    .expect("list Snapshots")
    .items
}

/// Purge one scenario's schedule, policy and produced `Snapshot` CRs, so a
/// re-run — a nextest retry, or a second call from a sibling test in the same
/// shard — actually RE-RUNS the scenario instead of dying in setup.
///
/// A panicked (or simply a preceding) try leaves three tripwires: the policy
/// (the fresh `create` dies `AlreadyExists`), the schedule (its `runOnCreate`
/// token is consumed, so even an idempotent create fires no new capture), and
/// stale children (which make an "exactly N children" wait unwinnable, and a
/// terminal `Failed` member makes an all-Succeeded wait unwinnable). Deletion
/// order matters — schedule first, so nothing re-produces children — and then it
/// waits for the children to fully go (their finalizers release the kopia-side
/// state through the batched delete path). A fresh cluster is a fast no-op.
pub async fn clear_scenario_leftovers(client: &Client, schedule: &str, policy: &str) {
    let schedules: Api<kopiur_api::SnapshotSchedule> =
        Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = schedules.delete(schedule, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    for child in children_of(client, policy).await {
        if let Some(n) = child.metadata.name {
            let _ = backups.delete(&n, &DeleteParams::default()).await;
        }
    }
    wait_until(
        &format!("leftovers of scenario `{policy}` are gone"),
        default_timeout(),
        poll_interval(),
        || async {
            let gone = schedules.get_opt(schedule).await?.is_none()
                && policies.get_opt(policy).await?.is_none()
                && children_of(client, policy).await.is_empty();
            Ok(gone.then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("previous try's `{policy}` leftovers must clear: {e}"));
}

/// Delete a PVC and its seed Pod and WAIT for both to go, so the next
/// [`csi_pvc_with_data`] starts from nothing rather than adopting a volume whose
/// labels or contents belong to a previous scenario.
pub async fn drop_csi_pvc(client: &Client, name: &str) {
    use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let seed = format!("{name}-seed");
    let _ = pods.delete(&seed, &DeleteParams::default()).await;
    let _ = pvcs.delete(name, &DeleteParams::default()).await;
    wait_until(
        &format!("PVC {name} and its seed pod are gone"),
        default_timeout(),
        poll_interval(),
        || async {
            let gone = pvcs.get_opt(name).await?.is_none() && pods.get_opt(&seed).await?.is_none();
            Ok(gone.then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("PVC {name} must be deleted before it is re-seeded: {e}"));
}

/// A populator `Restore` reading `seed` out of `repo` (`target.populator: {}`).
pub fn populator_restore_json(name: &str, repo: &str, seed: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": repo },
            "source": { "snapshotRef": { "name": seed } },
            "target": { "populator": {} }
        }
    })
}

/// A PVC whose `dataSourceRef` claims the populator `Restore` `restore`, so a
/// populator-aware provisioner defers to the handshake instead of binding it to an
/// empty volume.
pub fn claiming_pvc_json(name: &str, storage_class: &str, restore: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": storage_class,
            "resources": { "requests": { "storage": "1Gi" } },
            "dataSourceRef": {
                "apiGroup": "kopiur.home-operations.com",
                "kind": "Restore",
                "name": restore,
            }
        }
    })
}

/// One claiming PVC to drive, and what its reader pod must find inside it.
pub struct ClaimSpec<'a> {
    /// PVC name.
    pub name: &'a str,
    /// Path inside the restored volume the reader reads (relative to the mount).
    pub path: &'a str,
    /// The exact content that path must hold. For a fan-out this is the claim's
    /// OWN marker — reading a sibling's marker is the cross-volume bug.
    pub expect: &'a str,
}

/// The objects a [`populate_claims`] run left behind, so a caller can keep inspecting
/// them (and clean up when done) instead of tearing them down immediately.
pub struct PopulatedClaims {
    pub restore: String,
    pub claims: Vec<String>,
    pub readers: Vec<String>,
}

impl PopulatedClaims {
    pub async fn cleanup(&self, client: &Client) {
        use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
        use kopiur_api::Restore;
        let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
        let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
        for reader in &self.readers {
            let _ = pods.delete(reader, &DeleteParams::default()).await;
        }
        for claim in &self.claims {
            let _ = pvcs.delete(claim, &DeleteParams::default()).await;
        }
        let _ = restores
            .delete(&self.restore, &DeleteParams::default())
            .await;
    }
}

/// Drive ONE populator `Restore` over N claiming PVCs to completion and LEAVE the
/// objects in place (the caller owns cleanup).
///
/// Each claim gets a reader pod doing double duty: scheduling it produces the
/// `selected-node` a `WaitForFirstConsumer` claim needs (the controller pins that
/// claim's prime PVC to it), and it asserts the restored bytes. A reader can only run
/// once ITS claim binds, so N successes prove N prime→consumer rebinds AND that each
/// volume carries its own data.
///
/// Generalized from the single-claim helper by #443: before the fan-out a populator
/// filled exactly one claimant, so one claim was all the harness could express — which
/// is precisely why the bug shipped.
pub async fn populate_claims(
    client: &Client,
    storage_class: &str,
    restore_name: &str,
    restore: serde_json::Value,
    claims: &[ClaimSpec<'_>],
) -> PopulatedClaims {
    use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
    use kopiur_api::Restore;
    use kopiur_e2e::{builders, wait};

    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    restores
        .create(&PostParams::default(), &cr(restore))
        .await
        .expect("create populator Restore");

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let mut readers = Vec::new();
    for claim in claims {
        pvcs.create(
            &PostParams::default(),
            &cr(claiming_pvc_json(claim.name, storage_class, restore_name)),
        )
        .await
        .unwrap_or_else(|e| panic!("create claiming PVC {}: {e}", claim.name));

        let reader = format!("{}-reader", claim.name);
        let script = format!("test \"$(cat /mnt/{})\" = '{}'", claim.path, claim.expect);
        pods.create(
            &PostParams::default(),
            &builders::one_shot_pod(
                E2E_NAMESPACE,
                &reader,
                &["sh", "-c", &script],
                &[(claim.name, "/mnt")],
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("create reader pod for {}: {e}", claim.name));
        readers.push(reader);
    }

    for (claim, reader) in claims.iter().zip(&readers) {
        wait::pod_succeeded(client, E2E_NAMESPACE, reader)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "claiming PVC {} must bind carrying its OWN data ({} = {}): {e}",
                    claim.name, claim.path, claim.expect
                )
            });
    }
    wait_phase(&restores, restore_name, "Completed")
        .await
        .expect("the populator Restore reaches Completed once every claim is bound");

    PopulatedClaims {
        restore: restore_name.to_string(),
        claims: claims.iter().map(|c| c.name.to_string()).collect(),
        readers,
    }
}
