use super::*;

const MIN_AGE: i64 = 0;
const OLD: Option<i64> = Some(24 * 60 * 60);

fn claim(labels: serde_json::Value, annotations: serde_json::Value) -> PersistentVolumeClaim {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": "kopiur-cache-pg-94c57734fbfe76da", "namespace": "kopiur-system",
            "labels": labels, "annotations": annotations,
        },
    }))
    .expect("PVC")
}

fn policy(spec: serde_json::Value) -> SnapshotPolicy {
    serde_json::from_value(serde_json::json!({
        "apiVersion": crate::consts::API_VERSION, "kind": "SnapshotPolicy",
        "metadata": { "name": "pg", "namespace": "billing", "uid": "uid-pg" },
        "spec": spec,
    }))
    .expect("SnapshotPolicy")
}

/// A single-repository policy with a sized persistent cache.
fn persistent_policy() -> SnapshotPolicy {
    policy(serde_json::json!({
        "repository": { "kind": "ClusterRepository", "name": "shared" },
        "sources": [ { "pvc": { "name": "data" } } ],
        "mover": { "cache": { "capacity": "5Gi", "mode": "Persistent" } },
    }))
}

fn cluster_repo(spec: serde_json::Value) -> ClusterRepository {
    let mut base = serde_json::json!({
        "backend": { "s3": { "bucket": "b" } },
        "encryption": { "passwordSecretRef": { "name": "creds", "namespace": "kopiur-system" } },
        "allowedNamespaces": { "all": true },
    });
    for (k, v) in spec.as_object().expect("object") {
        base[k] = v.clone();
    }
    serde_json::from_value(serde_json::json!({
        "apiVersion": crate::consts::API_VERSION, "kind": "ClusterRepository",
        "metadata": { "name": "shared" },
        "spec": base,
    }))
    .expect("ClusterRepository")
}

fn central_repo() -> ClusterRepository {
    cluster_repo(serde_json::json!({ "execution": { "mode": "Central" } }))
}

#[test]
fn the_owner_is_read_off_the_claims_marks() {
    let policys = claim(
        serde_json::json!({ CENTRAL_POLICY_UID_LABEL: "uid-pg" }),
        serde_json::json!({ CENTRAL_POLICY_ANNOTATION: "billing/pg" }),
    );
    assert_eq!(
        claim_owner(&policys),
        Some(ClaimOwner {
            kind: ClaimOwnerKind::SnapshotPolicy,
            namespace: "billing".into(),
            name: "pg".into(),
            uid: "uid-pg".into(),
        })
    );
    let restores = claim(
        serde_json::json!({ CENTRAL_RESTORE_UID_LABEL: "uid-r" }),
        serde_json::json!({ CENTRAL_RESTORE_ANNOTATION: "billing/pg-restore" }),
    );
    assert_eq!(
        claim_owner(&restores).map(|o| (o.kind, o.name)),
        Some((ClaimOwnerKind::Restore, "pg-restore".to_string()))
    );
    // An owner-referenced cache of a ClusterRepository lives in the operator
    // namespace too. It is not a central claim, and the sweep never touches it.
    assert_eq!(
        claim_owner(&claim(serde_json::json!({}), serde_json::json!({}))),
        None
    );
    // Incomplete marks: never guess an owner.
    assert_eq!(
        claim_owner(&claim(
            serde_json::json!({ CENTRAL_POLICY_UID_LABEL: "uid-pg" }),
            serde_json::json!({})
        )),
        None
    );
    assert_eq!(
        claim_owner(&claim(
            serde_json::json!({ CENTRAL_POLICY_UID_LABEL: "uid-pg" }),
            serde_json::json!({ CENTRAL_POLICY_ANNOTATION: "no-slash" })
        )),
        None
    );
}

/// The whole decision table. A claim is deleted only on positive evidence that
/// nothing will mount it again.
#[test]
fn a_claim_is_deleted_only_on_positive_evidence() {
    use OwnerState::*;
    for (state, delete) in [
        (Gone, true),
        (Terminating, false),
        (LiveRestore, false),
        (LivePolicy(PolicyUse::InUse), false),
        (LivePolicy(PolicyUse::Unknown), false),
        (LivePolicy(PolicyUse::RepositoryDropped), true),
        (LivePolicy(PolicyUse::NotCentral), true),
        (LivePolicy(PolicyUse::NotPersistent), true),
        (LivePolicy(PolicyUse::SourceNotSelected), true),
    ] {
        let verdict = claim_verdict(OLD, MIN_AGE, state);
        assert_eq!(
            matches!(verdict, Verdict::Delete(_)),
            delete,
            "{state:?} -> {verdict:?}"
        );
    }
}

/// A run creates its claim before a cached view has caught up with the spec
/// edit that made the run want it. A young claim is never judged, whatever the
/// configured min-age.
#[test]
fn a_young_claim_is_never_deleted() {
    let young = Some(CACHE_MIN_AGE_FLOOR_SECS - 1);
    assert_eq!(claim_verdict(young, 0, OwnerState::Gone), Verdict::Keep);
    assert_eq!(claim_verdict(None, 0, OwnerState::Gone), Verdict::Keep);
    assert!(matches!(
        claim_verdict(Some(CACHE_MIN_AGE_FLOOR_SECS), 0, OwnerState::Gone),
        Verdict::Delete(_)
    ));
    // A larger configured min-age wins over the floor.
    assert_eq!(
        claim_verdict(
            Some(CACHE_MIN_AGE_FLOOR_SECS),
            2 * CACHE_MIN_AGE_FLOOR_SECS,
            OwnerState::Gone
        ),
        Verdict::Keep
    );
}

#[test]
fn a_policy_uses_its_claim_while_the_repository_is_central_and_the_cache_persistent() {
    let central = central_repo();
    assert_eq!(
        policy_use(&persistent_policy(), Some("shared"), Some(&central), None),
        PolicyUse::InUse
    );
    // The repository's moverDefaults supply the cache.
    let from_defaults = cluster_repo(serde_json::json!({
        "execution": { "mode": "Central" },
        "moverDefaults": { "cache": { "capacity": "5Gi", "mode": "Persistent" } },
    }));
    let bare = policy(serde_json::json!({
        "repository": { "kind": "ClusterRepository", "name": "shared" },
        "sources": [ { "pvc": { "name": "data" } } ],
    }));
    assert_eq!(
        policy_use(&bare, Some("shared"), Some(&from_defaults), None),
        PolicyUse::InUse
    );
    // A per-source claim whose source PVC exists.
    assert_eq!(
        policy_use(
            &persistent_policy(),
            Some("shared"),
            Some(&central),
            Some(true)
        ),
        PolicyUse::InUse
    );
}

#[test]
fn a_policy_stops_using_its_claim_for_each_of_four_reasons() {
    let central = central_repo();
    // 1. The policy points at another repository now.
    assert_eq!(
        policy_use(&persistent_policy(), Some("old-repo"), Some(&central), None),
        PolicyUse::RepositoryDropped
    );
    // A namespaced Repository of the same name is not the ClusterRepository.
    let namespaced = policy(serde_json::json!({
        "repository": { "kind": "Repository", "name": "shared" },
        "sources": [ { "pvc": { "name": "data" } } ],
        "mover": { "cache": { "capacity": "5Gi", "mode": "Persistent" } },
    }));
    assert_eq!(
        policy_use(&namespaced, Some("shared"), Some(&central), None),
        PolicyUse::RepositoryDropped
    );
    // 2. The repository runs movers in the workload namespace again.
    for spec in [
        serde_json::json!({}),
        serde_json::json!({ "execution": { "mode": "InNamespace" } }),
    ] {
        assert_eq!(
            policy_use(
                &persistent_policy(),
                Some("shared"),
                Some(&cluster_repo(spec)),
                None
            ),
            PolicyUse::NotCentral
        );
    }
    // 3. The cache is no longer a sized persistent one.
    for mover in [
        serde_json::json!({ "cache": { "capacity": "5Gi", "mode": "Ephemeral" } }),
        serde_json::json!({ "cache": { "capacity": "5Gi" } }),
        serde_json::json!({ "cache": { "mode": "Persistent" } }),
        serde_json::json!({}),
    ] {
        let changed = policy(serde_json::json!({
            "repository": { "kind": "ClusterRepository", "name": "shared" },
            "sources": [ { "pvc": { "name": "data" } } ],
            "mover": mover,
        }));
        assert_eq!(
            policy_use(&changed, Some("shared"), Some(&central), None),
            PolicyUse::NotPersistent,
            "{:?}",
            changed.spec.mover
        );
    }
    // 4. The source PVC of a per-source claim is gone, or not selected any more.
    assert_eq!(
        policy_use(
            &persistent_policy(),
            Some("shared"),
            Some(&central),
            Some(false)
        ),
        PolicyUse::SourceNotSelected
    );
}

/// What cannot be read is never taken as "unused".
#[test]
fn an_unreadable_claim_or_repository_is_unknown() {
    let central = central_repo();
    assert_eq!(
        policy_use(&persistent_policy(), None, Some(&central), None),
        PolicyUse::Unknown
    );
    // The repository does not exist right now (deleted and about to be
    // re-created by GitOps, say). Its cache is still valid when it returns.
    assert_eq!(
        policy_use(&persistent_policy(), Some("shared"), None, None),
        PolicyUse::Unknown
    );
}

#[test]
fn the_in_namespace_predecessor_is_the_claim_the_policy_used_beside_itself() {
    assert_eq!(
        in_namespace_predecessor(&persistent_policy(), "shared").as_deref(),
        Some("kopiur-cache-pg")
    );
    assert_eq!(
        in_namespace_predecessor(&persistent_policy(), "other"),
        None
    );
    // A multi-repository policy had one claim for each repository.
    let multi = policy(serde_json::json!({
        "repositories": [
            { "kind": "ClusterRepository", "name": "shared" },
            { "kind": "ClusterRepository", "name": "offsite" },
        ],
        "sources": [ { "pvc": { "name": "data" } } ],
    }));
    let shared = in_namespace_predecessor(&multi, "shared").expect("a name");
    assert_eq!(
        shared,
        kopiur_api::expand::cache_pvc_name(
            "pg",
            "billing",
            Some(&kopiur_api::common::RepositoryRef {
                kind: RepositoryKind::ClusterRepository,
                name: "shared".into(),
                namespace: None,
            })
        )
    );
    assert_ne!(Some(shared), in_namespace_predecessor(&multi, "offsite"));
}

/// The predecessor is deleted by name, and a cache reset deletes every claim
/// this selects, so it must select this policy's own cache and nothing else:
/// never a source PVC, a staged PVC, or another owner's cache.
#[test]
fn only_this_policys_owner_referenced_cache_is_a_predecessor() {
    let pvc = |labels: serde_json::Value, kind: &str, uid: &str| {
        serde_json::from_value::<PersistentVolumeClaim>(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": {
                "name": "kopiur-cache-pg", "labels": labels,
                "ownerReferences": [{
                    "apiVersion": crate::consts::API_VERSION, "kind": kind,
                    "name": "pg", "uid": uid,
                }],
            },
        }))
        .expect("PVC")
    };
    let cache = serde_json::json!({ CACHE_PVC_COMPONENT_LABEL: CACHE_PVC_COMPONENT });
    assert!(is_in_namespace_cache_of(
        &pvc(cache.clone(), "SnapshotPolicy", "uid-pg"),
        "uid-pg"
    ));
    // A user's own PVC that happens to carry the name, another policy's cache,
    // and a Maintenance cache (`kopiur-cache-<maintenance>`).
    assert!(!is_in_namespace_cache_of(
        &pvc(serde_json::json!({}), "SnapshotPolicy", "uid-pg"),
        "uid-pg"
    ));
    assert!(!is_in_namespace_cache_of(
        &pvc(cache.clone(), "SnapshotPolicy", "uid-other"),
        "uid-pg"
    ));
    assert!(!is_in_namespace_cache_of(
        &pvc(cache, "Maintenance", "uid-pg"),
        "uid-pg"
    ));
}

// --- one pass against a mock apiserver ----------------------------------------

/// What the mock apiserver of [`one_pass`] holds.
#[derive(Clone, Default)]
struct World {
    /// The cache claims of the operator namespace (`kopiur-system`).
    central: Vec<serde_json::Value>,
    /// The cache claims of every namespace (the drain's cluster-wide LIST).
    everywhere: Vec<serde_json::Value>,
    /// The claim `billing/kopiur-cache-pg`, when a GET by name finds it.
    predecessor: Option<serde_json::Value>,
    /// The kopiur-managed Jobs of every namespace.
    jobs: Vec<serde_json::Value>,
}

const LONG_AGO: &str = "2020-01-01T00:00:00Z";

/// A central cache claim of the policy `billing/<policy>`, created long ago.
fn central_claim(name: &str, pvc_uid: &str, policy: &str, policy_uid: &str) -> serde_json::Value {
    serde_json::json!({
        "metadata": {
            "name": name, "namespace": "kopiur-system", "uid": pvc_uid,
            "creationTimestamp": LONG_AGO,
            "labels": {
                CACHE_PVC_COMPONENT_LABEL: CACHE_PVC_COMPONENT,
                CENTRAL_POLICY_UID_LABEL: policy_uid,
            },
            "annotations": {
                CENTRAL_POLICY_ANNOTATION: format!("billing/{policy}"),
                CACHE_REPOSITORY_ANNOTATION: "shared",
            },
        },
    })
}

/// The claim the policy `billing/pg` used beside itself before its repository
/// ran centrally.
fn predecessor_claim() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": "kopiur-cache-pg", "namespace": "billing", "uid": "pvc-predecessor",
            "labels": { CACHE_PVC_COMPONENT_LABEL: CACHE_PVC_COMPONENT },
            "ownerReferences": [{
                "apiVersion": crate::consts::API_VERSION,
                "kind": "SnapshotPolicy", "name": "pg", "uid": "uid-pg",
            }],
        },
    })
}

fn retired(mut claim: serde_json::Value, at: &str) -> serde_json::Value {
    claim["metadata"]["annotations"][crate::io::CACHE_RETIRED_ANNOTATION] = at.into();
    claim
}

/// A kopiur mover Job in `namespace` whose pod mounts `claim`.
fn job_mounting(namespace: &str, claim: &str, conditions: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": { "name": "mover", "namespace": namespace },
        "spec": { "template": { "spec": {
            "containers": [{ "name": "mover", "image": "mover" }],
            "volumes": [
                { "name": "kopia-state", "emptyDir": {} },
                { "name": "kopia-cache", "persistentVolumeClaim": { "claimName": claim } },
            ],
        } } },
        "status": { "conditions": conditions },
    })
}

/// One pass over `world`. Returns the outcome and every request as
/// `METHOD path body`.
async fn one_pass(world: World) -> (CacheSweepOutcome, Vec<String>) {
    use http::{Request, Response, StatusCode};
    use kube::client::Body;
    use std::sync::{Arc, Mutex};

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let svc = {
        let log = log.clone();
        tower::service_fn(move |req: Request<Body>| {
            let (log, world) = (log.clone(), world.clone());
            async move {
                let method = req.method().as_str().to_string();
                let path = req.uri().path().to_string();
                let bytes = http_body_util::BodyExt::collect(req.into_body())
                    .await
                    .expect("collect request body")
                    .to_bytes();
                log.lock().unwrap().push(format!(
                    "{method} {path} {}",
                    String::from_utf8_lossy(&bytes)
                ));
                let ok = |v: serde_json::Value| (StatusCode::OK, v);
                let list = |kind: &str, items: Vec<serde_json::Value>| {
                    ok(serde_json::json!({
                        "apiVersion": "v1", "kind": kind, "metadata": {}, "items": items,
                    }))
                };
                let not_found = (
                    StatusCode::NOT_FOUND,
                    serde_json::json!({
                        "kind": "Status", "apiVersion": "v1", "status": "Failure",
                        "reason": "NotFound", "code": 404,
                    }),
                );
                let (status, json) = match (method.as_str(), path.as_str()) {
                    ("GET", "/api/v1/namespaces/kopiur-system/persistentvolumeclaims") => {
                        list("PersistentVolumeClaimList", world.central)
                    }
                    ("GET", "/api/v1/persistentvolumeclaims") => {
                        list("PersistentVolumeClaimList", world.everywhere)
                    }
                    ("GET", p) if p.ends_with("/namespaces/billing/snapshotpolicies/pg") => {
                        ok(serde_json::to_value(persistent_policy()).unwrap())
                    }
                    ("GET", p) if p.ends_with("/clusterrepositories/shared") => {
                        ok(serde_json::to_value(central_repo()).unwrap())
                    }
                    (
                        "GET",
                        "/api/v1/namespaces/billing/persistentvolumeclaims/kopiur-cache-pg",
                    ) => world.predecessor.map_or(not_found, ok),
                    ("GET", p) if p.starts_with("/apis/batch/v1/namespaces/") => {
                        let namespace = p.split('/').nth(5).unwrap_or_default().to_string();
                        let jobs = world
                            .jobs
                            .into_iter()
                            .filter(|j| j["metadata"]["namespace"] == namespace.as_str())
                            .collect();
                        list("JobList", jobs)
                    }
                    ("DELETE" | "PATCH", _) => ok(serde_json::json!({
                        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                        "metadata": { "name": "written" },
                    })),
                    // The policy `gone`, and anything else.
                    _ => not_found,
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&json).unwrap()))
                        .unwrap(),
                )
            }
        })
    };
    let client = kube::Client::new(svc, "kopiur-system");
    let outcome = run_cache_sweep(&client, "kopiur-system", 0)
        .await
        .expect("the pass");
    let log = log.lock().unwrap().clone();
    (outcome, log)
}

fn with_method<'a>(log: &'a [String], method: &str) -> Vec<&'a String> {
    log.iter().filter(|l| l.starts_with(method)).collect()
}

const PREDECESSOR: &str = "/namespaces/billing/persistentvolumeclaims/kopiur-cache-pg ";

/// The first pass that finds an unused claim of a LIVE policy only retires it.
/// Deleting it at once can take it from under a mover that Kubernetes does not
/// protect yet: a run that picked the claim and has not created its Job, or a
/// Job whose pod is not scheduled. The claim of an owner that is gone has no
/// mover left, and is deleted at once, by UID.
#[tokio::test]
async fn the_first_pass_retires_a_predecessor_and_deletes_only_the_orphan() {
    let (outcome, log) = one_pass(World {
        central: vec![
            central_claim("kopiur-cache-gone-1", "pvc-orphan", "gone", "uid-gone"),
            central_claim("kopiur-cache-pg-2", "pvc-in-use", "pg", "uid-pg"),
        ],
        predecessor: Some(predecessor_claim()),
        ..World::default()
    })
    .await;
    assert_eq!(
        outcome,
        CacheSweepOutcome {
            central: 1,
            predecessors: 0,
            retired: 0
        }
    );
    let deletes = with_method(&log, "DELETE");
    assert_eq!(deletes.len(), 1, "{log:?}");
    assert!(
        deletes[0].contains("/namespaces/kopiur-system/persistentvolumeclaims/kopiur-cache-gone-1")
            && deletes[0].contains(r#""preconditions":{"uid":"pvc-orphan"}"#),
        "the orphan is deleted, by UID: {}",
        deletes[0]
    );
    let patches = with_method(&log, "PATCH");
    assert_eq!(patches.len(), 1, "{log:?}");
    assert!(
        patches[0].contains(PREDECESSOR)
            && patches[0].contains(crate::io::CACHE_RETIRED_ANNOTATION)
            && patches[0].contains(r#""uid":"pvc-predecessor""#),
        "the predecessor is retired, and the patch names the claim that was read: {}",
        patches[0]
    );
}

/// A later pass deletes a retired claim, wherever it is and whatever retired
/// it: the predecessor of a policy, or the claims of a cache reset. This holds
/// when the repository has left `Central` again in between, which is why the
/// drain does not depend on a central claim that is in use.
#[tokio::test]
async fn a_later_pass_deletes_a_retired_claim_that_no_job_mounts() {
    let (outcome, log) = one_pass(World {
        everywhere: vec![retired(predecessor_claim(), LONG_AGO)],
        ..World::default()
    })
    .await;
    assert_eq!(outcome.retired, 1, "{log:?}");
    let position = |method: &str, needle: &str| {
        log.iter()
            .position(|l| l.starts_with(method) && l.contains(needle))
            .unwrap_or_else(|| panic!("no {method} {needle} in {log:?}"))
    };
    assert!(
        position("GET", "/apis/batch/v1/namespaces/billing/jobs") < position("DELETE", PREDECESSOR),
        "the Jobs are read before the claim is deleted: {log:?}"
    );
    assert!(
        with_method(&log, "PATCH").is_empty(),
        "a retired claim is not marked again: {log:?}"
    );
}

/// A Job with retries left starts a new pod after an attempt fails, when its
/// `Snapshot` already reads `Failed`. A retired claim stays while such a Job
/// mounts it, and while its grace period runs.
#[tokio::test]
async fn a_retired_claim_stays_while_a_job_mounts_it_or_its_grace_period_runs() {
    let (outcome, log) = one_pass(World {
        everywhere: vec![retired(predecessor_claim(), LONG_AGO)],
        jobs: vec![job_mounting(
            "billing",
            "kopiur-cache-pg",
            serde_json::json!([]),
        )],
        ..World::default()
    })
    .await;
    assert_eq!(outcome.retired, 0);
    assert!(with_method(&log, "DELETE").is_empty(), "{log:?}");

    let just_now = chrono::Utc::now().to_rfc3339();
    let (outcome, log) = one_pass(World {
        everywhere: vec![retired(predecessor_claim(), &just_now)],
        ..World::default()
    })
    .await;
    assert_eq!(outcome.retired, 0);
    assert!(with_method(&log, "DELETE").is_empty(), "{log:?}");
    assert!(
        !log.iter().any(|l| l.contains("/jobs")),
        "the Jobs are only read after the grace period: {log:?}"
    );
}

/// A central claim that an earlier pass or a reset retired is deleted on a
/// later pass, even when its policy uses a persistent cache again by then:
/// nothing mounts a retired claim, so keeping it would keep the runs cold.
#[tokio::test]
async fn a_retired_central_claim_is_deleted_even_when_its_policy_uses_it_again() {
    let mut claim = retired(
        central_claim("kopiur-cache-pg-2", "pvc-retired", "pg", "uid-pg"),
        LONG_AGO,
    );
    // Younger than the min-age floor: the mark decides, not the age.
    claim["metadata"]["creationTimestamp"] = chrono::Utc::now().to_rfc3339().into();
    let (outcome, log) = one_pass(World {
        central: vec![claim],
        ..World::default()
    })
    .await;
    assert_eq!(outcome.central, 1, "{log:?}");
    assert!(
        log.iter().any(|l| l.starts_with(
            "DELETE /api/v1/namespaces/kopiur-system/persistentvolumeclaims/kopiur-cache-pg-2 "
        )),
        "{log:?}"
    );
}

/// Which Jobs keep a retired claim: only one that can still start a pod on it.
#[test]
fn only_an_unfinished_job_that_mounts_the_claim_keeps_it() {
    let job = |claim: &str, conditions: serde_json::Value| -> k8s_openapi::api::batch::v1::Job {
        serde_json::from_value(job_mounting("billing", claim, conditions)).expect("Job")
    };
    let done = |kind: &str| serde_json::json!([{ "type": kind, "status": "True" }]);
    // Running, or between two attempts.
    assert!(claim_has_a_job(&[job("c", serde_json::json!([]))], "c"));
    assert!(claim_has_a_job(
        &[job(
            "c",
            serde_json::json!([{ "type": "Failed", "status": "False" }])
        )],
        "c"
    ));
    // Finished either way: it starts no more pods.
    assert!(!claim_has_a_job(&[job("c", done("Complete"))], "c"));
    assert!(!claim_has_a_job(&[job("c", done("Failed"))], "c"));
    // A Job of another claim, and no Jobs at all.
    assert!(!claim_has_a_job(
        &[job("other", serde_json::json!([]))],
        "c"
    ));
    assert!(!claim_has_a_job(&[], "c"));
    // Being deleted: its pods go with it, and no new one starts.
    let mut deleting = job("c", serde_json::json!([]));
    deleting.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        ));
    assert!(!claim_has_a_job(&[deleting], "c"));
}

/// A run reads its claim and creates its Job a few API calls later. The grace
/// period covers that gap, so the Job check sees every run that read the claim
/// before it was retired.
#[test]
fn a_retired_claim_is_kept_for_the_grace_period() {
    assert!(!grace_elapsed(1_000, 1_000));
    assert!(!grace_elapsed(1_000, 1_000 + RETIRE_GRACE_SECS - 1));
    assert!(grace_elapsed(1_000, 1_000 + RETIRE_GRACE_SECS));
    let meta = |annotations: serde_json::Value| -> kube::core::ObjectMeta {
        serde_json::from_value(serde_json::json!({ "annotations": annotations })).expect("meta")
    };
    assert_eq!(
        retired_at(&meta(serde_json::json!({
            crate::io::CACHE_RETIRED_ANNOTATION: "1970-01-01T00:16:40Z"
        }))),
        Some(1_000)
    );
    assert_eq!(retired_at(&meta(serde_json::json!({}))), None);
    // A mark that is not a time is not a retirement time. The claim is still
    // retired (`io::is_retired`), and `retire_claim` stamps it again.
    assert_eq!(
        retired_at(&meta(serde_json::json!({
            crate::io::CACHE_RETIRED_ANNOTATION: "yes"
        }))),
        None
    );
}

/// A PVC that still exists but that no `pvcSelector` of the policy matches any
/// more gets no further runs, so its claim has no further use.
#[test]
fn a_source_counts_only_while_a_selector_of_the_policy_matches_it() {
    let source = |labels: serde_json::Value| -> PersistentVolumeClaim {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": "data-a", "namespace": "billing", "labels": labels },
        }))
        .expect("PVC")
    };
    let selecting = |selector: serde_json::Value| {
        policy(serde_json::json!({
            "repository": { "kind": "ClusterRepository", "name": "shared" },
            "sources": [ { "pvcSelector": selector } ],
        }))
    };
    let by_label = selecting(serde_json::json!({
        "labelSelector": { "matchLabels": { "backup": "yes" } }
    }));
    assert!(policy_selects(
        &by_label,
        &source(serde_json::json!({ "backup": "yes" }))
    ));
    // The label was removed from the PVC, or the selector was changed.
    assert!(!policy_selects(&by_label, &source(serde_json::json!({}))));
    assert!(!policy_selects(
        &by_label,
        &source(serde_json::json!({ "backup": "no" }))
    ));
    // A selector with no labelSelector takes every PVC of the namespace.
    assert!(policy_selects(
        &selecting(serde_json::json!({})),
        &source(serde_json::json!({}))
    ));
    // The policy now names one PVC and has no selector at all.
    assert!(!policy_selects(
        &persistent_policy(),
        &source(serde_json::json!({ "backup": "yes" }))
    ));
}
