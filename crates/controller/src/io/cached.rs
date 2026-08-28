//! Reflector-store-backed **point reads** for the hot launch/gating paths
//! (#382 M2), replacing per-reconcile GETs of objects the process already
//! watches (`SnapshotPolicy`, `Repository`, `ClusterRepository`).
//!
//! The one rule (design rule R1): a store **hit** is served from cache; a store
//! **miss** — or an unset/not-yet-synced store — falls through to a live
//! `get_opt`. A miss is always live-confirmed, so a stale NEGATIVE ("object
//! missing" read off a cold cache) is impossible by construction. Staleness of
//! a HIT is the ordinary level-triggered-reconciler kind, corrected by the
//! watch event that follows.
//!
//! The `*_synced` flags are flipped from the owning reconcile
//! ([`Context::mark_config_synced`] and friends) — NEVER by a spawned
//! `wait_until_ready()`, which races the Controller applier's own getter on
//! kube-rs's single-waker `DelayedInit` and can hang forever (see the
//! `publish_store` doc comment in `crate::controllers`).
//!
//! **Deletion/finalizer/breaker paths must not use this module** (audit C7):
//! they stay on the live originals ([`super::resolve_repository_ref`],
//! `crate::snapshot`'s `resolve_recipe`) so a mass-deletion ack drain or a
//! finalizer decision never acts on a cached annotation/spec. Each `_cached`
//! fn's doc comment names the paths that must keep the live original.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use kube::api::ListParams;
use kube::runtime::reflector::ObjectRef;
use kube::{Api, ResourceExt};

use kopiur_api::common::RepositoryRef;
use kopiur_api::{ClusterRepository, Repository, Snapshot, SnapshotPolicy};

use crate::context::Context;
use crate::error::{Error, Result};

use super::repo::{
    RepoLookup, ResolvedRepository, repo_lookup, resolve_backend_ca, resolved_from_cluster,
    resolved_from_namespaced,
};

/// **Pure.** Whether a point read may consult the reflector store at all:
/// only when the store handle is `present` (published by `spawn_all`) AND its
/// reconcile-driven `synced` flag has flipped. Everything else — and every
/// store MISS even when this returns `true` — goes to the live API (R1: a
/// stale negative must be impossible).
pub fn read_from_store(present: bool, synced: bool) -> bool {
    present && synced
}

/// Point-read a namespaced [`Repository`]: reflector-store hit when the store
/// is published+synced, live `get_opt` on miss/cold (R1 — never a stale
/// negative). `Ok(None)` therefore always means "confirmed absent live".
pub async fn fetch_repository(
    ctx: &Context,
    namespace: &str,
    name: &str,
) -> Result<Option<Arc<Repository>>> {
    let store = ctx.repo_store.get();
    if read_from_store(store.is_some(), ctx.repo_synced.load(Ordering::Acquire))
        && let Some(hit) = store.and_then(|s| s.get(&ObjectRef::new(name).within(namespace)))
    {
        return Ok(Some(hit));
    }
    let api: Api<Repository> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(api.get_opt(name).await?.map(Arc::new))
}

/// Point-read a cluster-scoped [`ClusterRepository`] — same R1 contract as
/// [`fetch_repository`]. In a NAMESPACED install the `crepo_store` cell is
/// never published, so every read falls through to the live API and the
/// Role-RBAC 403 surfaces exactly as before (never a silent "missing").
pub async fn fetch_cluster_repository(
    ctx: &Context,
    name: &str,
) -> Result<Option<Arc<ClusterRepository>>> {
    let store = ctx.crepo_store.get();
    if read_from_store(store.is_some(), ctx.crepo_synced.load(Ordering::Acquire))
        && let Some(hit) = store.and_then(|s| s.get(&ObjectRef::new(name)))
    {
        return Ok(Some(hit));
    }
    let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
    Ok(api.get_opt(name).await?.map(Arc::new))
}

/// Point-read a [`SnapshotPolicy`] — same R1 contract as [`fetch_repository`].
pub async fn fetch_policy(
    ctx: &Context,
    namespace: &str,
    name: &str,
) -> Result<Option<Arc<SnapshotPolicy>>> {
    let store = ctx.config_store.get();
    if read_from_store(store.is_some(), ctx.config_synced.load(Ordering::Acquire))
        && let Some(hit) = store.and_then(|s| s.get(&ObjectRef::new(name).within(namespace)))
    {
        return Ok(Some(hit));
    }
    let api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(api.get_opt(name).await?.map(Arc::new))
}

/// Store-backed variant of [`super::resolve_repository_ref`] for the
/// **launch/pin/gating** callers only (#382 M2): the repository object comes
/// from the [`fetch_repository`]/[`fetch_cluster_repository`] kernel; the
/// `tls.caBundleRef` ConfigMap read inside stays LIVE (ConfigMaps are not
/// cached, and the bundle is inlined into the work spec).
///
/// **Must NOT be used by deletion/finalizer/breaker paths** (audit C7): the
/// Snapshot finalizer's `resolve_repo_for_deletion` (and anything else feeding
/// the mass-deletion breaker, e.g. the `mass_deletion_ack` annotation read)
/// keeps calling the live [`super::resolve_repository_ref`] — the ack-drain
/// trigger fires from the Snapshot controller's watch stream and would race
/// this reflector, regressing an ack release from immediate to minutes.
pub async fn resolve_repository_ref_cached(
    ctx: &Context,
    repo_ref: &RepositoryRef,
    default_ns: &str,
) -> Result<ResolvedRepository> {
    let operator_ns = ctx.operator_namespace.as_deref();
    match repo_lookup(repo_ref, default_ns) {
        RepoLookup::Namespaced { namespace, name } => {
            let repo = fetch_repository(ctx, &namespace, &name)
                .await?
                .ok_or_else(|| {
                    Error::MissingDependency(format!("Repository {namespace}/{name}"))
                })?;
            // One clone per reconcile: the builder wants an owned object (it
            // moves spec fields into the resolved surface).
            let repo = (*repo).clone();
            let ca_bundle_pem = resolve_backend_ca(
                &ctx.client,
                &repo.spec.backend,
                Some(&namespace),
                operator_ns,
            )
            .await?;
            resolved_from_namespaced(repo, namespace, ca_bundle_pem)
        }
        RepoLookup::Cluster { name } => {
            let repo = fetch_cluster_repository(ctx, &name)
                .await?
                .ok_or_else(|| Error::MissingDependency(format!("ClusterRepository {name}")))?;
            let repo = (*repo).clone();
            let ca_bundle_pem =
                resolve_backend_ca(&ctx.client, &repo.spec.backend, None, operator_ns).await?;
            resolved_from_cluster(repo, ca_bundle_pem)
        }
    }
}

/// Store-backed variant of [`super::repository_ready`] for the launch-side
/// readiness gates (#382 M2). Accepted staleness bound: a consumer reconcile
/// triggered by its own Repository watch can read a stale not-Ready phase from
/// the Repository controller's reflector — that heals at each gate's existing
/// short requeue backstop (15s for Snapshot/Restore/SnapshotPolicy, the
/// not-ready requeue for Maintenance/replication), so readiness is never
/// wedged, only (rarely) delayed by one requeue.
///
/// **Must NOT be used by deletion/finalizer/breaker paths** (audit C7) — those
/// keep the live [`super::repository_ready`]/[`super::resolve_repository_ref`].
pub async fn repository_ready_cached(
    ctx: &Context,
    repo_ref: &RepositoryRef,
    default_ns: &str,
) -> Result<bool> {
    let ready = kopiur_api::RepositoryPhase::Ready;
    match repo_lookup(repo_ref, default_ns) {
        RepoLookup::Namespaced { namespace, name } => {
            let repo = fetch_repository(ctx, &namespace, &name)
                .await?
                .ok_or_else(|| {
                    Error::MissingDependency(format!("Repository {namespace}/{name}"))
                })?;
            Ok(repo.status.as_ref().and_then(|s| s.phase.as_ref()) == Some(&ready))
        }
        RepoLookup::Cluster { name } => {
            let repo = fetch_cluster_repository(ctx, &name)
                .await?
                .ok_or_else(|| Error::MissingDependency(format!("ClusterRepository {name}")))?;
            Ok(repo.status.as_ref().and_then(|s| s.phase.as_ref()) == Some(&ready))
        }
    }
}

/// Store-backed MAINTENANCE gate (issue #413): whether the target repository
/// accepts maintenance now, per [`crate::health::maintenance_may_proceed`] —
/// keyed on bootstrapped-before + degradation cause, NOT strictly
/// `phase == Ready` like [`repository_ready_cached`]. Maintenance is the one
/// consumer deliberately EXEMPT from the #345 breaker's `Ready`-only gate,
/// because for an index-blob-bloated repository maintenance is the cure and
/// withholding it is a deadlock.
///
/// The Maintenance G7 gate is the sole consumer, and it is a launch-side gate
/// (#382 M2 charter), so no live twin exists. Same staleness bound and same
/// `MissingDependency` shape as [`repository_ready_cached`].
pub async fn repository_maintainable_cached(
    ctx: &Context,
    repo_ref: &RepositoryRef,
    default_ns: &str,
) -> Result<bool> {
    match repo_lookup(repo_ref, default_ns) {
        RepoLookup::Namespaced { namespace, name } => {
            let repo = fetch_repository(ctx, &namespace, &name)
                .await?
                .ok_or_else(|| {
                    Error::MissingDependency(format!("Repository {namespace}/{name}"))
                })?;
            let status = repo.status.as_ref();
            Ok(crate::health::maintenance_may_proceed(
                status.and_then(|s| s.phase.as_ref()),
                status.and_then(|s| s.unique_id.as_deref()),
                status.map(|s| s.conditions.as_slice()).unwrap_or(&[]),
            ))
        }
        RepoLookup::Cluster { name } => {
            let repo = fetch_cluster_repository(ctx, &name)
                .await?
                .ok_or_else(|| Error::MissingDependency(format!("ClusterRepository {name}")))?;
            let status = repo.status.as_ref();
            Ok(crate::health::maintenance_may_proceed(
                status.and_then(|s| s.phase.as_ref()),
                status.and_then(|s| s.unique_id.as_deref()),
                status.map(|s| s.conditions.as_slice()).unwrap_or(&[]),
            ))
        }
    }
}

/// **Pure** (#382 M3, audit C4). Whether a cached `Snapshot` row belongs to
/// the namespaced label population `namespace` + `label_key=label_value`.
///
/// The namespace check is NOT optional: the Snapshot reflector store is
/// install-scope-wide, so a label-only filter would merge another namespace's
/// same-named policy/schedule children into this owner's population — for the
/// retention path that means cross-namespace GFS math and cross-namespace
/// deletion. Every in-process filter that replaces a namespaced label-selector
/// LIST must go through this predicate (or reproduce both checks).
pub fn snapshot_matches(s: &Snapshot, namespace: &str, label_key: &str, label_value: &str) -> bool {
    s.namespace().as_deref() == Some(namespace)
        && s.labels().get(label_key).map(String::as_str) == Some(label_value)
}

/// Enumerate the `Snapshot` children in `namespace` carrying
/// `label_key=label_value`, served from the Snapshot reflector store when it
/// is published AND synced, else via today's live label-selector LIST (#382
/// M3 — this replaces the per-reconcile full-collection LISTs that were the
/// headline apiserver load).
///
/// - **R2**: an unset or not-yet-synced store is NEVER read as "no children"
///   — that would erase a policy's whole population from retention math (and
///   let a schedule's Forbid gate see "no active runs") on a cold cache. The
///   fallback is a live LIST, never a deferral.
/// - **C4**: the store path filters namespace AND label ([`snapshot_matches`]).
/// - **C8**: the store's `Arc` rows are filtered first; only matches are
///   deep-cloned.
///
/// Consumers that select rows for DESTRUCTION from this population must
/// live-verify each selected row first ([`confirm_row_live`], audit C2).
pub(crate) async fn snapshot_children(
    ctx: &Context,
    namespace: &str,
    label_key: &str,
    label_value: &str,
) -> Result<Vec<Snapshot>> {
    let store = ctx.snapshot_store.get();
    if read_from_store(store.is_some(), ctx.snapshot_synced.load(Ordering::Acquire))
        && let Some(store) = store
    {
        return Ok(store
            .state()
            .iter()
            .filter(|row| snapshot_matches(row.as_ref(), namespace, label_key, label_value))
            .map(|row| (**row).clone())
            .collect());
    }
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
    let lp = ListParams::default().labels(&format!("{label_key}={label_value}"));
    Ok(api.list(&lp).await?.items)
}

/// **Pure** (#382 M3, audits C2/C6): the trust-cache-for-selection /
/// verify-live-before-acting skip decision. A row selected from the reflector
/// store is actionable only if the LIVE read still finds it and it is not
/// already terminating:
///
/// - gone (`None`): a prune would 404-race a finished drain, and an adoption
///   CREATE would mint a phantom restore point for a snapshot whose CR
///   vanished (C6);
/// - terminating: its finalizer is already draining — re-deleting is noise,
///   and re-creating (adoption) would resurrect an id that is mid-expiry.
pub fn live_row_actionable(live: Option<&Snapshot>) -> bool {
    match live {
        Some(row) => row.metadata.deletion_timestamp.is_none(),
        None => false,
    }
}

/// Live-verify a store-derived selection before destruction or creation (#382
/// M3, audits C2/C6): one `get_opt` per SELECTED row (never per population
/// row), then the pure [`live_row_actionable`] decision. `Ok(false)` means
/// "skip this row" and has already been debug-logged.
pub(crate) async fn confirm_row_live(api: &Api<Snapshot>, name: &str) -> Result<bool> {
    let live = api.get_opt(name).await?;
    let actionable = live_row_actionable(live.as_ref());
    if !actionable {
        tracing::debug!(
            snapshot = %name,
            "skipping cache-selected row: gone or terminating on live verify (#382 C2/C6)"
        );
    }
    Ok(actionable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use http::{Request, Response, StatusCode};
    use kube::client::Body;
    use kube::runtime::watcher;
    use kube::{Client, ResourceExt};

    // --- read_from_store: the one gating decision, exhaustively -------------

    #[test]
    fn read_from_store_truth_table() {
        // Only present AND synced may consult the cache; everything else goes
        // live (R1 — an unset or cold store must never answer at all, and even
        // a consulted store's MISS is still live-confirmed by the callers).
        assert!(read_from_store(true, true));
        assert!(!read_from_store(true, false), "present but cold: go live");
        assert!(!read_from_store(false, true), "unset store: go live");
        assert!(!read_from_store(false, false));
    }

    // --- fetch_* kernel: zero HTTP on a primed hit; live GET on miss/cold ---

    /// A mock client that logs every request path and answers with `status` +
    /// `body` (the `tower::service_fn` harness, as in `Context::test_context`).
    fn logging_client(
        log: Arc<Mutex<Vec<String>>>,
        status: StatusCode,
        body: serde_json::Value,
    ) -> Client {
        let body = Arc::new(body);
        let svc = tower::service_fn(move |req: Request<Body>| {
            let log = log.clone();
            let body = body.clone();
            async move {
                log.lock().unwrap().push(req.uri().path().to_string());
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&*body).unwrap()))
                        .unwrap(),
                )
            }
        });
        Client::new(svc, "default")
    }

    fn not_found_body() -> serde_json::Value {
        serde_json::json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": "NotFound", "code": 404,
        })
    }

    fn policy_json(ns: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "SnapshotPolicy",
            "metadata": { "name": name, "namespace": ns, "uid": "uid-pol" },
            "spec": { "repository": { "name": "repo" } },
        })
    }

    fn policy(ns: &str, name: &str) -> SnapshotPolicy {
        serde_json::from_value(policy_json(ns, name)).expect("valid SnapshotPolicy fixture")
    }

    /// Prime a context's SnapshotPolicy store with `objs` and mark it synced.
    fn prime_config_store(ctx: &Context, objs: Vec<SnapshotPolicy>, synced: bool) {
        let (store, mut writer) = kube::runtime::reflector::store::<SnapshotPolicy>();
        for o in objs {
            writer.apply_watcher_event(&watcher::Event::Apply(o));
        }
        // Leak the writer so the store stays alive for reads (test-only).
        std::mem::forget(writer);
        ctx.config_store.set(store).ok();
        ctx.config_synced.store(synced, Ordering::Release);
    }

    #[tokio::test]
    async fn fetch_policy_store_hit_makes_no_http_request() {
        let log = Arc::new(Mutex::new(Vec::new()));
        // Any HTTP request would 404 — the object exists ONLY in the store.
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        prime_config_store(&ctx, vec![policy("team-a", "pg")], true);

        let got = fetch_policy(&ctx, "team-a", "pg").await.unwrap();
        assert_eq!(got.expect("hit").name_any(), "pg");
        assert!(
            log.lock().unwrap().is_empty(),
            "a primed-store hit must not touch the API server, got {:?}",
            log.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn fetch_policy_store_miss_is_live_confirmed() {
        let log = Arc::new(Mutex::new(Vec::new()));
        // The store is synced but does NOT contain the object; the live GET
        // returns it (a reflector lag scenario) — the kernel must trust live.
        let client = logging_client(log.clone(), StatusCode::OK, policy_json("team-a", "new"));
        let ctx = Context::test_context(client);
        prime_config_store(&ctx, vec![policy("team-a", "other")], true);

        let got = fetch_policy(&ctx, "team-a", "new").await.unwrap();
        assert_eq!(got.expect("live hit").name_any(), "new");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "a store MISS must be live-confirmed with exactly one GET"
        );
    }

    #[tokio::test]
    async fn fetch_policy_absent_everywhere_is_confirmed_none() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        prime_config_store(&ctx, vec![], true);

        let got = fetch_policy(&ctx, "team-a", "gone").await.unwrap();
        assert!(got.is_none());
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "None must always mean a live-confirmed 404, never a bare store miss"
        );
    }

    #[tokio::test]
    async fn fetch_policy_unsynced_store_goes_live_even_on_would_be_hit() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::OK, policy_json("team-a", "pg"));
        let ctx = Context::test_context(client);
        // Object IS in the store, but the synced flag never flipped: a cold
        // cache must not be consulted at all.
        prime_config_store(&ctx, vec![policy("team-a", "pg")], false);

        let got = fetch_policy(&ctx, "team-a", "pg").await.unwrap();
        assert!(got.is_some());
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "an unsynced store must fall through to a live GET"
        );
    }

    #[tokio::test]
    async fn fetch_policy_namespace_is_part_of_the_store_key() {
        // C4 guard at the point-read level: a same-named policy in ANOTHER
        // namespace must not satisfy this namespace's read — the miss goes
        // live and the live 404 is authoritative.
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        prime_config_store(&ctx, vec![policy("team-b", "pg")], true);

        let got = fetch_policy(&ctx, "team-a", "pg").await.unwrap();
        assert!(got.is_none(), "cross-namespace store row must not hit");
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    fn repository_json(ns: &str, name: &str, phase: Option<&str>) -> serde_json::Value {
        let mut v = serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Repository",
            "metadata": { "name": name, "namespace": ns, "uid": "uid-repo" },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "pw" } },
            },
        });
        if let Some(p) = phase {
            v["status"] = serde_json::json!({ "phase": p });
        }
        v
    }

    fn repository(ns: &str, name: &str, phase: Option<&str>) -> Repository {
        serde_json::from_value(repository_json(ns, name, phase)).expect("valid Repository fixture")
    }

    fn prime_repo_store(ctx: &Context, objs: Vec<Repository>, synced: bool) {
        let (store, mut writer) = kube::runtime::reflector::store::<Repository>();
        for o in objs {
            writer.apply_watcher_event(&watcher::Event::Apply(o));
        }
        std::mem::forget(writer);
        ctx.repo_store.set(store).ok();
        ctx.repo_synced.store(synced, Ordering::Release);
    }

    #[tokio::test]
    async fn fetch_repository_store_hit_makes_no_http_request() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        prime_repo_store(&ctx, vec![repository("team-a", "nas", Some("Ready"))], true);

        let got = fetch_repository(&ctx, "team-a", "nas").await.unwrap();
        assert!(got.is_some());
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fetch_cluster_repository_unset_store_falls_through_live() {
        // A namespaced install never publishes crepo_store: the read must go
        // live (where real RBAC yields the honest 403/404), never "missing".
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);

        let got = fetch_cluster_repository(&ctx, "shared").await.unwrap();
        assert!(got.is_none());
        assert_eq!(log.lock().unwrap().len(), 1, "unset store must read live");
    }

    // --- repository_ready_cached over the kernel ----------------------------

    #[tokio::test]
    async fn repository_ready_cached_reads_phase_from_the_store() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        prime_repo_store(
            &ctx,
            vec![
                repository("team-a", "ready", Some("Ready")),
                repository("team-a", "cold", Some("Initializing")),
                repository("team-a", "bare", None),
            ],
            true,
        );

        let rref = |name: &str| RepositoryRef {
            kind: kopiur_api::common::RepositoryKind::Repository,
            name: name.into(),
            namespace: None,
        };
        assert!(
            repository_ready_cached(&ctx, &rref("ready"), "team-a")
                .await
                .unwrap()
        );
        assert!(
            !repository_ready_cached(&ctx, &rref("cold"), "team-a")
                .await
                .unwrap()
        );
        assert!(
            !repository_ready_cached(&ctx, &rref("bare"), "team-a")
                .await
                .unwrap()
        );
        assert!(
            log.lock().unwrap().is_empty(),
            "all three served from cache"
        );
        // Absent repository: live-confirmed MissingDependency, same shape as
        // the live `repository_ready`.
        let err = repository_ready_cached(&ctx, &rref("gone"), "team-a")
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::MissingDependency(ref m) if m.contains("Repository team-a/gone")),
            "got {err:?}"
        );
    }

    // --- repository_maintainable_cached: the #413 maintenance gate ----------

    /// A Repository fixture with an arbitrary status body (the readiness
    /// fixture above only sets `phase`; the maintenance gate also reads
    /// `uniqueId` and `conditions`).
    fn repository_with_status(ns: &str, name: &str, status: serde_json::Value) -> Repository {
        let mut v = repository_json(ns, name, None);
        v["status"] = status;
        serde_json::from_value(v).expect("valid Repository fixture")
    }

    fn backend_reachable_false(reason: &str) -> serde_json::Value {
        serde_json::json!([{
            "type": "BackendReachable",
            "status": "False",
            "reason": reason,
            "message": "m",
            "lastTransitionTime": "2026-08-27T00:00:00Z",
        }])
    }

    #[tokio::test]
    async fn repository_maintainable_cached_reads_the_gate_inputs_from_the_store() {
        // #413 maintenance deadlock: Degraded-because-slow must not withhold
        // the cure. The gate reads phase + uniqueId + conditions off the
        // cached CR — all from the store, zero HTTP.
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        prime_repo_store(
            &ctx,
            vec![
                repository("team-a", "ready", Some("Ready")),
                repository("team-a", "cold", Some("Initializing")),
                // Degraded + bootstrapped, no BackendReachable=False: the
                // crash-loop / deadline-kill shape — maintenance proceeds.
                repository_with_status(
                    "team-a",
                    "slow",
                    serde_json::json!({ "phase": "Degraded", "uniqueId": "u1" }),
                ),
                // Degraded + bootstrapped + CONFIRMED unreachable: deferred.
                repository_with_status(
                    "team-a",
                    "down",
                    serde_json::json!({
                        "phase": "Degraded",
                        "uniqueId": "u2",
                        "conditions": backend_reachable_false("BackendUnreachable"),
                    }),
                ),
                // Degraded but never bootstrapped: nothing to maintain.
                repository_with_status(
                    "team-a",
                    "unborn",
                    serde_json::json!({ "phase": "Degraded" }),
                ),
            ],
            true,
        );

        let rref = |name: &str| RepositoryRef {
            kind: kopiur_api::common::RepositoryKind::Repository,
            name: name.into(),
            namespace: None,
        };
        let maintainable = |name: &str| {
            let ctx = &ctx;
            let rref = rref(name);
            async move {
                repository_maintainable_cached(ctx, &rref, "team-a")
                    .await
                    .unwrap()
            }
        };
        assert!(maintainable("ready").await);
        assert!(!maintainable("cold").await);
        assert!(
            maintainable("slow").await,
            "#413: Degraded-because-slow gets maintenance"
        );
        assert!(
            !maintainable("down").await,
            "confirmed-unreachable stays deferred"
        );
        assert!(
            !maintainable("unborn").await,
            "never-bootstrapped stays deferred"
        );
        assert!(log.lock().unwrap().is_empty(), "all served from cache");
        // Absent repository: same MissingDependency shape as the readiness gate.
        let err = repository_maintainable_cached(&ctx, &rref("gone"), "team-a")
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::MissingDependency(ref m) if m.contains("Repository team-a/gone")),
            "got {err:?}"
        );
    }

    // --- M3: store-served Snapshot enumeration ------------------------------

    fn snapshot_row(ns: &str, name: &str, labels: &[(&str, &str)]) -> Snapshot {
        let mut s: Snapshot = serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Snapshot",
            "metadata": { "name": name, "namespace": ns, "uid": format!("uid-{ns}-{name}") },
            "spec": {},
        }))
        .expect("valid Snapshot fixture");
        s.metadata.labels = Some(
            labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        s
    }

    fn prime_snapshot_store(ctx: &Context, objs: Vec<Snapshot>, synced: bool) {
        let (store, mut writer) = kube::runtime::reflector::store::<Snapshot>();
        for o in objs {
            writer.apply_watcher_event(&watcher::Event::Apply(o));
        }
        std::mem::forget(writer);
        ctx.snapshot_store.set(store).ok();
        ctx.snapshot_synced.store(synced, Ordering::Release);
    }

    const CONFIG_LABEL: &str = kopiur_api::consts::CONFIG_LABEL;
    const SCHEDULE_LABEL: &str = kopiur_api::consts::SCHEDULE_LABEL;

    #[test]
    fn snapshot_matches_requires_namespace_and_label() {
        // C4 matrix: the store is install-scope-wide, so BOTH checks are
        // load-bearing — a label-only filter would merge another namespace's
        // same-named policy children into this policy's GFS math.
        let row = snapshot_row("team-a", "child", &[(CONFIG_LABEL, "pg")]);
        assert!(snapshot_matches(&row, "team-a", CONFIG_LABEL, "pg"));
        assert!(
            !snapshot_matches(&row, "team-b", CONFIG_LABEL, "pg"),
            "cross-namespace intruder with the matching label must not match"
        );
        assert!(
            !snapshot_matches(&row, "team-a", CONFIG_LABEL, "other"),
            "label value mismatch"
        );
        assert!(
            !snapshot_matches(&row, "team-a", SCHEDULE_LABEL, "pg"),
            "missing label key"
        );
        let unlabeled = snapshot_row("team-a", "bare", &[]);
        assert!(!snapshot_matches(&unlabeled, "team-a", CONFIG_LABEL, "pg"));
    }

    #[tokio::test]
    async fn snapshot_children_store_hit_filters_ns_and_label_without_http() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        prime_snapshot_store(
            &ctx,
            vec![
                snapshot_row("team-a", "mine-1", &[(CONFIG_LABEL, "pg")]),
                snapshot_row("team-a", "mine-2", &[(CONFIG_LABEL, "pg")]),
                // C4 intruder: same label, ANOTHER namespace — must not leak in.
                snapshot_row("team-b", "intruder", &[(CONFIG_LABEL, "pg")]),
                snapshot_row("team-a", "other-policy", &[(CONFIG_LABEL, "redis")]),
                snapshot_row("team-a", "sched-child", &[(SCHEDULE_LABEL, "pg")]),
            ],
            true,
        );

        let mut got: Vec<String> = snapshot_children(&ctx, "team-a", CONFIG_LABEL, "pg")
            .await
            .unwrap()
            .iter()
            .map(|s| s.name_any())
            .collect();
        got.sort();
        assert_eq!(got, vec!["mine-1", "mine-2"]);
        assert!(
            log.lock().unwrap().is_empty(),
            "a synced-store enumeration must not touch the API server"
        );
    }

    #[tokio::test]
    async fn snapshot_children_store_hit_schedule_label_excludes_intruders() {
        // The same C4 guarantee for the SCHEDULE_LABEL call site (the Forbid
        // concurrency gate's population).
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        prime_snapshot_store(
            &ctx,
            vec![
                snapshot_row("team-a", "run-1", &[(SCHEDULE_LABEL, "nightly")]),
                snapshot_row("team-b", "intruder", &[(SCHEDULE_LABEL, "nightly")]),
            ],
            true,
        );
        let got = snapshot_children(&ctx, "team-a", SCHEDULE_LABEL, "nightly")
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name_any(), "run-1");
        assert!(log.lock().unwrap().is_empty());
    }

    fn snapshot_list_body(items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "SnapshotList",
            "metadata": {},
            "items": items,
        })
    }

    #[tokio::test]
    async fn snapshot_children_cold_store_falls_back_to_live_list() {
        // R2: an unsynced store must NEVER be read as "no children" (a cold
        // cache would erase the whole population from retention math). The
        // fallback is a live LIST — never a deferral.
        let log = Arc::new(Mutex::new(Vec::new()));
        let live_row = serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Snapshot",
            "metadata": {
                "name": "live-child", "namespace": "team-a",
                "labels": { (CONFIG_LABEL): "pg" },
            },
            "spec": {},
        });
        let client = logging_client(
            log.clone(),
            StatusCode::OK,
            snapshot_list_body(vec![live_row]),
        );
        let ctx = Context::test_context(client);
        // Rows ARE in the store, but the synced flag never flipped.
        prime_snapshot_store(
            &ctx,
            vec![snapshot_row(
                "team-a",
                "cached-child",
                &[(CONFIG_LABEL, "pg")],
            )],
            false,
        );

        let got = snapshot_children(&ctx, "team-a", CONFIG_LABEL, "pg")
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name_any(), "live-child", "served live, not cached");
        assert_eq!(log.lock().unwrap().len(), 1, "exactly one live LIST");
    }

    #[tokio::test]
    async fn snapshot_children_unset_store_falls_back_to_live_list() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::OK, snapshot_list_body(vec![]));
        let ctx = Context::test_context(client);

        let got = snapshot_children(&ctx, "team-a", CONFIG_LABEL, "pg")
            .await
            .unwrap();
        assert!(got.is_empty());
        assert_eq!(log.lock().unwrap().len(), 1, "unset store must LIST live");
    }

    // --- C2/C6: verify live before destruction/creation ---------------------

    #[test]
    fn live_row_actionable_truth_table() {
        assert!(!live_row_actionable(None), "gone: skip");
        let live = snapshot_row("team-a", "row", &[]);
        assert!(live_row_actionable(Some(&live)));
        let mut terminating = snapshot_row("team-a", "row", &[]);
        terminating.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
            ));
        assert!(
            !live_row_actionable(Some(&terminating)),
            "terminating: skip"
        );
    }

    #[tokio::test]
    async fn confirm_row_live_verifies_with_one_get_and_skips_absent() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
        let ctx = Context::test_context(client);
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), "team-a");

        assert!(!confirm_row_live(&api, "gone").await.unwrap());
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "the C2/C6 guard is exactly one live GET"
        );
    }

    #[tokio::test]
    async fn confirm_row_live_skips_a_terminating_row() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let body = serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Snapshot",
            "metadata": {
                "name": "draining", "namespace": "team-a",
                "deletionTimestamp": "2026-01-01T00:00:00Z",
                "finalizers": ["kopiur.home-operations.com/snapshot"],
            },
            "spec": {},
        });
        let client = logging_client(log.clone(), StatusCode::OK, body);
        let ctx = Context::test_context(client);
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), "team-a");

        assert!(!confirm_row_live(&api, "draining").await.unwrap());
    }

    #[tokio::test]
    async fn confirm_row_live_passes_a_live_row() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let body = serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Snapshot",
            "metadata": { "name": "alive", "namespace": "team-a" },
            "spec": {},
        });
        let client = logging_client(log.clone(), StatusCode::OK, body);
        let ctx = Context::test_context(client);
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), "team-a");

        assert!(confirm_row_live(&api, "alive").await.unwrap());
    }
}
