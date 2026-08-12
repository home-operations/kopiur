//! Client-side kube API request metrics (issue #382).
//!
//! A tower [`Layer`] wrapped around **every** kube [`kube::Client`] the
//! controller builds (`main`/`exec`/`election`, via
//! `ClientBuilder::with_layer` in `startup.rs`), counting each outgoing HTTP
//! request as `kopiur_kube_client_requests_total{verb, group, kind, client}`.
//!
//! Why client-side: kube-rs `Controller`s drive their own internal trigger
//! streams (primary reflectors, `.owns()`, `.watches()`), which offer no error
//! or event hook to instrument — during #382 the request load had to be
//! attributed from the **apiserver's** metrics. This layer sits below every
//! request those streams (and reconcilers, and the leader election) make, so
//! the controller can report its own apiserver footprint.
//!
//! Classification is the **pure** [`classify_kube_request`] over
//! `(method, path, query)` — mirroring the apiserver's request-info resolver —
//! so the label set is unit-testable without any HTTP machinery. Cardinality
//! is bounded by construction: labels carry the resource plural (plus
//! `/subresource`) and API group, never object names or namespaces, and any
//! unrecognizable path folds into the single [`OTHER`] bucket.

use http::{Method, Request};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use tower::{Layer, Service};

/// The bounded fallback label value for requests whose path is not a
/// recognizable resource URL (API discovery, `/version`, `/openapi`, …).
pub const OTHER: &str = "other";

/// The Kubernetes API verb of one client request, derived from the HTTP
/// method + path shape (+ `watch=true` query). A closed set — the `verb`
/// label can never grow beyond these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KubeVerb {
    /// `GET` of a single object.
    Get,
    /// `GET` of a collection.
    List,
    /// `GET` with `watch=true`.
    Watch,
    /// `POST` (create).
    Create,
    /// `PUT` (replace).
    Update,
    /// `PATCH` that is not server-side apply.
    Patch,
    /// `PATCH` with the `application/apply-patch` content type (server-side
    /// apply). Refined by [`refine_patch_verb`] — the content type is a
    /// header, deliberately outside [`classify_kube_request`]'s pure
    /// `(method, path, query)` signature.
    Apply,
    /// `DELETE` of a single object.
    Delete,
    /// `DELETE` of a collection.
    DeleteCollection,
    /// Any other HTTP method (`HEAD`, `OPTIONS`, WebSocket upgrades ride
    /// `GET` and classify as `get`).
    Other,
}

impl KubeVerb {
    /// The `verb` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            KubeVerb::Get => "get",
            KubeVerb::List => "list",
            KubeVerb::Watch => "watch",
            KubeVerb::Create => "create",
            KubeVerb::Update => "update",
            KubeVerb::Patch => "patch",
            KubeVerb::Apply => "apply",
            KubeVerb::Delete => "delete",
            KubeVerb::DeleteCollection => "deletecollection",
            KubeVerb::Other => "other",
        }
    }
}

/// The bounded label set of one kube client request:
/// `{verb, group, kind}` (`client` is stamped by the layer).
///
/// `group` is the API group (`""` for the core group, matching the
/// apiserver's own `apiserver_request_total` convention so the two can be
/// joined); `kind` is the **resource plural** from the path (e.g.
/// `snapshots`), with a `/subresource` suffix for subresource requests
/// (`snapshots/status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestLabels {
    /// The API verb.
    pub verb: KubeVerb,
    /// The API group (`""` = core, [`OTHER`] = unrecognizable path).
    pub group: String,
    /// The resource plural (+ `/subresource`), or [`OTHER`].
    pub kind: String,
}

/// Classify one kube API request into its bounded [`RequestLabels`].
///
/// Pure over `(method, path, query)` — the path grammar mirrors the
/// apiserver's request-info resolver:
///
/// - core group: `/api/v1/[namespaces/<ns>/]<resource>[/<name>[/<sub>]]`
/// - named group: `/apis/<group>/<version>/[namespaces/<ns>/]<resource>[/<name>[/<sub>]]`
/// - `GET` + `watch=true` query → `watch`, regardless of path shape
/// - anything else (discovery, `/version`, …) → the single [`OTHER`] bucket.
pub fn classify_kube_request(method: &Method, path: &str, query: Option<&str>) -> RequestLabels {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (group, resource_parts): (String, &[&str]) = match segments.as_slice() {
        ["api", "v1", rest @ ..] if !rest.is_empty() => (String::new(), rest),
        ["apis", group, _version, rest @ ..] if !rest.is_empty() => ((*group).to_string(), rest),
        // Not a resource URL (discovery, /version, /openapi, …): one bounded
        // bucket. The verb still reflects the method; `has_name: true` keeps a
        // discovery GET classified as `get`, not a phantom `list`.
        _ => {
            return RequestLabels {
                verb: verb_for(method, true, false),
                group: OTHER.to_string(),
                kind: OTHER.to_string(),
            };
        }
    };
    let (kind, has_name) = resource_shape(resource_parts);
    RequestLabels {
        verb: verb_for(method, has_name, is_watch_request(method, query)),
        group,
        kind,
    }
}

/// Refine a [`KubeVerb::Patch`] into [`KubeVerb::Apply`] when the request's
/// `Content-Type` is server-side apply (`application/apply-patch+yaml`).
/// Separate from [`classify_kube_request`] because the content type is a
/// header, not part of the pure `(method, path, query)` classification input.
pub fn refine_patch_verb(verb: KubeVerb, content_type: Option<&str>) -> KubeVerb {
    if verb == KubeVerb::Patch
        && content_type.is_some_and(|ct| ct.starts_with("application/apply-patch"))
    {
        KubeVerb::Apply
    } else {
        verb
    }
}

/// The `(kind label, has_name)` shape of the path segments AFTER the
/// group/version prefix, mirroring the apiserver's request-info resolver:
/// a leading `namespaces/<ns>/` pair is stripped when more segments follow
/// (otherwise `namespaces` IS the resource — `GET /api/v1/namespaces/x` is a
/// get of the Namespace object).
fn resource_shape(parts: &[&str]) -> (String, bool) {
    let parts: &[&str] = match parts {
        ["namespaces", _ns, rest @ ..] if !rest.is_empty() => rest,
        other => other,
    };
    match parts {
        [] => (OTHER.to_string(), false),
        [resource] => ((*resource).to_string(), false),
        [resource, _name] => ((*resource).to_string(), true),
        [resource, _name, subresource, ..] => (format!("{resource}/{subresource}"), true),
    }
}

/// Whether this is a watch request: a `GET` whose query carries `watch=true`
/// (kube-rs always spells it `watch=true`; `watch=1` is accepted for parity
/// with the apiserver's boolean parsing).
fn is_watch_request(method: &Method, query: Option<&str>) -> bool {
    *method == Method::GET
        && query.is_some_and(|q| q.split('&').any(|kv| kv == "watch=true" || kv == "watch=1"))
}

/// Map `(HTTP method, single-object vs collection, watch)` to the API verb.
/// The match is over wire data (method strings), so the default arm is data
/// handling, not an enum catch-all.
fn verb_for(method: &Method, has_name: bool, watch: bool) -> KubeVerb {
    if watch {
        return KubeVerb::Watch;
    }
    match (method.as_str(), has_name) {
        ("GET", true) => KubeVerb::Get,
        ("GET", false) => KubeVerb::List,
        ("POST", _) => KubeVerb::Create,
        ("PUT", _) => KubeVerb::Update,
        ("PATCH", _) => KubeVerb::Patch,
        ("DELETE", true) => KubeVerb::Delete,
        ("DELETE", false) => KubeVerb::DeleteCollection,
        _ => KubeVerb::Other,
    }
}

/// A tower [`Layer`] stamping `kopiur_kube_client_requests_total` for every
/// request through the wrapped service. Built once per client via
/// [`crate::metrics::Metrics::kube_client_layer`]; the `client` label
/// (`main`/`exec`/`election`) distinguishes the three connection pools.
#[derive(Clone)]
pub struct KubeClientMetricsLayer {
    counter: Counter<u64>,
    client: &'static str,
}

impl KubeClientMetricsLayer {
    /// Wrap `counter` (the shared `kopiur_kube_client_requests` instrument)
    /// with the given `client` label value.
    pub fn new(counter: Counter<u64>, client: &'static str) -> Self {
        Self { counter, client }
    }
}

impl<S> Layer<S> for KubeClientMetricsLayer {
    type Service = KubeClientMetrics<S>;

    fn layer(&self, inner: S) -> Self::Service {
        KubeClientMetrics {
            inner,
            counter: self.counter.clone(),
            client: self.client,
        }
    }
}

/// The [`Service`] produced by [`KubeClientMetricsLayer`]: counts, then
/// forwards. Fully transparent — response, error, and future types are the
/// inner service's, so it composes under kube's `ClientBuilder` stack without
/// touching the auth/TLS bounds.
pub struct KubeClientMetrics<S> {
    inner: S,
    counter: Counter<u64>,
    client: &'static str,
}

impl<S, B> Service<Request<B>> for KubeClientMetrics<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let mut labels = classify_kube_request(req.method(), req.uri().path(), req.uri().query());
        let content_type = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        labels.verb = refine_patch_verb(labels.verb, content_type);
        self.counter.add(
            1,
            &[
                KeyValue::new("verb", labels.verb.as_str()),
                KeyValue::new("group", labels.group),
                KeyValue::new("kind", labels.kind),
                KeyValue::new("client", self.client),
            ],
        );
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    fn expect(verb: KubeVerb, group: &str, kind: &str) -> RequestLabels {
        RequestLabels {
            verb,
            group: group.to_string(),
            kind: kind.to_string(),
        }
    }

    #[test]
    fn classify_core_group_object_and_collection() {
        // Single object → get.
        assert_eq!(
            classify_kube_request(&Method::GET, "/api/v1/namespaces/x/secrets/y", None),
            expect(KubeVerb::Get, "", "secrets"),
        );
        // Collection → list.
        assert_eq!(
            classify_kube_request(&Method::GET, "/api/v1/namespaces/x/secrets", None),
            expect(KubeVerb::List, "", "secrets"),
        );
        // Core cluster-scoped collection (PVs).
        assert_eq!(
            classify_kube_request(&Method::GET, "/api/v1/persistentvolumes", None),
            expect(KubeVerb::List, "", "persistentvolumes"),
        );
    }

    #[test]
    fn classify_named_group_paths() {
        // Namespaced CRD collection.
        assert_eq!(
            classify_kube_request(
                &Method::GET,
                "/apis/kopiur.home-operations.com/v1alpha1/namespaces/x/snapshots",
                None,
            ),
            expect(KubeVerb::List, "kopiur.home-operations.com", "snapshots"),
        );
        // Namespaced named group object (the election Lease).
        assert_eq!(
            classify_kube_request(
                &Method::PUT,
                "/apis/coordination.k8s.io/v1/namespaces/kopiur-system/leases/kopiur",
                None,
            ),
            expect(KubeVerb::Update, "coordination.k8s.io", "leases"),
        );
    }

    #[test]
    fn classify_cluster_scoped_and_namespace_objects() {
        // Cluster-scoped CRD object.
        assert_eq!(
            classify_kube_request(
                &Method::GET,
                "/apis/kopiur.home-operations.com/v1alpha1/clusterrepositories/nas",
                None,
            ),
            expect(
                KubeVerb::Get,
                "kopiur.home-operations.com",
                "clusterrepositories"
            ),
        );
        // `namespaces` IS the resource when nothing follows the name:
        // GET /api/v1/namespaces/x is a get of the Namespace object, and
        // GET /api/v1/namespaces is a list of them (requestinfo parity).
        assert_eq!(
            classify_kube_request(&Method::GET, "/api/v1/namespaces/x", None),
            expect(KubeVerb::Get, "", "namespaces"),
        );
        assert_eq!(
            classify_kube_request(&Method::GET, "/api/v1/namespaces", None),
            expect(KubeVerb::List, "", "namespaces"),
        );
    }

    #[test]
    fn classify_subresources_fold_into_kind() {
        assert_eq!(
            classify_kube_request(
                &Method::PATCH,
                "/apis/kopiur.home-operations.com/v1alpha1/namespaces/x/snapshots/y/status",
                None,
            ),
            expect(
                KubeVerb::Patch,
                "kopiur.home-operations.com",
                "snapshots/status"
            ),
        );
        // Core-group subresource (pod exec rides GET + upgrade).
        assert_eq!(
            classify_kube_request(&Method::GET, "/api/v1/namespaces/x/pods/y/exec", None),
            expect(KubeVerb::Get, "", "pods/exec"),
        );
    }

    #[test]
    fn classify_watch_query_wins_over_path_shape() {
        assert_eq!(
            classify_kube_request(
                &Method::GET,
                "/apis/kopiur.home-operations.com/v1alpha1/namespaces/x/snapshots",
                Some("watch=true&resourceVersion=5&timeoutSeconds=290"),
            )
            .verb,
            KubeVerb::Watch,
        );
        // watch=false (or absent) keeps the path-shape verb.
        assert_eq!(
            classify_kube_request(
                &Method::GET,
                "/api/v1/namespaces/x/secrets",
                Some("watch=false&labelSelector=a%3Db"),
            )
            .verb,
            KubeVerb::List,
        );
        // watch=true on a non-GET method is not a watch.
        assert_eq!(
            classify_kube_request(
                &Method::DELETE,
                "/api/v1/namespaces/x/secrets/y",
                Some("watch=true")
            )
            .verb,
            KubeVerb::Delete,
        );
    }

    #[test]
    fn classify_write_verbs_by_method_and_shape() {
        assert_eq!(
            classify_kube_request(&Method::POST, "/api/v1/namespaces/x/secrets", None).verb,
            KubeVerb::Create,
        );
        assert_eq!(
            classify_kube_request(&Method::DELETE, "/api/v1/namespaces/x/secrets/y", None).verb,
            KubeVerb::Delete,
        );
        assert_eq!(
            classify_kube_request(&Method::DELETE, "/api/v1/namespaces/x/secrets", None).verb,
            KubeVerb::DeleteCollection,
        );
        assert_eq!(
            classify_kube_request(&Method::HEAD, "/api/v1/namespaces/x/secrets", None).verb,
            KubeVerb::Other,
        );
    }

    #[test]
    fn classify_unknown_paths_use_the_bounded_other_bucket() {
        for path in [
            "/version",
            "/api",
            "/apis",
            "/api/v1",
            "/openapi/v2",
            "/",
            "",
        ] {
            let got = classify_kube_request(&Method::GET, path, None);
            assert_eq!(
                got,
                expect(KubeVerb::Get, OTHER, OTHER),
                "path {path:?} must fold into the bounded other bucket",
            );
        }
        // Discovery of a named group's versions is 2/3 segments — still other.
        assert_eq!(
            classify_kube_request(&Method::GET, "/apis/kopiur.home-operations.com", None),
            expect(KubeVerb::Get, OTHER, OTHER),
        );
        assert_eq!(
            classify_kube_request(
                &Method::GET,
                "/apis/kopiur.home-operations.com/v1alpha1",
                None
            ),
            expect(KubeVerb::Get, OTHER, OTHER),
        );
    }

    #[test]
    fn refine_patch_to_apply_only_on_apply_content_type() {
        assert_eq!(
            refine_patch_verb(KubeVerb::Patch, Some("application/apply-patch+yaml")),
            KubeVerb::Apply,
        );
        assert_eq!(
            refine_patch_verb(KubeVerb::Patch, Some("application/merge-patch+json")),
            KubeVerb::Patch,
        );
        assert_eq!(refine_patch_verb(KubeVerb::Patch, None), KubeVerb::Patch);
        // Only PATCH refines — a GET with a weird content type stays a get.
        assert_eq!(
            refine_patch_verb(KubeVerb::Get, Some("application/apply-patch+yaml")),
            KubeVerb::Get,
        );
    }

    /// The layer must add exactly one increment per request, carrying all four
    /// labels, and stay fully transparent to the wrapped service.
    #[tokio::test]
    async fn layer_counts_each_request_once_with_all_labels() {
        let metrics = crate::metrics::Metrics::new();
        let layer = metrics.kube_client_layer("main");
        let svc = tower::service_fn(|_req: Request<Vec<u8>>| async move {
            Ok::<_, std::convert::Infallible>(http::Response::new(Vec::<u8>::new()))
        });
        let mut svc = layer.layer(svc);

        let get = Request::builder()
            .method(Method::GET)
            .uri("/apis/kopiur.home-operations.com/v1alpha1/namespaces/x/snapshots/y")
            .body(Vec::new())
            .unwrap();
        futures::future::poll_fn(|cx| Service::poll_ready(&mut svc, cx))
            .await
            .unwrap();
        svc.call(get).await.unwrap();

        // A server-side apply must land in the `apply` verb bucket.
        let apply = Request::builder()
            .method(Method::PATCH)
            .uri("/api/v1/namespaces/x/secrets/y")
            .header(http::header::CONTENT_TYPE, "application/apply-patch+yaml")
            .body(Vec::new())
            .unwrap();
        futures::future::poll_fn(|cx| Service::poll_ready(&mut svc, cx))
            .await
            .unwrap();
        svc.call(apply).await.unwrap();

        let text = String::from_utf8(metrics.gather()).unwrap();
        let get_line = text
            .lines()
            .find(|l| {
                l.starts_with("kopiur_kube_client_requests_total{") && l.contains("verb=\"get\"")
            })
            .unwrap_or_else(|| panic!("missing get series in exposition:\n{text}"));
        for label in [
            "verb=\"get\"",
            "group=\"kopiur.home-operations.com\"",
            "kind=\"snapshots\"",
            "client=\"main\"",
        ] {
            assert!(get_line.contains(label), "missing {label} in {get_line}");
        }
        assert!(
            get_line.trim_end().ends_with(" 1"),
            "exactly one increment expected: {get_line}"
        );

        let apply_line = text
            .lines()
            .find(|l| {
                l.starts_with("kopiur_kube_client_requests_total{") && l.contains("verb=\"apply\"")
            })
            .unwrap_or_else(|| panic!("missing apply series in exposition:\n{text}"));
        for label in ["group=\"\"", "kind=\"secrets\"", "client=\"main\""] {
            assert!(
                apply_line.contains(label),
                "missing {label} in {apply_line}"
            );
        }
    }
}
