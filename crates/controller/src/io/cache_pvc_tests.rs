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
    CachePvcResize, EnsuredCachePvc, ResizeDirection, effective_cache_capacity, ensure_cache_pvc,
    resize_direction, storage_class_allows_expansion,
};

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
        Default::default(),
        requested,
        Some("fast"),
    )
    .await
    .expect("ensure_cache_pvc");
    let log = log.lock().unwrap().clone();
    (out, log)
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
