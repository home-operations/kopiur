# Watches & reconcile triggers

How a change to *anything a CR depends on* turns into a reconcile of that CR —
and how a terminally-failed object un-sticks. The implementation lives in
`crates/controller/src/watch.rs` (mappers) and `crates/controller/src/lib.rs::run`
(wiring); the terminal gate is `crates/controller/src/io/apply.rs::terminal_gate_holds`.

## Why referent watches exist

A kube `Controller` re-reconciles its primary kind and the children it owns —
nothing else. But most Kopiur reconcilers *read* objects they don't own: a
credential `Secret`, a TLS-CA `ConfigMap`, a `Repository`/`ClusterRepository`, a
`SnapshotPolicy`. Editing a Secret's *content* does not even bump any referrer's
`metadata.generation`, so without extra wiring a fixed password would sit unnoticed
until a multi-minute requeue. The referent watches close that gap: a referent
change re-triggers its referrers within seconds.

## Mapper architecture

Each `Controller::watches(Api<Referent>, …, mapper)` call installs a mapper that
answers "which referrers care about this referent event?". Two rules:

1. **Scan a shared reflector `Store`, never `Api::list`.** Every mapper reads the
   referrer controller's own store (shared via `Controller::store()`) and does one
   in-memory pass (`select()` in `watch.rs`). A per-event API list would hammer the
   API server on every Secret change in the cluster; the store scan is free, and a
   non-matching event (most Secrets) maps to the empty set.
2. **Matching is pure and unit-tested.** The predicates
   (`repo_references_secret`, `ref_matches_repository`, …) reproduce the exact
   namespace-defaulting the reconcilers use, so the watch can't disagree with the
   read path.

## Referent → referrer matrix

| When this changes… | …these re-reconcile (mapper) |
| --- | --- |
| `Secret` | `Repository` (`secret_to_repositories`), `ClusterRepository` (`secret_to_cluster_repositories`), `RepositoryReplication` destination creds (`secret_to_replications`) |
| `ConfigMap` (TLS CA bundle) | `Repository` (`configmap_to_repositories`), `ClusterRepository` (`configmap_to_cluster_repositories`) |
| `Repository` / `ClusterRepository` | `SnapshotPolicy` (`*_to_policies`), `Restore` (`*_to_restores`), `Maintenance` (`*_to_maintenances`), `RepositoryReplication` source (`*_to_replications`) |
| `SnapshotPolicy` | `Snapshot` (`policy_to_snapshots`), `SnapshotSchedule` (`policy_to_schedules`) |

A source-repository Secret fix thus propagates transitively: Secret →
`Repository` reconciles to `Ready` → the repository watch re-triggers its
policies/restores/maintenances/replications.

(Owned children — mover `Job`s, projected Secrets, the `Maintenance` a repository
projects — are covered by the ordinary `owns()` relation, not these mappers.)

## The terminal-failure gate (and why it keys on two things)

A non-retryable backend failure (wrong password, permission denied) parks the
object `Failed` so the operator stops hammering the backend — re-driven only by a
~30-minute heartbeat. The gate that keeps it parked, `terminal_gate_holds`, holds
only while **both**:

1. `status.observedGeneration == metadata.generation` (the spec is unchanged), and
2. the credential Secret's live `resourceVersion` still equals the
   `status.resolvedCredentialVersion` recorded at the failed attempt.

Keying on generation alone is the historical bug this design fixes: a Secret
content edit bumps neither generation nor any spec field, so a *fixed* password
would never reopen the gate. With (2), the Secret watch fires, the gate sees a new
`resourceVersion`, and the repository retries immediately. The e2e guard is
`fixed_credential_secret_unsticks_failed_repository` (`crates/e2e/tests/repository_lifecycle.rs`).

## Status-write discipline (don't self-trigger)

Watches make status churn dangerous: a reconcile that writes volatile content to
its own status (a fresh `now()`, a kopia temp filename) re-triggers itself in a
hot loop. House rules, enforced across reconcilers:

- Timestamps (`pinnedAt`, `lastTransitionTime`) are written **once per
  transition**, guarded by a presence/equality check — never re-stamped on a
  no-op reconcile.
- Phase writes are guarded by phase-equality checks.
- Condition updates go through `io::upsert_condition`, which preserves
  `lastTransitionTime` when the status is unchanged, so an identical patch is a
  server-side no-op.

## Field mutability stance

Spec fields are mutable unless immutability protects data (e.g. a pinned
resolution, a discovered snapshot's forced `Retain`). The guiding rule: **an edit
to a spec or credential must always be able to un-stick a `Failed` object** —
defensive immutability that forces delete-and-recreate is worse than the bug it
prevents. Anything resolved-then-pinned lives in `status.resolved.*`, not in spec.

## Two-pass terminal heal (phase precedes derived status)

For the mover-driven kinds (`Snapshot`, `Restore`, `RepositoryReplication`), the
**mover** stamps the terminal `status.phase` (`Succeeded`/`Completed`) — and only
that, plus its own outputs (`kopiaSnapshotID`, `lastReplicated`, `logTail`). The
**controller** then heals the *derived* status in a FOLLOW-UP reconcile: the
kstatus trio (`Ready`/`Reconciling`/`Stalled`) and `status.hooks.*`. The terminal
gate's self-check keys on the distinctive healed condition, not the phase,
precisely because the phase lands first (`restore.rs::kstatus_settled_for`). The
same split applies to controller-driven kinds whose Ready heal is a separate
write from their primary bookkeeping (`Maintenance::set_ready_if_changed`).

`kopiur_resource_phase` is not part of this heal: it is a store-backed
*observable* gauge (`Metrics::register_resource_observers`) whose callback reads
`status.phase` straight from each controller's reflector `Store` at collection
time, so it reflects whatever `status.phase` the mover already stamped as soon as
the reflector's watch delivers that update — it does not wait for the
follow-up-reconcile heal, and it needs no explicit write/reset on transition or
deletion (the series simply stops being observed once the CR is gone).

Consequence — the heal lags the phase by up to one **debounce window**
(`spawn_all`'s `ctrl_cfg`, currently 250 ms). That window is a deliberate
heal-latency floor: small enough to be imperceptible to `kubectl wait
--for=condition=Ready` and Flux/Argo health gates, large enough to coalesce
owned-Job event bursts. It was 1 s and was shrunk after it widened this gap enough
to break e2e assertions — see the commit history and `crates/controller/src/lib.rs`.

**Rule for tests and external consumers:** never gate on `status.phase` and then
read a *healed* field in the same breath — that races the heal. Gate on the healed
field itself. In e2e use `common::wait_ready` (or `wait_condition(.., "Ready",
"True")`) before asserting any kstatus condition or `hooks.*` (the
`kopiur_resource_phase` gauge is the exception — it observes `status.phase`
directly, not a healed field, so it needs no such wait). The guards live in
`restore.rs::restore_completed_reports_kstatus_ready`,
`hooks.rs::http_request_post_hook_hits_in_cluster_receiver`,
`lifecycle.rs::metrics_reflect_backup_lifecycle` /
`maintenance_claims_lease`, and
`replication.rs::repository_replication_mirrors_to_second_filesystem_repo`.

## Memory footprint

The controller is cluster-scoped and watches several core/v1 kinds across every
namespace, so its RSS is dominated by what it lists/watches and how the allocator
and runtime are sized. The levers, all in `spawn_all`/`main.rs`/`config.rs`:

- **Scoped owned watches.** Owned children (mover `Job`s, bootstrap-result `ConfigMap`s)
  ALWAYS carry `app.kubernetes.io/managed-by=kopiur` (`io::finalizer::child_labels`),
  so they are watched via `owns_with` + a server-side label selector (`owned_cfg`):
  the controller lists/watches only *kopiur's* Jobs/ConfigMaps, not every one in the
  cluster. Owner-ref mapping is unaffected by the filter.
- **Metadata-only referent watches.** The credential `Secret`, TLS-CA `ConfigMap`,
  workload-identity `ServiceAccount`, and privileged-opt-in `Namespace` watches feed
  `Controller::watches_stream` a `watcher` over `Api<PartialObjectMeta<K>>`
  (`referent_meta`) — kube serves such an `Api` via metadata-only requests — not a
  full-object `watcher`. Every referent mapper needs only the changed object's
  name/namespace (the spec/data it scans lives in the referrer `Store`), so
  `.data`/`.spec`/`.status` never cross the wire — no `Secret` plaintext is ever
  pulled into the controller (a memory **and** security win). We additionally drop
  `managedFields` + `annotations` (the largest remaining `ObjectMeta` bytes, unused by
  every mapper). Per the
  [kube.rs optimization guide](https://kube.rs/controllers/optimization/), this is the
  highest-leverage watch change (metadata-only watching alone ≈ 60% for Pods; field
  pruning adds ≈ 30%). Requires the kube `unstable-runtime` feature (`watches_stream`
  is still unstable in kube 4.0).
- **Worker-thread cap.** `main.rs` builds the tokio runtime with
  `config::worker_threads()` (`KOPIUR_WORKER_THREADS`, default 2) instead of
  `Runtime::new()`'s default — `available_parallelism()` sizes to the *host* core count,
  ignoring the cgroup CPU quota, so on a large node it spawns a worker thread (stack +
  malloc arena) per core for an I/O-bound process.
- **mimalloc.** `#[global_allocator]` (`mimalloc`). glibc malloc keeps RSS ~30–40%
  above the working set under many-thread fragmentation and is slow to return memory to
  the OS; mimalloc stays tight and decays dirty pages. (Chosen over jemalloc because it
  builds with only a C compiler — no make/autotools — so it compiles in the slim
  distroless builder image. Supersedes `MALLOC_ARENA_MAX`.)
- **Streaming lists (on by default).** `KOPIUR_STREAMING_LISTS` / `streamingLists`
  enables `Config::streaming_lists()` on the cluster-wide watches to cut peak memory on
  the initial resync. **On by default** (the chart's `kubeVersion` floor is 1.32): it
  needs apiserver WatchList support (beta 1.32/1.33, GA 1.34), so the controller gates
  it on the server version at startup and falls back to paged lists below 1.32. A
  **failed** version probe (e.g. booting mid-outage) also falls back to paged lists for
  that run — kube cannot self-degrade from WatchList, and streaming against a pre-1.32
  server would leave every watcher retrying an unsupported verb forever. Set
  `streamingLists: false` only if your apiserver has the WatchList feature gate disabled.
- **Reconcile concurrency cap.** `KOPIUR_RECONCILE_CONCURRENCY` /
  `reconcileConcurrency` (default **8 per controller**; the operator runs 8
  controllers, so ≤64 in-flight reconciles process-wide; `0` = unbounded, not
  recommended) — see [API-outage resilience](#api-outage-resilience) for why the
  default is bounded.

Observe it: `kopiur_process_resident_memory_bytes` (an observable gauge sampled from
`/proc/self/statm` at scrape time) exposes the controller's RSS on `/metrics` and
guards these wins against regressions.

## API-outage resilience

An OOM-flapping apiserver once drove the controller to fd exhaustion (`EMFILE`)
within ~11 seconds of startup: every watcher reconnect re-listed every primary, the
referent mappers fanned single events out to whole fleets, **unbounded** reconcile
concurrency dispatched all of it at once, every reconcile failed with a connect
error that pinned a socket for the 30s connect timeout, and every failure spawned
one more Event POST at the dead endpoint. With the fd table full, the
`/metrics`+`/healthz` listener could no longer `accept()`, the kubelet's liveness
probe failed at the TCP layer, and the restart's re-list repeated the storm (the
election Lease logged `transitions=80`). The defenses, layered:

- **Bounded reconcile concurrency** (`reconcileConcurrency`, above) — the primary
  fix: bounds in-flight API calls and sockets no matter how large the re-list or
  fan-out. A slot is held for the reconcile's full duration, so the floor is set by
  the slow holders: hooks (deliberately inline — detaching them would stretch the
  app's quiesce window) and in-process kopia ops, now themselves bounded by
  `config::KOPIA_SUBPROCESS_TIMEOUT` (120s; previously a hung NFS mount pinned a
  slot forever).
- **Futile-Event suppression + bounded publishes** — a failure whose cause is the
  kube transport being down (`Error::event_publish_futile()`, exhaustive over
  `kube::Error`) skips its Warning-Event POST; all remaining failure publishes
  share a 16-permit pool (saturation **drops**, never queues) and a 10s per-publish
  timeout. Drops are visible as
  `kopiur_controller_failure_events_dropped{cause=transport|saturated|timeout}` —
  the `/metrics` signature of an outage, when Events themselves can't get through.
- **Jittered transient requeue** — the flat 30s retry re-synchronized every failed
  object into lockstep waves; transient requeues now spread deterministically
  (FNV of kind/namespace/name, no RNG) across [30s, 60s). Watch-driven re-triggers
  override any requeue (the scheduler keeps the earlier deadline), so jitter shapes
  steady-state repeat failures; the recovery stampede is bounded by the concurrency
  cap.
- **Client timeouts** — connect 5s (was 30s: the dominant per-fd hold on a
  blackholed endpoint), read 305s (was unbounded; must exceed kube-runtime's
  290+5s watch idle window). The hooks `workloadExec` stream rides a second,
  read-timeout-exempt client (`Context::exec_client`) so a long-quiet quiesce
  command is not killed mid-stream.
- **Leader renew attempt deadline** — each Lease renew attempt runs under
  `RENEW_DEADLINE` (10s); a connected-but-stalled apiserver previously wedged the
  renew loop forever (in HA: split-brain once a standby claimed the expired
  Lease). Exit-on-lost-lease itself is deliberate (split-brain avoidance) — during
  a long outage the pod still restarts by design, but bounded concurrency makes
  each restart's re-list cheap.
- **`RLIMIT_NOFILE` soft→hard raise at startup** — defense in depth; logged,
  never fatal.

The axum accept loop needed no change: axum 0.8 already sleeps 1s and retries on
`EMFILE` (exactly the cadence the incident logs showed) and recovers once fds free.
Regression guards: hermetic tests across `config.rs`/`error.rs`/`io/tests.rs`/
`leader.rs`/`startup.rs`, plus the `apiserver_flap` e2e shard (kills the kind
apiserver under a seeded fleet; asserts no EMFILE, bounded restarts/lease
transitions, and post-recovery convergence).
