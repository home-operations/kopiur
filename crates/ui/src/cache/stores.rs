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
//!
//! Readiness itself is published on a `tokio::sync::watch` channel rather than
//! exposed as an awaitable function, because kube's per-store readiness holds a
//! single waker and two concurrent waiters on one cold store can deadlock. See
//! [`start`].

use std::sync::Arc;

use futures::StreamExt as _;
use kube::Api;
use kube::runtime::reflector::{Store, store};
use kube::runtime::{WatchStreamExt as _, reflector, watcher};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use kopiur_api::{
    ClusterRepository, Maintenance, Repository, RepositoryReplication, Restore, Snapshot,
    SnapshotPolicy, SnapshotReplication, SnapshotSchedule,
};

use crate::cache::KopiurKind;
use crate::metrics::UiMetrics;

/// A watch-fed read cache for every Kopiur kind.
///
/// Cheap to clone (each [`Store`] is an `Arc` handle onto shared state), so this
/// is passed by value into [`crate::cache::Source::Cache`] rather than behind
/// another layer of indirection.
///
/// Every field is `pub(super)`, not `pub`: the stores hold objects the apiserver
/// did NOT filter for any caller, so reaching one from outside this module would
/// be a way to skip [`crate::cache::authz::filter_visible`] entirely. Handlers
/// get their data from [`crate::cache::Source::list`]/`get` and nothing else.
/// [`KopiurKind::store`] can project a field out, but only for code holding a
/// [`StoreAccess`] witness, which cannot be constructed outside `cache`.
#[derive(Clone)]
pub struct Stores {
    /// Namespaced `Repository` objects.
    pub(super) repositories: Store<Repository>,
    /// Cluster-scoped `ClusterRepository` objects.
    pub(super) cluster_repositories: Store<ClusterRepository>,
    /// `SnapshotPolicy` recipes.
    pub(super) policies: Store<SnapshotPolicy>,
    /// `Snapshot` invocations. The largest store by object count in any real
    /// fleet — this is the one the cache exists for.
    pub(super) snapshots: Store<Snapshot>,
    /// `SnapshotSchedule` schedules.
    pub(super) schedules: Store<SnapshotSchedule>,
    /// `Restore` invocations.
    pub(super) restores: Store<Restore>,
    /// `Maintenance` objects, both operator-projected and externally authored.
    pub(super) maintenances: Store<Maintenance>,
    /// `RepositoryReplication` objects.
    pub(super) repository_replications: Store<RepositoryReplication>,
    /// `SnapshotReplication` objects.
    pub(super) snapshot_replications: Store<SnapshotReplication>,
}

impl std::fmt::Debug for Stores {
    /// Sizes only. The derived `Debug` would render every cached object, so a
    /// single `tracing` call at debug level could dump the whole fleet — object
    /// bodies the caller may not even be authorized to see — into the log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stores")
            .field("repositories", &self.repositories.len())
            .field("cluster_repositories", &self.cluster_repositories.len())
            .field("policies", &self.policies.len())
            .field("snapshots", &self.snapshots.len())
            .field("schedules", &self.schedules.len())
            .field("restores", &self.restores.len())
            .field("maintenances", &self.maintenances.len())
            .field(
                "repository_replications",
                &self.repository_replications.len(),
            )
            .field("snapshot_replications", &self.snapshot_replications.len())
            .finish()
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
/// retries forever.
///
/// # Readiness
///
/// The third return value reports whether every store has completed its initial
/// list. Until it reads `true` the cache is *cold*, and a cold store is worse
/// than no cache: a `Snapshot` list would answer `[]`, which is exactly what a
/// healthy cluster that has taken no backups looks like. The UI must not report
/// ready before it flips.
///
/// It is a `watch::Receiver` rather than a `wait_ready(&Stores).await` for a
/// specific reason. kube delivers per-store readiness through `DelayedInit`,
/// whose pending path is a `oneshot::Receiver` — which holds exactly **one**
/// waker. Two futures awaiting the *same* still-cold store clobber each other's
/// waker and one may never be woken; this repository has been bitten by that
/// before. So `start` performs the one and only `wait_until_ready` join itself,
/// in a task it owns, and publishes the result on a channel that any number of
/// callers may clone, poll, and await safely.
#[must_use]
pub fn start(
    client: kube::Client,
    metrics: Arc<UiMetrics>,
) -> (Stores, Vec<JoinHandle<()>>, watch::Receiver<bool>) {
    let mut tasks = Vec::with_capacity(10);

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

    let (ready_tx, ready_rx) = watch::channel(false);
    tasks.push(tokio::spawn(publish_readiness(stores.clone(), ready_tx)));

    (stores, tasks, ready_rx)
}

/// The single owner of every `wait_until_ready` future.
///
/// Joins the nine concurrently — safe, because each store has its own
/// `DelayedInit` and so its own waker slot; the hazard is only ever two waiters
/// on one store — and publishes `true` once all nine have listed.
///
/// On a dropped writer it publishes nothing and logs: the cache can never become
/// correct, and leaving the flag `false` is what keeps the UI from reporting
/// ready over a cache that will stay empty forever.
async fn publish_readiness(stores: Stores, ready_tx: watch::Sender<bool>) {
    let synced = tokio::try_join!(
        stores.repositories.wait_until_ready(),
        stores.cluster_repositories.wait_until_ready(),
        stores.policies.wait_until_ready(),
        stores.snapshots.wait_until_ready(),
        stores.schedules.wait_until_ready(),
        stores.restores.wait_until_ready(),
        stores.maintenances.wait_until_ready(),
        stores.repository_replications.wait_until_ready(),
        stores.snapshot_replications.wait_until_ready(),
    );

    match synced {
        Ok(_) => {
            tracing::info!("kopiur-ui cache synced; all nine stores have completed initial list");
            // A send error means every receiver was dropped, i.e. the process is
            // shutting down. Nothing to report to.
            let _ = ready_tx.send(true);
        }
        Err(e) => tracing::error!(
            error = %e,
            "a kopiur-ui cache writer was dropped before its first sync; the cache will \
             never become ready and the UI must not serve from it",
        ),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use kube::Resource;
    use kube::runtime::reflector::store::Writer;

    use crate::cache::StoreAccess;

    /// The nine writers, held so the stores stay cold until a test says
    /// otherwise.
    ///
    /// `store::<K>()` returns a reader and a writer; dropping a writer makes its
    /// store permanently un-syncable, so a test that wants to control readiness
    /// has to keep them. `Event::InitDone` is what kube uses to mark a store's
    /// initial list complete, which is exactly the transition under test.
    struct Writers {
        repositories: Writer<Repository>,
        cluster_repositories: Writer<ClusterRepository>,
        policies: Writer<SnapshotPolicy>,
        snapshots: Writer<Snapshot>,
        schedules: Writer<SnapshotSchedule>,
        restores: Writer<Restore>,
        maintenances: Writer<Maintenance>,
        repository_replications: Writer<RepositoryReplication>,
        snapshot_replications: Writer<SnapshotReplication>,
    }

    impl Writers {
        fn new() -> Self {
            Self {
                repositories: Writer::default(),
                cluster_repositories: Writer::default(),
                policies: Writer::default(),
                snapshots: Writer::default(),
                schedules: Writer::default(),
                restores: Writer::default(),
                maintenances: Writer::default(),
                repository_replications: Writer::default(),
                snapshot_replications: Writer::default(),
            }
        }

        fn stores(&self) -> Stores {
            Stores {
                repositories: self.repositories.as_reader(),
                cluster_repositories: self.cluster_repositories.as_reader(),
                policies: self.policies.as_reader(),
                snapshots: self.snapshots.as_reader(),
                schedules: self.schedules.as_reader(),
                restores: self.restores.as_reader(),
                maintenances: self.maintenances.as_reader(),
                repository_replications: self.repository_replications.as_reader(),
                snapshot_replications: self.snapshot_replications.as_reader(),
            }
        }

        /// Complete the initial list of every store but `snapshots`, so a test
        /// can prove one laggard holds the whole cache un-ready.
        fn mark_all_ready_except_snapshots(&mut self) {
            self.repositories
                .apply_watcher_event(&watcher::Event::InitDone);
            self.cluster_repositories
                .apply_watcher_event(&watcher::Event::InitDone);
            self.policies.apply_watcher_event(&watcher::Event::InitDone);
            self.schedules
                .apply_watcher_event(&watcher::Event::InitDone);
            self.restores.apply_watcher_event(&watcher::Event::InitDone);
            self.maintenances
                .apply_watcher_event(&watcher::Event::InitDone);
            self.repository_replications
                .apply_watcher_event(&watcher::Event::InitDone);
            self.snapshot_replications
                .apply_watcher_event(&watcher::Event::InitDone);
        }

        fn mark_snapshots_ready(&mut self) {
            self.snapshots
                .apply_watcher_event(&watcher::Event::InitDone);
        }
    }

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
        let a = StoreAccess::new();
        let addresses = [
            std::ptr::from_ref(Repository::store(&stores, a)).addr(),
            std::ptr::from_ref(ClusterRepository::store(&stores, a)).addr(),
            std::ptr::from_ref(SnapshotPolicy::store(&stores, a)).addr(),
            std::ptr::from_ref(Snapshot::store(&stores, a)).addr(),
            std::ptr::from_ref(SnapshotSchedule::store(&stores, a)).addr(),
            std::ptr::from_ref(Restore::store(&stores, a)).addr(),
            std::ptr::from_ref(Maintenance::store(&stores, a)).addr(),
            std::ptr::from_ref(RepositoryReplication::store(&stores, a)).addr(),
            std::ptr::from_ref(SnapshotReplication::store(&stores, a)).addr(),
        ];
        let unique: BTreeSet<usize> = addresses.iter().copied().collect();
        assert_eq!(
            unique.len(),
            addresses.len(),
            "two KopiurKind::store impls project the same field",
        );
    }

    /// `Debug` must never render cached objects. A `tracing` call at debug level
    /// would otherwise dump the whole fleet — bodies the caller may not even be
    /// authorized to see — into the log.
    #[test]
    fn debug_renders_sizes_not_objects() {
        let rendered = format!("{:?}", empty_stores());
        assert!(rendered.contains("snapshots: 0"), "{rendered}");
        assert!(
            !rendered.contains("ObjectMeta") && !rendered.contains("spec"),
            "Stores must never render object bodies: {rendered}",
        );
    }

    /// Readiness must not flip until every store has listed, and must flip once
    /// they have. This is the contract `/readyz` hangs off: a `true` here means
    /// an empty `Snapshot` list is a genuinely empty fleet rather than a cold
    /// cache.
    #[tokio::test]
    async fn readiness_flips_only_once_every_store_has_synced() {
        let mut writers = Writers::new();
        let stores = writers.stores();
        let (tx, mut rx) = watch::channel(false);
        let task = tokio::spawn(publish_readiness(stores, tx));

        assert!(!*rx.borrow(), "a cold cache must not report ready");

        // Mark eight of nine synced; readiness must still be false.
        writers.mark_all_ready_except_snapshots();
        tokio::task::yield_now().await;
        assert!(
            !*rx.borrow(),
            "one un-synced store must hold the whole cache un-ready",
        );

        writers.mark_snapshots_ready();
        rx.changed().await.expect("the publisher must send");
        assert!(*rx.borrow(), "all nine synced means ready");

        task.await.expect("publisher task must not panic");
    }

    /// A dropped writer must leave readiness `false` forever rather than hang or
    /// falsely report ready: startup has to be able to tell "still listing" from
    /// "this cache will never work".
    #[tokio::test]
    async fn a_dropped_writer_never_reports_ready() {
        let stores = empty_stores(); // every writer already dropped
        let (tx, rx) = watch::channel(false);

        // The publisher must RETURN (not hang) and must not have sent `true`.
        publish_readiness(stores, tx).await;
        assert!(
            !*rx.borrow(),
            "a cache that can never sync must never report ready",
        );
    }
}
