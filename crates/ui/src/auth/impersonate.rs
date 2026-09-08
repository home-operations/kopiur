//! The impersonation layer: the outermost middleware on every apiserver call.
//!
//! It removes every inbound `Impersonate-*` header before inserting its own, so a
//! caller can never choose the identity kopiur-ui asserts, and hands back a
//! per-identity `kube::Client` from a bounded LRU cache. The bound is not a
//! latency optimisation: each client owns a connection pool, so an unbounded
//! cache would be an unbounded socket and memory footprint keyed by whatever
//! usernames the proxy sends.
//!
//! # Why the layer is outermost, and why the base config is sanitized
//!
//! `kube::client::ClientBuilder::with_layer` wraps the stack it is given, so a
//! layer added last is the *outermost* one and therefore runs **first** on the
//! outbound path. That is what makes the strip total in the direction that
//! matters: a caller's inbound `Impersonate-*` headers are removed before any
//! inner layer — including kube's own `ExtraHeadersLayer` — has seen the request,
//! so nothing downstream can be confused by them.
//!
//! Running first also means this layer cannot *undo* what an inner layer adds
//! afterwards. Exactly one inner layer emits `Impersonate-*`: kube's
//! `ExtraHeadersLayer`, which `extra_headers_layer()` populates from
//! `Config::auth_info.impersonate` / `.impersonate_groups` and from
//! `Config::headers`, and whose `call` does `headers.extend(...)` — an *append*.
//! A base `kube::Config` inferred from a kubeconfig with `as-groups:
//! [system:masters]` would therefore union `system:masters` onto every request
//! this cache ever builds, no matter what identity the layer asserted.
//!
//! [`sanitize_base`] closes that: `ClientCache::new` clears all four
//! `auth_info.impersonate*` fields and drops any `impersonate-*` from
//! `Config::headers`, so the inner layer has nothing left to append. The
//! guarantee is not "we leave those fields alone" — it is "we empty them".
//!
//! The same stack carries `Client::connect`, so a browse session's `pods/exec` is
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

/// Which part of an identity could not be turned into a header.
///
/// An enum rather than a string so a new impersonation header cannot be added
/// without every reader of this error accounting for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityHeaderKind {
    /// `Impersonate-User`.
    User,
    /// `Impersonate-Group`.
    Group,
    /// `Impersonate-Extra-<key>` — the key or one of its values.
    Extra,
}

impl std::fmt::Display for IdentityHeaderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Group => "group",
            Self::Extra => "userextra",
        })
    }
}

/// An identity that cannot be expressed as impersonation headers.
///
/// [`super::identity::extract_identity`] restricts every principal to visible
/// ASCII, so this is unreachable for anything that came through the middleware —
/// it exists because the *failure mode* of not having it is catastrophic. Dropping
/// a bad value and carrying on would send a request with no `Impersonate-User`,
/// which the apiserver executes as **the UI's own ServiceAccount**: a fail-open
/// straight past the impersonation guarantee. Refusing to build the layer at all
/// turns that into a 500.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the caller's {what} {value:?} cannot be sent as an impersonation header. Every apiserver \
     call kopiur-ui makes must carry the caller's identity, and a request missing one would \
     run as the UI's own ServiceAccount instead — so it is refused rather than sent. \
     Fix: this is a bug in kopiur-ui (identity extraction should already have rejected this \
     value); report it at https://github.com/home-operations/kopiur/issues"
)]
pub struct InvalidIdentityHeader {
    /// Which impersonation header could not be built.
    pub what: IdentityHeaderKind,
    /// The offending value (or, for an extra, the offending key).
    pub value: String,
}

/// Asserts one identity on every request that passes through it.
///
/// The header list is computed once, when the layer is built, so the per-request
/// work is a fixed number of `remove`/`append` calls with no allocation.
#[derive(Clone, Debug)]
pub struct ImpersonateLayer {
    headers: Arc<[(HeaderName, HeaderValue)]>,
}

impl ImpersonateLayer {
    /// Build the layer that asserts `identity`, emitting only the `userextras`
    /// keys named in `extra_keys`.
    ///
    /// Fails — rather than degrading — on any value that cannot become a header;
    /// see [`InvalidIdentityHeader`] for why a partial identity is worse than no
    /// request at all.
    pub fn new(identity: &Identity, extra_keys: &[String]) -> Result<Self, InvalidIdentityHeader> {
        let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();

        headers.push((
            HeaderName::from_static(IMPERSONATE_USER),
            header_value(&identity.user, IdentityHeaderKind::User)?,
        ));
        for group in &identity.groups {
            headers.push((
                HeaderName::from_static(IMPERSONATE_GROUP),
                header_value(group, IdentityHeaderKind::Group)?,
            ));
        }
        for key in extra_keys {
            let Some(values) = identity.extra.get(key) else {
                continue;
            };
            let name =
                HeaderName::try_from(format!("{IMPERSONATE_EXTRA_PREFIX}{key}")).map_err(|_| {
                    InvalidIdentityHeader {
                        what: IdentityHeaderKind::Extra,
                        value: key.clone(),
                    }
                })?;
            for value in values {
                headers.push((
                    name.clone(),
                    header_value(value, IdentityHeaderKind::Extra)?,
                ));
            }
        }

        Ok(Self {
            headers: headers.into(),
        })
    }

    /// The headers this layer will assert, in the order it asserts them.
    ///
    /// Exposed so the impersonation contract can be asserted directly, without
    /// standing up a service stack.
    pub fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }
}

/// Convert one identity value into a header value.
fn header_value(
    value: &str,
    what: IdentityHeaderKind,
) -> Result<HeaderValue, InvalidIdentityHeader> {
    HeaderValue::from_str(value).map_err(|_| InvalidIdentityHeader {
        what,
        value: value.to_string(),
    })
}

/// Strip every impersonation instruction out of a base `kube::Config`.
///
/// The UI's own credentials are the only thing the base config is allowed to
/// carry. Anything in `auth_info.impersonate*` — a kubeconfig with `as:` /
/// `as-groups:`, most plausibly a developer's own file picked up by
/// `Config::infer` — is emitted by kube's inner `ExtraHeadersLayer` and *appended*
/// to whatever [`ImpersonateLayer`] asserted, so it would silently union extra
/// groups (up to and including `system:masters`) onto every caller's requests.
/// The same goes for a hand-set `impersonate-*` in `Config::headers`.
///
/// Called by [`ClientCache::new`], so no caller has to remember it.
pub fn sanitize_base(mut config: kube::Config) -> kube::Config {
    config.auth_info.impersonate = None;
    config.auth_info.impersonate_uid = None;
    config.auth_info.impersonate_groups = None;
    config.auth_info.impersonate_user_extra = None;
    config
        .headers
        .retain(|(name, _)| !name.as_str().starts_with(IMPERSONATE_PREFIX));
    config
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

/// Why a per-identity client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ClientBuildError {
    /// The identity cannot be expressed as impersonation headers.
    #[error(transparent)]
    InvalidIdentity(#[from] InvalidIdentityHeader),

    /// kube refused to build a client from the base configuration (TLS material,
    /// proxy settings, a malformed cluster URL).
    #[error(
        "kopiur-ui could not build a Kubernetes client from its own configuration: {0}. \
         Fix: check the UI's ServiceAccount token mount and any KUBECONFIG/proxy settings on \
         the Deployment, then restart it"
    )]
    Kube(#[from] kube::Error),
}

impl ClientCache {
    /// Build a cache over `base` — the in-cluster (or inferred) config `main`
    /// resolved, whose credentials are the UI's own ServiceAccount.
    ///
    /// `base` is passed through [`sanitize_base`] first: any impersonation the
    /// configuration itself carries would be *appended* by kube's inner
    /// `ExtraHeadersLayer` to whatever identity a client asserts, so it is cleared
    /// here rather than assumed absent.
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
            base: sanitize_base(base),
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
    pub fn client_for(&self, identity: &Identity) -> Result<kube::Client, ClientBuildError> {
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

    /// Build one impersonating client from the sanitized base config.
    fn build(&self, identity: &Identity) -> Result<kube::Client, ClientBuildError> {
        let layer = ImpersonateLayer::new(identity, &self.extra_keys)?;
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

        let headers = through(
            ImpersonateLayer::new(&id, &[]).expect("valid identity"),
            req,
        )
        .await;

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
            ImpersonateLayer::new(&id, &[]).expect("valid identity"),
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
            ImpersonateLayer::new(&id, &["email".to_string()]).expect("valid identity"),
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
            ImpersonateLayer::new(&id, &["email".to_string()]).expect("valid identity"),
            Request::builder().body(()).expect("test request"),
        )
        .await;

        assert_eq!(headers.len(), 2, "{headers:?}");
    }

    #[test]
    fn an_identity_that_cannot_be_a_header_is_refused_rather_than_partially_sent() {
        // A request with no Impersonate-User runs as the UI's ServiceAccount, so
        // "drop the bad value and carry on" would be a privilege escalation.
        let bad_user = ImpersonateLayer::new(&identity("alice\nbob", &[]), &[])
            .expect_err("a control character in the user must not build a layer");
        assert_eq!(bad_user.what, IdentityHeaderKind::User);
        assert_eq!(bad_user.value, "alice\nbob");

        let bad_group = ImpersonateLayer::new(&identity("alice", &["ops\r\nx"]), &[])
            .expect_err("a control character in a group must not build a layer");
        assert_eq!(bad_group.what, IdentityHeaderKind::Group);

        let mut with_bad_extra = identity("alice", &[]);
        with_bad_extra
            .extra
            .insert("email".to_string(), vec!["a\u{0}b".to_string()]);
        let bad_extra = ImpersonateLayer::new(&with_bad_extra, &["email".to_string()])
            .expect_err("a control character in an extra must not build a layer");
        assert_eq!(bad_extra.what, IdentityHeaderKind::Extra);

        let mut bad_key = identity("alice", &[]);
        bad_key
            .extra
            .insert("not a header".to_string(), vec!["x".to_string()]);
        let bad_key = ImpersonateLayer::new(&bad_key, &["not a header".to_string()])
            .expect_err("an extras key that is not a header name must not build a layer");
        assert_eq!(bad_key.what, IdentityHeaderKind::Extra);
        assert_eq!(bad_key.value, "not a header");

        assert!(bad_user.to_string().contains("Fix:"), "{bad_user}");
    }

    // --- sanitize_base ------------------------------------------------------

    /// A base config that impersonates on its own behalf — a developer's
    /// kubeconfig with `as-groups: [system:masters]`, most plausibly.
    fn impersonating_base() -> kube::Config {
        let mut cfg = base_config();
        cfg.auth_info.impersonate = Some("cluster-admin".to_string());
        cfg.auth_info.impersonate_uid = Some("1".to_string());
        cfg.auth_info.impersonate_groups = Some(vec!["system:masters".to_string()]);
        cfg.auth_info.impersonate_user_extra = Some(
            [("scopes".to_string(), vec!["everything".to_string()])]
                .into_iter()
                .collect(),
        );
        cfg.headers.push((
            HeaderName::from_static("impersonate-group"),
            HeaderValue::from_static("system:masters"),
        ));
        cfg.headers.push((
            HeaderName::from_static("x-audit-id"),
            HeaderValue::from_static("keep-me"),
        ));
        cfg
    }

    #[test]
    fn sanitize_base_empties_every_impersonation_the_config_carries() {
        let clean = sanitize_base(impersonating_base());

        assert_eq!(clean.auth_info.impersonate, None);
        assert_eq!(clean.auth_info.impersonate_uid, None);
        assert_eq!(clean.auth_info.impersonate_groups, None);
        assert_eq!(clean.auth_info.impersonate_user_extra, None);
        assert!(
            !clean
                .headers
                .iter()
                .any(|(name, _)| name.as_str().starts_with("impersonate-")),
            "{:?}",
            clean.headers
        );
        assert!(
            clean.headers.iter().any(|(name, _)| name == "x-audit-id"),
            "unrelated static headers are the operator's, not ours to drop"
        );
    }

    #[test]
    fn a_cache_built_over_an_impersonating_config_sanitizes_it() {
        let cache = ClientCache::new(impersonating_base(), limits(8, 600), Vec::new());
        assert_eq!(cache.base.auth_info.impersonate_groups, None);
        assert_eq!(cache.base.auth_info.impersonate, None);

        // And the only thing left that can emit Impersonate-* is the layer, whose
        // set is exactly the identity's.
        let layer =
            ImpersonateLayer::new(&identity("alice", &["ops"]), &[]).expect("valid identity");
        let emitted: Vec<String> = layer
            .headers()
            .iter()
            .map(|(name, value)| format!("{}: {}", name.as_str(), value.to_str().expect("ascii")))
            .collect();
        assert_eq!(
            emitted,
            vec![
                "impersonate-user: alice".to_string(),
                "impersonate-group: ops".to_string(),
            ]
        );
        assert!(
            !emitted.iter().any(|h| h.contains("system:masters")),
            "{emitted:?}"
        );
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
