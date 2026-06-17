# Maintenance

Kopia repositories need periodic **maintenance** to stay healthy: compacting indexes, advancing epochs, and — most importantly — **reclaiming storage** by garbage-collecting content that deleted snapshots no longer reference. Without it, a repository keeps growing even as you expire old backups.

Kopiur makes maintenance a first-class, **default-managed** concern. You don't have to remember to schedule it: every `Repository` and `ClusterRepository` gets a `Maintenance` resource automatically, and the operator runs `kopia maintenance` on a schedule for **every backend** — filesystem and object stores (S3, Azure, GCS, B2, …) alike.

/// info | How it runs

Each scheduled run executes in a short-lived **mover Job** (the same mechanism used for backups and restores), so maintenance works identically whether your repository lives on a PVC or in an object store the operator can't reach directly.

///

## Quick vs. full

kopia has two maintenance passes, and Kopiur schedules them independently:

| Pass      | kopia command                     | What it does                                                                       | Default schedule                        |
| --------- | --------------------------------- | ---------------------------------------------------------------------------------- | --------------------------------------- |
| **Quick** | `kopia maintenance run --no-full` | Cheap, frequent: index compaction, epoch advance.                                  | every 6h (`0 */6 * * *`), 30m jitter    |
| **Full**  | `kopia maintenance run --full`    | Heavier: content garbage-collection + rewrite — this is what **reclaims storage**. | daily at 03:00 (`0 3 * * *`), 1h jitter |

A **full** run subsumes a **quick** run, so when both are due at once the operator runs full and advances both clocks.

## The default-managed model

Maintenance is **on by default**. For every `Repository`/`ClusterRepository`, the operator projects a `Maintenance` resource (named after the repository) with the default schedule above. You can see it with:

```console
$ kubectl get maintenance -A
NAMESPACE   NAME          REPOSITORY    OWNER                          AGE
billing     nas-primary   nas-primary   kopiur/billing/nas-primary     4h44m
```

There are three ways to control it, in increasing order of explicitness.

## Try it end-to-end

Prove the default-managed model — *a `Maintenance` appears with nothing but a `Repository`* — and then force a run on demand. The bundle [`deploy/examples/tryit/maintenance.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/maintenance.yaml) is deliberately minimal: namespace, a repo PVC, a `KOPIA_PASSWORD` Secret, and a filesystem `Repository` — and **no** `Maintenance` resource. The operator projects one for you.

The `Repository` below is the only CR in the bundle — note it carries no `spec.maintenance` block and there is no standalone `Maintenance`; the managed one is auto-projected the moment this repository is `Ready`:

```yaml
--8<-- "deploy/examples/tryit/maintenance.yaml:repository"
```

Fill in the single `REPLACE_ME` (`KOPIA_PASSWORD`) and apply once:

```console
$ kubectl apply -f deploy/examples/tryit/maintenance.yaml
$ kubectl -n kopiur-tryit wait --for=condition=Ready repository/primary --timeout=2m
```

**1. The managed `Maintenance` auto-appears.** You authored none — the operator created `primary` (named after the repository) with the default quick-6h / full-daily schedule:

```console
$ kubectl -n kopiur-tryit get maintenance
NAME      REPOSITORY   OWNER                      AGE
primary   primary      kopiur/kopiur-tryit/primary   20s
```

**2. Request a `full` run NOW.** Stamp the two on-demand annotations (`--overwrite` because the timestamp changes each time):

```console
$ kubectl annotate maintenance primary -n kopiur-tryit --overwrite \
    kopiur.home-operations.com/run-requested="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    kopiur.home-operations.com/run-mode=full
```

**3. Prove the run completed (deep).** The outcome lands in `status.manualRun`:

```console
$ kubectl -n kopiur-tryit wait \
    --for=jsonpath='{.status.manualRun.phase}'=Succeeded \
    maintenance/primary --timeout=5m
$ kubectl -n kopiur-tryit get maintenance primary \
    -o jsonpath='{.status.manualRun}'
{"requestedAt":"2026-06-17T14:05:00Z","mode":"full","phase":"Succeeded","completedAt":"2026-06-17T14:05:42Z"}
```

*(Illustrative timestamps.)*

**4. Confirm the full clock advanced.** A `full` run also stamps `status.full.lastRunAt`:

```console
$ kubectl -n kopiur-tryit get maintenance primary \
    -o jsonpath='{.status.full.lastRunAt}'
2026-06-17T14:05:42Z    # illustrative
```

/// note | `lastContentReclaimedBytes` reads `0` even on a successful run

`kopia maintenance run` does not emit a machine-readable reclaimed-bytes figure, so `status.full.lastContentReclaimedBytes` is reported as `0` today even though the run does reclaim space. The field exists and round-trips; populating it precisely is a planned enhancement.

///

To tear down: `kubectl delete namespace kopiur-tryit`.

### 1. Tune it inline on the repository

Set `spec.maintenance` on the `Repository`/`ClusterRepository` to override the schedule (or other knobs) while keeping it operator-managed:

```yaml
--8<-- "deploy/examples/maintenance-inline-on-repository.yaml"
```

`spec.maintenance` fields:

| Field            | Purpose                                                                                                                   |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `enabled`        | Default `true`. Set `false` to opt out (see [Disabling](#disabling-maintenance)).                                         |
| `schedule`       | Override `quick`/`full` cron + jitter (and `timezone`). Absent ⇒ the defaults above.                                      |
| `mover`          | Pod overrides for the maintenance Job (resources, scheduling, security context).                                          |
| `failurePolicy`  | `backoffLimit` / `activeDeadlineSeconds` for the Job.                                                                     |
| `takeoverPolicy` | Ownership-lease policy (see [Ownership](#ownership-and-shared-repositories)).                                             |
| `namespace`      | **`ClusterRepository` only** — which namespace the managed `Maintenance` lives in (defaults to the operator's namespace). |

### 2. Author a standalone `Maintenance`

For fine-grained control — a custom ownership identity or takeover policy — author a `Maintenance` directly ([example 08](examples.md#example-08--maintenance)). When one references a repository, the operator **defers to it and never creates a duplicate**, even if `spec.maintenance` is otherwise default-on.

```yaml
--8<-- "deploy/examples/08-maintenance.yaml:standalone"
```

### Disabling maintenance

Set `spec.maintenance.enabled: false` on the repository. The operator stops managing a `Maintenance` for it.

/// warning | Disabling stops space reclamation

With no maintenance, the repository never garbage-collects, so storage grows without bound even as you expire backups. Only disable it if something else runs `kopia maintenance` against the repository. Note that `enabled: false` only tells the operator not to create _its own_ `Maintenance` — an externally-authored one referencing the repository is always honored.

///

## Ownership and shared repositories

kopia tracks a single **maintenance owner** per repository. When several clusters (or operators) share one repository, only one should run maintenance at a time. `spec.ownership` encodes who that is and what to do on conflict:

- `owner` — a stable identity string for this `Maintenance`.
- `takeoverPolicy` — a closed enum:

| Policy                      | Behavior when another owner holds the lease                    |
| --------------------------- | -------------------------------------------------------------- |
| `Never` _(default, safest)_ | Do nothing; surface that the lease is held elsewhere and wait. |
| `PromptCondition`           | Set a condition asking an operator to decide; don't seize it.  |
| `Force`                     | Forcibly claim the lease and run.                              |

The lease is read inside the maintenance Job (which is the only place with repository access for object stores). If the policy declines to take over, the run is a successful no-op that records why on the resource's conditions.

### Self-healing a stale owner

kopia stamps a maintenance owner when a repository is **created**. kopiur stamps its own stable, lease-derived owner (e.g. `kopiur@kopiur-clusterrepository-nas`) so that every maintenance Job — each from a fresh, throwaway pod — recognizes itself as the owner and runs.

A repository created by an **older** kopiur (or by another tool) can instead carry kopia's auto-assigned **ephemeral** pod identity as the owner (e.g. `nonroot@nas-bootstrap-5trlr`). With the default `takeoverPolicy: Never`, every maintenance run then sees a "foreign" owner and yields **forever** — full maintenance never compacts, and index blobs pile up (see below).

kopiur now **self-heals this automatically**: on every bootstrap (initial connect and each catalog refresh), if the recorded owner doesn't match the stable lease owner, the operator re-stamps it (`kopia maintenance set --owner` is not owner-gated). No manual intervention is needed. `takeoverPolicy: Force` remains available as an immediate, one-time override if you'd rather not wait for the next bootstrap cycle.

## Index-blob health

kopia stores its content index as a set of **index blobs**. Each backup adds one; **maintenance compacts them back down**. So in a healthy repository the count rises during the day and falls after the next full-maintenance run. If maintenance stops keeping up — most often a stale owner (above), but also a disabled/failing `Maintenance` — the count climbs without bound, and once it gets high enough kopia warns "Found too many index blobs (N)" and backup/restore performance degrades.

kopiur observes the count on every bootstrap and surfaces it three ways, **without blocking the repository** (it stays `Ready`; this is a degradation warning, not an outage):

- a print column — `kubectl get repository` / `kubectl get clusterrepository` shows an `IndexBlobs` column (wide output);
- `status.storageStats.indexBlobCount`;
- when the count crosses the threshold, an `IndexBlobHealth=False` condition (reason `TooManyIndexBlobs`) **and** a Kubernetes **Warning** event with the remediation in its message:

```console
$ kubectl describe clusterrepository nas-shared | grep -A2 TooManyIndexBlobs
$ kubectl get events --field-selector reason=TooManyIndexBlobs -A
```

### The threshold knob

`spec.health.indexBlobWarnThreshold` sets the count above which the warning fires. It's optional on both `Repository` and `ClusterRepository`:

| Value             | Meaning                                                              |
| ----------------- | ------------------------------------------------------------------- |
| _absent_          | Use the built-in default of **1000** (well above a healthy repo).   |
| a positive number | Warn when the count exceeds it (lower = earlier warning).           |
| `0`               | **Disable** the warning entirely.                                   |

```yaml
spec:
  health:
    indexBlobWarnThreshold: 500   # warn sooner than the default 1000
```

/// warning | A high index-blob count means maintenance isn't running

The warning is a symptom; the fix is to get maintenance compacting again. Check that a `Maintenance` exists and is `Ready`, that it isn't yielding (`LeaseOwned=False`, reason `LeaseHeldByOther`), and that its owner is the stable lease owner — not an ephemeral `…-bootstrap-…` identity. The operator self-heals a stale owner on the next bootstrap; to recover **now**, set `spec.maintenance.takeoverPolicy: Force` once. Raising or zeroing `indexBlobWarnThreshold` only silences the warning — it does not compact the index.

///

## Running maintenance on demand

Maintenance normally fires on its quick/full crons, but you can request an
out-of-band run at any time by stamping two annotations on the `Maintenance` —
the operator routes it through the **same** mover, ownership-lease, and
single-flight path as the scheduled slots, so a manual run can never violate the
one-job-per-repository guarantee.

Declaratively (GitOps-friendly — set them in Git on the managed `Maintenance`,
as in [example 08](examples.md#example-08--maintenance)):

```yaml
metadata:
    annotations:
        # A NEW value requests a new run; re-applying the same value is a no-op.
        kopiur.home-operations.com/run-requested: "2026-01-01T00:00:00Z"
        kopiur.home-operations.com/run-mode: full # quick (default when absent) | full
```

- `run-requested` is an RFC3339 timestamp. A **new** timestamp requests a new
  run; re-applying the same value is a no-op once that request was handled.
- `run-mode` is `quick` (the default when absent) or `full`.

/// note | Imperative equivalent

For a one-off run without editing Git, stamp the same two annotations with
`kubectl` (`--overwrite` because the timestamp changes each time):

```console
$ kubectl annotate maintenance nas-primary -n billing --overwrite \
    kopiur.home-operations.com/run-requested="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    kopiur.home-operations.com/run-mode=full
```

///

The outcome lands in `status.manualRun` (`requestedAt`, `mode`, `phase`:
`Running`/`Succeeded`/`Failed`, `completedAt`). The
[kubectl plugin](cli/index.md) wraps this as
`kubectl kopiur maintenance run [NAME | --repository NAME] [--full] [--wait]`.

/// warning | Repositories bootstrapped by kopiur ≤ 0.3.x: claim the lease once
kopia records the repository CREATOR as the maintenance owner. Older kopiur
releases left that as the (ephemeral) bootstrap pod's identity, so every
maintenance run saw a "foreign" owner and `takeoverPolicy: Never` yielded
forever — maintenance silently never ran. New bootstraps stamp a stable,
lease-derived owner (`kopiur@kopiur-<ns>-<repo>`); for repositories created
before that, set `spec.ownership.takeoverPolicy: Force` once (or run
`kopia maintenance set --owner kopiur@<lease>` by hand) — the next run claims
the lease and subsequent runs proceed normally.

///

## Inspecting status

```console
$ kubectl get maintenance nas-primary -n billing -o yaml
```

Key `status` fields:

| Field                                                                | Meaning                                                                                                                                       |
| -------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `ownership.owner` / `ownership.claimedAt`                            | Current lease holder and when it was claimed.                                                                                                 |
| `quick.lastRunAt` / `full.lastRunAt`                                 | Timestamp of the most recent run of each pass.                                                                                                |
| `quick.lastHandledAt` / `full.lastHandledAt`                     | The most recent cron slot whose Job finished — including a *yield* (which doesn't move `lastRunAt`), so a handled slot never re-fires.        |
| `quick.lastContentReclaimedBytes` / `full.lastContentReclaimedBytes` | Storage reclaimed — **the only place this is surfaced.**                                                                                      |
| `conditions[type=LeaseOwned]`                                        | `True` when this resource holds the lease and is running; `False` (with a reason) when waiting on the repository, a held lease, or a failure. |

The running mover Jobs are labeled, so you can watch them directly:

```console
$ kubectl get jobs -n billing -l app.kubernetes.io/component=maintenance
```

/// note | Reclaimed bytes currently reports 0

`kopia maintenance run` does not emit a machine-readable reclaimed-bytes figure, so `lastContentReclaimedBytes` is reported as `0` today even though the run does reclaim space. The field exists and round-trips; populating it precisely is a planned enhancement.

///

## Behavior you can rely on

- **Runs at the scheduled time.** Spawning is gated on the same cron + jitter logic as `SnapshotSchedule`, seeded deterministically per resource, so two replicas agree and the run lands in its window — it is not "every reconcile".
- **Waits for the repository.** Maintenance only runs once the target repository reports `Ready` (an object-store repository must finish connecting or being created first). Until then the resource shows `LeaseOwned=False, reason=WaitingForRepository`.
- **One run at a time.** The operator never starts a second maintenance Job for a repository while one is in flight.
- **Catches up after downtime — once.** If the operator is down across several scheduled slots, it runs a single catch-up pass on recovery, not a storm of missed runs.
- **Self-cleaning Jobs.** Finished maintenance Jobs are removed automatically (`ttlSecondsAfterFinished`); a failed run is retried with backoff.
- **A handled slot never re-fires.** Each scheduled slot runs once — its outcome (a real run *or* a deliberate yield to a foreign lease holder) is recorded durably in `status.<quick|full>.lastHandledAt`, so the Job self-cleanup above cannot make the same slot run again. Only a *failed* slot is retried.
- **Yielding is loud, not silent.** When every run yields (a foreign owner holds the lease and `takeoverPolicy: Never`), kopia GC/compaction is **not** happening — the resource reports `Ready=False`, reason `MaintenanceYielding`, with the `takeoverPolicy: Force` remediation in the message, instead of a misleading `Ready=True`.

## See also

- [`deploy/examples/08-maintenance.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/08-maintenance.yaml) — a standalone `Maintenance`.
- [`deploy/examples/01-single-pvc-scheduled.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/01-single-pvc-scheduled.yaml) — inline `spec.maintenance`.
- [ADR-0003 §4.5](adr/0003-kopiur-rust-operator.md) — the design rationale.
