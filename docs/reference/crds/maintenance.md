# Maintenance

Schedules `kopia maintenance run` (quick and full) for one repository and manages the ownership lease that keeps two operators from maintaining the same repository at once. For the terse type/default table see the [field reference](../../field-reference.md); for how-to guidance see [Maintenance](../../maintenance.md).

Maintenance is **default-managed**: a [`Repository`](repository.md) or [`ClusterRepository`](cluster-repository.md) with `spec.maintenance` absent (or `enabled: true`) is projected by the repository reconciler into an *owned* `Maintenance` child CR, so kopia storage is reclaimed without you authoring a separate object. An externally-authored `Maintenance` that references the repository is always honored — even with `enabled: false`, which only tells the operator not to create its own; it never deletes, ignores, or warns about a user-managed one. At most one `Maintenance` may own a repository at a time.

## `spec`

### `repository`

The `Repository` or `ClusterRepository` this maintenance run targets. Credentials and connect details are resolved from it.

### `schedule`

Two crons, evaluated in a shared `timezone` (IANA; absent uses the controller default):

#### `schedule.quick`

Cron and jitter for `kopia maintenance run` — the cheap pass that does index and log work. The managed default is every 6h with 30m jitter.

#### `schedule.full`

Cron and jitter for `kopia maintenance run --full` — the heavier pass that reclaims unreferenced content. The managed default is daily at 03:00 with 1h jitter.

### `ownership`

The maintenance lease. At most one `Maintenance` may own a repository at a time, so a run never collides with another operator's.

#### `ownership.owner`

A stable lease-holder identity (e.g. `kopia-operator/nas-primary`). Two `Maintenance` CRs claiming the same repository compare this string to decide who holds the lease. Keep it stable — the pod's own identity is ephemeral (a new hostname every run), so the operator stamps a stable kopia client identity derived from the lease.

#### `ownership.takeoverPolicy`

What to do when the lease is already held by a *different* owner. A free or already-owned lease is always (re)claimed regardless of policy. When another owner holds it:

- **`Never`** (default, safest) — do not seize the lease; the run yields and a condition records `LeaseHeldByOther`.
- **`PromptCondition`** — do not seize, but surface a `LeaseTakeoverPrompt` condition so an operator can decide whether to escalate to `Force`.
- **`Force`** — forcibly claim the lease from the current holder.

A yielded run completes cleanly (its Job succeeds) without running maintenance; check the `LeaseOwned` condition to tell a real run from a yield.

### `mover`

Mover (Job pod) overrides for the maintenance run — resources, scheduling, security context. Object-store repositories typically tune this.

### `failurePolicy`

How a failed maintenance run is retried and bounded (backoff limit, active deadline).

### `credentialProjection`

Opt-in (default off). When `enabled: true`, the operator copies the referenced repository's credential Secret(s) into the namespace this `Maintenance` runs in — a no-op when they already live there. Useful when maintaining a shared `ClusterRepository` from a namespace that lacks the Secret.

## Inline form on a repository (`spec.maintenance`)

A `Repository`/`ClusterRepository` carries `spec.maintenance` to control its default-managed `Maintenance`:

- **`enabled`** — whether the operator manages a `Maintenance` CR for this repository (default `true`).
- **`schedule`** — schedule override; absent uses the quick-6h / full-daily default.
- **`mover`** / **`failurePolicy`** — overrides for the managed run.
- **`takeoverPolicy`** — lease takeover policy for the managed run (default `Never`).
- **`namespace`** — *ClusterRepository only* — the namespace the managed (namespaced) `Maintenance` CR is created in, defaulting to the operator's own namespace. Forbidden on a namespaced `Repository` (whose `Maintenance` always lives in the repository's namespace) and rejected by the admission webhook.

## Out-of-band runs

Annotating a `Maintenance` with `kopiur.home-operations.com/run-requested` (an RFC3339 timestamp) and optionally `…/run-mode` (`quick` or `full`, defaulting to `quick`) triggers a one-off run. The timestamp pins *which* request the status answers, so re-applying the same value is a no-op and a new timestamp starts a new run.

## `status`

### `ownership`

The current lease holder (`owner`) and the RFC3339 instant it was `claimedAt`, present once the lease is claimed.

### `quick` / `full`

Per-kind run status:

| Field | Meaning |
| --- | --- |
| `lastRunAt` | RFC3339 instant of the most recent run of this kind. |
| `nextScheduledAt` | RFC3339 instant of the next scheduled run (cron + jitter, pinned). |
| `lastHandledAt` | RFC3339 instant the controller last observed this kind's per-slot Job reach terminal success — set whether the run maintained or merely yielded the lease, so a yielded slot never re-fires endlessly. |
| `consecutiveFailures` | Count of back-to-back failed runs of this kind; resets on success. |
| `lastContentReclaimedBytes` | Bytes of storage reclaimed by the most recent run of this kind. |

### `manualRun`

State of the most recent annotation-requested run: the `requestedAt` value it answers, the `mode` performed, its `phase` (`Running` / `Succeeded` / `Failed`), and the `completedAt` instant it reached a terminal phase. Absent until a run is requested.

### others

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` this status reflects, for staleness detection. |
| `conditions` | Standard Kubernetes conditions surfacing maintenance health (including `LeaseOwned`). |
