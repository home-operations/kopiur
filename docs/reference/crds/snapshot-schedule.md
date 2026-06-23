# SnapshotSchedule

The cron schedule that fires [`Snapshot`](snapshot.md) CRs from a [`SnapshotPolicy`](snapshot-policy.md) — it decides *when* a backup runs, separate from the *what* (the policy) and the *one run* (the Snapshot). For the terse type/default table see the [field reference](../../field-reference.md); for how-to guidance see [Backups & schedules](../../backups.md).

Each firing creates `Snapshot` CRs in the schedule's own namespace. Suspending or deleting a schedule does not affect in-flight or already-completed runs.

## `spec`

Exactly one of `policyRef` or `policySelector` is required (enforced by both the admission webhook and an apiserver CEL validation).

### `policyRef`

The single `SnapshotPolicy` recipe this schedule invokes, resolved in the schedule's own namespace. Mutually exclusive with `policySelector`.

### `policySelector`

The fan-out form: a label selector over `SnapshotPolicy` objects in the schedule's namespace. Each matching policy gets its own `Snapshot` per firing — "back up everything tagged `tier=critical` nightly" expressed as one object. Mutually exclusive with `policyRef`, and mirrors the `pvcSelector` pattern used elsewhere.

### `schedule`

The firing cadence. Its sub-fields:

#### `schedule.cron`

The cron expression, with Jenkins-style `H` substitution — `H` picks a deterministic per-object slot within the field's range so identical schedules don't stampede the same minute.

#### `schedule.jitter`

A deterministic offset (Go-style duration, e.g. `30m`) added to each firing, derived from `(scheduleUID, slot)` so it's stable across restarts rather than random.

#### `schedule.timezone`

The IANA timezone the cron is evaluated in (e.g. `America/Los_Angeles`). Absent uses the controller's configured default.

#### `schedule.runOnCreate`

Whether to fire immediately when the schedule is created. Defaults to `false` — the GitOps-friendly choice, so applying a manifest doesn't trigger an unexpected backup. This default materializes into the stored object and `kubectl explain`.

#### `schedule.suspend`

When `true`, skip future firings. In-flight and completed runs are untouched.

#### `schedule.concurrencyPolicy`

What to do when a slot fires while a prior run from this schedule is still in flight:

- **`Forbid`** (default) — skip the new run and surface a condition, rather than let runs pile up.
- **`Allow`** — start the new run alongside the in-flight one.
- **`Replace`** — cancel the in-flight run and start the new one in its place.

This default also materializes into the stored object and `kubectl explain`.

#### `schedule.startingDeadlineSeconds`

If a slot is missed by more than this many seconds (e.g. the operator was down), skip it instead of firing late.

### `failedJobsHistoryLimit`

The maximum number of *failed* `Snapshot` CRs from this schedule to retain (default `10`; `0` keeps none). The oldest failures beyond the limit are pruned each reconcile (newest kept, by completion time) — bounding failure history, including backups held back by a [`preflight`](snapshot-policy.md#preflight) check. There is deliberately **no** `successfulJobsHistoryLimit`: retention of successful snapshots is GFS-driven on the [`SnapshotPolicy`](snapshot-policy.md)'s `retention` block, not a flat count.

## `status`

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` this status reflects, for staleness detection. |
| `lastSchedule` | The most recent firing (cron + jitter, pinned), and the `Snapshot` it produced. |
| `nextSchedule` | The next firing slot the controller has computed. |
| `lastSuccessfulSchedule` | The most recent firing whose `Snapshot` succeeded. |
| `consecutiveFailures` | Count of back-to-back failed runs; resets on success. Drives alerting. |
| `conditions` | Standard Kubernetes conditions surfacing schedule health. |

Each schedule slot is recorded as an `at` (the RFC3339 instant it fired or is scheduled to) plus an optional `snapshotRef` naming the `Snapshot` CR that slot produced.
