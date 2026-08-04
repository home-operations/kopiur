# Repository health & preflight checks

This page enumerates **every health/preflight check Kopiur runs today** — what each
one does, where it surfaces, and what it gates — including the default-on **backend
health probe** (which doubles as the **repository circuit breaker**,
[ADR-0007](adr/0007-repository-circuit-breaker.md)) and the opt-in **CEL backup
preflight** (user-declared preconditions a backup must satisfy before it runs).

The mental model: Kopiur separates the **repository** (a first-class resource whose
reconcile owns connectivity) from the **work** (`Snapshot`/`Restore`/`Maintenance`/…
that runs in a short-lived mover Job). Most "preflight" is therefore **the work
refusing to start until the repository is known healthy**, rather than each Job
re-testing the backend itself.

## What runs today

| Check | Where it runs | Surfaced as | Gates |
|---|---|---|---|
| **Connectivity probe** (`kopia repository connect`) | Repository reconcile | `status.phase` (`Pending`→`Initializing`→`Ready`/`Degraded`/`Failed`) + `Ready`/`Stalled` conditions | Everything downstream keys off `phase == Ready` |
| **Readiness gate** (`repository_ready`) | `Snapshot`, `Maintenance`, `SnapshotPolicy`, `RepositoryReplication`, `Restore` reconcilers | `RepositoryNotReady` / `WaitingForRepository` reason, held in `Pending`/`Reconciling` | Building & launching the mover Job |
| **Backup preflight** (opt-in, `spec.preflight`) | `Snapshot` reconcile (before launch) | `PreflightFailed` reason, held in `Pending` then `Failed` after `timeout` | User-declared CEL preconditions (e.g. maintenance freshness) before the backup Job runs |
| **Reactive re-probe on failure** | `Snapshot` reconciler → repository | `reverify-requested-at` annotation → `status.lastReverifyAt` | Forces a fresh connectivity probe within ~60s of a failed backup |
| **Backend health probe** (default-ON, `spec.health.probe`) | Repository reconcile (post-`Ready`) | `BackendReachable` condition (`RepositoryVanished` / `BackendUnreachable`) + Warning Event + `kopiur_repository_health_probe_failures` | **The circuit breaker's sensor**: past `failureThreshold`, `onFailure: Degrade` (default) moves the repo to `Degraded` and pauses all consumers until a re-connect succeeds; `onFailure: Alert` keeps it advisory (repo stays `Ready`) |
| **Credentials available** | Mover preflight | `CredentialsAvailable=False` + Warning Event | The mover starting (the credential Secret must exist in the workload namespace) |
| **Mover permitted** | Admission / reconcile | `MoverPermitted=False` | A privileged mover that wasn't opted in |
| **Security-context compatibility** | Admission (advisory) + post-run | admission Warning + `SecurityContextCompatible=False` | Advisory — warns the mover UID likely can't read the source |
| **Index-blob health** | Repository reconcile (post-`Ready`) | `IndexBlobHealth` condition + Warning Event | Advisory — flags maintenance falling behind; non-blocking |
| **Scratch writability** (`/scratch`) | Mover, **deep verify only** | `ScratchNotWritable` error | The restore-test, before kopia runs — turns a cryptic `mkdir` failure into an actionable one |
| **Terminal gate** | Repository reconcile | stays `Failed`, long heartbeat | Stops hammering the backend after a non-retryable failure until an input (spec/Secret) changes |

### The fail-fast gate (the headline behavior)

A `Snapshot` will **not** spawn a mover Job while its repository is not `Ready`. Instead
of a storm of pods that each only fail on `kopia repository connect` (the classic
"volsync spins up jobs that can't do anything" after a NAS doesn't come back from a
power loss), the backup holds in `Pending` with reason `RepositoryNotReady` and
resumes automatically once the repository reconnects.

```console
$ kubectl get snapshot <name> -n <ns> \
    -o jsonpath='{.status.conditions[?(@.type=="Ready")].message}'
# → "waiting for repository `nas` to become `Ready` before launching the backup…"
```

This is the same gate `Maintenance`, `SnapshotPolicy`, and `RepositoryReplication`
already applied; `Snapshot` and `Restore` were the write paths that skipped it, and
both are gated now.

### How the repository's `phase` is kept current

- **Bare-path filesystem** repos (reachable from the controller's own filesystem) connect
  **in-process on every reconcile** — steady-state every 5 minutes, or immediately when a
  re-probe is requested. Detection here is prompt.
- **Object-store and volume-backed filesystem** repos connect in a **short bootstrap
  Job** (the controller can't reach the backend or mount the volume in-process). The
  default-on [backend health probe](#backend-health-probe-default-on) re-runs that
  connect every `probe.interval` (default `30m`), so `phase` tracks the backend on a
  timer — plus immediately on a spec change or the re-probe nudge below.
  `catalog.periodicRefresh: true` additionally recycles the bootstrap Job every
  `catalog.refreshInterval` (default `1h`) for catalog freshness.
- **While the circuit breaker is open** (phase `Degraded`), the repository retries the
  connect itself on an exponential backoff — 120s doubling to a 600s cap per
  consecutive failure — and **any** successful connect (probe or retry) heals it back
  to `Ready` automatically. The phase holds `Degraded` **stably** between retries (no
  `Initializing` flapping), so alerts with a `for:` clause and the consumer gates see
  one coherent open state.

### Reactive re-probe (closing most of the latency window)

When a backup mover Job fails, the `Snapshot` stamps a rate-limited
`reverify-requested-at` annotation on its repository, asking it to re-probe connectivity
**now** rather than waiting for the next probe interval. The repository honors a fresh
token once (loop-guarded on `status.lastReverifyAt`). What the re-probe's verdict does
depends on its class: a **retryable outage** (connection refused/timeout, DNS — the
`RepositoryUnavailable` class) on an already-bootstrapped repository lands `Degraded`
(kstatus `Reconciling` — self-healing, `flux wait` keeps waiting) and enters the retry
loop above; a **terminal** verdict (bad credentials, locked, vanished-and-confirmed)
lands `Failed` (kstatus `Stalled` — a human is needed). Either way the gate then
suppresses further Jobs.

/// note | The detection window is one Job, and only the first

An outage that begins between backups is detected either by the next scheduled probe
(within `probe.interval`) or by the next backup's failure — whichever comes first. A
backup already in flight (or launched inside that window) fails: **one** doomed Job
per outage. It cannot become one per schedule tick any more — the failure nudges the
re-probe, the probe failures cross `failureThreshold`, and the breaker opens. This
replaces the old "known limitation": before the breaker, a *retryable* outage never
flipped the phase at all, so the gate stayed open and every slot burned a Job
(issue [#345](https://github.com/home-operations/kopiur/issues/345): 53 Failed CRs,
23 dead Jobs). Bare-path filesystem repos don't even have the one-Job window (they
re-probe every reconcile).

///

## Backend health probe (default-ON)

`spec.health.probe` gives every Repository (and ClusterRepository) a **periodic
backend re-connect** so a wiped or unreachable repository is detected proactively —
without waiting for the next backup to fail. Since
[ADR-0007](adr/0007-repository-circuit-breaker.md) it is **on by default**; you only
write the block to tune it or opt out.

```yaml
--8<-- "deploy/examples/27-repository-health-probe.yaml:health"
```

The full apply-ready example (Secret + Repository):
[`deploy/examples/27-repository-health-probe.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/27-repository-health-probe.yaml).

**What sustained failure does is `probe.onFailure`:**

- **`Degrade` (default) — the circuit breaker.** Past `failureThreshold`
  consecutive failed connects the repository moves to phase **`Degraded`**
  (`BackendReachable=False`, `Ready=False`) and every consumer gate closes:
  backups, maintenance, replication, and restores **pause** instead of burning
  mover Jobs against a dead backend. Recovery is automatic — the repository
  keeps re-connecting on a 120s→600s backoff, and any success heals it to
  `Ready`, cleared streak and all. Nothing needs restarting or acknowledging.
- **`Alert` — the opt-out.** The repository **stays `Ready`** even when the
  probe raises an alert, so backups keep running (and failing) against the
  unhealthy backend. This is the pre-breaker behavior for users who prefer
  try-anyway.

Under either mode a failure surfaces as:

- a `BackendReachable` **condition** (`True` healthy; `False` with reason
  `RepositoryVanished` or `BackendUnreachable`),
- a **Warning Event** (`kubectl describe`), fired once per episode (after the
  debounce, and again if the failure *reason* escalates),
- the `kopiur_repository_health_probe_failures{kind,namespace,name,outcome}` metric.

### What "paused" looks like (and how it recovers)

While the breaker is open, gated work **parks** — it is deferred, never refused or
lost. A scheduled `Snapshot` holds in `Pending` with `Ready` reason
`RepositoryNotReady`; with the default `concurrencyPolicy: Forbid` that parked run
counts as active, so later slots wait and parked work is **bounded at one** per
schedule. On recovery the parked (pinned stale) slot fires **exactly once** as the
catch-up backup, and the normal cadence resumes. (`concurrencyPolicy: Allow`
schedules park one `Pending` per slot — that policy's declared overlap contract.)

On the wire this is visible as:

- `kopiur_repository_breaker_trips_total{kind,namespace,name,probe_kind}` — one
  increment per breaker opening (the transition, never re-confirmations),
- `kopiur_repository_consecutive_backend_failures{kind,namespace,name}` — the
  live failure streak (a `0` after recovery means "healed"),
- `kopiur_repository_breaker_open_since_timestamp_seconds{kind,namespace,name}` —
  exists **only while open**; `time() - metric` is the open duration,
- `kopiur_snapshot_gated{namespace,policy}` — the parked-`Pending` population,
  draining to absence on recovery,
- Helm alert rules `KopiurRepositoryBreakerOpen` (warning, 15m) and
  `KopiurSnapshotsGated` (info, 30m) — see
  [observability](dev/observability.md).

Two failures are reported distinctly, because they demand different responses:

| Alert | Means | What to do |
|---|---|---|
| `RepositoryVanished` | backend **reachable**, kopia repository **absent** (format blob gone) | Verify the backend is *truly* empty before any re-create (see warning below) |
| `BackendUnreachable` | backend unreachable, mount/path missing, or auth/lock failed | Fix the backend / credentials / volume; **not** a wipe |

/// warning | kopiur never auto-recreates a repository it once trusted

A wiped repository and a transient outage look alike, and silently creating a
fresh empty repository over a real one destroys restorability. So
`create.enabled` governs the **first** bootstrap only — once a repository has
been `Ready` (it carries a pinned `status.uniqueId`), kopiur will **never**
recreate it, even on a `RepositoryVanished` alert. Re-creating is always a
deliberate human action. Under the default `onFailure: Degrade` a vanish first
opens the breaker (`Degraded` — pausing is right either way) and the retry loop
then confirms it: a repository that is genuinely gone escalates to **terminal
`Failed`** for a human, still without recreating anything. And a
`RepositoryVanished` alert means the *format blob* is gone — **data blobs may
still remain** and be recoverable, so verify the backend is genuinely empty
(and that no other Repository points at the same backend) before you act.

///

/// tip | Tuning & opting out

- `interval` — how often to re-connect (Go-style duration; min `30s`, default
  `30m`). Each probe runs a short connect, so leave it long for metered stores.
- `failureThreshold` — consecutive failing probes required before the failure
  is acted on (default `3`). Debounces a single transient blip (an S3
  list-after-delete race, a NAS reboot) from alarming or tripping the breaker.
  Any success resets the counter and clears the condition.
- `onFailure: Alert` — keep the repository `Ready` through failures
  (alert-only; backups never pause).
- `enabled: false` — no probe at all, which also disables the breaker (the
  probe is its only sensor): detection falls back to the next backup's failure.

///

/// note | How a probe run is tracked

On an object-store, server, or volume-backed backend a probe re-connects by
running the repository's `<name>-discovery` mover Job, so kopiur tracks each run
across two reconciles:

- `status.health.probeAttemptAt` is stamped when the Job is **launched** and
  cleared when its result is finalized. While it is set, the finished Job is
  recognised as *that probe's* result rather than a stale one to recycle.
- `status.health.lastProbeAt` is stamped when the run **finishes** (success or
  failure) and drives the interval timer.

A probe consumes its Job exactly once, so a healthy repository creates and
destroys **one** mover Job per `interval` — if you see the bootstrap Job
recreated every few seconds, that is [#273][issue-273], fixed in v0.7.6. A
successful bootstrap (or breaker-recovery connect) also **seeds** `lastProbeAt`,
so the first periodic probe lands one full `interval` after the connect that
just proved the backend healthy — never immediately on top of it.

A probe also stands aside while a real (re-)bootstrap is in flight: a repository
that is not `Ready` (a spec change is being applied, the bootstrap has failed,
or the breaker is open) does not run the interval probe. While `Degraded` the
**strict retry loop** is the sensor instead — same connect, same
`consecutiveProbeFailures` streak, on the 120s→600s backoff — so the streak keeps
counting across the whole outage and any success heals. `phase: Failed` /
`phase: Degraded` is the louder signal in that window.

[issue-273]: https://github.com/home-operations/kopiur/issues/273

///

## Backup preflight (opt-in)

The readiness gate above is a single hard-coded precondition: *the repository is
`Ready`*. `spec.preflight` on a **`SnapshotPolicy`** generalizes that into
**user-declared preconditions** — named CEL expressions that must **all** hold before
a backup's mover Job launches. It's the same CEL engine `successExpr` and the identity
`*Expr` fields use, evaluated by the operator at reconcile against **live repository +
maintenance state**.

```yaml
--8<-- "deploy/examples/28-preflight-checks.yaml:preflight"
```

The full apply-ready example (Secret + Repository + SnapshotPolicy + SnapshotSchedule):
[`deploy/examples/28-preflight-checks.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/28-preflight-checks.yaml).

**How a failing check behaves.** A `Snapshot` whose preflight isn't satisfied is held in
`Pending` with reason `PreflightFailed` (no mover Job is created). Once
`spec.preflight.timeout` elapses (default `10m`; `0` holds forever), it transitions to
`Failed` — bounded so a schedule firing against a never-met precondition doesn't pile up
`Pending` CRs. The timeout clock starts when the check **first fails** (after the
repository is `Ready`), not at Snapshot creation, so a slow-to-connect repository doesn't
eat the budget. `Failed` preflight Snapshots are pruned by the schedule's
[`failedJobsHistoryLimit`](#bounding-failed-snapshots).

### The CEL environment

Each check is a CEL **bool** expression over two variables:

| Variable | Type | Meaning |
|---|---|---|
| `repository.phase` | string | repository `status.phase` (`Ready`, …) |
| `repository.ready` | bool | `phase == Ready` |
| `repository.backendReachable` | bool | the [health probe](#backend-health-probe-default-on)'s `BackendReachable` condition is `True` — **`true` when the probe is disabled** (no evidence of a fault). On an `onFailure: Alert` repository this check can hold backups `Pending` through an outage and `Fail` them once `preflight.timeout` elapses — the user-configured bound |
| `repository.snapshotCountKnown` | bool | the snapshot count has been observed (guard `snapshotCount` checks with this) |
| `repository.snapshotCount` | int | snapshots in the repository |
| `repository.indexBlobCountKnown` | bool | the index-blob count has been observed |
| `repository.indexBlobCount` | int | content-index blobs (maintenance-backlog signal) |
| `repository.sizeBytesKnown` | bool | the repository size has been observed |
| `repository.sizeBytes` | int | logical bytes under management (repository **total size**, *not* backend free space) |
| `repository.lastHealthyKnown` | bool | a successful health probe has been recorded |
| `repository.lastHealthyAgeSeconds` | int | seconds since the last successful probe |
| `repository.lastReverifyKnown` | bool | a reverify has been recorded |
| `repository.lastReverifyAgeSeconds` | int | seconds since the last reverify |
| `maintenance.hasRun` | bool | the repo's `Maintenance` has a recorded successful run (scheduled **or** manual run-now) |
| `maintenance.lastSuccessAgeSeconds` | int | seconds since the most recent successful maintenance of any mode |

/// warning | Unknown values — always pair with the `*Known`/`hasRun` companion bool

An unobserved age/count/size is `i64::MAX`. For a **freshness** check
(`maintenance.lastSuccessAgeSeconds < 604800`) that fails *closed* — the unknown value
is "infinitely old", so the check blocks, which is what you want. But for a
**count/size** check the same sentinel fails *open*: `repository.snapshotCount > 0`
is `true` against `i64::MAX`, so an unscanned repository would wrongly pass. Always
guard with the boolean companion so the unknown case fails closed:

- `maintenance.hasRun && maintenance.lastSuccessAgeSeconds < 604800`
- `repository.snapshotCountKnown && repository.snapshotCount > 0`
- `repository.sizeBytesKnown && repository.sizeBytes < 1000000000000`

///

/// tip | Validation & the AND rule

Each `expr` is compiled and trial-evaluated **at admission** (`kubectl apply`), so a
typo or non-bool expression is rejected up front, not at the first backup. Check
`name`s must be unique. All checks must pass; the first failing one names itself in
the Snapshot's `Ready` condition message (`kubectl describe snapshot`).

///

### Bounding failed Snapshots

GFS retention prunes only **successful** snapshots, so failures (including preflight
`Failed`) are bounded separately by `SnapshotSchedule.spec.failedJobsHistoryLimit` — the
maximum number of `Failed` Snapshots a schedule keeps (newest by completion time;
default `10`, `0` keeps none). The oldest beyond the limit are deleted each reconcile.
Manually-created (non-scheduled) Snapshots are one-offs and aren't affected.

## See also

- [Backups & schedules → verification](backups.md#verification--prove-the-snapshots-are-restorable)
- [Troubleshooting → stuck in `Pending` with no Job](troubleshooting.md#backup-or-restore-stuck-in-pending-with-no-job)
- [Repositories & backends](repositories.md)
