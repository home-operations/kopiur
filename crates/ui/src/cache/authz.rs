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

/// The TTL'd decision map, with no apiserver attached.
///
/// Split out from [`SarCache`] precisely so the expiry rules — the part that can
/// silently over-serve a stale allow — are unit-testable with a fake clock and
/// no `kube::Client`.
struct Decisions {
    ttl: Duration,
    clock: Clock,
    entries: Mutex<HashMap<SarKey, (bool, Instant)>>,
}

impl Decisions {
    fn new(ttl: Duration, clock: Clock) -> Self {
        Self {
            ttl,
            clock,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The cached decision for `key`, if one is present and still fresh.
    ///
    /// An expired entry is evicted on read rather than left behind: the map is
    /// keyed by identity, so a long-lived process that never evicted would grow
    /// with every user who ever logged in.
    fn get(&self, key: &SarKey) -> Option<bool> {
        let now = (self.clock)();
        let mut entries = self.lock();
        match entries.get(key) {
            // `duration_since` rather than subtraction: saturating, so a clock
            // that went backwards expires the entry instead of panicking.
            Some(&(allowed, stored_at)) if now.duration_since(stored_at) < self.ttl => {
                Some(allowed)
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Record a fresh decision.
    fn put(&self, key: SarKey, allowed: bool) {
        let now = (self.clock)();
        self.lock().insert(key, (allowed, now));
    }

    /// Take the map lock, recovering from a poisoned mutex.
    ///
    /// A panic while holding this lock cannot corrupt the map — it is a plain
    /// `HashMap` of owned values with no cross-entry invariant — so refusing to
    /// authorize anything for the rest of the process would be a strictly worse
    /// outcome than continuing.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SarKey, (bool, Instant)>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A TTL'd `SubjectAccessReview` cache, queried under the UI's own
/// ServiceAccount.
///
/// The client here is deliberately the UI's own — `create` on
/// `subjectaccessreviews` is one of the two permissions the UI's ClusterRole
/// grants (the other being `impersonate`). Asking on the caller's behalf with an
/// *impersonating* client would answer "may this user create a SAR", which is a
/// different and useless question.
pub struct SarCache {
    client: kube::Client,
    decisions: Decisions,
    metrics: Arc<UiMetrics>,
}

impl std::fmt::Debug for SarCache {
    /// Redacted: the decision map is keyed by real usernames and group
    /// memberships, which have no business in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SarCache")
            .field("ttl", &self.decisions.ttl)
            .finish_non_exhaustive()
    }
}

impl SarCache {
    /// Build a cache that asks `client` and remembers each answer for `ttl`.
    #[must_use]
    pub fn new(client: kube::Client, ttl: Duration, metrics: Arc<UiMetrics>) -> Self {
        Self {
            client,
            decisions: Decisions::new(ttl, Arc::new(Instant::now)),
            metrics,
        }
    }

    /// May `id` perform `verb` on `resource` at this scope?
    ///
    /// Answers from the TTL cache when it can, otherwise posts a
    /// `SubjectAccessReview` and remembers the outcome — allow or deny alike.
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

        let review = SubjectAccessReview {
            metadata: Default::default(),
            spec: SubjectAccessReviewSpec {
                user: Some(id.user.clone()),
                // Omitted rather than sent empty: an empty list and an absent
                // one mean the same thing to the apiserver, and omitting keeps
                // the request identical to what impersonation would assert.
                groups: (!key.groups.is_empty()).then(|| key.groups.clone()),
                extra: (!id.extra.is_empty()).then(|| id.extra.clone()),
                resource_attributes: Some(ResourceAttributes {
                    verb: Some(verb.to_owned()),
                    group: Some(KOPIUR_GROUP.to_owned()),
                    resource: Some(resource.to_owned()),
                    namespace: namespace.map(str::to_owned),
                    name: name.map(str::to_owned),
                    ..Default::default()
                }),
                ..Default::default()
            },
            status: None,
        };

        let api: Api<SubjectAccessReview> = Api::all(self.client.clone());
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

        let allowed = decision(reviewed.status.as_ref());
        self.metrics.inc_sar(allowed);
        self.decisions.put(key, allowed);
        Ok(allowed)
    }

    /// How much of `resource` `id` may list.
    ///
    /// Asks the cluster-wide question first — one SAR answers it for the common
    /// case of an operator or admin — and only falls back to per-namespace SARs
    /// when that is denied. `candidate_namespaces` bounds that fallback: it is
    /// the namespaces the stores actually hold objects in, so the cost is the
    /// size of the fleet's *populated* namespaces, not the cluster's.
    ///
    /// The per-namespace probes are sequential. They are all cache misses only
    /// on the first request of a TTL window, and issuing them concurrently would
    /// mean N simultaneous SAR creates from one page load — the shape most
    /// likely to trip apiserver flow control exactly when the UI is cold.
    ///
    /// # Errors
    ///
    /// [`OpsError`] if any SAR could not be created; see [`Self::allowed`].
    pub async fn visible_namespaces(
        &self,
        id: &Identity,
        resource: &'static str,
        candidate_namespaces: &BTreeSet<String>,
    ) -> Result<Visibility, OpsError> {
        if self.allowed(id, "list", resource, None, None).await? {
            return Ok(Visibility::All);
        }

        let mut visible = BTreeSet::new();
        for namespace in candidate_namespaces {
            if self
                .allowed(id, "list", resource, Some(namespace), None)
                .await?
            {
                visible.insert(namespace.clone());
            }
        }
        Ok(Visibility::Namespaces(visible))
    }
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
        let decisions = Decisions::new(Duration::from_secs(60), clock);
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
        let decisions = Decisions::new(Duration::from_secs(60), clock);
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
        let decisions = Decisions::new(Duration::from_secs(30), clock);
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
        let decisions = Decisions::new(Duration::from_secs(10), clock);
        let key = SarKey::new(&identity("ada", &[]), "list", "snapshots", None, None);
        decisions.put(key.clone(), true);

        offset.store(10_001, Ordering::SeqCst);
        assert_eq!(decisions.get(&key), None);
        assert!(
            decisions.lock().is_empty(),
            "a stale entry must be dropped, not merely ignored",
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
