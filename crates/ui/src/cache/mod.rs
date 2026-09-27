//! Where reads come from.
//!
//! Two shapes, chosen once at startup by `KOPIUR_UI_CACHE`: watch-fed reflector
//! stores under the UI's own ServiceAccount, gated per identity by
//! `SubjectAccessReview` ([`authz`]), or one impersonated LIST per request. Every
//! `load_*` matches [`Source`] exhaustively, so a third backing store cannot be
//! added until every reader has accounted for it.
//!
//! # The two paths must answer the same question
//!
//! Turning the cache on is a performance decision, never a permissions one. A
//! caller must see exactly the same objects either way, so the two arms of every
//! [`Source`] method are written to converge:
//!
//! | | `Impersonated` | `Cache` |
//! |---|---|---|
//! | who asks | the caller, via `Impersonate-*` | the UI's ServiceAccount |
//! | who filters | the apiserver, on the real request | [`authz::filter_visible`], after a `SubjectAccessReview` |
//! | denied `get` | apiserver 403 | a synthesized [`OpsError::Forbidden`] of the same shape |
//! | staleness | none | one watch lag + one SAR TTL |
//!
//! The `Cache` arm's job is to reconstruct what the apiserver would have done.
//! Anything it returns that an impersonated request would not have is a leak, so
//! read [`authz`] before changing it.

pub mod authz;
pub mod stores;

use std::collections::BTreeSet;
use std::sync::Arc;

use kube::api::{Api, ListParams};
use kube::runtime::reflector::{ObjectRef, Store};

use kopiur_api::{
    ClusterRepository, Maintenance, Repository, RepositoryReplication, Restore, Snapshot,
    SnapshotPolicy, SnapshotReplication, SnapshotSchedule,
};
use kopiur_ops::{OpsError, classify_kube, scope_suffix};

use crate::auth::identity::Identity;
use crate::cache::authz::{SarCache, Visibility};
use crate::cache::stores::Stores;

/// Proof that the holder is inside the `cache` module.
///
/// A capability token with no fields and a private constructor: any code can
/// *name* the type (it must be, to appear in [`KopiurKind::store`]'s public
/// signature) but only `cache` can produce a value of it. That is what keeps a
/// public trait method from becoming a hole around the authorization gate.
#[derive(Debug, Clone, Copy)]
pub struct StoreAccess(());

impl StoreAccess {
    /// Mint a witness.
    ///
    /// Deliberately **not** `pub`/`pub(crate)`: a module's private items are
    /// visible to that module and its descendants, so `cache`, `cache::stores`
    /// and `cache::authz` can call this and nothing else in the crate can.
    fn new() -> Self {
        Self(())
    }
}

/// A Kopiur CRD the UI can read, plus the three facts a generic read needs about
/// it.
///
/// The associated constants exist so a caller never hand-writes a plural or a
/// kind next to a type parameter — the pairing that would otherwise let a
/// `Restore` read authorize itself against `snapshots`. The trait is sealed by
/// construction rather than by a sealed-supertrait: it is implemented exactly
/// nine times, once per CRD, immediately below.
///
/// `DynamicType = ()` is load-bearing: it is what lets [`Store`] and
/// [`ObjectRef`] be built for `K` without carrying an `ApiResource` around, and
/// it holds for every `kube::CustomResource`-derived type.
pub trait KopiurKind:
    kube::Resource<DynamicType = ()>
    + Clone
    + serde::de::DeserializeOwned
    + std::fmt::Debug
    + Send
    + Sync
    + 'static
{
    /// The Kubernetes kind (`Snapshot`). Used as the `kind` metric label and as
    /// the kind `kopiur_ops::classify_kube` names in an error message.
    const KIND: &'static str;
    /// The plural resource name (`snapshots`). This is the string a
    /// `SubjectAccessReview` and an RBAC rule are written against, so it must be
    /// the CRD's real plural, not a guess derived from the kind.
    const PLURAL: &'static str;
    /// Whether objects of this kind live in a namespace. `ClusterRepository` is
    /// the one `false`. Verified against the type's real `kube::Resource::Scope`
    /// in this module's tests, so it cannot drift.
    const NAMESPACED: bool;

    /// Whether a UI running against an older operator may legitimately find this
    /// CRD absent, in which case a list answers `[]` instead of failing.
    ///
    /// True for exactly one kind today (`SnapshotReplication`, the newest CRD).
    /// A `const` rather than a string comparison against `PLURAL` so that adding
    /// a future optional kind is a declaration on that kind, not another arm in
    /// a predicate — and so a typo cannot silently make a mandatory CRD's
    /// absence render as an empty backup list.
    const OPTIONAL: bool;

    /// Project this kind's store out of [`Stores`].
    ///
    /// The [`StoreAccess`] witness cannot be constructed outside this module, so
    /// this is callable only from `cache` even though the trait is public. That
    /// is deliberate: the stores are unfiltered, and a handler that could reach
    /// one directly would bypass [`authz::filter_visible`] and every
    /// `SubjectAccessReview` with it.
    fn store(stores: &Stores, access: StoreAccess) -> &Store<Self>;

    /// Build the `Api` for an impersonated read at this scope.
    ///
    /// On the trait rather than a free function because `Api::namespaced` is
    /// bounded on `Scope = NamespaceResourceScope`: only the impl for a
    /// namespaced kind can call it, which is precisely the type-level
    /// distinction that keeps a cluster-scoped read from building a namespaced
    /// URL. A namespace passed for a cluster-scoped kind is ignored, not an
    /// error — it is meaningless, not malformed.
    fn api(client: kube::Client, namespace: Option<&str>) -> Api<Self>;
}

/// Write the `KopiurKind` impls for the namespaced kinds.
///
/// Split from [`cluster_scoped_kinds`] because the scope is not just a `bool`:
/// `Api::namespaced` exists only for `Scope = NamespaceResourceScope`, so a
/// cluster-scoped type listed here would fail to compile rather than quietly
/// build a namespaced URL.
macro_rules! namespaced_kinds {
    ($($ty:ty => { kind: $kind:literal, plural: $plural:literal, optional: $opt:literal, store: $field:ident }),+ $(,)?) => {
        $(
            impl KopiurKind for $ty {
                const KIND: &'static str = $kind;
                const PLURAL: &'static str = $plural;
                const NAMESPACED: bool = true;
                const OPTIONAL: bool = $opt;

                fn store(stores: &Stores, _access: StoreAccess) -> &Store<Self> {
                    &stores.$field
                }

                fn api(client: kube::Client, namespace: Option<&str>) -> Api<Self> {
                    match namespace {
                        Some(ns) => Api::namespaced(client, ns),
                        None => Api::all(client),
                    }
                }
            }
        )+
    };
}

/// Write the `KopiurKind` impls for the cluster-scoped kinds.
macro_rules! cluster_scoped_kinds {
    ($($ty:ty => { kind: $kind:literal, plural: $plural:literal, optional: $opt:literal, store: $field:ident }),+ $(,)?) => {
        $(
            impl KopiurKind for $ty {
                const KIND: &'static str = $kind;
                const PLURAL: &'static str = $plural;
                const NAMESPACED: bool = false;
                const OPTIONAL: bool = $opt;

                fn store(stores: &Stores, _access: StoreAccess) -> &Store<Self> {
                    &stores.$field
                }

                fn api(client: kube::Client, _namespace: Option<&str>) -> Api<Self> {
                    Api::all(client)
                }
            }
        )+
    };
}

namespaced_kinds! {
    Repository => { kind: "Repository", plural: "repositories", optional: false, store: repositories },
    SnapshotPolicy => { kind: "SnapshotPolicy", plural: "snapshotpolicies", optional: false, store: policies },
    Snapshot => { kind: "Snapshot", plural: "snapshots", optional: false, store: snapshots },
    SnapshotSchedule => { kind: "SnapshotSchedule", plural: "snapshotschedules", optional: false, store: schedules },
    Restore => { kind: "Restore", plural: "restores", optional: false, store: restores },
    Maintenance => { kind: "Maintenance", plural: "maintenances", optional: false, store: maintenances },
    RepositoryReplication => { kind: "RepositoryReplication", plural: "repositoryreplications", optional: false, store: repository_replications },
    SnapshotReplication => { kind: "SnapshotReplication", plural: "snapshotreplications", optional: true, store: snapshot_replications },
}

cluster_scoped_kinds! {
    ClusterRepository => { kind: "ClusterRepository", plural: "clusterrepositories", optional: false, store: cluster_repositories },
}

/// Evaluate `$check::<K>()` once per Kopiur kind, as an ARRAY of the results.
///
/// Test-only, and the single place the nine kinds are enumerated for assertions
/// — a tenth CRD added to the impl macros above but not here would escape every
/// whole-surface test. Because it expands to an array rather than a statement
/// list, each call site annotates the binding as `[_; 9]`, so the count is
/// asserted by the type checker at every use rather than in one test.
///
/// A `$check` returning `()` yields `[(); 9]`; one returning a value yields that
/// value nine times over, which is how the plural and scope tests collect their
/// data without restating the list.
#[cfg(test)]
macro_rules! for_each_kopiur_kind {
    ($check:ident) => {
        [
            $check::<Repository>(),
            $check::<ClusterRepository>(),
            $check::<SnapshotPolicy>(),
            $check::<Snapshot>(),
            $check::<SnapshotSchedule>(),
            $check::<Restore>(),
            $check::<Maintenance>(),
            $check::<RepositoryReplication>(),
            $check::<SnapshotReplication>(),
        ]
    };
}

/// Which backing store a read goes to.
///
/// Chosen once at startup and never per request: mixing the two would mean two
/// callers of the same endpoint could see different staleness, and a bug in one
/// arm would surface only intermittently.
// The `Cache` variant is far larger than `Impersonated`, but exactly one `Source`
// exists per process and it is held behind an `Arc` in `AppState`. Boxing the
// payload would add an indirection to every read to save a few hundred bytes once.
#[allow(clippy::large_enum_variant)]
pub enum Source {
    /// Reads are served from the shared reflector stores and gated per identity
    /// by `SubjectAccessReview`.
    ///
    /// The stores hold everything the UI's ServiceAccount can see, so `sar` is
    /// not an optimization — it is the only thing standing between one caller's
    /// request and another tenant's objects.
    Cache {
        /// The watch-fed caches.
        stores: Stores,
        /// The per-identity authorization gate.
        sar: SarCache,
    },
    /// One impersonated LIST/GET per request. No shared cache, so the apiserver's
    /// own RBAC is the only filter the read needs.
    Impersonated,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cache { .. } => f.write_str("Source::Cache"),
            Self::Impersonated => f.write_str("Source::Impersonated"),
        }
    }
}

impl Source {
    /// List objects of kind `K`, scoped to `namespace` when given.
    ///
    /// `client` must already be impersonating `id`; it is used only by the
    /// [`Source::Impersonated`] arm, where the apiserver does the filtering. The
    /// [`Source::Cache`] arm ignores it and filters with `id` itself.
    ///
    /// # Errors
    ///
    /// [`OpsError`] if the apiserver refused or could not answer.
    ///
    /// The two arms agree on what a denial looks like. A list of *one named
    /// namespace* the caller may not list is [`OpsError::Forbidden`] in both —
    /// the apiserver 403s it, and the cache synthesizes the same error rather
    /// than answering `[]`, which would tell the user their namespace is empty
    /// when in fact they may not look at it. A **cluster-wide** list is `[]` in
    /// both: the apiserver returns only what the caller may see, so a caller
    /// permitted nowhere gets an empty list, exactly as `kubectl get -A` does.
    pub async fn list<K: KopiurKind>(
        &self,
        id: &Identity,
        client: &kube::Client,
        namespace: Option<&str>,
    ) -> Result<Vec<Arc<K>>, OpsError> {
        let namespace = effective_namespace::<K>(namespace);

        match self {
            Self::Cache { stores, sar } => {
                let store = K::store(stores, StoreAccess::new());
                let items = match namespace {
                    Some(ns) => store.state_filter(|obj| {
                        kube::Resource::meta(obj).namespace.as_deref() == Some(ns)
                    }),
                    None => store.state(),
                };

                // Only ever ask about namespaces that could contribute an
                // object, derived from the objects already in hand rather than
                // from a second `state()` sweep of the store. For a
                // cluster-scoped kind the set is empty, so the cluster-wide
                // probe inside `visible_namespaces` is the only question asked —
                // and the only one that can return anything.
                let candidates: BTreeSet<String> = match namespace {
                    Some(ns) => std::iter::once(ns.to_owned()).collect(),
                    None => items
                        .iter()
                        .filter_map(|obj| kube::Resource::meta(obj.as_ref()).namespace.clone())
                        .collect(),
                };

                let visibility = sar.visible_namespaces(id, K::PLURAL, &candidates).await?;

                if let Some(ns) = denied_namespace(namespace, &visibility) {
                    return Err(forbidden::<K>("list", Some(ns), None));
                }

                Ok(authz::filter_visible(items, &visibility))
            }
            Self::Impersonated => {
                let api = K::api(client.clone(), namespace);
                match api.list(&ListParams::default()).await {
                    Ok(list) => Ok(list.items.into_iter().map(Arc::new).collect()),
                    Err(e) if kind_absent_and_optional::<K>(&e) => Ok(Vec::new()),
                    Err(e) => Err(classify_kube(
                        "list",
                        K::KIND,
                        K::PLURAL,
                        namespace,
                        None,
                        e,
                    )),
                }
            }
        }
    }

    /// Fetch one object of kind `K`, or `None` if it does not exist.
    ///
    /// `client` must already be impersonating `id`; see [`Self::list`].
    ///
    /// # Errors
    ///
    /// [`OpsError::Forbidden`] if the caller may not `get` this object, and
    /// other [`OpsError`] variants for apiserver failures. The `Cache` arm
    /// synthesizes its 403 rather than returning `None`, so that "you may not
    /// look at this" stays distinguishable from "it is not there" — collapsing
    /// them would turn every authorization failure into a confusing 404.
    pub async fn get<K: KopiurKind>(
        &self,
        id: &Identity,
        client: &kube::Client,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<Arc<K>>, OpsError> {
        let namespace = effective_namespace::<K>(namespace);

        match self {
            Self::Cache { stores, sar } => {
                if !sar
                    .allowed(id, "get", K::PLURAL, namespace, Some(name))
                    .await?
                {
                    return Err(forbidden::<K>("get", namespace, Some(name)));
                }

                let key = match namespace {
                    Some(ns) => ObjectRef::<K>::new(name).within(ns),
                    None => ObjectRef::<K>::new(name),
                };
                Ok(K::store(stores, StoreAccess::new()).get(&key))
            }
            Self::Impersonated => {
                let api = K::api(client.clone(), namespace);
                match api.get_opt(name).await {
                    Ok(found) => Ok(found.map(Arc::new)),
                    Err(e) => Err(classify_kube(
                        "get",
                        K::KIND,
                        K::PLURAL,
                        namespace,
                        Some(name),
                        e,
                    )),
                }
            }
        }
    }
}

/// The namespace whose list must be refused outright, if any.
///
/// A list of ONE named namespace that resolves to no visible namespaces is a
/// denial: the impersonated path would have been 403'd by the apiserver, so
/// answering `[]` would not be a subset of what impersonation returns but a
/// *different* answer — one that tells the user their namespace is empty when in
/// fact they may not look at it.
///
/// A **cluster-wide** list is deliberately not a denial. The apiserver returns
/// only what the caller may see, so a caller permitted nowhere gets an empty
/// list there too; that is what `kubectl get -A` shows a namespace-scoped user.
fn denied_namespace<'ns>(namespace: Option<&'ns str>, visibility: &Visibility) -> Option<&'ns str> {
    match (namespace, visibility) {
        (Some(ns), Visibility::Namespaces(allowed)) if allowed.is_empty() => Some(ns),
        (Some(_) | None, Visibility::Namespaces(_) | Visibility::All) => None,
    }
}

/// The namespace a read of kind `K` should actually use.
///
/// A cluster-scoped kind has no namespace, so one supplied by a caller — a
/// `/api/v1/clusterrepositories/{namespace}/{name}` route shape, or a UI that
/// carries the currently-selected namespace on every request — is meaningless
/// and must be dropped.
///
/// Applied once at the top of each [`Source`] method so it lands on *all* of
/// the store filter, the `SubjectAccessReview` scope, the impersonated URL and
/// the error message's scope suffix. Dropping it in only one place is what makes
/// the two `Source` arms disagree: `KopiurKind::api` already ignores the
/// namespace for a cluster-scoped kind, so a `Cache` arm that honored it would
/// filter every `ClusterRepository` out of a list the `Impersonated` arm answers
/// in full — and would ask a namespaced SAR about a resource that lives in no
/// namespace, which an authorizer answers "no".
fn effective_namespace<K: KopiurKind>(namespace: Option<&str>) -> Option<&str> {
    if K::NAMESPACED { namespace } else { None }
}

/// Whether an error is the "this CRD is not installed" 404 for a kind the UI
/// treats as optional.
///
/// `SnapshotReplication` is the newest CRD, so a UI running against an older
/// operator will get a 404 for the resource *type* rather than an empty list.
/// That is a degraded-but-usable cluster, not an error: the replication view
/// simply has nothing in it. Every other kind is mandatory and its absence is
/// reported through `kopiur_ops::classify_kube`'s `KindNotInstalled`, which
/// tells the operator to install or upgrade the CRDs.
///
/// The `Cache` path has no equivalent branch: a missing CRD there means the
/// watch never syncs, which [`stores::wait_ready`] surfaces at startup instead.
///
/// Exercised end to end in PR5 (an e2e scenario that deletes the
/// `snapshotreplications` CRD and asserts the list endpoint still answers `[]`);
/// there is no hermetic way to produce a real apiserver 404 here.
fn kind_absent_and_optional<K: KopiurKind>(err: &kube::Error) -> bool {
    if !K::OPTIONAL {
        return false;
    }
    // `kube::Error` is `#[non_exhaustive]`, so a catch-all is unavoidable — but
    // it is a catch-all over an open enum of transport failures, none of which
    // could mean "the CRD is absent".
    match err {
        // Narrowed on the message with the same needle `classify_kube` uses to
        // tell "this resource TYPE is unknown" from any other 404. A 404 that is
        // not the missing-CRD one must still be reported: laundering it into an
        // empty list is how a real failure becomes a reassuring blank page.
        kube::Error::Api(status) => {
            status.code == 404 && status.message.contains(kopiur_ops::KIND_NOT_FOUND_NEEDLE)
        }
        _other => false,
    }
}

/// The 403 a cache-backed read returns when the `SubjectAccessReview` said no.
///
/// Built to the same shape `kopiur_ops::classify_kube` produces for a real
/// apiserver 403, so a handler cannot tell the two paths apart and the UI's
/// error rendering has one case to handle rather than two. The synthesized
/// `Status` carries the message the apiserver would have written, because it is
/// what ends up in the rendered error.
fn forbidden<K: KopiurKind>(
    verb: &'static str,
    namespace: Option<&str>,
    name: Option<&str>,
) -> OpsError {
    // A collection-level denial names no object, matching the apiserver's own
    // message for a forbidden list.
    let subject = match name {
        Some(n) => format!("{plural} {n:?}", plural = K::PLURAL),
        None => K::PLURAL.to_owned(),
    };
    let status = kube::core::Status::failure(
        &format!(
            "{subject}.{group} is forbidden: User cannot {verb} resource \
             \"{plural}\" in API group \"{group}\"{scope}",
            plural = K::PLURAL,
            group = authz::KOPIUR_GROUP,
            scope = scope_suffix(namespace),
        ),
        "Forbidden",
    )
    .with_code(403);

    OpsError::Forbidden {
        verb,
        resource: K::PLURAL,
        scope: scope_suffix(namespace),
        source: Box::new(kube::Error::Api(Box::new(status))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_ops::OpsErrorKind;
    use kube::Resource;

    /// Every `PLURAL` must be the plural `kube::Resource` derives from the CRD
    /// definition. This is the assertion that matters most in this file: the
    /// plural is what a `SubjectAccessReview` and an RBAC rule are written
    /// against, so a typo would ask the apiserver about a resource that does not
    /// exist — and an authorizer's answer for a nonexistent resource is a
    /// confident, useless "no".
    #[test]
    fn every_plural_matches_the_generated_crd() {
        fn check<K: KopiurKind>() {
            assert_eq!(
                K::PLURAL,
                <K as Resource>::plural(&()).as_ref(),
                "{}: KopiurKind::PLURAL must match the CRD's real plural",
                K::KIND,
            );
            assert_eq!(
                K::KIND,
                <K as Resource>::kind(&()).as_ref(),
                "KopiurKind::KIND must match the type's real kind",
            );
            assert_eq!(
                <K as Resource>::group(&()).as_ref(),
                authz::KOPIUR_GROUP,
                "{}: every KopiurKind lives in the kopiur API group",
                K::KIND,
            );
        }

        let _: [(); 9] = for_each_kopiur_kind!(check);
    }

    /// Reads a kube scope marker as a `bool`, so [`KopiurKind::NAMESPACED`] can
    /// be checked against the type system's own answer instead of against a
    /// string the same author wrote twice.
    trait ScopeIsNamespaced {
        const NAMESPACED: bool;
    }
    impl ScopeIsNamespaced for kube::core::NamespaceResourceScope {
        const NAMESPACED: bool = true;
    }
    impl ScopeIsNamespaced for kube::core::ClusterResourceScope {
        const NAMESPACED: bool = false;
    }

    /// `NAMESPACED` decides whether a read builds a namespaced URL and whether a
    /// namespace SAR is even meaningful. Getting it backwards would authorize a
    /// namespaced read cluster-wide, so it is checked against `K::Scope` — the
    /// marker type `kube` derives from the CRD itself — rather than against a
    /// restated kind name.
    #[test]
    fn namespaced_agrees_with_the_types_own_kube_scope() {
        fn check<K: KopiurKind>()
        where
            K::Scope: ScopeIsNamespaced,
        {
            assert_eq!(
                K::NAMESPACED,
                <K::Scope as ScopeIsNamespaced>::NAMESPACED,
                "{}: NAMESPACED disagrees with the type's kube::Resource::Scope",
                K::KIND,
            );
        }

        let _: [(); 9] = for_each_kopiur_kind!(check);
    }

    /// `ClusterRepository` is the sole cluster-scoped kind — the fact the rest of
    /// the crate's scope handling is written around.
    #[test]
    fn exactly_one_kind_is_cluster_scoped() {
        fn is_namespaced<K: KopiurKind>() -> bool {
            K::NAMESPACED
        }
        let flags: [bool; 9] = for_each_kopiur_kind!(is_namespaced);

        assert_eq!(
            flags.iter().filter(|n| **n).count(),
            8,
            "eight of the nine kinds are namespaced; ClusterRepository is the exception",
        );
    }

    /// The `Impersonated` path's URL must match the kind's scope. A namespaced
    /// read has to hit `/namespaces/<ns>/…`, and a cluster-scoped one must never
    /// name a namespace even when a caller supplies one — that URL 404s in a way
    /// that reads like the object is missing.
    // `#[tokio::test]`: `kube::Client::new` wraps the service in a `tower::Buffer`,
    // which spawns its worker on the current runtime.
    #[tokio::test]
    async fn the_impersonated_api_url_matches_the_kinds_scope() {
        // `Client::new` takes any tower service; the request is never sent, so a
        // service that would panic if called is exactly right here.
        let client = kube::Client::new(
            tower::service_fn(|_req: http::Request<kube::client::Body>| async {
                unreachable!("the URL is built without issuing a request");
                #[allow(unreachable_code)]
                Ok::<_, std::convert::Infallible>(http::Response::new(kube::client::Body::empty()))
            }),
            "default",
        );

        let namespaced = Snapshot::api(client.clone(), Some("prod"));
        assert_eq!(namespaced.namespace(), Some("prod"));
        assert!(
            namespaced
                .resource_url()
                .contains("/namespaces/prod/snapshots"),
            "{}",
            namespaced.resource_url(),
        );

        let all = Snapshot::api(client.clone(), None);
        assert_eq!(all.namespace(), None);
        assert!(!all.resource_url().contains("/namespaces/"));

        // The case only a scope-aware constructor gets right.
        let cluster = ClusterRepository::api(client, Some("prod"));
        assert_eq!(cluster.namespace(), None);
        assert!(
            !cluster.resource_url().contains("/namespaces/"),
            "a cluster-scoped read must ignore a namespace, not encode one: {}",
            cluster.resource_url(),
        );
    }

    /// Nine kinds, nine distinct plurals. A duplicated plural would silently
    /// authorize one kind's reads against another's RBAC.
    #[test]
    fn the_nine_kinds_have_nine_distinct_plurals() {
        fn plural_of<K: KopiurKind>() -> &'static str {
            K::PLURAL
        }
        let plurals: [&'static str; 9] = for_each_kopiur_kind!(plural_of);

        let unique: BTreeSet<&'static str> = plurals.iter().copied().collect();
        assert_eq!(unique.len(), 9, "duplicate plural among the Kopiur kinds");
    }

    /// A denied cache-backed `get` must be indistinguishable from a real
    /// apiserver 403 — same variant, same kind classification, and a message
    /// that names the resource and the scope so the user knows what to ask for.
    #[test]
    fn a_denied_cache_get_looks_exactly_like_an_apiserver_403() {
        let err = forbidden::<Snapshot>("get", Some("prod"), Some("nightly"));

        assert_eq!(err.kind(), OpsErrorKind::Forbidden);
        let rendered = err.to_string();
        assert!(rendered.contains("snapshots"), "{rendered}");
        assert!(rendered.contains("in namespace prod"), "{rendered}");
        assert!(
            rendered.contains("Fix:"),
            "every OpsError carries a remediation: {rendered}",
        );

        match err {
            OpsError::Forbidden {
                verb,
                resource,
                ref source,
                ..
            } => {
                assert_eq!(verb, "get");
                assert_eq!(resource, "snapshots");
                match source.as_ref() {
                    kube::Error::Api(status) => assert_eq!(status.code, 403),
                    other => panic!("expected an Api error, got {other:?}"),
                }
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    /// A cluster-scoped kind has no namespace to name, so its 403 must not claim
    /// one.
    #[test]
    fn a_cluster_scoped_denial_names_no_namespace() {
        let rendered = forbidden::<ClusterRepository>("get", None, Some("central")).to_string();
        assert!(rendered.contains("clusterrepositories"), "{rendered}");
        assert!(
            !rendered.contains("in namespace"),
            "a cluster-scoped resource has no namespace: {rendered}",
        );
    }

    /// A collection-level denial must name the resource but no object — the
    /// apiserver's own shape for a forbidden list. A message claiming an object
    /// name the caller never asked for would send them looking for the wrong
    /// thing.
    #[test]
    fn a_denied_list_names_the_resource_but_no_object() {
        let err = forbidden::<Snapshot>("list", Some("prod"), None);

        assert_eq!(err.kind(), OpsErrorKind::Forbidden);
        let rendered = err.to_string();
        assert!(rendered.contains("snapshots"), "{rendered}");
        assert!(rendered.contains("in namespace prod"), "{rendered}");

        match err {
            OpsError::Forbidden {
                verb,
                resource,
                ref source,
                ..
            } => {
                assert_eq!(verb, "list");
                assert_eq!(resource, "snapshots");
                match source.as_ref() {
                    kube::Error::Api(status) => {
                        assert_eq!(status.code, 403);
                        // The apiserver's shape for a forbidden collection:
                        // "<plural>.<group> is forbidden: …", with no object.
                        assert!(
                            status
                                .message
                                .starts_with("snapshots.kopiur.home-operations.com is forbidden:"),
                            "a collection denial must not name an object: {}",
                            status.message,
                        );
                    }
                    other => panic!("expected an Api error, got {other:?}"),
                }
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    /// The rule the two `Source` arms must agree on, in both directions.
    ///
    /// An explicitly-named namespace the caller may not list is a 403 on the
    /// impersonated path, so the cache must not answer `[]` — that would tell
    /// the user their namespace is empty when they simply may not look at it.
    /// A *cluster-wide* list is the opposite: the apiserver returns only what the
    /// caller may see, so a caller permitted nowhere legitimately gets `[]`,
    /// exactly as `kubectl get -A` does.
    #[test]
    fn only_an_explicitly_named_namespace_turns_an_empty_visibility_into_a_denial() {
        let nothing = Visibility::Namespaces(BTreeSet::new());
        let something = Visibility::Namespaces(["prod".to_owned()].into());

        assert_eq!(
            denied_namespace(Some("prod"), &nothing),
            Some("prod"),
            "a named namespace the caller may not list is a denial, not an empty list",
        );
        assert_eq!(
            denied_namespace(None, &nothing),
            None,
            "a cluster-wide list by a caller permitted nowhere is an empty list, not a denial",
        );
        assert_eq!(
            denied_namespace(Some("prod"), &something),
            None,
            "a permitted namespace is never a denial",
        );
        assert_eq!(denied_namespace(Some("prod"), &Visibility::All), None);
        assert_eq!(denied_namespace(None, &Visibility::All), None);
    }

    /// A namespace supplied for the cluster-scoped kind must be dropped before
    /// it reaches the store filter, the SAR, the URL, or the error message.
    ///
    /// This is the one place the two `Source` arms could silently disagree:
    /// `KopiurKind::api` ignores the namespace for a cluster-scoped kind, so if
    /// the `Cache` arm honored it, a caller who passed a namespace would get
    /// every `ClusterRepository` from `Impersonated` and none from `Cache`.
    #[test]
    fn a_namespace_is_dropped_for_the_cluster_scoped_kind() {
        assert_eq!(
            effective_namespace::<ClusterRepository>(Some("prod")),
            None,
            "a cluster-scoped read must ignore a supplied namespace",
        );
        assert_eq!(effective_namespace::<ClusterRepository>(None), None);

        // …and is preserved for every namespaced kind.
        fn check<K: KopiurKind>() {
            if !K::NAMESPACED {
                return;
            }
            assert_eq!(
                effective_namespace::<K>(Some("prod")),
                Some("prod"),
                "{}: a namespaced read must keep its namespace",
                K::KIND,
            );
            assert_eq!(effective_namespace::<K>(None), None);
        }
        let _: [(); 9] = for_each_kopiur_kind!(check);
    }

    /// The optional-CRD escape hatch is for `SnapshotReplication` alone. If it
    /// widened, a missing `snapshots` CRD would render as an empty backup list
    /// on a broken cluster — the exact silent-success failure the UI must not
    /// have.
    #[test]
    fn only_snapshot_replication_tolerates_a_missing_crd() {
        let not_found = kube::Error::Api(Box::new(
            kube::core::Status::failure("could not find the requested resource", "NotFound")
                .with_code(404),
        ));

        assert!(kind_absent_and_optional::<SnapshotReplication>(&not_found));

        fn check<K: KopiurKind>() {
            if K::PLURAL == SnapshotReplication::PLURAL {
                return;
            }
            let err = kube::Error::Api(Box::new(
                kube::core::Status::failure("could not find the requested resource", "NotFound")
                    .with_code(404),
            ));
            assert!(
                !kind_absent_and_optional::<K>(&err),
                "{}: a missing CRD must be reported, not rendered as an empty list",
                K::KIND,
            );
        }
        let _: [(); 9] = for_each_kopiur_kind!(check);
    }

    /// Any status other than 404 is a real failure even for the optional kind —
    /// a 403 on `snapshotreplications` must not be laundered into an empty list.
    #[test]
    fn a_non_404_on_the_optional_kind_is_still_an_error() {
        let forbidden_err = kube::Error::Api(Box::new(
            kube::core::Status::failure("denied", "Forbidden").with_code(403),
        ));
        assert!(!kind_absent_and_optional::<SnapshotReplication>(
            &forbidden_err
        ));
    }
}
