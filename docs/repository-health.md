# Repository health & preflight checks

This page enumerates **every health/preflight check Kopiur runs today** — what each
one does, where it surfaces, and what it gates — plus the **known limitation** of the
current fail-fast design and the **planned** stronger preflight that is not yet built.

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
| **Reactive re-probe on failure** | `Snapshot` reconciler → repository | `reverify-requested-at` annotation → `status.lastReverifyAt` | Forces a fresh connectivity probe within ~60s of a failed backup |
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
  Job** (the controller can't reach the backend or mount the volume in-process). Their
  `phase` is only refreshed when that Job is recycled on the **catalog refresh cadence
  (`catalog.refreshInterval`, default `1h`)** — *or* on demand via the re-probe nudge below.

### Reactive re-probe (closing most of the latency window)

When a backup mover Job fails, the `Snapshot` stamps a rate-limited
`reverify-requested-at` annotation on its repository, asking it to re-probe connectivity
**now** rather than waiting for the next refresh. The repository honors a fresh token
once (loop-guarded on `status.lastReverifyAt`) and flips to `Failed` if the backend is
gone — at which point the gate suppresses all further Jobs.

!!! warning "Known limitation: a one-Job detection window"
    For object-store / volume-backed repositories, the gate reads `status.phase`, which
    is only refreshed on the `catalog.refreshInterval` cadence (default `1h`). So at the
    **onset** of an outage, **one** scheduled backup can still launch a doomed Job before
    anything flips the phase. The reactive re-probe then engages within ~60s of that
    first failure, so you get **one** doomed Job per outage instead of one per schedule
    tick — not zero. Bare-path filesystem repos don't have this window (they re-probe
    every reconcile). To tighten it for object stores, lower `catalog.refreshInterval`,
    or see the active probe below.

## Not yet implemented — the stronger preflight

The current design is **reactive fail-fast**, deliberately scoped (it adds no standing
Jobs and re-uses the repository's existing connectivity probe). Two stronger forms were
discussed and intentionally deferred; this section is the design intent, not current
behavior.

!!! note "Status: design only"
    Nothing in this section ships yet. It records *how* we'd build a proactive preflight
    so the choice is deliberate when we do.

### 1. Active periodic connectivity probe

Have the controller proactively run a lightweight `kopia repository connect` (it already
ships the kopia binary) on a short cadence for object-store backends — which need only
network + credentials, no volume mount — and flip `phase` *before* the next scheduled
Job. This converts "one doomed Job per outage" into "zero" for those backends.

Implementation sketch:

- A short idempotent connect probe in the repository reconcile (ADR §5.4 already permits
  short idempotent ops in the controller), gated to object-store backends and bounded by
  a dedicated `health.probeInterval` (default off / conservative) so it doesn't hammer
  metered object stores.
- Keep the result on the existing `phase` machine — no new condition vocabulary.
- Volume-backed filesystem repos still can't be probed in-process (no mount); they keep
  the reactive path.

### 2. CEL-configurable enumerated preflight (tuppr-style)

Let a user declare arbitrary preconditions a backup must satisfy — expressed as CEL,
the same engine `successExpr` and the identity `*Expr` fields use — evaluated before the
Job is created (e.g. "the repository's last successful maintenance is < 7d old", "free
space > X"). This generalizes the hard-coded gate into a user-extensible preflight.

Implementation sketch:

- A `preflightExpr` (or list) on `SnapshotPolicy`, validated at admission exactly like
  `successExpr` (compile + trial-evaluate + bool result; see
  [verification](backups.md#verification--prove-the-snapshots-are-restorable)).
- An environment exposing repository status (`phase`, `lastReverifyAt`, storage stats,
  maintenance recency) and recent run history.
- A failed predicate holds the `Snapshot` in `Pending` with a `PreflightFailed` reason,
  symmetric with the existing readiness gate.

If you want either of these, open an issue (or ask) — they're additive to what's here.

## See also

- [Backups & schedules → verification](backups.md#verification--prove-the-snapshots-are-restorable)
- [Troubleshooting → stuck in `Pending` with no Job](troubleshooting.md#backup-or-restore-stuck-in-pending-with-no-job)
- [Repositories & backends](repositories.md)
