//! Per-identity authorization for cache-backed reads.
//!
//! Objects in the shared stores were fetched by the UI's own ServiceAccount, so
//! the apiserver did not filter them for the caller — which means the cache path
//! has to re-establish what the impersonated path would have got for free. Every
//! cache read is gated by a `SubjectAccessReview` for that identity (cluster-wide
//! first, then per-namespace over the namespaces the stores actually hold), with
//! a short TTL so a revoked RoleBinding stops mattering in seconds.
//!
//! # The invariant
//!
//! A cache-backed answer must be a subset of what an impersonated request would
//! have returned. Everything here exists to keep that true:
//!
//! * The decision cache is keyed by the **whole** identity — user, groups *and*
//!   `extra` — so two callers who differ in any impersonated attribute can never
//!   share an answer. See [`SarKey`].
//! * A missing or ambiguous `status` fails **closed**. An authorizer that
//!   errored, or one with no opinion, is not an allow.
//! * The TTL is the only staleness window. It is deliberately short (the
//!   `KOPIUR_UI_SAR_TTL` default is a minute) because it is the lag between
//!   revoking someone's access and the UI honoring that.
//!
//! There is no negative-vs-positive asymmetry: denials are cached as eagerly as
//! allows. A denial that is not cached is a free amplification lever — an
//! unauthorized caller could turn one page load into a SAR per namespace on
//! every refresh.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::{StreamExt as _, TryStreamExt as _};
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use kube::api::{Api, PostParams};

use kopiur_ops::{OpsError, classify_kube};

use crate::auth::identity::Identity;
use crate::metrics::UiMetrics;

/// The API group every Kopiur kind lives in. A `SubjectAccessReview` that named
/// the wrong group would ask about a different resource entirely and answer
/// confidently about nothing.
pub const KOPIUR_GROUP: &str = "kopiur.home-operations.com";

/// How much of a kind the caller may see.
///
/// Two variants, not a bare `BTreeSet`: "may list cluster-wide" is genuinely
/// different from "may list in exactly these namespaces". Collapsing them would
/// mean re-deriving the cluster-wide case from a namespace enumeration, which is
/// wrong the moment a namespace appears that the store has not seen yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// The caller may list this resource cluster-wide. Everything in the store
    /// is theirs to see, including cluster-scoped objects.
    All,
    /// The caller may list this resource only in these namespaces. Objects
    /// outside the set — and every cluster-scoped object, which has no
    /// namespace to match — are withheld.
    Namespaces(BTreeSet<String>),
}

/// The identity + question a cached authorization decision answers.
///
/// Every field is part of the question, which is why the key is this wide.
/// `groups` and `extra` are here because impersonation asserts them and an
/// authorizer may key on them: two callers with the same username but different
/// groups are different subjects, and sharing a decision between them would let
/// one inherit the other's access. `groups` is canonicalized (sorted, deduped)
/// on construction so that the same subject spelled in a different header order
/// is one cache entry rather than two.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SarKey {
    /// The impersonated username.
    pub user: String,
    /// The impersonated groups, sorted and deduped — a group list is a set, so
    /// header order must not fork the cache.
    pub groups: Vec<String>,
    /// The impersonated `userextras`. A `BTreeMap` so iteration (and therefore
    /// hashing) is deterministic.
    pub extra: BTreeMap<String, Vec<String>>,
    /// The verb asked about (`list`, `get`).
    pub verb: &'static str,
    /// The API group — always [`KOPIUR_GROUP`] today, but carried explicitly so
    /// the key stays honest if that ever stops being true.
    pub group: &'static str,
    /// The plural resource name (`snapshots`, `clusterrepositories`, …).
    pub resource: &'static str,
    /// The namespace asked about; `None` means cluster-wide.
    pub namespace: Option<String>,
    /// The object name asked about; `None` means the whole collection.
    pub name: Option<String>,
}

impl SarKey {
    /// Build the canonical key for one authorization question.
    #[must_use]
    pub fn new(
        id: &Identity,
        verb: &'static str,
        resource: &'static str,
        namespace: Option<&str>,
        name: Option<&str>,
    ) -> Self {
        let mut groups = id.groups.clone();
        groups.sort_unstable();
        groups.dedup();

        Self {
            user: id.user.clone(),
            groups,
            extra: id.extra.clone(),
            verb,
            group: KOPIUR_GROUP,
            resource,
            namespace: namespace.map(str::to_owned),
            name: name.map(str::to_owned),
        }
    }
}

/// Reads the wall clock. Injected so TTL expiry is testable without sleeping.
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Posts one `SubjectAccessReview` and reports the answer.
///
/// A seam, not an abstraction: production has exactly one implementation (the
/// apiserver). It exists so the cache's own behaviour — single-flight, TTL,
/// eviction — can be tested by counting how many reviews actually escape it,
/// which is the only way to prove a de-duplication actually de-duplicates.
type Reviewer = Arc<dyn Fn(SarKey) -> BoxFuture<'static, Result<bool, OpsError>> + Send + Sync>;

/// One cached decision.
///
/// `stored_at` is the moment the apiserver answered and is **never** refreshed
/// on a read. That is the difference between this and an idle-expiring cache
/// like `ClientCache`: the TTL here is the window in which a revoked RoleBinding
/// is still honoured, so extending it on use would let a continuously-active
/// session keep a stale allow forever — exactly the caller whose access being
/// revoked matters most.
#[derive(Debug, Clone, Copy)]
struct Decision {
    allowed: bool,
    stored_at: Instant,
}

/// The bounded, TTL'd decision map, with no apiserver attached.
///
/// Split out from [`SarCache`] precisely so the expiry and eviction rules — the
/// parts that can silently over-serve a stale allow, or grow without limit — are
/// unit-testable with a fake clock and no `kube::Client`.
struct Decisions {
    ttl: Duration,
    /// Maximum entries retained. `0` disables caching outright (every question
    /// re-asks the apiserver), which is correct-but-slow rather than unbounded.
    size: usize,
    clock: Clock,
    entries: Mutex<HashMap<SarKey, Decision>>,
}

impl Decisions {
    fn new(ttl: Duration, size: usize, clock: Clock) -> Self {
        Self {
            ttl,
            size,
            clock,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The cached decision for `key`, if one is present and still fresh.
    ///
    /// An expired entry is evicted on read rather than left behind.
    fn get(&self, key: &SarKey) -> Option<bool> {
        let now = (self.clock)();
        let mut entries = self.lock();
        match entries.get(key) {
            Some(d) if Self::fresh(d, now, self.ttl) => Some(d.allowed),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Record a fresh decision, sweeping expired entries and evicting the
    /// oldest if the cache is full.
    ///
    /// Eviction is by `stored_at`, oldest first — NOT by least-recently-*used*.
    /// Since a read never refreshes `stored_at`, the oldest entry is also the
    /// one closest to expiring anyway, so this discards the least useful entry
    /// without needing a second timestamp. Every entry has a bounded lifetime
    /// regardless, so the worst an imperfect victim choice costs is one extra
    /// SAR.
    fn put(&self, key: SarKey, allowed: bool) {
        let now = (self.clock)();
        let mut entries = self.lock();

        // Sweep first: entries that aged out are free to drop, and doing this
        // before evicting means a cache full of stale entries never evicts a
        // live one. This is also what keeps the map bounded when the cache is
        // large but traffic is thin — nothing else would ever read those keys
        // again to trigger evict-on-read.
        entries.retain(|_, d| Self::fresh(d, now, self.ttl));

        if self.size == 0 {
            return;
        }
        while entries.len() >= self.size {
            let Some(victim) = entries
                .iter()
                .min_by_key(|(_, d)| d.stored_at)
                .map(|(k, _)| k.clone())
            else {
                return;
            };
            entries.remove(&victim);
        }

        entries.insert(
            key,
            Decision {
                allowed,
                stored_at: now,
            },
        );
    }

    /// How many decisions are held, after sweeping expired ones.
    ///
    /// Reported on `kopiur_ui_sar_cache_size`; sweeping first means the gauge
    /// tracks what is actually usable rather than what was once inserted.
    fn len(&self) -> usize {
        let now = (self.clock)();
        let mut entries = self.lock();
        entries.retain(|_, d| Self::fresh(d, now, self.ttl));
        entries.len()
    }

    /// Whether a decision stored at `stored_at` is still inside `ttl`.
    ///
    /// `duration_since` is saturating, so a clock that went backwards expires
    /// the entry rather than panicking.
    fn fresh(d: &Decision, now: Instant, ttl: Duration) -> bool {
        now.duration_since(d.stored_at) < ttl
    }

    /// Take the map lock, recovering from a poisoned mutex.
    ///
    /// A panic while holding this lock cannot corrupt the map — it is a plain
    /// `HashMap` of owned values with no cross-entry invariant — so refusing to
    /// authorize anything for the rest of the process would be a strictly worse
    /// outcome than continuing.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SarKey, Decision>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// How many per-namespace `SubjectAccessReview`s may be in flight at once for
/// one [`SarCache::visible_namespaces`] call.
///
/// A cold cross-namespace list asks one review per populated namespace. Issuing
/// them all at once is the shape most likely to trip apiserver flow control
/// exactly when the UI is cold; issuing them one at a time makes a
/// hundred-namespace fleet's first page load a hundred serial round trips. This
/// is the compromise.
const NAMESPACE_SAR_CONCURRENCY: usize = 8;

/// A bounded, TTL'd `SubjectAccessReview` cache, queried under the UI's own
/// ServiceAccount.
///
/// The client is deliberately the UI's own — `create` on `subjectaccessreviews`
/// is one of the two permissions the UI's ClusterRole grants (the other being
/// `impersonate`). Asking on the caller's behalf with an *impersonating* client
/// would answer "may this user create a SAR", which is a different and useless
/// question.
///
/// Concurrent identical questions are **single-flighted**: the first caller
/// posts the review and every other caller waits for that answer instead of
/// posting its own. Without it, one cold page load that fans out over N
/// namespaces across M concurrent requests is N×M reviews for what is really N
/// distinct questions.
pub struct SarCache {
    reviewer: Reviewer,
    decisions: Decisions,
    /// One lock per in-flight question. Entries are removed once the answer is
    /// cached, so this holds only questions currently being asked.
    in_flight: Mutex<HashMap<SarKey, Arc<tokio::sync::Mutex<()>>>>,
    metrics: Arc<UiMetrics>,
}

impl std::fmt::Debug for SarCache {
    /// Redacted: the decision map is keyed by real usernames and group
    /// memberships, which have no business in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SarCache")
            .field("ttl", &self.decisions.ttl)
            .field("size", &self.decisions.size)
            .field("entries", &self.decisions.len())
            .finish_non_exhaustive()
    }
}

impl SarCache {
    /// Build a cache that asks `client` and remembers up to `size` answers for
    /// `ttl` each.
    #[must_use]
    pub fn new(client: kube::Client, ttl: Duration, size: usize, metrics: Arc<UiMetrics>) -> Self {
        Self::with_reviewer(
            apiserver_reviewer(client),
            ttl,
            size,
            metrics,
            Arc::new(Instant::now),
        )
    }

    /// [`SarCache::new`] with the clock the TTL policy reads.
    ///
    /// The seam an integration test uses to drive expiry without sleeping.
    #[must_use]
    pub fn new_with_clock(
        client: kube::Client,
        ttl: Duration,
        size: usize,
        metrics: Arc<UiMetrics>,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self::with_reviewer(apiserver_reviewer(client), ttl, size, metrics, clock)
    }

    /// The common constructor. Private because [`Reviewer`] is an internal seam.
    fn with_reviewer(
        reviewer: Reviewer,
        ttl: Duration,
        size: usize,
        metrics: Arc<UiMetrics>,
        clock: Clock,
    ) -> Self {
        Self {
            reviewer,
            decisions: Decisions::new(ttl, size, clock),
            in_flight: Mutex::new(HashMap::new()),
            metrics,
        }
    }

    /// May `id` perform `verb` on `resource` at this scope?
    ///
    /// Answers from the TTL cache when it can. On a miss, exactly one caller per
    /// distinct question posts the `SubjectAccessReview`; concurrent askers of
    /// the *same* question wait for that answer rather than piling on. Both
    /// outcomes are cached — a denial that is not cached is a free amplification
    /// lever, since an unauthorized caller could otherwise turn every page
    /// refresh into a review per namespace.
    ///
    /// # Errors
    ///
    /// [`OpsError`] if the SAR itself could not be created. That is a failure of
    /// the UI's own permissions or of the apiserver, and it is surfaced rather
    /// than degraded: an unanswerable authorization question must never be read
    /// as an allow.
    pub async fn allowed(
        &self,
        id: &Identity,
        verb: &'static str,
        resource: &'static str,
        namespace: Option<&str>,
        name: Option<&str>,
    ) -> Result<bool, OpsError> {
        let key = SarKey::new(id, verb, resource, namespace, name);
        if let Some(cached) = self.decisions.get(&key) {
            return Ok(cached);
        }

        // Single-flight. The guard is a `tokio::sync::Mutex` because it IS held
        // across the `.await` on the apiserver; the two `std::sync::Mutex`es
        // (the decision map and this registry) are only ever held for a map
        // operation and never across a yield point.
        let guard = self.flight_lock(&key);
        let _held = guard.lock().await;

        // Re-check under the guard: whoever we queued behind has by now stored
        // the answer, and this is what turns N concurrent askers into 1 review.
        if let Some(cached) = self.decisions.get(&key) {
            self.release_flight(&key);
            return Ok(cached);
        }

        match (self.reviewer)(key.clone()).await {
            Ok(allowed) => {
                self.metrics.inc_sar(allowed);
                self.decisions.put(key.clone(), allowed);
                self.metrics.set_sar_cache_size(self.decisions.len());
                self.release_flight(&key);
                Ok(allowed)
            }
            Err(e) => {
                // Nothing cached: a failed review must be re-asked, never
                // remembered as a deny.
                self.release_flight(&key);
                Err(e)
            }
        }
    }

    /// The single-flight guard for one question, creating it if absent.
    fn flight_lock(&self, key: &SarKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(map.entry(key.clone()).or_default())
    }

    /// Drop the single-flight registry entry once nobody else is queued on it.
    ///
    /// `Arc::strong_count <= 2` means only this caller's handle and the map's
    /// own remain, so removing it cannot strand a waiter. Leaving it would make
    /// the registry the very unbounded map the decision cache was bounded to
    /// avoid.
    fn release_flight(&self, key: &SarKey) {
        let mut map = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.get(key).is_some_and(|g| Arc::strong_count(g) <= 2) {
            map.remove(key);
        }
    }

    /// How much of `resource` `id` may list.
    ///
    /// Asks the cluster-wide question first — one review answers it for the
    /// common case of an operator or admin — and only falls back to
    /// per-namespace reviews when that is denied. `candidate_namespaces` bounds
    /// that fallback: it is the namespaces the stores actually hold objects in,
    /// so the cost is the size of the fleet's *populated* namespaces, not the
    /// cluster's.
    ///
    /// The fallback runs at most [`NAMESPACE_SAR_CONCURRENCY`] reviews at once.
    ///
    /// # Errors
    ///
    /// [`OpsError`] if any review could not be created; see [`Self::allowed`].
    pub async fn visible_namespaces(
        &self,
        id: &Identity,
        resource: &'static str,
        candidate_namespaces: &BTreeSet<String>,
    ) -> Result<Visibility, OpsError> {
        if self.allowed(id, "list", resource, None, None).await? {
            return Ok(Visibility::All);
        }

        // The stream yields OWNED namespaces, not `&String` borrowed from
        // `candidate_namespaces`. A closure whose returned future borrows its
        // argument is not general enough over lifetimes for rustc to prove once
        // this is awaited from an axum handler: it fails with "implementation of
        // `FnOnce` is not general enough" reported at the *route*, pointing
        // nowhere near here. Owning the `String` sidesteps it and costs nothing
        // on the allow path, which cloned it anyway.
        let visible: BTreeSet<String> = futures::stream::iter(
            candidate_namespaces
                .iter()
                .cloned()
                .collect::<Vec<String>>(),
        )
        .map(|namespace| async move {
            let allowed = self
                .allowed(id, "list", resource, Some(&namespace), None)
                .await?;
            Ok::<_, OpsError>(allowed.then_some(namespace))
        })
        .buffer_unordered(NAMESPACE_SAR_CONCURRENCY)
        .try_filter_map(|found| std::future::ready(Ok(found)))
        .try_collect()
        .await?;

        Ok(Visibility::Namespaces(visible))
    }
}

/// The production [`Reviewer`]: post the review to the apiserver.
fn apiserver_reviewer(client: kube::Client) -> Reviewer {
    Arc::new(move |key: SarKey| {
        let client = client.clone();
        Box::pin(async move {
            let review = SubjectAccessReview {
                metadata: Default::default(),
                spec: SubjectAccessReviewSpec {
                    user: Some(key.user.clone()),
                    // Omitted rather than sent empty: an empty list and an
                    // absent one mean the same thing to the apiserver, and
                    // omitting keeps the request identical to what
                    // impersonation would assert.
                    groups: (!key.groups.is_empty()).then(|| key.groups.clone()),
                    extra: (!key.extra.is_empty()).then(|| key.extra.clone()),
                    resource_attributes: Some(ResourceAttributes {
                        verb: Some(key.verb.to_owned()),
                        group: Some(key.group.to_owned()),
                        resource: Some(key.resource.to_owned()),
                        namespace: key.namespace.clone(),
                        name: key.name.clone(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                status: None,
            };

            let api: Api<SubjectAccessReview> = Api::all(client);
            let reviewed = api
                .create(&PostParams::default(), &review)
                .await
                .map_err(|e| {
                    classify_kube(
                        "create",
                        "SubjectAccessReview",
                        "subjectaccessreviews",
                        None,
                        None,
                        e,
                    )
                })?;

            Ok(decision(reviewed.status.as_ref()))
        }) as BoxFuture<'static, Result<bool, OpsError>>
    })
}

/// Read a `SubjectAccessReviewStatus` as a yes-or-no, failing closed.
///
/// Kubernetes lets an authorizer answer three ways: allow, explicit deny, or no
/// opinion (`allowed: false, denied: false`). Only the first is an allow. An
/// explicit `denied` alongside `allowed` is contradictory and is read as a deny,
/// and an absent status — which the apiserver does not produce, but a proxy or a
/// mock might — is also a deny.
fn decision(
    status: Option<&k8s_openapi::api::authorization::v1::SubjectAccessReviewStatus>,
) -> bool {
    match status {
        Some(s) => s.allowed && !s.denied.unwrap_or(false),
        None => false,
    }
}

/// Keep only the objects `vis` permits.
///
/// Pure and total, so the filter that stands between the shared cache and a
/// caller is directly testable. Cluster-scoped objects carry no
/// `metadata.namespace`, so they survive [`Visibility::All`] and nothing else —
/// which is the correct reading: permission to list in some namespaces says
/// nothing about a cluster-scoped resource.
#[must_use]
pub fn filter_visible<K: kube::Resource>(items: Vec<Arc<K>>, vis: &Visibility) -> Vec<Arc<K>> {
    match vis {
        Visibility::All => items,
        Visibility::Namespaces(allowed) => items
            .into_iter()
            .filter(|obj| {
                kube::Resource::meta(obj.as_ref())
                    .namespace
                    .as_deref()
                    .is_some_and(|ns| allowed.contains(ns))
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use k8s_openapi::api::authorization::v1::SubjectAccessReviewStatus;
    use kopiur_api::{ClusterRepository, Snapshot};
    use kopiur_ui_model::identity::IdentitySource;
    use kube::api::ObjectMeta;

    /// A clock the test drives by hand. `Instant` cannot be constructed from a
    /// timestamp, so it is anchored at one `Instant::now()` and advanced by an
    /// offset the test controls.
    struct FakeClock {
        base: Instant,
        offset_millis: Arc<AtomicU64>,
    }

    /// A [`Clock`] plus the millisecond offset handle that advances it.
    fn fake_clock() -> (Clock, Arc<AtomicU64>) {
        let offset_millis = Arc::new(AtomicU64::new(0));
        let clock = FakeClock {
            base: Instant::now(),
            offset_millis: Arc::clone(&offset_millis),
        };
        let f: Clock = Arc::new(move || {
            clock.base + Duration::from_millis(clock.offset_millis.load(Ordering::SeqCst))
        });
        (f, offset_millis)
    }

    fn identity(user: &str, groups: &[&str]) -> Identity {
        Identity {
            user: user.to_owned(),
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
            email: None,
            extra: BTreeMap::new(),
            source: IdentitySource::TrustedHeaders,
        }
    }

    /// A `Snapshot` that exists only to carry a `metadata.namespace`. Every
    /// `SnapshotSpec` field is optional, so an empty spec is a valid one.
    fn snapshot_in(namespace: Option<&str>) -> Arc<Snapshot> {
        let mut s = Snapshot::new(
            "nightly",
            serde_json::from_value(serde_json::json!({})).expect("SnapshotSpec is all-optional"),
        );
        s.metadata = ObjectMeta {
            name: Some("nightly".to_owned()),
            namespace: namespace.map(str::to_owned),
            ..Default::default()
        };
        Arc::new(s)
    }

    /// The minimum `ClusterRepositorySpec` the CRD accepts. Parsed the way the
    /// cluster parses it (YAML -> JSON value -> typed) per the repo convention.
    fn cluster_repository() -> Arc<ClusterRepository> {
        let spec = kopiur_api::testutil::from_yaml(
            r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s, namespace: kopiur-system } }
allowedNamespaces: { all: true }
"#,
        );
        Arc::new(ClusterRepository::new("central", spec))
    }

    // --- SarKey canonicalization --------------------------------------------

    /// Group ORDER is an accident of how the proxy joined the header. Two
    /// spellings of the same subject must be one cache entry, or the cache
    /// silently halves its hit rate and doubles the SAR load.
    #[test]
    fn group_order_does_not_fork_the_key() {
        let a = SarKey::new(
            &identity("ada", &["platform", "sre"]),
            "list",
            "snapshots",
            None,
            None,
        );
        let b = SarKey::new(
            &identity("ada", &["sre", "platform"]),
            "list",
            "snapshots",
            None,
            None,
        );
        assert_eq!(a, b, "group order must not produce two different keys");
        assert_eq!(a.groups, vec!["platform".to_owned(), "sre".to_owned()]);
    }

    /// A duplicated group is the same membership. Deduping keeps `platform` and
    /// `platform,platform` on one entry.
    #[test]
    fn duplicate_groups_collapse() {
        let key = SarKey::new(
            &identity("ada", &["platform", "platform"]),
            "list",
            "snapshots",
            None,
            None,
        );
        assert_eq!(key.groups, vec!["platform".to_owned()]);
    }

    /// The security-critical direction: different subjects must never collide.
    /// If they did, one caller would inherit another's authorization.
    #[test]
    fn different_subjects_and_questions_are_different_keys() {
        let base = SarKey::new(&identity("ada", &["sre"]), "list", "snapshots", None, None);

        assert_ne!(
            base,
            SarKey::new(&identity("bob", &["sre"]), "list", "snapshots", None, None),
            "a different user must not share a decision",
        );
        assert_ne!(
            base,
            SarKey::new(
                &identity("ada", &["sre", "admins"]),
                "list",
                "snapshots",
                None,
                None
            ),
            "an extra group must not share a decision",
        );
        assert_ne!(
            base,
            SarKey::new(&identity("ada", &["sre"]), "get", "snapshots", None, None),
            "a different verb is a different question",
        );
        assert_ne!(
            base,
            SarKey::new(&identity("ada", &["sre"]), "list", "restores", None, None),
            "a different resource is a different question",
        );
        assert_ne!(
            base,
            SarKey::new(
                &identity("ada", &["sre"]),
                "list",
                "snapshots",
                Some("prod"),
                None
            ),
            "cluster-wide and namespaced are different questions",
        );
        assert_ne!(
            base,
            SarKey::new(
                &identity("ada", &["sre"]),
                "list",
                "snapshots",
                None,
                Some("nightly")
            ),
            "a named object is a different question from the collection",
        );
    }

    /// `extra` is impersonated and an authorizer may key on it, so it belongs in
    /// the key. Without it, two callers whose only difference is a `userextras`
    /// value would share one decision.
    #[test]
    fn userextras_are_part_of_the_key() {
        let mut with_extra = identity("ada", &["sre"]);
        with_extra
            .extra
            .insert("scopes".to_owned(), vec!["read".to_owned()]);

        assert_ne!(
            SarKey::new(&identity("ada", &["sre"]), "list", "snapshots", None, None),
            SarKey::new(&with_extra, "list", "snapshots", None, None),
            "a differing userextras value must not share a decision",
        );
    }

    // --- TTL ----------------------------------------------------------------

    #[test]
    fn a_fresh_decision_is_served_from_the_cache() {
        let (clock, _offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(60), 64, clock);
        let key = SarKey::new(&identity("ada", &[]), "list", "snapshots", None, None);

        assert_eq!(decisions.get(&key), None, "cold cache must miss");
        decisions.put(key.clone(), true);
        assert_eq!(decisions.get(&key), Some(true));
    }

    /// The whole point of the TTL: a revoked RoleBinding must stop mattering
    /// within one window, so an entry past its TTL must miss and force a fresh
    /// SAR.
    #[test]
    fn a_decision_expires_exactly_at_the_ttl() {
        let (clock, offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(60), 64, clock);
        let key = SarKey::new(&identity("ada", &[]), "list", "snapshots", None, None);
        decisions.put(key.clone(), true);

        offset.store(59_999, Ordering::SeqCst);
        assert_eq!(decisions.get(&key), Some(true), "still inside the window");

        offset.store(60_000, Ordering::SeqCst);
        assert_eq!(
            decisions.get(&key),
            None,
            "at exactly the TTL the decision is stale and must be re-asked",
        );
    }

    /// Denials expire too. A cached deny that outlived its TTL would lock a user
    /// out of access they have just been granted.
    #[test]
    fn a_cached_denial_also_expires() {
        let (clock, offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(30), 64, clock);
        let key = SarKey::new(&identity("ada", &[]), "list", "snapshots", None, None);

        decisions.put(key.clone(), false);
        assert_eq!(
            decisions.get(&key),
            Some(false),
            "denials are cached, not just allows — an uncached deny is an amplification lever",
        );

        offset.store(30_001, Ordering::SeqCst);
        assert_eq!(decisions.get(&key), None);
    }

    /// An expired entry must not sit in the map forever: the key includes the
    /// username, so a process that never evicted would grow with every distinct
    /// caller.
    #[test]
    fn an_expired_entry_is_evicted_on_read() {
        let (clock, offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(10), 64, clock);
        let key = SarKey::new(&identity("ada", &[]), "list", "snapshots", None, None);
        decisions.put(key.clone(), true);

        offset.store(10_001, Ordering::SeqCst);
        assert_eq!(decisions.get(&key), None);
        assert!(
            decisions.lock().is_empty(),
            "a stale entry must be dropped, not merely ignored",
        );
    }

    // --- bounding ------------------------------------------------------------

    /// A distinct key per user, so the map grows with logins. Without a size
    /// bound this is an unbounded leak in a process that runs for months.
    #[test]
    fn the_cache_never_exceeds_its_size_bound() {
        let (clock, _offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(3600), 4, clock);

        for i in 0..50 {
            let user = format!("user-{i}");
            decisions.put(
                SarKey::new(&identity(&user, &[]), "list", "snapshots", None, None),
                true,
            );
            assert!(
                decisions.len() <= 4,
                "cache grew past its bound at insert {i}: {}",
                decisions.len(),
            );
        }
    }

    /// Eviction drops the OLDEST-stored entry, which — because a read never
    /// refreshes `stored_at` — is also the one closest to expiring.
    #[test]
    fn eviction_discards_the_oldest_entry_first() {
        let (clock, offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(3600), 2, clock);

        let oldest = SarKey::new(&identity("first", &[]), "list", "snapshots", None, None);
        decisions.put(oldest.clone(), true);

        offset.store(1_000, Ordering::SeqCst);
        let newer = SarKey::new(&identity("second", &[]), "list", "snapshots", None, None);
        decisions.put(newer.clone(), true);

        // Inserting a third with size 2 must evict `oldest`, not `newer`.
        offset.store(2_000, Ordering::SeqCst);
        let newest = SarKey::new(&identity("third", &[]), "list", "snapshots", None, None);
        decisions.put(newest.clone(), true);

        assert_eq!(decisions.get(&oldest), None, "the oldest entry is evicted");
        assert_eq!(decisions.get(&newer), Some(true));
        assert_eq!(decisions.get(&newest), Some(true));
    }

    /// Expired entries are swept on insert, not only on read. Without this a
    /// cache whose keys are never read again — the normal case for a user who
    /// logged off — would hold them until eviction pressure alone removed them,
    /// and would evict LIVE entries to make room for new ones.
    #[test]
    fn insert_sweeps_expired_entries_before_evicting_live_ones() {
        let (clock, offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(60), 4, clock);

        for i in 0..4 {
            decisions.put(
                SarKey::new(
                    &identity(&format!("gone-{i}"), &[]),
                    "list",
                    "snapshots",
                    None,
                    None,
                ),
                true,
            );
        }
        assert_eq!(decisions.len(), 4);

        // Everything ages out, then one fresh insert arrives.
        offset.store(60_001, Ordering::SeqCst);
        let fresh = SarKey::new(&identity("here", &[]), "list", "snapshots", None, None);
        decisions.put(fresh.clone(), true);

        assert_eq!(
            decisions.len(),
            1,
            "the four expired entries must be swept, leaving only the fresh one",
        );
        assert_eq!(decisions.get(&fresh), Some(true));
    }

    /// A size of zero disables the cache rather than meaning "one". Every
    /// question then re-asks the apiserver: slow, but never stale and never
    /// unbounded.
    #[test]
    fn a_zero_size_disables_caching() {
        let (clock, _offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(60), 0, clock);
        let key = SarKey::new(&identity("ada", &[]), "list", "snapshots", None, None);

        decisions.put(key.clone(), true);
        assert_eq!(decisions.get(&key), None);
        assert_eq!(decisions.len(), 0);
    }

    /// The subtlety that separates this from an idle-expiring cache: reading a
    /// decision must NOT extend its life. If it did, a continuously-active
    /// session would hold a stale allow forever — precisely the caller for whom
    /// a revoked RoleBinding matters most.
    #[test]
    fn reading_a_decision_does_not_extend_its_ttl() {
        let (clock, offset) = fake_clock();
        let decisions = Decisions::new(Duration::from_secs(60), 64, clock);
        let key = SarKey::new(&identity("ada", &[]), "list", "snapshots", None, None);
        decisions.put(key.clone(), true);

        // Read it repeatedly as the clock advances, the way a busy session would.
        for t in [10_000, 20_000, 30_000, 40_000, 50_000] {
            offset.store(t, Ordering::SeqCst);
            assert_eq!(decisions.get(&key), Some(true));
        }

        offset.store(60_000, Ordering::SeqCst);
        assert_eq!(
            decisions.get(&key),
            None,
            "the TTL runs from when the apiserver answered, not from the last read",
        );
    }

    // --- decision(): failing closed -----------------------------------------

    /// Three-valued in, two-valued out — and only a clean allow is an allow.
    #[test]
    fn only_an_unqualified_allow_reads_as_allowed() {
        let allow = SubjectAccessReviewStatus {
            allowed: true,
            denied: None,
            evaluation_error: None,
            reason: None,
        };
        assert!(decision(Some(&allow)));

        let no_opinion = SubjectAccessReviewStatus {
            allowed: false,
            denied: Some(false),
            evaluation_error: None,
            reason: None,
        };
        assert!(!decision(Some(&no_opinion)), "no opinion is not an allow");

        let contradictory = SubjectAccessReviewStatus {
            allowed: true,
            denied: Some(true),
            evaluation_error: None,
            reason: None,
        };
        assert!(
            !decision(Some(&contradictory)),
            "an explicit deny wins over allow — fail closed",
        );

        let errored = SubjectAccessReviewStatus {
            allowed: false,
            denied: None,
            evaluation_error: Some("webhook unreachable".to_owned()),
            reason: None,
        };
        assert!(!decision(Some(&errored)));

        assert!(!decision(None), "a status-less review is not an allow");
    }

    // --- SarCache: single-flight, caching, fan-out ---------------------------

    /// A [`SarCache`] over a counting reviewer, so tests can assert how many
    /// reviews actually escaped the cache.
    ///
    /// `answer` decides each verdict from the key; `delay` makes the review slow
    /// enough that concurrent callers genuinely overlap, which is the only way
    /// to observe single-flighting rather than accidental serialization.
    fn counting_cache(
        answer: impl Fn(&SarKey) -> bool + Send + Sync + 'static,
        delay: Duration,
    ) -> (SarCache, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&calls);
        let reviewer: Reviewer = Arc::new(move |key: SarKey| {
            seen.fetch_add(1, Ordering::SeqCst);
            let allowed = answer(&key);
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                Ok(allowed)
            }) as BoxFuture<'static, Result<bool, OpsError>>
        });

        let metrics = Arc::new(UiMetrics::new(Arc::new(
            kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"),
        )));
        let cache = SarCache::with_reviewer(
            reviewer,
            Duration::from_secs(60),
            256,
            metrics,
            Arc::new(Instant::now),
        );
        (cache, calls)
    }

    /// The finding this guards: N concurrent askers of the SAME question must
    /// produce ONE review, not N. Without single-flight, one cold page load that
    /// fans out over namespaces across concurrent requests multiplies into N×M
    /// reviews for N distinct questions.
    #[tokio::test]
    async fn concurrent_identical_questions_produce_one_review() {
        let (cache, calls) = counting_cache(|_| true, Duration::from_millis(50));
        let id = identity("ada", &["sre"]);

        let results = futures::future::join_all(
            (0..16).map(|_| cache.allowed(&id, "list", "snapshots", Some("prod"), None)),
        )
        .await;

        for r in &results {
            assert!(*r.as_ref().expect("no review may fail here"));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "16 concurrent identical questions must cost exactly one SubjectAccessReview",
        );
    }

    /// Single-flight must key on the whole question: different subjects and
    /// different scopes are genuinely different reviews and must NOT be merged.
    /// Merging them would be an authorization leak, not an optimization.
    #[tokio::test]
    async fn different_questions_are_not_merged() {
        let (cache, calls) = counting_cache(|_| true, Duration::from_millis(20));
        let ada = identity("ada", &["sre"]);
        let bob = identity("bob", &["sre"]);

        let _ = tokio::join!(
            cache.allowed(&ada, "list", "snapshots", Some("prod"), None),
            cache.allowed(&bob, "list", "snapshots", Some("prod"), None),
            cache.allowed(&ada, "list", "snapshots", Some("dev"), None),
            cache.allowed(&ada, "get", "snapshots", Some("prod"), None),
        );

        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "a different user, namespace or verb is a different question",
        );
    }

    /// After the first answer the cache serves everyone, so a repeated question
    /// costs nothing.
    #[tokio::test]
    async fn a_repeated_question_is_answered_from_the_cache() {
        let (cache, calls) = counting_cache(|_| true, Duration::ZERO);
        let id = identity("ada", &[]);

        for _ in 0..5 {
            assert!(
                cache
                    .allowed(&id, "list", "snapshots", None, None)
                    .await
                    .expect("review must succeed")
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The single-flight registry must not become the unbounded map the decision
    /// cache was bounded to avoid: entries are dropped once nobody is queued.
    #[tokio::test]
    async fn the_in_flight_registry_does_not_leak() {
        let (cache, _calls) = counting_cache(|_| true, Duration::ZERO);
        let id = identity("ada", &[]);

        for i in 0..20 {
            let ns = format!("ns-{i}");
            let _ = cache
                .allowed(&id, "list", "snapshots", Some(&ns), None)
                .await;
        }

        let held = cache
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(
            held, 0,
            "no question is still in flight, so none may be held"
        );
    }

    /// A cluster-wide allow short-circuits: one review, no per-namespace fan-out
    /// at all. This is the common admin/operator case and must stay cheap.
    #[tokio::test]
    async fn a_cluster_wide_allow_costs_exactly_one_review() {
        let (cache, calls) = counting_cache(|_| true, Duration::ZERO);
        let id = identity("admin", &["cluster-admins"]);
        let candidates: BTreeSet<String> = (0..50).map(|i| format!("ns-{i}")).collect();

        let vis = cache
            .visible_namespaces(&id, "snapshots", &candidates)
            .await
            .expect("review must succeed");

        assert_eq!(vis, Visibility::All);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a cluster-wide allow must not fan out over 50 namespaces",
        );
    }

    /// When the cluster-wide question is denied, exactly the permitted
    /// namespaces come back — and the fan-out asks one review per candidate,
    /// bounded by `NAMESPACE_SAR_CONCURRENCY` rather than issued all at once.
    #[tokio::test]
    async fn a_denied_cluster_wide_list_falls_back_to_the_permitted_namespaces() {
        // Deny cluster-wide (namespace: None); allow only `prod` and `dev`.
        let (cache, calls) = counting_cache(
            |key| matches!(key.namespace.as_deref(), Some("prod" | "dev")),
            Duration::ZERO,
        );
        let id = identity("ada", &["sre"]);
        let candidates: BTreeSet<String> = ["prod", "dev", "staging", "sandbox"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        let vis = cache
            .visible_namespaces(&id, "snapshots", &candidates)
            .await
            .expect("review must succeed");

        assert_eq!(
            vis,
            Visibility::Namespaces(["prod".to_owned(), "dev".to_owned()].into()),
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            5,
            "one cluster-wide review plus one per candidate namespace",
        );
    }

    /// A failed review must not be cached — neither as an allow nor as a deny.
    /// Remembering it as a deny would lock a permitted user out for the whole
    /// TTL because of one transient apiserver blip.
    #[tokio::test]
    async fn a_failed_review_is_not_cached() {
        let calls = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&calls);
        let reviewer: Reviewer = Arc::new(move |_key: SarKey| {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    Err(OpsError::Forbidden {
                        verb: "create",
                        resource: "subjectaccessreviews",
                        scope: String::new(),
                        source: Box::new(kube::Error::Api(Box::new(
                            kube::core::Status::failure("denied", "Forbidden").with_code(403),
                        ))),
                    })
                } else {
                    Ok(true)
                }
            }) as BoxFuture<'static, Result<bool, OpsError>>
        });
        let metrics = Arc::new(UiMetrics::new(Arc::new(
            kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"),
        )));
        let cache = SarCache::with_reviewer(
            reviewer,
            Duration::from_secs(60),
            256,
            metrics,
            Arc::new(Instant::now),
        );
        let id = identity("ada", &[]);

        assert!(
            cache
                .allowed(&id, "list", "snapshots", None, None)
                .await
                .is_err(),
            "the first review fails and must surface as an error",
        );
        assert!(
            cache
                .allowed(&id, "list", "snapshots", None, None)
                .await
                .expect("the retry succeeds"),
            "a failed review must be re-asked, never remembered",
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // --- filter_visible -----------------------------------------------------

    #[test]
    fn visibility_all_passes_everything_through() {
        let items = vec![
            snapshot_in(Some("prod")),
            snapshot_in(Some("staging")),
            snapshot_in(None),
        ];
        assert_eq!(filter_visible(items, &Visibility::All).len(), 3);
    }

    #[test]
    fn namespaced_visibility_keeps_only_the_permitted_namespaces() {
        let items = vec![
            snapshot_in(Some("prod")),
            snapshot_in(Some("staging")),
            snapshot_in(Some("dev")),
        ];
        let vis = Visibility::Namespaces(["prod".to_owned(), "dev".to_owned()].into());

        let kept = filter_visible(items, &vis);
        let names: BTreeSet<String> = kept
            .iter()
            .filter_map(|s| s.metadata.namespace.clone())
            .collect();
        assert_eq!(names, ["prod".to_owned(), "dev".to_owned()].into());
    }

    #[test]
    fn an_empty_namespace_set_withholds_everything() {
        let items = vec![snapshot_in(Some("prod")), snapshot_in(Some("dev"))];
        assert!(
            filter_visible(items, &Visibility::Namespaces(BTreeSet::new())).is_empty(),
            "a caller allowed nowhere sees nothing",
        );
    }

    /// The case a namespace-only filter gets wrong if it is not thought about:
    /// a `ClusterRepository` has no `metadata.namespace`, so it must NOT fall
    /// through a `Namespaces` filter. Permission to list Snapshots in `prod`
    /// says nothing about cluster-scoped repositories.
    #[test]
    fn a_cluster_scoped_object_needs_cluster_wide_visibility() {
        let repo = cluster_repository();

        assert_eq!(
            filter_visible(
                vec![Arc::clone(&repo)],
                &Visibility::Namespaces(["prod".to_owned()].into())
            )
            .len(),
            0,
            "namespace-scoped permission must not leak a cluster-scoped object",
        );
        assert_eq!(
            filter_visible(vec![repo], &Visibility::All).len(),
            1,
            "cluster-wide permission does show cluster-scoped objects",
        );
    }
}
