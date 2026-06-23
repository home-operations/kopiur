# Repository health & preflight checks

This page enumerates **every health/preflight check Kopiur runs today** — what each
one does, where it surfaces, and what it gates — including the opt-in **backend health
probe** and the opt-in **CEL backup preflight** (user-declared preconditions a backup
must satisfy before it runs).

The mental model: Kopiur separates the **repository** (a first-class resource whose
reconcile owns connectivity) from the **work** (`Snapshot`/`Restore`/`Maintenance`/…
that runs in a short-lived mover Job). Most "preflight" is therefore **the work
refusing to start until the repository is known healthy**, rather than each Job
re-testing the backend itself.

## What runs today

| Check | Where it runs | Surfaced as | Gates |
|---|---|---|---|
| **Connectivity probe** (`kopia repository connect`) | Repository reconcile | `status.phase` (`Pending`→`Initializing`→`Ready`/`Degraded`/`Failed`) + `Ready`/`Stalled` conditions | Everything downstream keys off `phase == Ready` |
| **Readiness gate** (`repository_ready`) | `Snapshot`, `Maintenance`, `SnapshotPolicy`, `RepositoryReplication` reconcilers | `RepositoryNotReady` / `WaitingForRepository` reason, held in `Pending`/`Reconciling` | Building & launching the mover Job |
| **Backup preflight** (opt-in, `spec.preflight`) | `Snapshot` reconcile (before launch) | `PreflightFailed` reason, held in `Pending` then `Failed` after `timeout` | User-declared CEL preconditions (e.g. maintenance freshness) before the backup Job runs |
| **Reactive re-probe on failure** | `Snapshot` reconciler → repository | `reverify-requested-at` annotation → `status.lastReverifyAt` | Forces a fresh connectivity probe within ~60s of a failed backup |
| **Backend health probe** (opt-in, `spec.health.probe`) | Repository reconcile (post-`Ready`) | `BackendReachable` condition (`RepositoryVanished` / `BackendUnreachable`) + Warning Event + `kopiur_repository_health_probe_failures` | **Advisory** — proactively detects a wiped/unreachable backend; repo **stays `Ready`** (alert-only) |
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
already applied — `Snapshot` was the only write path that skipped it.

### How the repository's `phase` is kept current

- **Bare-path filesystem** repos (reachable from the controller's own filesystem) connect
  **in-process on every reconcile** — steady-state every 5 minutes, or immediately when a
  re-probe is requested. Detection here is prompt.
- **Object-store and volume-backed filesystem** repos connect in a **short bootstrap
  Job** (the controller can't reach the backend or mount the volume in-process). By
  default they bootstrap **once** and are **not** re-probed on a timer — `phase` then
  changes only on a spec change or via the re-probe nudge below. Opting in to
  `catalog.periodicRefresh: true` recycles the bootstrap Job every
  `catalog.refreshInterval` (default `1h`), which also re-probes connectivity proactively.

### Reactive re-probe (closing most of the latency window)

When a backup mover Job fails, the `Snapshot` stamps a rate-limited
`reverify-requested-at` annotation on its repository, asking it to re-probe connectivity
**now** rather than waiting for the next refresh. The repository honors a fresh token
once (loop-guarded on `status.lastReverifyAt`) and flips to `Failed` if the backend is
gone — at which point the gate suppresses all further Jobs.

!!! warning "Known limitation: a one-Job detection window"
    For object-store / volume-backed repositories, the gate reads `status.phase`. By
    default (`periodicRefresh` off) the phase isn't re-probed on a timer, so an outage
    that begins between backups is detected when the **next** backup fails: that failure
    fires the reactive re-probe, the phase flips to `Failed` within ~60s, and the gate
    then suppresses every subsequent Job — **one** doomed Job per outage instead of one
    per schedule tick, not zero. Bare-path filesystem repos don't have this window (they
    re-probe every reconcile). To get proactive timed detection for object stores, enable
    the [backend health probe](#backend-health-probe-opt-in) below (or
    `catalog.periodicRefresh`, at the cost of re-running the bootstrap Job on that cadence).

## Backend health probe (opt-in)

`spec.health.probe` opts a Repository (or ClusterRepository) into a **periodic
backend re-connect** so a wiped or unreachable repository is detected proactively —
without waiting for the next backup to fail. It closes the detection window above
for object-store and volume-backed repositories.

```yaml
--8<-- "deploy/examples/27-repository-health-probe.yaml:health"
```

The full apply-ready example (Secret + Repository):
[`deploy/examples/27-repository-health-probe.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/27-repository-health-probe.yaml).

**It is alert-only by design.** The repository **stays `Ready`** while the probe
runs and even when it raises an alert — so backups and replication are never
paused. The outcome surfaces three ways, never as a phase flip:

- a `BackendReachable` **condition** (`True` healthy; `False` with reason
  `RepositoryVanished` or `BackendUnreachable`),
- a **Warning Event** (`kubectl describe`), fired once per episode (after the
  debounce, and again if the failure *reason* escalates),
- the `kopiur_repository_health_probe_failures{kind,namespace,name,outcome}` metric.

Two failures are reported distinctly, because they demand different responses:

| Alert | Means | What to do |
|---|---|---|
| `RepositoryVanished` | backend **reachable**, kopia repository **absent** (format blob gone) | Verify the backend is *truly* empty before any re-create (see warning below) |
| `BackendUnreachable` | backend unreachable, mount/path missing, or auth/lock failed | Fix the backend / credentials / volume; **not** a wipe |

!!! warning "kopiur never auto-recreates a repository it once trusted"
    A wiped repository and a transient outage look alike, and silently creating a
    fresh empty repository over a real one destroys restorability. So
    `create.enabled` governs the **first** bootstrap only — once a repository has
    been `Ready` (it carries a pinned `status.uniqueId`), kopiur will **never**
    recreate it, even on a `RepositoryVanished` alert. Re-creating is always a
    deliberate human action. And a `RepositoryVanished` alert means the *format
    blob* is gone — **data blobs may still remain** and be recoverable, so verify
    the backend is genuinely empty (and that no other Repository points at the same
    backend) before you act.

!!! tip "Tuning"
    - `interval` — how often to re-connect (Go-style duration; min `30s`, default
      `30m`). Each probe runs a short connect, so leave it long for metered stores.
    - `failureThreshold` — consecutive failing probes required before the alert
      fires (default `3`). Debounces a single transient blip (an S3
      list-after-delete race, a NAS reboot) from paging on-call. Any success resets
      the counter and clears the condition.

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
| `repository.backendReachable` | bool | the [health probe](#backend-health-probe-opt-in)'s `BackendReachable` condition is `True` — **`true` when the probe is disabled** (no evidence of a fault) |
| `repository.snapshotCount` | int | snapshots in the repository |
| `repository.indexBlobCount` | int | content-index blobs (maintenance-backlog signal) |
| `repository.sizeBytes` | int | logical bytes under management (repository **total size**, *not* backend free space) |
| `repository.lastHealthyKnown` | bool | a successful health probe has been recorded |
| `repository.lastHealthyAgeSeconds` | int | seconds since the last successful probe |
| `repository.lastReverifyKnown` | bool | a reverify has been recorded |
| `repository.lastReverifyAgeSeconds` | int | seconds since the last reverify |
| `maintenance.hasRun` | bool | the repo's `Maintenance` has a recorded successful run (scheduled **or** manual run-now) |
| `maintenance.lastSuccessAgeSeconds` | int | seconds since the most recent successful maintenance of any mode |

!!! warning "Unknown values fail closed — guard freshness checks with `hasRun`/`*Known`"
    Every age/count is `i64::MAX` ("infinitely old") when never observed, so a naive
    freshness check like `maintenance.lastSuccessAgeSeconds < 604800` correctly **blocks**
    the backup when maintenance has never run — rather than silently passing. Write the
    intent explicitly with the boolean companion:
    `maintenance.hasRun && maintenance.lastSuccessAgeSeconds < 604800`.

!!! tip "Validation & the AND rule"
    Each `expr` is compiled and trial-evaluated **at admission** (`kubectl apply`), so a
    typo or non-bool expression is rejected up front, not at the first backup. Check
    `name`s must be unique. All checks must pass; the first failing one names itself in
    the Snapshot's `Ready` condition message (`kubectl describe snapshot`).

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
