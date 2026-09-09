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
//!
//! That channel carries a [`CacheHealth`], not a `bool`, and it stays open for
//! the process's life. A cache that synced and then lost a watch is not the same
//! thing as a cache that never synced — but it is just as unservable, and a
//! `bool` that only ever went `false → true` could not say so. The supervisor
//! that publishes the first `Synced` therefore keeps the reflector handles and
//! publishes `WatchEnded` the moment one of them finishes.

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

/// What the reflector cache is doing, as the stores themselves see it.
///
/// Published on the single `watch` channel [`start`] returns, which is the whole
/// truth about the cache: nothing else in the process may decide the cache is
/// healthy. A closed enum rather than the `bool` this used to be, because
/// "not synced yet" and "synced, then a watch died" need completely different
/// answers from `/readyz` and only one of them can ever improve on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheHealth {
    /// At least one store has not completed its initial list.
    Syncing,
    /// Every store has listed and every reflector is still running.
    Synced,
    /// A reflector task ended, so this kind's store is frozen at whatever it
    /// last held and will silently go stale.
    ///
    /// Terminal: a reflector that gave up does not restart itself, so this never
    /// returns to [`CacheHealth::Synced`] without a new process.
    WatchEnded {
        /// The Kubernetes kind whose reflector ended.
        kind: &'static str,
    },
}

/// One reflector task, labelled with the kind it keeps cached.
///
/// The label is the whole point: a bare `JoinHandle` that finished would tell an
/// operator that *something* stopped refreshing, which is exactly the half of the
/// message that does not help.
struct ReflectorTask {
    kind: &'static str,
    handle: JoinHandle<()>,
}

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
/// This function never fails: a watch that cannot start backs off and retries
/// forever. The reflector tasks are owned by the supervisor it spawns, so the
/// caller has nothing to hold on to and nothing it can accidentally drop.
///
/// # Readiness
///
/// The second return value is the single source of truth about the cache, for
/// the whole process's life. Until it reads [`CacheHealth::Synced`] the cache is
/// *cold*, and a cold store is worse than no cache: a `Snapshot` list would
/// answer `[]`, which is exactly what a healthy cluster that has taken no
/// backups looks like. It reads [`CacheHealth::WatchEnded`] once any reflector
/// ends, because a frozen store is the same lie told later.
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
) -> (Stores, watch::Receiver<CacheHealth>) {
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

    let (health_tx, health_rx) = watch::channel(CacheHealth::Syncing);
    tokio::spawn(supervise(stores.clone(), tasks, health_tx));

    (stores, health_rx)
}

/// The one task that owns the readiness join AND the reflector handles.
///
/// Both halves live here because they are one question — "may the UI serve from
/// this cache?" — and splitting them is what let readiness lie: a supervisor that
/// returned after publishing `Synced` would drop the sender and never again be in
/// a position to say otherwise, so a watch that gave up an hour later left
/// `/readyz` answering `ok` over a store frozen at whatever it last held.
///
/// It therefore never returns while the process is healthy. It publishes
/// `Synced` when every store has listed, and then waits for the *first* reflector
/// to finish — which, since the reflector stream is infinite, only happens when
/// that watch has given up entirely.
async fn supervise(
    stores: Stores,
    tasks: Vec<ReflectorTask>,
    health_tx: watch::Sender<CacheHealth>,
) {
    if initial_list_complete(&stores).await {
        tracing::info!("kopiur-ui cache synced; all nine stores have completed initial list");
        // A send error means every receiver was dropped, i.e. the process is
        // shutting down. Nothing to report to.
        let _ = health_tx.send(CacheHealth::Synced);
    }

    // Reached on both paths. A writer dropped before the first sync means its
    // reflector task has ALREADY ended, so this resolves immediately and names
    // the kind that the `wait_until_ready` error could not.
    let kind = first_reflector_to_end(tasks).await;
    tracing::error!(
        kind,
        "a kopiur-ui cache watch ended; that store is now frozen and will go stale, so the \
         UI must stop serving from the cache. /readyz reports cache-watch-ended. This does \
         not recover on its own — restart the pod, and check the apiserver's health and the \
         UI ServiceAccount's list/watch RBAC for this kind."
    );
    let _ = health_tx.send(CacheHealth::WatchEnded { kind });
}

/// Await every store's initial list. `false` means a writer was dropped first, so
/// the cache can never become correct.
///
/// Joins the nine concurrently — safe, because each store has its own
/// `DelayedInit` and so its own waker slot; the hazard is only ever two waiters
/// on one store.
async fn initial_list_complete(stores: &Stores) -> bool {
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
        Ok(_) => true,
        Err(e) => {
            tracing::error!(
                error = %e,
                "a kopiur-ui cache writer was dropped before its first sync; the cache will \
                 never become ready and the UI must not serve from it",
            );
            false
        }
    }
}

/// The kind of the first reflector task to finish.
///
/// Each handle is wrapped in a future that yields its own label, so the answer
/// carries which kind died rather than merely that one did. A task that panicked
/// counts the same as one that returned: either way that kind has stopped
/// refreshing.
///
/// Never resolves while every reflector is alive, which is the healthy case.
async fn first_reflector_to_end(tasks: Vec<ReflectorTask>) -> &'static str {
    let waits: Vec<_> = tasks
        .into_iter()
        .map(|task| {
            Box::pin(async move {
                let _ = task.handle.await;
                task.kind
            })
        })
        .collect();

    // `select_all` panics on an empty list; nine kinds are spawned
    // unconditionally, so this is unreachable, and hanging forever would be a
    // worse answer than saying the cache is unusable.
    if waits.is_empty() {
        return "none";
    }
    let (kind, _index, _rest) = futures::future::select_all(waits).await;
    kind
}

/// Create one kind's store and spawn the task that keeps it filled.
///
/// Generic over [`KopiurKind`] so the metric label and the watched type cannot
/// drift apart: `K::KIND` is asserted against `<K as kube::Resource>::kind` in
/// this module's tests, so there is no hand-paired string to get wrong.
fn spawn<K: KopiurKind>(
    client: &kube::Client,
    metrics: &Arc<UiMetrics>,
    tasks: &mut Vec<ReflectorTask>,
) -> Store<K> {
    let (reader, writer) = store::<K>();
    let api: Api<K> = Api::all(client.clone());
    let gauge_reader = reader.clone();
    let metrics = Arc::clone(metrics);

    let handle = tokio::spawn(async move {
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
        // `supervise` turns this task ending into `CacheHealth::WatchEnded`, so
        // the log below is the detail and readiness is the consequence.
        tracing::error!(
            kind = K::KIND,
            "kopiur-ui cache watch ended; this kind's cache is now frozen and will go stale",
        );
    });
    tasks.push(ReflectorTask {
        kind: K::KIND,
        handle,
    });

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

    /// One reflector task per kind that never ends on its own, so a test can end
    /// exactly the one it wants to and prove the supervisor names it.
    fn immortal_tasks() -> Vec<ReflectorTask> {
        KINDS
            .iter()
            .map(|kind| ReflectorTask {
                kind,
                handle: tokio::spawn(std::future::pending::<()>()),
            })
            .collect()
    }

    /// The nine kinds, in the order `start` spawns them.
    const KINDS: [&str; 9] = [
        "Repository",
        "ClusterRepository",
        "SnapshotPolicy",
        "Snapshot",
        "SnapshotSchedule",
        "Restore",
        "Maintenance",
        "RepositoryReplication",
        "SnapshotReplication",
    ];

    /// Readiness must not flip until every store has listed, and must flip once
    /// they have. This is the contract `/readyz` hangs off: a `Synced` here means
    /// an empty `Snapshot` list is a genuinely empty fleet rather than a cold
    /// cache.
    #[tokio::test]
    async fn readiness_flips_only_once_every_store_has_synced() {
        let mut writers = Writers::new();
        let stores = writers.stores();
        let (tx, mut rx) = watch::channel(CacheHealth::Syncing);
        let tasks = immortal_tasks();
        let task = tokio::spawn(supervise(stores, tasks, tx));

        assert_eq!(
            *rx.borrow(),
            CacheHealth::Syncing,
            "a cold cache must not report ready"
        );

        // Mark eight of nine synced; readiness must still be Syncing.
        writers.mark_all_ready_except_snapshots();
        tokio::task::yield_now().await;
        assert_eq!(
            *rx.borrow(),
            CacheHealth::Syncing,
            "one un-synced store must hold the whole cache un-ready",
        );

        writers.mark_snapshots_ready();
        rx.changed().await.expect("the supervisor must send");
        assert_eq!(
            *rx.borrow(),
            CacheHealth::Synced,
            "all nine synced means ready"
        );

        // And the supervisor is STILL running, holding the sender: that is what
        // lets it degrade later. A supervisor that returned here is the bug this
        // whole shape exists to prevent.
        assert!(
            !task.is_finished(),
            "the supervisor must outlive the first Synced so it can still say otherwise",
        );
        task.abort();
    }

    /// The degrade path: a reflector that dies AFTER the cache synced must flip
    /// health back, naming the kind that stopped.
    ///
    /// This is the case a long-lived pod actually hits — a watch gives up hours
    /// in — and the one where a `bool` that only went `false → true` left
    /// `/readyz` answering `ok` over a store frozen at whatever it last held. A
    /// frozen `Snapshot` store answers a shape indistinguishable from a healthy
    /// quiet cluster, which is why this must never be silent.
    #[tokio::test]
    async fn a_reflector_dying_after_the_sync_degrades_the_cache_and_names_the_kind() {
        let mut writers = Writers::new();
        let stores = writers.stores();
        let (tx, mut rx) = watch::channel(CacheHealth::Syncing);

        // Every kind immortal except `Snapshot`, which this test ends by hand.
        let tasks = immortal_tasks();
        let doomed = tasks
            .iter()
            .position(|t| t.kind == "Snapshot")
            .expect("Snapshot is one of the nine");
        let doomed_handle = tasks[doomed].handle.abort_handle();

        let supervisor = tokio::spawn(supervise(stores, tasks, tx));

        writers.mark_all_ready_except_snapshots();
        writers.mark_snapshots_ready();
        rx.changed()
            .await
            .expect("the supervisor must publish Synced");
        assert_eq!(*rx.borrow(), CacheHealth::Synced);

        // The watch gives up.
        doomed_handle.abort();

        rx.changed()
            .await
            .expect("a dead reflector must be published, not swallowed");
        assert_eq!(
            *rx.borrow(),
            CacheHealth::WatchEnded { kind: "Snapshot" },
            "readiness must degrade AND name the kind whose store is now frozen",
        );

        supervisor.await.expect("the supervisor must not panic");
    }

    /// A dropped writer must leave the cache un-ready rather than hang or falsely
    /// report ready: startup has to be able to tell "still listing" from "this
    /// cache will never work".
    ///
    /// It ends as `WatchEnded` rather than staying `Syncing`, because the writer
    /// is owned by the reflector task — a dropped writer means that task is
    /// already gone, and "will never sync" deserves the terminal answer, not the
    /// one that says to keep waiting.
    #[tokio::test]
    async fn a_dropped_writer_never_reports_ready() {
        let stores = empty_stores(); // every writer already dropped
        let (tx, rx) = watch::channel(CacheHealth::Syncing);

        // One already-finished task stands in for the reflector whose writer was
        // dropped, so the supervisor RETURNS rather than hanging.
        let tasks = vec![ReflectorTask {
            kind: "Snapshot",
            handle: tokio::spawn(std::future::ready(())),
        }];

        supervise(stores, tasks, tx).await;
        assert_eq!(
            *rx.borrow(),
            CacheHealth::WatchEnded { kind: "Snapshot" },
            "a cache that can never sync must never report ready",
        );
    }

    /// `first_reflector_to_end` must answer with the kind that actually ended,
    /// not merely that something did — the label is the half of the message an
    /// operator can act on.
    #[tokio::test]
    async fn the_supervisor_names_whichever_reflector_ends_first() {
        for expected in ["Repository", "Maintenance", "SnapshotReplication"] {
            let tasks = immortal_tasks();
            let doomed = tasks
                .iter()
                .position(|t| t.kind == expected)
                .expect("a spawned kind");
            tasks[doomed].handle.abort();

            assert_eq!(first_reflector_to_end(tasks).await, expected);
        }
    }

    /// The labels the supervisor reports must be the kinds' real names, since
    /// they reach an operator's screen through `/readyz` and the error log.
    #[test]
    fn every_spawned_task_is_labelled_with_its_real_kind() {
        fn check<K: KopiurKind>(expected: &str) {
            assert_eq!(K::KIND, expected);
            assert_eq!(K::KIND, <K as Resource>::kind(&()).as_ref());
        }
        check::<Repository>(KINDS[0]);
        check::<ClusterRepository>(KINDS[1]);
        check::<SnapshotPolicy>(KINDS[2]);
        check::<Snapshot>(KINDS[3]);
        check::<SnapshotSchedule>(KINDS[4]);
        check::<Restore>(KINDS[5]);
        check::<Maintenance>(KINDS[6]);
        check::<RepositoryReplication>(KINDS[7]);
        check::<SnapshotReplication>(KINDS[8]);
    }
}
