//! One `kube::runtime::reflector::Store` per Kopiur kind, fed by watches under
//! the UI's own ServiceAccount.
//!
//! Readiness is gated on every store having synced, so the UI never serves a
//! still-filling cache as if it were the real fleet — an empty repository list
//! reads identically to a healthy cluster with nothing configured.
//!
//! # What these stores are and are not
//!
//! They are a *performance* structure, nothing else. Objects land here because
//! the UI's own ServiceAccount could list them, which means the apiserver did
//! **not** filter them for whoever is asking. Nothing in this module may be
//! handed to a caller directly: every read goes out through
//! [`crate::cache::Source`], which gates it with a `SubjectAccessReview`
//! ([`crate::cache::authz`]). That is the whole reason the cache is allowed to
//! exist without breaking the crate's impersonation guarantee.
//!
//! # Driving the watches
//!
//! kube's `Store::wait_until_ready()` does **not** drive the underlying watch —
//! the reflector stream has to be polled separately, or readiness never
//! arrives. [`start`] therefore returns a `JoinHandle` per kind; the caller owns
//! them for the process lifetime. Dropping one silently stops refreshing that
//! kind, which would serve an ever-staler cache with no signal, so they are
//! returned rather than detached here.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::StreamExt as _;
use kube::Api;
use kube::runtime::reflector::{Store, store};
use kube::runtime::{WatchStreamExt as _, reflector, watcher};
use tokio::task::JoinHandle;

use kopiur_api::{
    ClusterRepository, Maintenance, Repository, RepositoryReplication, Restore, Snapshot,
    SnapshotPolicy, SnapshotReplication, SnapshotSchedule,
};

use crate::cache::KopiurKind;
use crate::metrics::UiMetrics;

pub use kube::runtime::reflector::store::WriterDropped;

/// A watch-fed read cache for every Kopiur kind.
///
/// Cheap to clone (each [`Store`] is an `Arc` handle onto shared state), so this
/// is passed by value into [`crate::cache::Source::Cache`] rather than behind
/// another layer of indirection.
///
/// The fields are `pub` because [`KopiurKind::store`] projects one out
/// generically; they are still not a read API. Reach a store through
/// [`crate::cache::Source`], never directly — see the module docs for why.
#[derive(Debug, Clone)]
pub struct Stores {
    /// Namespaced `Repository` objects.
    pub repositories: Store<Repository>,
    /// Cluster-scoped `ClusterRepository` objects.
    pub cluster_repositories: Store<ClusterRepository>,
    /// `SnapshotPolicy` recipes.
    pub policies: Store<SnapshotPolicy>,
    /// `Snapshot` invocations. The largest store by object count in any real
    /// fleet — this is the one the cache exists for.
    pub snapshots: Store<Snapshot>,
    /// `SnapshotSchedule` schedules.
    pub schedules: Store<SnapshotSchedule>,
    /// `Restore` invocations.
    pub restores: Store<Restore>,
    /// `Maintenance` objects, both operator-projected and externally authored.
    pub maintenances: Store<Maintenance>,
    /// `RepositoryReplication` objects.
    pub repository_replications: Store<RepositoryReplication>,
    /// `SnapshotReplication` objects.
    pub snapshot_replications: Store<SnapshotReplication>,
}

impl Stores {
    /// Every namespace that currently holds at least one object of kind `K`.
    ///
    /// This is the candidate set a cache-backed cross-namespace list hands to
    /// [`crate::cache::authz::SarCache::visible_namespaces`]: there is no point
    /// asking the apiserver whether the caller may list in a namespace that
    /// holds nothing to show them. Cluster-scoped kinds yield an empty set,
    /// which is correct — for them only a cluster-wide allow returns anything.
    #[must_use]
    pub fn namespaces_present<K: KopiurKind>(&self) -> BTreeSet<String> {
        K::store(self)
            .state()
            .iter()
            .filter_map(|obj| kube::Resource::meta(obj.as_ref()).namespace.clone())
            .collect()
    }
}

/// Start one reflector per Kopiur kind and return the read handles plus the
/// tasks driving them.
///
/// Every watch is cluster-wide (`Api::all`) because the UI serves the whole
/// fleet; per-caller scoping is authorization's job, not the watch's. Streaming
/// lists are deliberately **not** enabled: kube's watcher does not degrade from
/// `WatchList` to paged lists on a server that lacks the feature, so enabling it
/// here would wedge all nine watches against an older apiserver with no
/// fallback. The UI would then report ready with nine permanently empty stores.
///
/// The returned handles must be kept alive for as long as the cache is served
/// from. This function never fails: a watch that cannot start backs off and
/// retries forever, and [`wait_ready`] is what decides whether the UI may
/// declare itself ready.
#[must_use]
pub fn start(client: kube::Client, metrics: Arc<UiMetrics>) -> (Stores, Vec<JoinHandle<()>>) {
    let mut tasks = Vec::with_capacity(9);

    let repositories = spawn::<Repository>(&client, &metrics, &mut tasks);
    let cluster_repositories = spawn::<ClusterRepository>(&client, &metrics, &mut tasks);
    let policies = spawn::<SnapshotPolicy>(&client, &metrics, &mut tasks);
    let snapshots = spawn::<Snapshot>(&client, &metrics, &mut tasks);
    let schedules = spawn::<SnapshotSchedule>(&client, &metrics, &mut tasks);
    let restores = spawn::<Restore>(&client, &metrics, &mut tasks);
    let maintenances = spawn::<Maintenance>(&client, &metrics, &mut tasks);
    let repository_replications = spawn::<RepositoryReplication>(&client, &metrics, &mut tasks);
    let snapshot_replications = spawn::<SnapshotReplication>(&client, &metrics, &mut tasks);

    let stores = Stores {
        repositories,
        cluster_repositories,
        policies,
        snapshots,
        schedules,
        restores,
        maintenances,
        repository_replications,
        snapshot_replications,
    };
    (stores, tasks)
}

/// Create one kind's store and spawn the task that keeps it filled.
///
/// Generic over [`KopiurKind`] so the metric label and the watched type cannot
/// drift apart: `K::KIND` is asserted against `<K as kube::Resource>::kind` in
/// this module's tests, so there is no hand-paired string to get wrong.
fn spawn<K: KopiurKind>(
    client: &kube::Client,
    metrics: &Arc<UiMetrics>,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Store<K> {
    let (reader, writer) = store::<K>();
    let api: Api<K> = Api::all(client.clone());
    let gauge_reader = reader.clone();
    let metrics = Arc::clone(metrics);

    tasks.push(tokio::spawn(async move {
        // `.default_backoff()` on the watcher (not on the reflector output) so a
        // failing watch retries with jitter instead of hot-looping the
        // apiserver; the reflector then sees an already-debounced stream.
        let stream = reflector(
            writer,
            watcher(api, watcher::Config::default()).default_backoff(),
        );
        futures::pin_mut!(stream);

        while let Some(event) = stream.next().await {
            match event {
                // Any event may have changed the store's size — including a
                // deletion, which is exactly the case a "record on insert only"
                // gauge would get wrong. Read the authoritative length instead
                // of tracking a delta.
                Ok(_) => metrics.set_cache_objects(K::KIND, gauge_reader.len()),
                // Loud and counted-by-absence: a watch error means this kind's
                // store stops refreshing until the backoff expires, and a
                // silently stale store is indistinguishable from a quiet
                // cluster. The stream restarts itself, so this is a warning,
                // not a shutdown.
                Err(e) => tracing::warn!(
                    kind = K::KIND,
                    error = %e,
                    "kopiur-ui cache watch error; backing off and restarting the watch",
                ),
            }
        }

        // The reflector stream is infinite, so reaching here means the watcher
        // gave up entirely. The store is now frozen at whatever it last held.
        tracing::error!(
            kind = K::KIND,
            "kopiur-ui cache watch ended; this kind's cache is now frozen and will go stale",
        );
    }));

    reader
}

/// Wait until every kind's store has completed its initial list.
///
/// Until this resolves the stores are *cold*, and a cold store is worse than no
/// cache: `Snapshot` list would answer `[]`, which reads exactly like a healthy
/// cluster that has taken no backups. The UI must not report ready before this
/// returns `Ok`.
///
/// # Errors
///
/// [`WriterDropped`] if some kind's reflector task was dropped before its first
/// sync — the cache can never become correct, so the caller should fail startup
/// rather than serve from it.
///
/// # Concurrency
///
/// Call this from **one** task at a time. Readiness is delivered through kube's
/// `DelayedInit`, whose pending path is a `oneshot::Receiver` and therefore
/// stores a single waker: two futures awaiting the *same* store while it is
/// still cold clobber each other's waker and one may never be woken. Once a
/// store is ready the value is memoized and any number of callers get it
/// immediately, so a readiness probe that polls after startup is fine — what is
/// not fine is two concurrent `wait_ready` calls racing a cold cache.
pub async fn wait_ready(stores: &Stores) -> Result<(), WriterDropped> {
    // Concurrent, not sequential: nine independent initial lists, and each store
    // has its own `DelayedInit`, so there is no single-waker contention between
    // these nine distinct futures.
    tokio::try_join!(
        stores.repositories.wait_until_ready(),
        stores.cluster_repositories.wait_until_ready(),
        stores.policies.wait_until_ready(),
        stores.snapshots.wait_until_ready(),
        stores.schedules.wait_until_ready(),
        stores.restores.wait_until_ready(),
        stores.maintenances.wait_until_ready(),
        stores.repository_replications.wait_until_ready(),
        stores.snapshot_replications.wait_until_ready(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::Resource;

    /// Build a `Stores` whose every store is empty and permanently cold. No
    /// client, no cluster: `store::<K>()` hands back a reader and a writer, and
    /// dropping the writer is what the pure read-path tests want.
    fn empty_stores() -> Stores {
        Stores {
            repositories: store::<Repository>().0,
            cluster_repositories: store::<ClusterRepository>().0,
            policies: store::<SnapshotPolicy>().0,
            snapshots: store::<Snapshot>().0,
            schedules: store::<SnapshotSchedule>().0,
            restores: store::<Restore>().0,
            maintenances: store::<Maintenance>().0,
            repository_replications: store::<RepositoryReplication>().0,
            snapshot_replications: store::<SnapshotReplication>().0,
        }
    }

    /// `K::KIND` is the metric label AND the `classify_kube` kind argument. If
    /// it drifted from the real kind, `kopiur_ui_cache_objects{kind=…}` would
    /// name the wrong resource and error messages would misreport what failed.
    #[test]
    fn every_kind_label_matches_the_resources_real_kind() {
        fn check<K: KopiurKind>() {
            assert_eq!(
                K::KIND,
                <K as Resource>::kind(&()).as_ref(),
                "KopiurKind::KIND must equal the type's real Kubernetes kind",
            );
        }

        check::<Repository>();
        check::<ClusterRepository>();
        check::<SnapshotPolicy>();
        check::<Snapshot>();
        check::<SnapshotSchedule>();
        check::<Restore>();
        check::<Maintenance>();
        check::<RepositoryReplication>();
        check::<SnapshotReplication>();
    }

    /// `KopiurKind::store` must project out the field for its *own* kind. The
    /// nine impls are hand-written, so a copy-paste that pointed `Restore` at
    /// the `Snapshot` store would compile (both are `Store<_>` only after
    /// monomorphization catches it — but a field swap between two same-typed
    /// stores would not). Distinct pointer identities prove no two projections
    /// alias.
    #[test]
    fn each_kind_projects_a_distinct_store() {
        let stores = empty_stores();

        // Compare by the address of the projected field: nine different fields
        // must yield nine different addresses.
        let addresses = [
            std::ptr::from_ref(Repository::store(&stores)).addr(),
            std::ptr::from_ref(ClusterRepository::store(&stores)).addr(),
            std::ptr::from_ref(SnapshotPolicy::store(&stores)).addr(),
            std::ptr::from_ref(Snapshot::store(&stores)).addr(),
            std::ptr::from_ref(SnapshotSchedule::store(&stores)).addr(),
            std::ptr::from_ref(Restore::store(&stores)).addr(),
            std::ptr::from_ref(Maintenance::store(&stores)).addr(),
            std::ptr::from_ref(RepositoryReplication::store(&stores)).addr(),
            std::ptr::from_ref(SnapshotReplication::store(&stores)).addr(),
        ];
        let unique: BTreeSet<usize> = addresses.iter().copied().collect();
        assert_eq!(
            unique.len(),
            addresses.len(),
            "two KopiurKind::store impls project the same field",
        );
    }

    #[test]
    fn an_empty_store_offers_no_candidate_namespaces() {
        let stores = empty_stores();
        assert!(stores.namespaces_present::<Snapshot>().is_empty());
        // Cluster-scoped: never any namespaces, however full the store is.
        assert!(stores.namespaces_present::<ClusterRepository>().is_empty());
    }

    /// A dropped writer must surface as `WriterDropped`, not hang: startup has
    /// to be able to tell "still listing" from "this cache will never work".
    #[tokio::test]
    async fn wait_ready_reports_a_dropped_writer_rather_than_hanging() {
        let stores = empty_stores(); // every writer already dropped
        assert!(
            wait_ready(&stores).await.is_err(),
            "a cache that can never sync must fail readiness, not block forever",
        );
    }
}
