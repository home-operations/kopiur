//! The impersonation layer: the outermost middleware on every apiserver call.
//!
//! It removes every inbound `Impersonate-*` header before inserting its own, so a
//! caller can never choose the identity kopiur-ui asserts, and hands back a
//! per-identity `kube::Client` from a bounded LRU cache. The bound is not a
//! latency optimisation: each client owns a connection pool, so an unbounded
//! cache would be an unbounded socket and memory footprint keyed by whatever
//! usernames the proxy sends.
//!
//! # Why the layer is outermost, and why it strips before it writes
//!
//! `kube::client::ClientBuilder::with_layer` wraps the stack it is given, so a
//! layer added last runs *first* on the way out — outside kube's own
//! `extra_headers_layer`, which is the only other thing that would emit
//! `Impersonate-*` (and does so solely from `Config::auth_info.impersonate*`,
//! which kopiur-ui leaves `None`). Being outermost is what makes the removal
//! total: no inner layer can have added a header this one did not see. The same
//! stack carries `Client::connect`, so a browse session's `pods/exec` is
//! impersonated exactly like a `GET`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::Request;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use tower::{Layer, Service};

use crate::config::CacheLimits;

use super::identity::Identity;

/// Prefix of every header the apiserver reads as an impersonation instruction.
const IMPERSONATE_PREFIX: &str = "impersonate-";

/// `Impersonate-User`.
const IMPERSONATE_USER: &str = "impersonate-user";

/// `Impersonate-Group`, repeated once per group.
const IMPERSONATE_GROUP: &str = "impersonate-group";

/// Prefix of `Impersonate-Extra-<key>`.
const IMPERSONATE_EXTRA_PREFIX: &str = "impersonate-extra-";

/// Asserts one identity on every request that passes through it.
///
/// The header list is computed once, when the layer is built, so the per-request
/// work is a fixed number of `remove`/`append` calls with no allocation.
#[derive(Clone)]
pub struct ImpersonateLayer {
    headers: Arc<[(HeaderName, HeaderValue)]>,
}

impl ImpersonateLayer {
    /// Build the layer that asserts `identity`, emitting only the `userextras`
    /// keys named in `extra_keys`.
    ///
    /// A value that cannot become a `HeaderValue` is dropped with a warning
    /// rather than panicking: [`super::identity::extract_identity`] already
    /// restricts principals to visible ASCII, so this is unreachable for anything
    /// that came through the middleware, and a hand-built identity should degrade
    /// to *fewer* permissions, never to a crashed request.
    pub fn new(identity: &Identity, extra_keys: &[String]) -> Self {
        let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();

        if let Some(value) = header_value(&identity.user, "user") {
            headers.push((HeaderName::from_static(IMPERSONATE_USER), value));
        }
        for group in &identity.groups {
            if let Some(value) = header_value(group, "group") {
                headers.push((HeaderName::from_static(IMPERSONATE_GROUP), value));
            }
        }
        for key in extra_keys {
            let Some(values) = identity.extra.get(key) else {
                continue;
            };
            let Ok(name) = HeaderName::try_from(format!("{IMPERSONATE_EXTRA_PREFIX}{key}")) else {
                tracing::warn!(
                    key,
                    "userextras key is not a valid header name; not impersonated"
                );
                continue;
            };
            for value in values {
                if let Some(value) = header_value(value, "userextra") {
                    headers.push((name.clone(), value));
                }
            }
        }

        Self {
            headers: headers.into(),
        }
    }

    /// The headers this layer will assert, in the order it asserts them.
    ///
    /// Exposed so the impersonation contract can be asserted directly, without
    /// standing up a service stack.
    pub fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }
}

/// Convert one identity value into a header value, warning if it cannot be.
fn header_value(value: &str, what: &'static str) -> Option<HeaderValue> {
    match HeaderValue::from_str(value) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(
                what,
                "identity value cannot be sent as a header; not impersonated"
            );
            None
        }
    }
}

impl<S> Layer<S> for ImpersonateLayer {
    type Service = Impersonate<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Impersonate {
            inner,
            headers: Arc::clone(&self.headers),
        }
    }
}

/// The service [`ImpersonateLayer`] produces.
#[derive(Clone)]
pub struct Impersonate<S> {
    inner: S,
    headers: Arc<[(HeaderName, HeaderValue)]>,
}

impl<S, B> Service<Request<B>> for Impersonate<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        set_impersonation(req.headers_mut(), &self.headers);
        self.inner.call(req)
    }
}

/// Replace every `Impersonate-*` header with exactly `headers`.
///
/// Strip first, then write. Removing by name clears *all* values under that name,
/// which is what makes a repeated `Impersonate-Group` from a caller disappear
/// rather than merge with ours.
fn set_impersonation(headers: &mut HeaderMap, wanted: &[(HeaderName, HeaderValue)]) {
    let inbound: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with(IMPERSONATE_PREFIX))
        .cloned()
        .collect();
    for name in inbound {
        headers.remove(&name);
    }
    for (name, value) in wanted {
        headers.append(name, value.clone());
    }
}

/// The canonical form of an [`Identity`], used as the cache key.
///
/// Two callers share a client only if the apiserver would see them as the same
/// subject: same user, same set of groups, same `userextras`. Ordering is
/// normalised so that a proxy listing groups in a different order on two requests
/// does not build two clients — `Identity` already sorts and dedupes its groups,
/// and this re-establishes the invariant for identities built by other means.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IdentityKey {
    user: String,
    groups: Vec<String>,
    extras: Vec<(String, Vec<String>)>,
}

impl IdentityKey {
    fn new(identity: &Identity) -> Self {
        let mut groups = identity.groups.clone();
        groups.sort_unstable();
        groups.dedup();
        // `extra` is a BTreeMap, so key order is already canonical; only the
        // values need normalising.
        let extras = identity
            .extra
            .iter()
            .map(|(k, v)| {
                let mut values = v.clone();
                values.sort_unstable();
                values.dedup();
                (k.clone(), values)
            })
            .collect();
        Self {
            user: identity.user.clone(),
            groups,
            extras,
        }
    }
}

/// One cached client and when it was last handed out.
struct Entry {
    client: kube::Client,
    last_used: Instant,
}

/// A bounded, idle-expiring cache of impersonating `kube::Client`s.
///
/// Hand-rolled rather than pulled from a crate because the policy is three lines
/// and the alternative is a new dependency in the one crate whose dependency
/// surface is a security argument. `std::sync::Mutex`, not `tokio`'s: every
/// operation is a map lookup plus, on a miss, building a client — no `.await`
/// ever happens while the lock is held, so the lock cannot be held across a yield
/// point.
pub struct ClientCache {
    base: kube::Config,
    limits: CacheLimits,
    extra_keys: Arc<[String]>,
    entries: Mutex<HashMap<IdentityKey, Entry>>,
    /// Injectable so the idle-expiry policy is testable without sleeping.
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
}

impl std::fmt::Debug for ClientCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCache")
            .field("cluster_url", &self.base.cluster_url)
            .field("limits", &self.limits)
            .field("extra_keys", &self.extra_keys)
            .field("entries", &self.len())
            .finish()
    }
}

impl ClientCache {
    /// Build a cache over `base` — the in-cluster (or inferred) config `main`
    /// resolved, whose credentials are the UI's own ServiceAccount.
    ///
    /// `base.auth_info.impersonate*` is left untouched (and must stay `None`):
    /// kube's own `extra_headers_layer` emits `Impersonate-User`/`-Group` from
    /// those fields, and a value there would be asserted on *every* client,
    /// including ones built for a different identity.
    pub fn new(base: kube::Config, limits: CacheLimits, extra_keys: Vec<String>) -> Self {
        Self::with_clock(base, limits, extra_keys, Box::new(Instant::now))
    }

    /// [`ClientCache::new`] with the clock the idle-expiry policy reads.
    pub fn with_clock(
        base: kube::Config,
        limits: CacheLimits,
        extra_keys: Vec<String>,
        clock: Box<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self {
            base,
            limits,
            extra_keys: extra_keys.into(),
            entries: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// The client that speaks to the apiserver as `identity`.
    ///
    /// Cheap and safe to call per request: a hit clones an `Arc`-backed
    /// `kube::Client`, and a miss builds one under the lock so that two
    /// simultaneous first requests from the same person cannot produce two
    /// connection pools.
    pub fn client_for(&self, identity: &Identity) -> Result<kube::Client, kube::Error> {
        let now = (self.clock)();
        let key = IdentityKey::new(identity);
        let mut entries = self.lock();
        Self::expire(&mut entries, now, self.limits.ttl);

        if let Some(entry) = entries.get_mut(&key) {
            entry.last_used = now;
            return Ok(entry.client.clone());
        }

        let client = self.build(identity)?;
        // A size of zero disables caching outright rather than meaning "one":
        // every request then builds its own client, which is slow but correct.
        if self.limits.size > 0 {
            Self::make_room(&mut entries, self.limits.size);
            entries.insert(
                key,
                Entry {
                    client: client.clone(),
                    last_used: now,
                },
            );
        }
        Ok(client)
    }

    /// Whether a client for this identity is currently cached.
    #[cfg(test)]
    fn contains(&self, identity: &Identity) -> bool {
        let now = (self.clock)();
        let mut entries = self.lock();
        Self::expire(&mut entries, now, self.limits.ttl);
        entries.contains_key(&IdentityKey::new(identity))
    }

    /// How many clients are currently cached, after expiring idle entries.
    ///
    /// Reported on `kopiur_ui_identity_cache_size`; expiring first means that
    /// gauge tracks what is actually held rather than what was once inserted.
    pub fn len(&self) -> usize {
        let now = (self.clock)();
        let mut entries = self.lock();
        Self::expire(&mut entries, now, self.limits.ttl);
        entries.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build one impersonating client from the base config.
    fn build(&self, identity: &Identity) -> Result<kube::Client, kube::Error> {
        let layer = ImpersonateLayer::new(identity, &self.extra_keys);
        Ok(kube::client::ClientBuilder::try_from(self.base.clone())?
            .with_layer(&layer)
            .build())
    }

    /// Take the entries lock, recovering from a poisoned mutex.
    ///
    /// A panic while the lock was held cannot have left a *torn* map — the guarded
    /// value is a plain `HashMap` and every mutation is a single call — so
    /// refusing to serve requests forever afterwards would turn one panicked
    /// request into an outage.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IdentityKey, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop entries that have not been used within `ttl`.
    ///
    /// Idle expiry, not absolute: a person browsing continuously keeps one client,
    /// and someone who logged off an hour ago stops occupying a connection pool.
    fn expire(entries: &mut HashMap<IdentityKey, Entry>, now: Instant, ttl: Duration) {
        entries.retain(|_, entry| now.saturating_duration_since(entry.last_used) < ttl);
    }

    /// Evict least-recently-used entries until one more fits under `size`.
    fn make_room(entries: &mut HashMap<IdentityKey, Entry>, size: usize) {
        while entries.len() >= size {
            let Some(victim) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            entries.remove(&victim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicU64, Ordering};

    use http::Response;
    use kopiur_ui_model::identity::IdentitySource;
    use tower::{ServiceExt as _, service_fn};

    fn identity(user: &str, groups: &[&str]) -> Identity {
        Identity {
            user: user.to_string(),
            groups: groups.iter().map(|g| (*g).to_string()).collect(),
            email: None,
            extra: BTreeMap::new(),
            source: IdentitySource::TrustedHeaders,
        }
    }

    /// Every value of `name` on the request the inner service received.
    fn values(headers: &HeaderMap, name: &str) -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().expect("ascii header").to_string())
            .collect()
    }

    /// Run one request through the layer and hand back the headers the inner
    /// service saw.
    async fn through(layer: ImpersonateLayer, req: Request<()>) -> HeaderMap {
        let captured = Arc::new(Mutex::new(HeaderMap::new()));
        let sink = {
            let captured = Arc::clone(&captured);
            service_fn(move |req: Request<()>| {
                let captured = Arc::clone(&captured);
                async move {
                    *captured.lock().expect("capture lock") = req.headers().clone();
                    Ok::<_, Infallible>(Response::new(()))
                }
            })
        };

        layer
            .layer(sink)
            .oneshot(req)
            .await
            .expect("the sink never fails");

        let captured = captured.lock().expect("capture lock");
        captured.clone()
    }

    #[tokio::test]
    async fn inbound_impersonation_headers_are_removed_and_replaced() {
        let id = identity("alice", &["ops", "dev", "system:authenticated"]);
        let req = Request::builder()
            .header("impersonate-user", "evil")
            .header("impersonate-group", "system:masters")
            .header("impersonate-extra-scopes", "everything")
            .header("x-unrelated", "kept")
            .body(())
            .expect("test request");

        let headers = through(ImpersonateLayer::new(&id, &[]), req).await;

        assert_eq!(values(&headers, IMPERSONATE_USER), vec!["alice"]);
        assert_eq!(
            values(&headers, IMPERSONATE_GROUP),
            vec!["ops", "dev", "system:authenticated"],
            "the identity's groups, in the identity's order, and nothing else"
        );
        assert!(
            !values(&headers, IMPERSONATE_GROUP).contains(&"system:masters".to_string()),
            "an inbound Impersonate-Group must never survive"
        );
        assert!(
            headers.get("impersonate-extra-scopes").is_none(),
            "every impersonate-* header is stripped, not just the ones we set"
        );
        assert_eq!(values(&headers, "x-unrelated"), vec!["kept"]);
    }

    #[tokio::test]
    async fn groups_are_emitted_in_the_identitys_canonical_sorted_order() {
        // What `extract_identity` produces: sorted and deduped.
        let id = identity("alice", &["dev", "ops", "system:authenticated"]);
        let headers = through(
            ImpersonateLayer::new(&id, &[]),
            Request::builder().body(()).expect("test request"),
        )
        .await;

        assert_eq!(
            values(&headers, IMPERSONATE_GROUP),
            vec!["dev", "ops", "system:authenticated"]
        );
    }

    #[tokio::test]
    async fn only_configured_extra_keys_are_emitted() {
        let mut id = identity("alice", &["system:authenticated"]);
        id.extra
            .insert("email".to_string(), vec!["alice@example.com".to_string()]);
        id.extra
            .insert("scopes".to_string(), vec!["openid".to_string()]);

        let headers = through(
            ImpersonateLayer::new(&id, &["email".to_string()]),
            Request::builder().body(()).expect("test request"),
        )
        .await;

        assert_eq!(
            values(&headers, "impersonate-extra-email"),
            vec!["alice@example.com"]
        );
        assert!(
            headers.get("impersonate-extra-scopes").is_none(),
            "an extra the ClusterRole does not enumerate must not be asserted"
        );
    }

    #[tokio::test]
    async fn an_identity_with_no_extras_emits_only_user_and_groups() {
        let id = identity("alice", &["system:authenticated"]);
        let headers = through(
            ImpersonateLayer::new(&id, &["email".to_string()]),
            Request::builder().body(()).expect("test request"),
        )
        .await;

        assert_eq!(headers.len(), 2, "{headers:?}");
    }

    // --- ClientCache --------------------------------------------------------

    fn base_config() -> kube::Config {
        // `ClientBuilder::try_from` builds a rustls connector even for an http
        // URL, and rustls refuses to construct one without a process-wide crypto
        // provider. `main` installs ring before anything else; tests must too.
        static PROVIDER: std::sync::Once = std::sync::Once::new();
        PROVIDER.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        // Never connected to: building a client only constructs the stack.
        kube::Config::new("http://127.0.0.1:1/".parse().expect("test cluster url"))
    }

    fn limits(size: usize, ttl_secs: u64) -> CacheLimits {
        CacheLimits {
            size,
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// A clock the test moves by hand.
    struct TestClock {
        origin: Instant,
        offset: Arc<AtomicU64>,
    }

    impl TestClock {
        fn new() -> Self {
            Self {
                origin: Instant::now(),
                offset: Arc::new(AtomicU64::new(0)),
            }
        }

        fn handle(&self) -> Arc<AtomicU64> {
            Arc::clone(&self.offset)
        }

        fn as_fn(&self) -> Box<dyn Fn() -> Instant + Send + Sync> {
            let origin = self.origin;
            let offset = Arc::clone(&self.offset);
            Box::new(move || origin + Duration::from_secs(offset.load(Ordering::SeqCst)))
        }
    }

    fn cache(size: usize, ttl_secs: u64, clock: &TestClock) -> ClientCache {
        ClientCache::with_clock(
            base_config(),
            limits(size, ttl_secs),
            Vec::new(),
            clock.as_fn(),
        )
    }

    #[tokio::test]
    async fn a_repeated_identity_is_one_cached_client() {
        let clock = TestClock::new();
        let cache = cache(8, 600, &clock);
        let id = identity("alice", &["ops", "system:authenticated"]);

        cache.client_for(&id).expect("first build");
        assert_eq!(cache.len(), 1);
        cache.client_for(&id).expect("cache hit");
        assert_eq!(cache.len(), 1, "the same identity must not build twice");

        cache
            .client_for(&identity("bob", &["system:authenticated"]))
            .expect("second identity");
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn the_key_is_insensitive_to_group_order_and_repetition() {
        let clock = TestClock::new();
        let cache = cache(8, 600, &clock);

        cache
            .client_for(&identity("alice", &["ops", "dev"]))
            .expect("build");
        cache
            .client_for(&identity("alice", &["dev", "ops"]))
            .expect("build");
        cache
            .client_for(&identity("alice", &["ops", "dev", "ops"]))
            .expect("build");

        assert_eq!(cache.len(), 1, "one subject is one client");
    }

    #[tokio::test]
    async fn a_different_group_set_is_a_different_subject() {
        let clock = TestClock::new();
        let cache = cache(8, 600, &clock);

        cache
            .client_for(&identity("alice", &["ops"]))
            .expect("build");
        cache
            .client_for(&identity("alice", &["ops", "admins"]))
            .expect("build");

        assert_eq!(
            cache.len(),
            2,
            "one extra group changes what the apiserver authorizes"
        );
    }

    #[tokio::test]
    async fn the_cache_never_grows_past_its_size_and_evicts_the_least_recently_used() {
        let clock = TestClock::new();
        let offset = clock.handle();
        let cache = cache(2, 600, &clock);

        let alice = identity("alice", &[]);
        let bob = identity("bob", &[]);
        let carol = identity("carol", &[]);

        cache.client_for(&alice).expect("build");
        offset.store(1, Ordering::SeqCst);
        cache.client_for(&bob).expect("build");
        offset.store(2, Ordering::SeqCst);
        // Touch alice so bob is the least recently used.
        cache.client_for(&alice).expect("hit");
        assert_eq!(cache.len(), 2);

        offset.store(3, Ordering::SeqCst);
        cache.client_for(&carol).expect("build");

        assert_eq!(cache.len(), 2, "the bound is absolute");
        assert!(!cache.contains(&bob), "the least recently used one goes");
        assert!(cache.contains(&alice), "the one touched at t=2 stays");
        assert!(cache.contains(&carol));
    }

    #[tokio::test]
    async fn an_idle_client_is_dropped_after_the_ttl() {
        let clock = TestClock::new();
        let offset = clock.handle();
        let cache = cache(8, 60, &clock);

        cache.client_for(&identity("alice", &[])).expect("build");
        assert_eq!(cache.len(), 1);

        offset.store(59, Ordering::SeqCst);
        assert_eq!(cache.len(), 1, "still within the idle window");

        offset.store(60, Ordering::SeqCst);
        assert_eq!(cache.len(), 0, "an idle client releases its connections");
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn using_a_client_refreshes_its_idle_deadline() {
        let clock = TestClock::new();
        let offset = clock.handle();
        let cache = cache(8, 60, &clock);
        let alice = identity("alice", &[]);

        cache.client_for(&alice).expect("build");
        offset.store(59, Ordering::SeqCst);
        cache.client_for(&alice).expect("hit");
        offset.store(118, Ordering::SeqCst);

        assert_eq!(cache.len(), 1, "the hit at t=59 reset the deadline");
    }
}
