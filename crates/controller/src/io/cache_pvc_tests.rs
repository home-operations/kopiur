//! #613: changing `mover.cache.capacity` on a `Persistent` cache used to leave the
//! existing PVC at its old size. [`ensure_cache_pvc`] now grows it (grow-only), and
//! refuses — visibly — when the StorageClass cannot expand or a shrink is asked
//! for. Driven against a mock apiserver that logs every request, so the tests
//! assert exactly which writes happen (and which never do).

use std::sync::{Arc, Mutex};

use http::{Request, Response, StatusCode};
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::Client;
use kube::client::Body;

use super::{
    CENTRAL_POLICY_UID_LABEL, CachePvcClaim, CachePvcOwner, CachePvcResize, EnsuredCachePvc,
    ResizeDirection, effective_cache_capacity, ensure_cache_pvc, resize_direction,
    storage_class_allows_expansion,
};

/// An owner in the claim's own namespace.
fn in_namespace() -> CachePvcOwner {
    CachePvcOwner::Reference(Default::default())
}

/// A `SnapshotPolicy` of another namespace, under central execution.
fn central_policy(uid: &str) -> CachePvcOwner {
    CachePvcOwner::CentralPolicy {
        namespace: "billing".into(),
        name: "pg".into(),
        uid: uid.into(),
    }
}

fn ensured(claim: CachePvcClaim) -> EnsuredCachePvc {
    match claim {
        CachePvcClaim::Ensured(e) => e,
        other @ (CachePvcClaim::Foreign | CachePvcClaim::Orphaned) => {
            panic!("expected the owner's claim, got {other:?}")
        }
    }
}

/// How the mock apiserver answers the StorageClass GET.
#[derive(Clone, Copy)]
enum Sc {
    Allows,
    Forbids,
    Missing,
    Unreadable,
}

fn pvc_json(size: &str, bound: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": { "name": "kopiur-cache-pg", "namespace": "ns" },
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": "fast",
            "resources": { "requests": { "storage": size } },
        },
    });
    if let Some(b) = bound {
        v["status"] = serde_json::json!({ "phase": "Bound", "capacity": { "storage": b } });
    }
    v
}

fn status_body(code: u16) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status", "apiVersion": "v1", "status": "Failure",
        "message": "only dynamically provisioned pvc can be resized",
        "reason": if code == 404 { "NotFound" } else { "Forbidden" }, "code": code,
    })
}

fn storage_class(allow: bool) -> serde_json::Value {
    let mut v = serde_json::json!({
        "apiVersion": "storage.k8s.io/v1", "kind": "StorageClass",
        "metadata": { "name": "fast" }, "provisioner": "x",
    });
    if allow {
        v["allowVolumeExpansion"] = true.into();
    }
    v
}

/// The mock's answer to one request.
fn answer(
    method: &str,
    path: &str,
    body: &[u8],
    current: &str,
    sc: Sc,
    patch_refused: bool,
) -> (StatusCode, serde_json::Value) {
    match method {
        "GET" if path.ends_with("/persistentvolumeclaims/kopiur-cache-pg") => {
            (StatusCode::OK, pvc_json(current, Some(current)))
        }
        "GET" if path.ends_with("/storageclasses/fast") => match sc {
            Sc::Allows => (StatusCode::OK, storage_class(true)),
            Sc::Forbids => (StatusCode::OK, storage_class(false)),
            Sc::Missing => (StatusCode::NOT_FOUND, status_body(404)),
            Sc::Unreadable => (StatusCode::FORBIDDEN, status_body(403)),
        },
        "PATCH" if patch_refused => (StatusCode::FORBIDDEN, status_body(403)),
        "PATCH" => {
            let patch: serde_json::Value = serde_json::from_slice(body).expect("patch json");
            let to = patch["spec"]["resources"]["requests"]["storage"]
                .as_str()
                .expect("storage in patch")
                .to_string();
            // The claim's request grows; its bound capacity lags until the resize
            // completes.
            (StatusCode::OK, pvc_json(&to, Some(current)))
        }
        _ => (StatusCode::NOT_FOUND, status_body(404)),
    }
}

/// A mock apiserver: the cache PVC exists at `current` (bound at that size); the
/// StorageClass answers per `sc`; a PATCH echoes the new size, or is refused with
/// 403 when `patch_refused`. Every request is logged as `METHOD path body`.
fn apiserver(
    current: &'static str,
    sc: Sc,
    patch_refused: bool,
    log: Arc<Mutex<Vec<String>>>,
) -> Client {
    let svc = tower::service_fn(move |req: Request<Body>| {
        let log = log.clone();
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
            let (status, json) = answer(&method, &path, &bytes, current, sc, patch_refused);
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&json).unwrap()))
                    .unwrap(),
            )
        }
    });
    Client::new(svc, "ns")
}

async fn ensure(
    current: &'static str,
    requested: &str,
    sc: Sc,
    patch_refused: bool,
) -> (EnsuredCachePvc, Vec<String>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let client = apiserver(current, sc, patch_refused, log.clone());
    let out = ensure_cache_pvc(
        &client,
        "ns",
        "kopiur-cache-pg",
        &in_namespace(),
        requested,
        Some("fast"),
    )
    .await
    .expect("ensure_cache_pvc");
    let log = log.lock().unwrap().clone();
    (ensured(out), log)
}

fn patches(log: &[String]) -> Vec<&String> {
    log.iter().filter(|l| l.starts_with("PATCH")).collect()
}

#[tokio::test]
async fn a_larger_capacity_grows_the_claim_when_the_class_allows_expansion() {
    let (out, log) = ensure("1Gi", "2Gi", Sc::Allows, false).await;
    assert_eq!(
        out.resize,
        CachePvcResize::Expanded {
            from: "1Gi".into(),
            to: "2Gi".into()
        }
    );
    let p = patches(&log);
    assert_eq!(p.len(), 1, "exactly one PATCH: {log:?}");
    assert!(
        p[0].contains(r#""storage":"2Gi""#),
        "the PATCH must set spec.resources.requests.storage: {}",
        p[0]
    );
    // Budgets fit what is BOUND now (the resize is still pending), not the new
    // request — a pending resize can never let kopia outgrow the volume.
    assert_eq!(out.capacity, "1Gi");
}

#[tokio::test]
async fn a_class_without_expansion_is_refused_without_patching() {
    let (out, log) = ensure("1Gi", "2Gi", Sc::Forbids, false).await;
    match &out.resize {
        CachePvcResize::ExpansionNotAllowed {
            current,
            requested,
            reason,
        } => {
            assert_eq!((current.as_str(), requested.as_str()), ("1Gi", "2Gi"));
            assert!(reason.contains("allowVolumeExpansion"), "{reason}");
        }
        other => panic!("expected ExpansionNotAllowed, got {other:?}"),
    }
    assert!(patches(&log).is_empty(), "must not PATCH: {log:?}");
    assert_eq!(out.capacity, "1Gi");
}

#[tokio::test]
async fn a_missing_class_is_refused_without_patching() {
    let (out, log) = ensure("1Gi", "2Gi", Sc::Missing, false).await;
    assert!(
        matches!(&out.resize, CachePvcResize::ExpansionNotAllowed { reason, .. } if reason.contains("no longer exists")),
        "{:?}",
        out.resize
    );
    assert!(patches(&log).is_empty());
}

#[tokio::test]
async fn an_unreadable_class_lets_the_apiserver_decide() {
    // A namespace-scoped install cannot GET StorageClasses: try the patch.
    let (out, log) = ensure("1Gi", "2Gi", Sc::Unreadable, false).await;
    assert!(
        matches!(out.resize, CachePvcResize::Expanded { .. }),
        "{:?}",
        out.resize
    );
    assert_eq!(patches(&log).len(), 1);
    // ...and the apiserver refusing that patch is surfaced, not a reconcile error.
    let (out, _) = ensure("1Gi", "2Gi", Sc::Unreadable, true).await;
    assert!(
        matches!(&out.resize, CachePvcResize::ExpansionNotAllowed { reason, .. } if reason.contains("refused")),
        "{:?}",
        out.resize
    );
}

#[tokio::test]
async fn a_smaller_capacity_never_shrinks_the_claim() {
    let (out, log) = ensure("4Gi", "2Gi", Sc::Allows, false).await;
    assert_eq!(
        out.resize,
        CachePvcResize::ShrinkRefused {
            current: "4Gi".into(),
            requested: "2Gi".into()
        }
    );
    assert!(
        patches(&log).is_empty(),
        "a shrink must never PATCH: {log:?}"
    );
    // Budgets follow the smaller requested capacity.
    assert_eq!(out.capacity, "2Gi");
}

#[tokio::test]
async fn an_unchanged_capacity_touches_nothing() {
    // `2048Mi` == `2Gi`: compared as quantities, not strings.
    let (out, log) = ensure("2Gi", "2048Mi", Sc::Allows, false).await;
    assert_eq!(out.resize, CachePvcResize::Unchanged);
    assert!(patches(&log).is_empty());
    assert!(
        !log.iter().any(|l| l.contains("storageclasses")),
        "no StorageClass read without a resize: {log:?}"
    );
}

#[test]
fn resize_direction_compares_quantities() {
    assert_eq!(resize_direction("1Gi", "2Gi"), ResizeDirection::Grow);
    assert_eq!(resize_direction("2Gi", "1Gi"), ResizeDirection::Shrink);
    assert_eq!(resize_direction("1Gi", "1024Mi"), ResizeDirection::Same);
    assert_eq!(resize_direction("1G", "1Gi"), ResizeDirection::Grow);
    // Never resize on a guess.
    assert_eq!(resize_direction("garbage", "2Gi"), ResizeDirection::Same);
}

#[test]
fn effective_capacity_is_the_smallest_known_size() {
    let pvc: PersistentVolumeClaim = serde_json::from_value(pvc_json("4Gi", Some("3Gi"))).unwrap();
    assert_eq!(effective_cache_capacity("8Gi", &pvc), "3Gi");
    assert_eq!(effective_cache_capacity("2Gi", &pvc), "2Gi");
    let unbound: PersistentVolumeClaim = serde_json::from_value(pvc_json("4Gi", None)).unwrap();
    assert_eq!(effective_cache_capacity("8Gi", &unbound), "4Gi");
}

#[test]
fn storage_class_expansion_needs_an_explicit_true() {
    let sc = |allow: Option<bool>| k8s_openapi::api::storage::v1::StorageClass {
        allow_volume_expansion: allow,
        ..Default::default()
    };
    assert!(storage_class_allows_expansion(&sc(Some(true))));
    assert!(!storage_class_allows_expansion(&sc(Some(false))));
    assert!(!storage_class_allows_expansion(&sc(None)));
}

/// Greptile on #707: when a sibling run wins the create race (409), the claim
/// that now exists may be SMALLER than this run's requested capacity (the policy's
/// capacity changed between the two runs). Reporting the requested capacity sized
/// this run's budgets against a disk it does not have; the winner is re-read and
/// its effective capacity is used instead.
#[tokio::test]
async fn a_lost_create_race_reports_the_winning_claims_capacity() {
    let gets = Arc::new(Mutex::new(0u32));
    let log = Arc::new(Mutex::new(Vec::new()));
    let svc = {
        let (gets, log) = (gets.clone(), log.clone());
        tower::service_fn(move |req: Request<Body>| {
            let (gets, log) = (gets.clone(), log.clone());
            async move {
                let method = req.method().as_str().to_string();
                log.lock().unwrap().push(method.clone());
                let (status, json) = match method.as_str() {
                    // First GET: absent (so this run tries to create). Later GETs:
                    // the sibling's winning claim, 64Mi.
                    "GET" => {
                        let mut n = gets.lock().unwrap();
                        *n += 1;
                        if *n == 1 {
                            (StatusCode::NOT_FOUND, status_body(404))
                        } else {
                            (StatusCode::OK, pvc_json("64Mi", Some("64Mi")))
                        }
                    }
                    "POST" => (
                        StatusCode::CONFLICT,
                        serde_json::json!({
                            "kind": "Status", "apiVersion": "v1", "status": "Failure",
                            "message": "already exists", "reason": "AlreadyExists", "code": 409,
                        }),
                    ),
                    _ => (StatusCode::NOT_FOUND, status_body(404)),
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
    let client = Client::new(svc, "ns");
    let out = ensured(
        ensure_cache_pvc(
            &client,
            "ns",
            "kopiur-cache-pg",
            &in_namespace(),
            "128Mi",
            Some("fast"),
        )
        .await
        .expect("a lost create race is not an error"),
    );
    assert_eq!(
        out.capacity, "64Mi",
        "budgets must fit the winning claim, not this run's request"
    );
    assert_eq!(out.claim_name, "kopiur-cache-pg");
    assert!(
        log.lock().unwrap().iter().filter(|m| *m == "GET").count() >= 2,
        "the winner must be re-read"
    );
}

// --- #760: a central policy's claim lives in the operator namespace ---------

/// The `SnapshotPolicy` `billing/pg` as the API server stores it while it is
/// live: UID `uid-pg`, carrying the cleanup finalizer.
fn live_policy() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": crate::consts::API_VERSION, "kind": "SnapshotPolicy",
        "metadata": {
            "name": "pg", "namespace": "billing", "uid": "uid-pg",
            "finalizers": [crate::consts::POLICY_CLEANUP_FINALIZER],
        },
        "spec": { "repository": { "kind": "ClusterRepository", "name": "shared" } },
    })
}

/// A mock apiserver for one claim name. `stored` is the claim a GET returns
/// (`None` is a 404), `policy` is the `SnapshotPolicy` a GET returns, and a
/// POST answers `post` (`201` echoes the claim with the UID `pvc-created`;
/// nothing is stored: these tests only look at what was sent). Every request
/// is logged as `METHOD path body`.
fn claim_apiserver(
    stored: Option<serde_json::Value>,
    policy: Option<serde_json::Value>,
    post: StatusCode,
    log: Arc<Mutex<Vec<String>>>,
) -> Client {
    let svc = tower::service_fn(move |req: Request<Body>| {
        let (stored, policy, log) = (stored.clone(), policy.clone(), log.clone());
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
            let not_found = (StatusCode::NOT_FOUND, status_body(404));
            let (status, json) = match method.as_str() {
                "GET" if path.contains("/snapshotpolicies/") => {
                    policy.map_or(not_found, |p| (StatusCode::OK, p))
                }
                "GET" if path.contains("/persistentvolumeclaims/") => {
                    stored.map_or(not_found, |c| (StatusCode::OK, c))
                }
                "GET" if path.ends_with("/storageclasses/fast") => {
                    (StatusCode::OK, storage_class(true))
                }
                "POST" if post == StatusCode::CREATED => {
                    let mut created: serde_json::Value =
                        serde_json::from_slice(&bytes).expect("posted claim json");
                    created["metadata"]["uid"] = "pvc-created".into();
                    (StatusCode::CREATED, created)
                }
                "POST" => (post, status_body(post.as_u16())),
                "PATCH" | "DELETE" => stored.map_or(not_found, |c| (StatusCode::OK, c)),
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
    });
    Client::new(svc, "kopiur-system")
}

/// `pvc_json` with the UID `pvc-stored`, marked as the central claim of the
/// policy with UID `uid`.
fn central_claim_json(size: &str, uid: &str) -> serde_json::Value {
    let mut v = pvc_json(size, Some(size));
    v["metadata"]["uid"] = "pvc-stored".into();
    v["metadata"]["labels"] = serde_json::json!({ CENTRAL_POLICY_UID_LABEL: uid });
    v
}

/// [`ensure_cache_pvc`] against [`claim_apiserver`], with the policy live.
async fn ensure_central(
    stored: Option<serde_json::Value>,
    post: StatusCode,
    owner: &CachePvcOwner,
    requested: &str,
) -> (CachePvcClaim, Vec<String>) {
    ensure_central_of(stored, Some(live_policy()), post, owner, requested).await
}

async fn ensure_central_of(
    stored: Option<serde_json::Value>,
    policy: Option<serde_json::Value>,
    post: StatusCode,
    owner: &CachePvcOwner,
    requested: &str,
) -> (CachePvcClaim, Vec<String>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let client = claim_apiserver(stored, policy, post, log.clone());
    let out = ensure_cache_pvc(
        &client,
        "kopiur-system",
        "kopiur-cache-pg",
        owner,
        requested,
        Some("fast"),
    )
    .await
    .expect("ensure_cache_pvc");
    let log = log.lock().unwrap().clone();
    (out, log)
}

fn writes(log: &[String]) -> Vec<&String> {
    log.iter().filter(|l| !l.starts_with("GET")).collect()
}

/// The claim is in the operator namespace and its policy is not, so it must
/// carry the policy's UID label and NO owner reference: the garbage collector
/// deletes a dependent whose owner it cannot find in the dependent's namespace.
#[tokio::test]
async fn a_central_policys_claim_is_created_with_its_uid_label_and_no_owner_reference() {
    let (out, log) =
        ensure_central(None, StatusCode::CREATED, &central_policy("uid-pg"), "5Gi").await;
    assert_eq!(ensured(out).capacity, "5Gi");
    let posts: Vec<&String> = log.iter().filter(|l| l.starts_with("POST")).collect();
    assert_eq!(posts.len(), 1, "exactly one create: {log:?}");
    assert!(
        posts[0].starts_with("POST /api/v1/namespaces/kopiur-system/persistentvolumeclaims "),
        "the claim is created in the mover's namespace: {}",
        posts[0]
    );
    let body = posts[0].splitn(3, ' ').nth(2).expect("POST body");
    let created: PersistentVolumeClaim = serde_json::from_str(body).expect("posted claim");
    assert_eq!(created.metadata.owner_references, None);
    assert_eq!(
        created
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(CENTRAL_POLICY_UID_LABEL))
            .map(String::as_str),
        Some("uid-pg")
    );
}

/// The second run of a policy finds the claim of the first, and grows it.
#[tokio::test]
async fn a_central_policy_reuses_and_grows_its_own_claim() {
    let (out, log) = ensure_central(
        Some(central_claim_json("1Gi", "uid-pg")),
        StatusCode::CREATED,
        &central_policy("uid-pg"),
        "1Gi",
    )
    .await;
    assert_eq!(ensured(out).resize, CachePvcResize::Unchanged);
    assert!(writes(&log).is_empty(), "a reuse writes nothing: {log:?}");

    let (out, log) = ensure_central(
        Some(central_claim_json("1Gi", "uid-pg")),
        StatusCode::CREATED,
        &central_policy("uid-pg"),
        "2Gi",
    )
    .await;
    assert!(
        matches!(ensured(out).resize, CachePvcResize::Expanded { .. }),
        "{log:?}"
    );
    assert_eq!(patches(&log).len(), 1);
}

/// Policies of every namespace, and in-namespace owners of the operator
/// namespace, share one namespace. A claim that does not carry this policy's UID
/// holds someone else's kopia cache: it is never mounted, resized, or adopted.
#[tokio::test]
async fn a_central_policy_never_touches_a_claim_that_is_not_its_own() {
    for (what, stored) in [
        (
            "another policy's central claim",
            central_claim_json("1Gi", "uid-other"),
        ),
        (
            "an in-namespace owner's claim",
            pvc_json("1Gi", Some("1Gi")),
        ),
    ] {
        // A larger capacity: a claim taken for its own would be PATCHed.
        let (out, log) = ensure_central(
            Some(stored),
            StatusCode::CREATED,
            &central_policy("uid-pg"),
            "2Gi",
        )
        .await;
        assert_eq!(out, CachePvcClaim::Foreign, "{what}");
        assert!(
            writes(&log).is_empty(),
            "{what} must not be written: {log:?}"
        );
    }
}

/// The other direction: an in-namespace owner of the operator namespace whose
/// claim name equals a central claim's must not mount or resize that claim.
#[tokio::test]
async fn an_in_namespace_owner_never_touches_a_central_claim() {
    let (out, log) = ensure_central(
        Some(central_claim_json("1Gi", "uid-pg")),
        StatusCode::CREATED,
        &in_namespace(),
        "2Gi",
    )
    .await;
    assert_eq!(out, CachePvcClaim::Foreign);
    assert!(writes(&log).is_empty(), "{log:?}");
}

/// A create that loses to a claim of someone else is foreign too, not "reuse
/// the winner".
#[tokio::test]
async fn a_lost_create_race_against_a_foreign_claim_is_foreign() {
    let gets = Arc::new(Mutex::new(0u32));
    let svc = {
        let gets = gets.clone();
        tower::service_fn(move |req: Request<Body>| {
            let gets = gets.clone();
            async move {
                let (status, json) = match req.method().as_str() {
                    "GET" => {
                        let mut n = gets.lock().unwrap();
                        *n += 1;
                        if *n == 1 {
                            (StatusCode::NOT_FOUND, status_body(404))
                        } else {
                            (StatusCode::OK, central_claim_json("1Gi", "uid-other"))
                        }
                    }
                    "POST" => (StatusCode::CONFLICT, status_body(409)),
                    _ => (StatusCode::NOT_FOUND, status_body(404)),
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
    let client = Client::new(svc, "kopiur-system");
    let out = ensure_cache_pvc(
        &client,
        "kopiur-system",
        "kopiur-cache-pg",
        &central_policy("uid-pg"),
        "1Gi",
        Some("fast"),
    )
    .await
    .expect("a lost create race is not an error");
    assert_eq!(out, CachePvcClaim::Foreign);
}

// --- #760 review: the claim must not outlive its policy ---------------------

#[test]
fn a_central_policy_reaps_its_claims_only_while_it_is_live_and_finalized() {
    use super::central_policy_reaps;
    let meta = |v: serde_json::Value| -> kube::core::ObjectMeta {
        serde_json::from_value(v["metadata"].clone()).expect("ObjectMeta")
    };
    assert!(central_policy_reaps(&meta(live_policy()), "uid-pg"));
    // A re-created policy of the same name is a different owner.
    assert!(!central_policy_reaps(&meta(live_policy()), "uid-old"));
    let mut deleting = live_policy();
    deleting["metadata"]["deletionTimestamp"] = "2026-01-01T00:00:00Z".into();
    assert!(!central_policy_reaps(&meta(deleting), "uid-pg"));
    // No cleanup finalizer yet: deleting the policy now would run no cleanup.
    let mut bare = live_policy();
    bare["metadata"]["finalizers"] = serde_json::json!(["example.com/other"]);
    assert!(!central_policy_reaps(&meta(bare.clone()), "uid-pg"));
    bare["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("finalizers");
    assert!(!central_policy_reaps(&meta(bare), "uid-pg"));
}

/// The policy's finalizer lists and deletes its central claims once. A run that
/// was launched from a stale view of the policy can create the claim after that
/// list, and nothing would ever delete it. So the policy is read back from the
/// API server after the claim exists, and a claim without a live owner is
/// deleted again, by UID.
#[tokio::test]
async fn a_claim_created_for_a_policy_that_cannot_reap_it_is_deleted_again() {
    let mut deleting = live_policy();
    deleting["metadata"]["deletionTimestamp"] = "2026-01-01T00:00:00Z".into();
    let mut replaced = live_policy();
    replaced["metadata"]["uid"] = "uid-new".into();
    let mut unfinalized = live_policy();
    unfinalized["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("finalizers");
    for (what, policy) in [
        ("a deleted policy", None),
        ("a policy being deleted", Some(deleting)),
        ("a re-created policy", Some(replaced)),
        ("a policy without its finalizer", Some(unfinalized)),
    ] {
        let (out, log) = ensure_central_of(
            None,
            policy,
            StatusCode::CREATED,
            &central_policy("uid-pg"),
            "5Gi",
        )
        .await;
        assert_eq!(out, CachePvcClaim::Orphaned, "{what}");
        let order: Vec<&str> = log
            .iter()
            .map(|l| l.split(' ').next().unwrap_or_default())
            .collect();
        assert_eq!(
            order,
            ["GET", "POST", "GET", "DELETE"],
            "{what}: the policy is read AFTER the create: {log:?}"
        );
        assert!(
            log[2].contains("/namespaces/billing/snapshotpolicies/pg"),
            "{what}: {}",
            log[2]
        );
        assert!(
            log[3].contains("/namespaces/kopiur-system/persistentvolumeclaims/kopiur-cache-pg")
                && log[3].contains(r#""preconditions":{"uid":"pvc-created"}"#),
            "{what}: exactly the created claim is deleted: {}",
            log[3]
        );
    }
}

/// The same holds for a claim that already exists: a previous pass may have
/// failed between the create and the delete above.
#[tokio::test]
async fn an_existing_claim_of_a_policy_that_cannot_reap_it_is_deleted() {
    let (out, log) = ensure_central_of(
        Some(central_claim_json("1Gi", "uid-pg")),
        None,
        StatusCode::CREATED,
        &central_policy("uid-pg"),
        "1Gi",
    )
    .await;
    assert_eq!(out, CachePvcClaim::Orphaned);
    let deletes: Vec<&String> = log.iter().filter(|l| l.starts_with("DELETE")).collect();
    assert_eq!(deletes.len(), 1, "{log:?}");
    assert!(
        deletes[0].contains(r#""preconditions":{"uid":"pvc-stored"}"#),
        "{}",
        deletes[0]
    );
}

/// An owner-referenced claim needs no such check: the garbage collector
/// deletes it with its owner. No `SnapshotPolicy` is read.
#[tokio::test]
async fn an_owner_referenced_claim_reads_no_policy() {
    let (out, log) =
        ensure_central_of(None, None, StatusCode::CREATED, &in_namespace(), "5Gi").await;
    assert!(matches!(out, CachePvcClaim::Ensured(_)), "{out:?}");
    assert!(
        !log.iter().any(|l| l.contains("snapshotpolicies")),
        "{log:?}"
    );
    assert!(!log.iter().any(|l| l.starts_with("DELETE")), "{log:?}");
}

// --- #760 review: a resize can only change the claim that was read ----------

/// The ownership check and the resize are two requests. The PATCH carries the
/// UID of the claim that was checked, so the API server refuses it for a claim
/// that replaced that one under the same name in between.
#[tokio::test]
async fn the_resize_patch_is_pinned_to_the_uid_of_the_claim_that_was_read() {
    let (_, log) = ensure_central(
        Some(central_claim_json("1Gi", "uid-pg")),
        StatusCode::CREATED,
        &central_policy("uid-pg"),
        "2Gi",
    )
    .await;
    let p = patches(&log);
    assert_eq!(p.len(), 1, "{log:?}");
    assert!(
        p[0].contains(r#""metadata":{"uid":"pvc-stored"}"#),
        "the PATCH must carry the UID of the claim it read: {}",
        p[0]
    );
}

/// The answer of a real kube-apiserver (v1.35) to a merge patch whose
/// `metadata.uid` is not the stored object's.
fn uid_mismatch_status() -> serde_json::Value {
    serde_json::json!({
        "kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure",
        "message": "PersistentVolumeClaim \"kopiur-cache-pg\" is invalid: metadata.uid: \
                    Invalid value: \"pvc-stored\": field is immutable",
        "reason": "Invalid",
        "details": {
            "name": "kopiur-cache-pg", "kind": "PersistentVolumeClaim",
            "causes": [{
                "reason": "FieldValueInvalid",
                "message": "Invalid value: \"pvc-stored\": field is immutable",
                "field": "metadata.uid",
            }],
        },
        "code": 422,
    })
}

#[test]
fn a_uid_mismatch_is_told_apart_from_a_refused_expansion() {
    use super::refused_for_uid;
    let status =
        |v: serde_json::Value| -> kube::core::Status { serde_json::from_value(v).expect("Status") };
    assert!(refused_for_uid(&status(uid_mismatch_status())));
    // The apiserver's own expansion refusals (403 and 422) name other fields.
    assert!(!refused_for_uid(&status(status_body(403))));
    let mut expansion = uid_mismatch_status();
    expansion["details"]["causes"][0]["field"] = "spec.resources.requests.storage".into();
    assert!(!refused_for_uid(&status(expansion)));
}

/// A replaced claim is not "this StorageClass cannot expand": that outcome
/// would mount the replacement by name. The pass fails, and the next one
/// decides from the claim that is there then.
#[tokio::test]
async fn a_claim_replaced_before_the_resize_fails_the_pass_instead_of_being_mounted() {
    let svc = tower::service_fn(move |req: Request<Body>| async move {
        let path = req.uri().path().to_string();
        let (status, json) = match req.method().as_str() {
            "GET" if path.contains("/persistentvolumeclaims/") => {
                (StatusCode::OK, central_claim_json("1Gi", "uid-pg"))
            }
            "GET" if path.ends_with("/storageclasses/fast") => {
                (StatusCode::OK, storage_class(true))
            }
            "PATCH" => (StatusCode::UNPROCESSABLE_ENTITY, uid_mismatch_status()),
            _ => (StatusCode::NOT_FOUND, status_body(404)),
        };
        Ok::<_, std::convert::Infallible>(
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&json).unwrap()))
                .unwrap(),
        )
    });
    let err = ensure_cache_pvc(
        &Client::new(svc, "kopiur-system"),
        "kopiur-system",
        "kopiur-cache-pg",
        &central_policy("uid-pg"),
        "2Gi",
        Some("fast"),
    )
    .await
    .expect_err("a replaced claim must not be reported as a refused expansion");
    assert!(matches!(err, crate::error::Error::Kube(_)), "{err:?}");
}

/// A create that conflicts with a claim that is gone again by the re-read has
/// no claim to mount. Reporting success left the mover pod waiting on a PVC
/// that does not exist.
#[tokio::test]
async fn a_create_conflict_with_a_vanished_claim_is_retried_not_mounted() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let client = claim_apiserver(None, Some(live_policy()), StatusCode::CONFLICT, log.clone());
    let err = ensure_cache_pvc(
        &client,
        "kopiur-system",
        "kopiur-cache-pg",
        &central_policy("uid-pg"),
        "1Gi",
        Some("fast"),
    )
    .await
    .expect_err("there is no claim to mount");
    assert_eq!(
        err.class(),
        crate::error::ErrorClass::Transient,
        "a transient error, so the next pass creates the claim: {err:?}"
    );
}
