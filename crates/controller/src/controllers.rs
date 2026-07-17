//! The controller fan-out: [`spawn_all`] registers every reconciler's primary,
//! owned, and referent watches — scoped to the install scope — and the small
//! watch-plumbing helpers ([`scoped_api`], [`referent_meta`]).

use std::sync::Arc;

use futures::{Stream, StreamExt};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{
    ConfigMap, Namespace, PersistentVolumeClaim, Secret, Service, ServiceAccount,
};
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher::Config as WatcherConfig;
use kube::runtime::{Controller, WatchStreamExt, reflector, watcher};
use kube::{Api, Client, ResourceExt};

use kopiur_api::common::RepositoryKind;
use kopiur_api::{
    ClusterRepository, Maintenance, Repository, RepositoryReplication, Restore, Snapshot,
    SnapshotPolicy, SnapshotSchedule,
};

use crate::config;
use crate::context::Context;
use crate::metrics::ResourceStores;
use crate::{
    cluster_repository, consts, maintenance, repository, repository_replication, restore, snapshot,
    snapshot_policy, snapshot_schedule, watch,
};

/// A namespaced-kind `Api` matching the install scope: cluster-wide for
/// `installScope: cluster`, single-namespace for `installScope: namespaced`
/// (where Role-only RBAC makes a cluster-wide LIST a 403 — the bug that used
/// to leave namespaced installs silently inert).
pub(crate) fn scoped_api<K>(client: &Client, scope: &config::WatchScope) -> Api<K>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>,
    K::DynamicType: Default,
{
    match scope {
        config::WatchScope::Cluster => Api::all(client.clone()),
        config::WatchScope::Namespaced(ns) => Api::namespaced(client.clone(), ns),
    }
}

/// A metadata-only trigger stream for an external referent `K`
/// (`Secret`/`ConfigMap`/`ServiceAccount`/`Namespace`). The referent mappers in
/// [`crate::watch`] need only the changed object's name/namespace — never its
/// `.data`/`.spec`/`.status` — so watching `Api<PartialObjectMeta<K>>` (which
/// kube serves via metadata-only requests) keeps those payloads off the wire
/// entirely (no `Secret` plaintext ever reaches the controller, a memory AND
/// security win) and we additionally drop `managedFields` + `annotations` (the
/// largest remaining `ObjectMeta` bytes, unused by every mapper) before the events
/// fan in. Fed to [`Controller::watches_stream`] in place of a full-object
/// `.watches(..)`; `touched_objects` is the decode `watches_stream` expects.
///
/// Takes the `Api` (rather than building `Api::all` itself) so the caller
/// scopes it to the install scope — a namespaced install must not register a
/// cluster-wide (Role-RBAC-forbidden) referent watch.
fn referent_meta<K>(
    api: Api<PartialObjectMeta<K>>,
    cfg: &WatcherConfig,
) -> impl Stream<Item = Result<PartialObjectMeta<K>, watcher::Error>> + Send + use<K>
where
    K: kube::Resource<DynamicType = ()>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug
        + Send
        + 'static,
{
    watcher(api, cfg.clone())
        .modify(|m| {
            m.metadata.managed_fields = None;
            m.metadata.annotations = None;
        })
        .touched_objects()
}

/// Map a kopia web-UI server child (Deployment/Service) to its owning
/// `ClusterRepository` via the back-reference label. Cluster-scoped owners can't be
/// ownerReferences of a namespaced child, so `.owns()` won't fire — this label
/// mapping replaces it.
fn map_to_cluster_repository<K: kube::Resource>(obj: K) -> Option<ObjectRef<ClusterRepository>> {
    obj.labels()
        .get(consts::CLUSTER_REPOSITORY_LABEL)
        .map(|name| ObjectRef::new(name))
}

/// Publish a controller's own reflector `Store` handle into the shared
/// `Context` cell, and spawn a task that flips the paired `synced` flag once
/// the reflector reports its first sync — the `OnceLock` analog of the shared
/// Maintenance informer's `maintenance_synced` precedent (`startup::run`).
/// Unlike that standalone reflector, THIS store's underlying watch is driven
/// by the owning `Controller`'s own `run()` future (joined at the bottom of
/// `spawn_all`), so this only *observes* readiness — it never drives the
/// stream itself. Consumers (M4/M5) MUST treat an unset cell, or one whose
/// `synced` flag hasn't flipped, as fail-safe: requeue, never fire destructive
/// work off a possibly-cold cache.
fn publish_store<K>(
    cell: &Arc<std::sync::OnceLock<kube::runtime::reflector::Store<K>>>,
    store: kube::runtime::reflector::Store<K>,
) where
    K: kube::runtime::reflector::Lookup + Clone + Send + Sync + 'static,
    K::DynamicType: Eq + std::hash::Hash + Clone + Send + Sync,
{
    // Only PUBLISH the handle so cross-controller reads (the mass-deletion breaker)
    // can reach it — readiness is tracked SEPARATELY. We deliberately do NOT spawn a
    // `store.wait_until_ready()` here: that is a second getter on the very
    // `DelayedInit` the Controller's own applier already awaits
    // (`delay_tasks_until(store.wait_until_ready())`), and kube-rs's `DelayedInit`
    // multiplexes a single-waker oneshot behind a Mutex — the two getters race, the
    // loser's waker is overwritten and it hangs FOREVER. When our getter lost, the
    // `*_synced` flag never flipped and every external destructive deletion deferred
    // (`StoresNotSynced`) indefinitely. Each controller now flips its own
    // `*_synced` from its reconcile (`Context::mark_*_synced`) — a running reconcile
    // is proof the reflector synced, with no getter contention.
    // Best-effort: a second call (there is only ever one per kind) keeps the first
    // handle.
    let _ = cell.set(store);
}

/// Spawn all eight controllers and join them. Split out so it can be driven
/// independently of the metrics server. The shared Maintenance informer that the
/// repo reconcilers read is set up separately in [`run`].
pub(crate) async fn spawn_all(
    client: Client,
    ctx: Arc<Context>,
    streaming_lists: bool,
    scope: config::WatchScope,
) {
    // Cluster-scoped kinds (ClusterRepository primaries + referents, Namespace
    // referents) are registered ONLY in cluster scope: a namespaced install
    // has Role-only RBAC, where a cluster-wide LIST/WATCH 403s forever — the
    // failure mode that used to leave namespaced installs Ready-but-inert.
    let cluster_wide = matches!(scope, config::WatchScope::Cluster);
    let mut cfg = WatcherConfig::default();
    // Opt-in (off by default for older-apiserver safety): stream the initial list
    // via the WatchList API to cut peak memory on the cluster-wide resync.
    if streaming_lists {
        cfg = cfg.streaming_lists();
    }
    // Owned children (mover Jobs, work-spec ConfigMaps) ALWAYS carry the managed-by
    // label (io::finalizer::child_labels), so scope their watches server-side to
    // ours — the controller then lists/watches only kopiur's Jobs/ConfigMaps, not
    // every one in the cluster. Owner-ref mapping is unaffected by the label filter.
    let owned_cfg = cfg.clone().labels(&format!(
        "{}={}",
        crate::consts::MANAGED_BY_LABEL,
        crate::consts::MANAGED_BY_VALUE
    ));
    // Trailing-edge debounce on every controller: coalesce rapid re-triggers of
    // the same object (own status writes, owned-Job event bursts, referent
    // fan-out) into one reconcile. Belt-and-braces against write-triggered
    // SELF-loops — with order-stable conditions and guarded status writes the
    // steady state emits no events at all, but if a future write churns, the
    // event stream pauses while no reconcile runs, the quiet window always
    // arrives, and the loop is capped at ~1/window per object instead of
    // reconcile speed (~30/s, a pegged core).
    //
    // SIZING — this is a HEAL-LATENCY floor, not just a loop cap. Terminal state
    // is written in two passes: the mover stamps the terminal `phase`, then the
    // controller's follow-up reconcile heals the derived fields (kstatus
    // `Ready=True`, post-hook `hooks.postCompletedAt`, the `kopiur_resource_phase`
    // gauge). The debounce delays that second pass by its full duration, so a
    // consumer that gates on `phase` sees the derived fields lag by ~one window.
    // At 1s this was visible: e2e assertions reading conditions immediately after
    // `phase: Completed`/`Succeeded` raced the heal and read stale values
    // (restore_completed_reports_kstatus_ready, http_request_post_hook…,
    // metrics_reflect_backup_lifecycle). 250ms still coalesces owned-Job event
    // bursts and caps any residual self-loop at ~4/s (nowhere near a hot core),
    // while keeping the heal lag imperceptible to `kubectl wait --for=condition`
    // and Flux/Argo health gates.
    //
    // CAVEAT (trailing edge = deadline RESETS on every event): a sustained
    // EXTERNAL writer churning one object faster than the window would defer its
    // reconcile until the stream quiets, not rate-limit it — no such writer
    // exists today (the controller is the sole steady-state status writer; mover
    // stamps are one-shot), so if a reconciler ever looks starved, look for a new
    // sub-window event source on its primary.
    let ctrl_cfg = kube::runtime::controller::Config::default()
        .debounce(std::time::Duration::from_millis(250));

    // Snapshot owns its mover Job + ConfigMap (reaped via owner-ref GC, §4.10), and
    // watches its `SnapshotPolicy` recipe so a policy edit (or a policy whose
    // repository just became Ready) re-runs the snapshot promptly instead of waiting
    // out its requeue.
    let snapshot_api: Api<Snapshot> = scoped_api(&client, &scope);
    let snapshot_ctx = ctx.clone();
    let snapshot_ctrl = Controller::new(snapshot_api, cfg.clone()).with_config(ctrl_cfg.clone());
    let snapshot_store = snapshot_ctrl.store();
    publish_store(&ctx.snapshot_store, snapshot_store.clone());
    let mut snapshot_ctrl = snapshot_ctrl
        .owns_with(scoped_api::<Job>(&client, &scope), (), owned_cfg.clone())
        .owns_with(
            scoped_api::<ConfigMap>(&client, &scope),
            (),
            owned_cfg.clone(),
        )
        .watches(
            scoped_api::<SnapshotPolicy>(&client, &scope),
            cfg.clone(),
            {
                let store = snapshot_store.clone();
                move |p: SnapshotPolicy| watch::policy_to_snapshots(&store, &p)
            },
        );
    // A Snapshot refused for an elevated mover waits on the namespace's
    // privileged-movers opt-in annotation — deliver that grant the moment it
    // lands instead of leaving the CR to its slow backstop requeue. Namespace
    // is cluster-scoped, so only in cluster scope; in a namespaced install the
    // opt-in check fails open anyway (io::namespace_allows_privileged_movers).
    if cluster_wide {
        snapshot_ctrl = snapshot_ctrl.watches_stream(
            referent_meta::<Namespace>(Api::all(client.clone()), &cfg),
            {
                let store = snapshot_store.clone();
                move |n: PartialObjectMeta<Namespace>| watch::namespace_to_snapshots(&store, &n)
            },
        );
    }
    // Mass-deletion ack drain: when a Repository's `allow-mass-deletion` annotation
    // (or `deletionProtection`) changes, re-reconcile the DELETING Snapshots that
    // resolve to it so a held deletion proceeds at once, instead of waiting out its
    // long `Held` requeue. ClusterRepository is cluster-scoped, so its watch is
    // registered only in cluster scope (like the other kind-conditional watches).
    let mut snapshot_ctrl =
        snapshot_ctrl.watches(scoped_api::<Repository>(&client, &scope), cfg.clone(), {
            let store = snapshot_store.clone();
            move |r: Repository| watch::repository_to_deleting_snapshots(&store, &r)
        });
    if cluster_wide {
        snapshot_ctrl = snapshot_ctrl.watches(
            Api::<ClusterRepository>::all(client.clone()),
            cfg.clone(),
            {
                let store = snapshot_store.clone();
                move |r: ClusterRepository| {
                    watch::cluster_repository_to_deleting_snapshots(&store, &r)
                }
            },
        );
    }
    let snapshot_ctrl = snapshot_ctrl
        .run(snapshot::reconcile, snapshot::error_policy, snapshot_ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::debug!(error = %e, "snapshot reconcile error");
            }
        });

    // SnapshotSchedule owns the Snapshot CRs it creates, and watches SnapshotPolicy
    // (by `policyRef` or `policySelector`) so a new/relabeled/edited policy is picked
    // up promptly rather than only on the schedule's periodic re-list.
    let sched_api: Api<SnapshotSchedule> = scoped_api(&client, &scope);
    let sched_ctx = ctx.clone();
    let sched_ctrl = Controller::new(sched_api, cfg.clone()).with_config(ctrl_cfg.clone());
    let sched_store = sched_ctrl.store();
    publish_store(&ctx.schedule_store, sched_store.clone());
    let mut sched_ctrl = sched_ctrl
        .owns(scoped_api::<Snapshot>(&client, &scope), cfg.clone())
        .watches(
            scoped_api::<SnapshotPolicy>(&client, &scope),
            cfg.clone(),
            {
                let store = sched_store.clone();
                move |p: SnapshotPolicy| watch::policy_to_schedules(&store, &p)
            },
        )
        // A schedule with no `spec.schedule.timezone` inherits its cron timezone
        // from its target policies' repository `scheduleDefaults.timezone`. Watch
        // Repository/ClusterRepository so an edit to that default re-triggers the
        // affected schedules promptly (recomputing any stale pinned slot) instead of
        // waiting out the requeue — the schedule reconciler never reads its
        // generation, so nothing else would notice the change.
        .watches(scoped_api::<Repository>(&client, &scope), cfg.clone(), {
            let store = sched_store.clone();
            move |r: Repository| watch::repository_to_schedules(&store, &r)
        });
    if cluster_wide {
        sched_ctrl = sched_ctrl.watches(
            Api::<ClusterRepository>::all(client.clone()),
            cfg.clone(),
            {
                let store = sched_store.clone();
                move |r: ClusterRepository| watch::cluster_repository_to_schedules(&store, &r)
            },
        );
    }
    let sched_ctrl = sched_ctrl
        .run(
            snapshot_schedule::reconcile,
            snapshot_schedule::error_policy,
            sched_ctx,
        )
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::debug!(error = %e, "schedule reconcile error");
            }
        });

    // Repository/ClusterRepository additionally watch Maintenance: a Maintenance
    // create/delete maps to the repo it references and triggers an immediate
    // re-reconcile, so the MaintenanceConfigured condition/warning clears within
    // seconds instead of waiting for the 300s requeue. The mappers are exhaustive
    // over RepositoryKind (a Repository ref never triggers a ClusterRepository
    // reconcile and vice versa).
    // Repository/ClusterRepository additionally watch their credential `Secret`(s)
    // and TLS-CA `ConfigMap`: a content edit to a referenced Secret/ConfigMap does
    // NOT bump the repo's `generation`, so without these watches a fixed password
    // never re-triggers a connect (and the terminal-failure gate, keyed on the
    // Secret's `resourceVersion`, would only reopen on the 30-min heartbeat).
    let repo_api: Api<Repository> = scoped_api(&client, &scope);
    let repo_ctx = ctx.clone();
    let repo_ctrl = Controller::new(repo_api, cfg.clone()).with_config(ctrl_cfg.clone());
    let repo_store = repo_ctrl.store();
    let repo_ctrl = repo_ctrl
        // Own the bootstrap Job (carries a controller ownerRef already): its
        // terminal state arrives as an EVENT instead of hoping a 15s poll lands
        // inside the Job's ttlSecondsAfterFinished window — a short TTL used to
        // reap the finished Job between polls, so the result was never read and
        // the repo churned Initializing + fresh bootstrap Jobs forever.
        .owns_with(scoped_api::<Job>(&client, &scope), (), owned_cfg.clone())
        // Own the optional kopia web-UI server children (`spec.server`): the
        // Repository owner-refs them in its own namespace, so an edit/delete of the
        // Deployment/Service/ConfigMap self-heals via owner-ref mapping. Scoped to
        // the managed-by label (the server builder stamps it) so we don't watch
        // every Deployment/Service in the cluster.
        .owns_with(
            scoped_api::<Deployment>(&client, &scope),
            (),
            owned_cfg.clone(),
        )
        .owns_with(
            scoped_api::<Service>(&client, &scope),
            (),
            owned_cfg.clone(),
        )
        .owns_with(
            scoped_api::<ConfigMap>(&client, &scope),
            (),
            owned_cfg.clone(),
        )
        .watches_stream(
            referent_meta::<Secret>(scoped_api(&client, &scope), &cfg),
            {
                let store = repo_store.clone();
                move |s: PartialObjectMeta<Secret>| watch::secret_to_repositories(&store, &s)
            },
        )
        .watches_stream(
            referent_meta::<ConfigMap>(scoped_api(&client, &scope), &cfg),
            {
                let store = repo_store.clone();
                move |cm: PartialObjectMeta<ConfigMap>| {
                    watch::configmap_to_repositories(&store, &cm)
                }
            },
        )
        // Workload identity: creating the `auth.workloadIdentity` ServiceAccount
        // un-sticks a repository blocked on the SA preflight immediately.
        .watches_stream(
            referent_meta::<ServiceAccount>(scoped_api(&client, &scope), &cfg),
            {
                let store = repo_store.clone();
                move |sa: PartialObjectMeta<ServiceAccount>| {
                    watch::serviceaccount_to_repositories(&store, &sa)
                }
            },
        )
        .watches(
            scoped_api::<Maintenance>(&client, &scope),
            cfg.clone(),
            |m: Maintenance| {
                let r = &m.spec.repository;
                match r.kind {
                    RepositoryKind::Repository => {
                        let ns = r.namespace.clone().or_else(|| m.namespace());
                        ns.map(|ns| ObjectRef::<Repository>::new(&r.name).within(&ns))
                    }
                    RepositoryKind::ClusterRepository => None,
                }
            },
        )
        .run(repository::reconcile, repository::error_policy, repo_ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::debug!(error = %e, "repository reconcile error");
            }
        });

    // ClusterRepository is a cluster-scoped kind: its controller (and every
    // referent watch that feeds it) exists ONLY in cluster scope. A namespaced
    // install's Role RBAC cannot even read the kind — the docs have always
    // said it is not reconciled there; now the watch registration matches.
    let crepo = if cluster_wide {
        let crepo_api: Api<ClusterRepository> = Api::all(client.clone());
        let crepo_ctx = ctx.clone();
        let crepo_ctrl = Controller::new(crepo_api, cfg.clone()).with_config(ctrl_cfg.clone());
        let crepo_store = crepo_ctrl.store();
        let crepo_ctrl = crepo_ctrl
            // Own the bootstrap Job — same prompt-terminal-observation rationale as
            // the Repository controller above.
            .owns_with(Api::<Job>::all(client.clone()), (), owned_cfg.clone())
            // The kopia web-UI server children (`spec.server`) are namespaced and cannot
            // carry an ownerReference to a cluster-scoped owner, so `.owns()` would never
            // fire. Map child events back to the parent via the back-reference label
            // instead (scoped to the managed-by label, same as everything else).
            .watches(
                Api::<Deployment>::all(client.clone()),
                owned_cfg.clone(),
                map_to_cluster_repository,
            )
            .watches(
                Api::<Service>::all(client.clone()),
                owned_cfg.clone(),
                map_to_cluster_repository,
            )
            .watches_stream(referent_meta::<Secret>(Api::all(client.clone()), &cfg), {
                let store = crepo_store.clone();
                move |s: PartialObjectMeta<Secret>| {
                    watch::secret_to_cluster_repositories(&store, &s)
                }
            })
            .watches_stream(
                referent_meta::<ConfigMap>(Api::all(client.clone()), &cfg),
                {
                    let store = crepo_store.clone();
                    move |cm: PartialObjectMeta<ConfigMap>| {
                        watch::configmap_to_cluster_repositories(&store, &cm)
                    }
                },
            )
            // Workload identity: same SA-preflight un-stick as the Repository
            // controller (name-only match; movers run in many namespaces).
            .watches_stream(
                referent_meta::<ServiceAccount>(Api::all(client.clone()), &cfg),
                {
                    let store = crepo_store.clone();
                    move |sa: PartialObjectMeta<ServiceAccount>| {
                        watch::serviceaccount_to_cluster_repositories(&store, &sa)
                    }
                },
            )
            .watches(
                Api::<Maintenance>::all(client.clone()),
                cfg.clone(),
                |m: Maintenance| {
                    let r = &m.spec.repository;
                    match r.kind {
                        RepositoryKind::ClusterRepository => {
                            Some(ObjectRef::<ClusterRepository>::new(&r.name))
                        }
                        RepositoryKind::Repository => None,
                    }
                },
            )
            .run(
                cluster_repository::reconcile,
                cluster_repository::error_policy,
                crepo_ctx,
            )
            .for_each(|res| async move {
                if let Err(e) = res {
                    tracing::debug!(error = %e, "cluster_repository reconcile error");
                }
            });
        Some((crepo_store, crepo_ctrl))
    } else {
        None
    };

    // SnapshotPolicy owns the verification mover Jobs + ConfigMaps it spawns
    // (ADR-0005 §4), so they're GC'd with the policy and a Job event re-triggers the
    // reconcile (the verify scheduler).
    let config_api: Api<SnapshotPolicy> = scoped_api(&client, &scope);
    let config_ctx = ctx.clone();
    let config_ctrl = Controller::new(config_api, cfg.clone()).with_config(ctrl_cfg.clone());
    let config_store = config_ctrl.store();
    let mut config_ctrl = config_ctrl
        .owns_with(scoped_api::<Job>(&client, &scope), (), owned_cfg.clone())
        .owns_with(
            scoped_api::<ConfigMap>(&client, &scope),
            (),
            owned_cfg.clone(),
        )
        // A produced `Snapshot` carries its owning policy in the config label. Watch
        // Snapshots so GFS retention (ADR §4.4) reconciles PROMPTLY when one is created
        // or deleted — without this the policy only re-runs on its periodic requeue, so
        // a new snapshot's prune (and a pinned snapshot's exemption) lags by minutes.
        .watches(
            scoped_api::<Snapshot>(&client, &scope),
            cfg.clone(),
            |s: Snapshot| match (s.labels().get(crate::consts::CONFIG_LABEL), s.namespace()) {
                (Some(policy), Some(ns)) => {
                    Some(ObjectRef::<SnapshotPolicy>::new(policy).within(&ns))
                }
                _ => None,
            },
        )
        // Watch the backing repository: when it becomes Ready (e.g. a credential was
        // fixed) the policy re-reconciles at once instead of waiting out its requeue.
        .watches(scoped_api::<Repository>(&client, &scope), cfg.clone(), {
            let store = config_store.clone();
            move |r: Repository| watch::repository_to_policies(&store, &r)
        });
    if cluster_wide {
        config_ctrl = config_ctrl.watches(
            Api::<ClusterRepository>::all(client.clone()),
            cfg.clone(),
            {
                let store = config_store.clone();
                move |r: ClusterRepository| watch::cluster_repository_to_policies(&store, &r)
            },
        );
    }
    let config_ctrl = config_ctrl
        .run(
            snapshot_policy::reconcile,
            snapshot_policy::error_policy,
            config_ctx,
        )
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::debug!(error = %e, "snapshot_policy reconcile error");
            }
        });

    // Restore watches its repository so a restore blocked on a not-yet-Ready repo
    // proceeds the moment the repo connects, rather than on the restore's requeue.
    let restore_api: Api<Restore> = scoped_api(&client, &scope);
    let restore_ctx = ctx.clone();
    let restore_ctrl = Controller::new(restore_api, cfg.clone()).with_config(ctrl_cfg.clone());
    let restore_store = restore_ctrl.store();
    let mut restore_ctrl = restore_ctrl
        .watches(scoped_api::<Repository>(&client, &scope), cfg.clone(), {
            let store = restore_store.clone();
            move |r: Repository| watch::repository_to_restores(&store, &r)
        })
        // Volume-populator handshake (ADR-0005 §9): a PVC claiming a populator Restore
        // re-enqueues it so the prime-PVC/rebind dance advances on the watch.
        .watches(
            scoped_api::<PersistentVolumeClaim>(&client, &scope),
            cfg.clone(),
            {
                let store = restore_store.clone();
                move |pvc: PersistentVolumeClaim| watch::pvc_to_restores(&store, &pvc)
            },
        );
    if cluster_wide {
        restore_ctrl = restore_ctrl
            .watches(
                Api::<ClusterRepository>::all(client.clone()),
                cfg.clone(),
                {
                    let store = restore_store.clone();
                    move |r: ClusterRepository| watch::cluster_repository_to_restores(&store, &r)
                },
            )
            // Same privileged-mover opt-in delivery as the Snapshot controller
            // (Namespace is cluster-scoped; see the note there).
            .watches_stream(
                referent_meta::<Namespace>(Api::all(client.clone()), &cfg),
                {
                    let store = restore_store.clone();
                    move |n: PartialObjectMeta<Namespace>| watch::namespace_to_restores(&store, &n)
                },
            );
    }
    let restore_ctrl = restore_ctrl
        .run(restore::reconcile, restore::error_policy, restore_ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::debug!(error = %e, "restore reconcile error");
            }
        });

    // Maintenance watches its repository (same Ready-gate prompting as the others).
    let maint_api: Api<Maintenance> = scoped_api(&client, &scope);
    let maint_ctx = ctx.clone();
    let maint_ctrl = Controller::new(maint_api, cfg.clone()).with_config(ctrl_cfg.clone());
    let maint_store = maint_ctrl.store();
    let mut maint_ctrl = maint_ctrl
        // Own the per-slot maintenance Jobs (controller ownerRef already set):
        // a yield/run's terminal state must be OBSERVED (it records
        // `lastHandledAt`) — with only the 30s poll, a short Job TTL could reap
        // the finished Job between polls and the slot would re-fire unrecorded.
        .owns_with(scoped_api::<Job>(&client, &scope), (), owned_cfg.clone())
        .watches(scoped_api::<Repository>(&client, &scope), cfg.clone(), {
            let store = maint_store.clone();
            move |r: Repository| watch::repository_to_maintenances(&store, &r)
        });
    if cluster_wide {
        maint_ctrl = maint_ctrl.watches(
            Api::<ClusterRepository>::all(client.clone()),
            cfg.clone(),
            {
                let store = maint_store.clone();
                move |r: ClusterRepository| watch::cluster_repository_to_maintenances(&store, &r)
            },
        );
    }
    let maint_ctrl = maint_ctrl
        .run(maintenance::reconcile, maintenance::error_policy, maint_ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::debug!(error = %e, "maintenance reconcile error");
            }
        });

    // RepositoryReplication owns its per-slot mover Jobs + ConfigMaps (ADR-0005 §13(d)),
    // watches its *source* repository (Ready-gate prompting), and its *destination*
    // credential Secret (a dest password/auth fix re-triggers the mirror promptly).
    let repl_api: Api<RepositoryReplication> = scoped_api(&client, &scope);
    let repl_ctx = ctx.clone();
    let repl_ctrl = Controller::new(repl_api, cfg.clone()).with_config(ctrl_cfg.clone());
    let repl_store = repl_ctrl.store();
    let mut repl_ctrl = repl_ctrl
        .owns_with(scoped_api::<Job>(&client, &scope), (), owned_cfg.clone())
        .owns_with(
            scoped_api::<ConfigMap>(&client, &scope),
            (),
            owned_cfg.clone(),
        )
        .watches_stream(
            referent_meta::<Secret>(scoped_api(&client, &scope), &cfg),
            {
                let store = repl_store.clone();
                move |s: PartialObjectMeta<Secret>| watch::secret_to_replications(&store, &s)
            },
        )
        .watches(scoped_api::<Repository>(&client, &scope), cfg.clone(), {
            let store = repl_store.clone();
            move |r: Repository| watch::repository_to_replications(&store, &r)
        });
    if cluster_wide {
        repl_ctrl = repl_ctrl.watches(
            Api::<ClusterRepository>::all(client.clone()),
            cfg.clone(),
            {
                let store = repl_store.clone();
                move |r: ClusterRepository| watch::cluster_repository_to_replications(&store, &r)
            },
        );
    }
    let repl_ctrl = repl_ctrl
        .run(
            repository_replication::reconcile,
            repository_replication::error_policy,
            repl_ctx,
        )
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::debug!(error = %e, "repository_replication reconcile error");
            }
        });

    // Register the store-backed observable gauges (kopiur_resource_phase, the
    // per-Snapshot stat series, and the per-policy "latest" family). Their callbacks
    // read these reflector caches at scrape time, so each series exists only while
    // its CR does. The reader handles are `Clone`d out of the controllers' own
    // stores; the watch closures above already hold their own clones. In
    // namespaced scope there is no ClusterRepository controller, so its gauge
    // reads a writer-less (permanently empty) store — no series, never panics.
    let crepo_metrics_store = crepo
        .as_ref()
        .map(|(store, _)| store.clone())
        .unwrap_or_else(|| reflector::store::<ClusterRepository>().0);
    ctx.metrics.register_resource_observers(ResourceStores {
        snapshots: snapshot_store.clone(),
        repositories: repo_store.clone(),
        cluster_repositories: crepo_metrics_store,
        restores: restore_store.clone(),
    });

    let crepo_ctrl = async move {
        if let Some((_, fut)) = crepo {
            fut.await;
        }
    };

    tokio::join!(
        snapshot_ctrl,
        sched_ctrl,
        repo_ctrl,
        crepo_ctrl,
        config_ctrl,
        restore_ctrl,
        maint_ctrl,
        repl_ctrl,
    );
}
