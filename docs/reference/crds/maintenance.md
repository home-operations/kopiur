# Maintenance

A `Maintenance` schedules `kopia maintenance run`, both quick and full, for one repository. It also manages the ownership lease that keeps two operators from maintaining the same repository at once.

For the short type-and-default table see the [field reference](../../field-reference.md). For task guidance see [Maintenance](../../maintenance.md).

Maintenance is **managed by default**. When a [`Repository`](repository.md) or [`ClusterRepository`](cluster-repository.md) leaves `spec.maintenance` out, or sets `enabled: true`, the repository reconciler creates a `Maintenance` child object it owns. That way kopia storage is reclaimed without you writing a separate object.

A `Maintenance` you write yourself and point at the repository is always honored, even with `enabled: false`. That setting only tells the operator not to create its own; the operator never deletes, ignores or warns about one you manage. At most one `Maintenance` may own a repository at a time.

## `spec`

### `repository`

The `Repository` or `ClusterRepository` this maintenance run targets. Credentials and connect details come from it.

### `schedule`

Two crons, evaluated in a shared `timezone`. The timezone is an IANA zone name; leaving it out uses the controller default.

#### `schedule.quick`

Cron and jitter for `kopia maintenance run`, the cheap pass that does index and log work. The managed default is every 6 hours with 30 minutes of jitter.

#### `schedule.full`

Cron and jitter for `kopia maintenance run --full`, the heavier pass that reclaims unreferenced content. The managed default is daily at 03:00 with 1 hour of jitter.

### `ownership`

The maintenance lease. At most one `Maintenance` may own a repository at a time, so a run never collides with another operator's.

#### `ownership.owner`

A stable identity for the lease holder, such as `kopia-operator/nas-primary`. When two `Maintenance` objects claim the same repository, they compare this string to decide who holds the lease.

Keep it stable. The pod's own identity is not, because it gets a new hostname every run, so the operator stamps a stable kopia client identity derived from the lease instead.

#### `ownership.takeoverPolicy`

What to do when a *different* owner already holds the lease. A free lease, or one this object already owns, is always claimed again whatever the policy says. When another owner holds it:

- **`Never`** is the default and the safest. Do not seize the lease. The run yields, and a condition records `LeaseHeldByOther`.
- **`PromptCondition`** does not seize the lease either, but raises a `LeaseTakeoverPrompt` condition so a human can decide whether to escalate to `Force`.
- **`Force`** claims the lease from the current holder.

A run that yields still completes cleanly and its Job succeeds, but no maintenance runs. Check the `LeaseOwned` condition to tell a real run from a yield.

### `mover`

Overrides for the maintenance run's mover Job pod: resources, scheduling and security context. Object-store repositories usually tune this.

### `failurePolicy`

How a failed maintenance run is retried and bounded, through a backoff limit and an active deadline.

### `credentialProjection`

Opt-in and off by default. With `enabled: true`, the operator copies the referenced repository's credential Secrets into the namespace this `Maintenance` runs in. It does nothing when they already live there. Use it when you maintain a shared `ClusterRepository` from a namespace that lacks the Secret.

## Inline form on a repository (`spec.maintenance`)

A `Repository` or `ClusterRepository` carries `spec.maintenance` to control the `Maintenance` the operator manages for it:

- **`enabled`** decides whether the operator manages a `Maintenance` object for this repository. It defaults to `true`.
- **`schedule`** overrides the schedule. Leaving it out uses the quick-every-6h and full-daily default.
- **`mover`** and **`failurePolicy`** override those parts of the managed run.
- **`takeoverPolicy`** sets the lease takeover policy for the managed run, defaulting to `Never`.
- **`namespace`** applies to a *ClusterRepository only*. It is the namespace the managed `Maintenance` object is created in, defaulting to the operator's own namespace. It is forbidden on a namespaced `Repository`, whose `Maintenance` always lives in the repository's namespace, and the admission webhook rejects it there.

## Out-of-band runs

To trigger a one-off run, annotate a `Maintenance` with `kopiur.home-operations.com/run-requested` set to an RFC 3339 timestamp. Optionally add `kopiur.home-operations.com/run-mode` set to `quick` or `full`; it defaults to `quick`.

The timestamp identifies *which* request the status answers, so re-applying the same value does nothing and a new timestamp starts a new run.

## `status`

### `ownership`

The current lease holder in `owner`, and the RFC 3339 instant it was `claimedAt`. Both appear once the lease is claimed.

### `quick` / `full`

Run status per kind:

| Field | Meaning |
| --- | --- |
| `lastRunAt` | RFC 3339 instant of the most recent run of this kind. |
| `nextScheduledAt` | RFC 3339 instant of the next scheduled run, with cron and jitter already applied. |
| `lastHandledAt` | RFC 3339 instant the controller last saw this kind's per-slot Job succeed. It is set whether the run maintained the repository or only yielded the lease, so a yielded slot never re-fires endlessly. |
| `consecutiveFailures` | How many runs of this kind failed back to back. It resets on success. |
| `lastContentReclaimedBytes` | Bytes of storage reclaimed by the most recent run of this kind. |

### `manualRun`

The state of the most recent annotation-requested run: the `requestedAt` value it answers, the `mode` it performed, its `phase` (`Running`, `Succeeded` or `Failed`), and the `completedAt` instant it reached a terminal phase. It is absent until a run is requested.

### others

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` this status reflects, for staleness detection. |
| `conditions` | Standard Kubernetes conditions for maintenance health, including `LeaseOwned`. |
