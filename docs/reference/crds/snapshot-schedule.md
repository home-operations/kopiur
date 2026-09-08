# SnapshotSchedule

A `SnapshotSchedule` is the cron that fires [`Snapshot`](snapshot.md) objects from a [`SnapshotPolicy`](snapshot-policy.md). It decides *when* a backup runs, separately from *what* runs (the policy) and from *one run* (the Snapshot).

For the short type-and-default table see the [field reference](../../field-reference.md). For task guidance see [Backups & schedules](../../backups.md).

Each firing creates `Snapshot` objects in the schedule's own namespace. Suspending or deleting a schedule does not affect runs that are in flight or already finished.

## `spec`

Set exactly one of `policyRef` or `policySelector`. Both the admission webhook and an apiserver CEL rule enforce that.

### `policyRef`

The single `SnapshotPolicy` recipe this schedule invokes, resolved in the schedule's own namespace. It is mutually exclusive with `policySelector`.

### `policySelector`

The fan-out form: a label selector over `SnapshotPolicy` objects in the schedule's namespace. Every matching policy gets its own `Snapshot` on each firing, so "back up everything tagged `tier=critical` nightly" is one object. It is mutually exclusive with `policyRef`, and mirrors the `pvcSelector` pattern used elsewhere.

### `schedule`

The firing cadence. Its sub-fields follow.

#### `schedule.cron`

The cron expression, with Jenkins-style `H` substitution. `H` picks a deterministic per-object slot inside the field's range, so identical schedules do not all stampede the same minute.

#### `schedule.jitter`

A deterministic offset added to each firing, written as a Go-style duration such as `30m`. It is derived from the schedule's UID and slot, so it stays the same across restarts rather than being random.

Leave it out and the schedule inherits the [`scheduleDefaults.jitter`](repository.md#scheduledefaults) of its target policy's repository. That is resolved when the slot is computed, exactly as `timezone` is, following `policyRef` or each `policySelector` match.

Unlike `timezone` there is no built-in fallback, so a value missing at both levels means no spread. The resolved window is recorded in [`status.nextSchedule.jitter`](#status). A change at either level re-triggers the schedule through a referent watch and recomputes the pinned slot in the new window.

If matched policies' repositories **disagree** on the window, the schedule resolves to no jitter and logs the candidate windows, recommending an explicit `schedule.jitter`.

/// warning | Capped at 24h, at admission only

A window over 24 hours is rejected by the webhook with `jitter of 25h exceeds the 24h maximum`, because jitter is a spread *inside* a cron period rather than a schedule offset.

The rule applies **at admission only**. It tightens a field that already shipped, so a stored schedule carrying an over-cap window keeps reconciling instead of being bricked by an upgrade. The next apply that touches it must satisfy the cap. The verification, maintenance and replication jitter windows work the same way.

///

#### `schedule.timezone`

The IANA timezone the cron is evaluated in, such as `America/Los_Angeles`. When you set it, it wins outright.

Leave it out and the schedule inherits the [`scheduleDefaults.timezone`](repository.md#scheduledefaults) of its target policy's repository, resolved when the slot is computed, following `policyRef` or each `policySelector` match. If there is none, it uses UTC.

The resolved zone is recorded in [`status.nextSchedule.timezone`](#status). A change to the repository default re-triggers the schedule through a referent watch and recomputes the pinned slot.

If a `policySelector` schedule's matched policies' repositories disagree on the zone, it falls back to UTC and raises a `TimezoneDefaultAmbiguous` condition recommending an explicit `schedule.timezone`.

#### `schedule.runOnCreate`

Whether to fire immediately when the schedule is created. It defaults to `false`, which is the GitOps-friendly choice: applying a manifest does not trigger an unexpected backup. This default is written into the stored object and shows up in `kubectl explain`.

#### `schedule.suspend`

With `true`, skip future firings. Runs in flight and runs already finished are untouched.

#### `schedule.concurrencyPolicy`

What to do when a slot fires while an earlier run from this schedule is still in flight:

- **`Forbid`**, the default, skips the new run and raises a condition, rather than letting runs pile up.
- **`Allow`** starts the new run alongside the in-flight one.
- **`Replace`** cancels the in-flight runs and starts the new one in their place.

This default is also written into the stored object and shows up in `kubectl explain`.

##### What `Replace` actually does

When a slot comes due and this schedule still has unfinished children, meaning children at phase `Pending` or `Running` or not yet stamped, the controller does this for each victim, in this order.

First it deletes the run's **mover Job**, which stops the pod deterministically instead of racing ownership garbage collection. Then it annotates the `Snapshot` object with `kopiur.home-operations.com/pruned-by: replaced-run` and deletes it. Only then does it mint the new slot's `Snapshot`, in the same reconcile. One Normal event, `ReplacedActiveRun`, lists every run that was cancelled.

The `replaced-run` stamp marks the deletion as an **operator prune**, which exempts it from the repository's [mass-deletion breaker](repository.md). Without the stamp, every `Replace` firing would look like an external mass deletion, and a busy schedule would trip its own breaker.

/// warning | What is and isn't reclaimed

`deletionPolicy` governs **committed** kopia snapshots. A run cancelled mid-flight has not committed one, so `status.snapshot` is unset and there is nothing for the finalizer to delete in the repository. The object is simply released.

What the killed mover had already written, meaning data blobs and an incomplete manifest if it checkpointed, is reclaimed by kopia's blob garbage collection during [maintenance](maintenance.md), not by the finalizer.

Each victim is re-read live and skipped if it has already finished, so the selection cannot cancel a completed backup. In the remaining sub-millisecond race, where a run commits its snapshot just as the delete lands, that snapshot is deliberately **kept**: you asked to cancel an in-flight run, not to destroy a finished backup.

The snapshot then exists in the repository with nothing tracking it, and reclaiming it is **not** automatic. Nothing re-scans the repository on a timer unless [`catalog.periodicRefresh`](repository.md) is on, and it is off by default. Otherwise the catalog is rescanned on a repository spec change, which re-bootstraps it, on a failure re-probe, or on an explicit on-demand scan request. Only after such a scan does the snapshot reappear as a `Discovered` row that adoption and GFS retention can govern.

///

Two situations make `Replace` decline to replace anything. In both it waits rather than firing.

- **A child at an unrecognized phase.** If a run sits at a `status.phase` this operator build does not know, which almost always means a newer operator wrote it, the schedule refuses to delete what it cannot classify. It raises `ScheduleRunnable=False` with reason `BlockedOnUnreadableRun`, exactly as `Forbid` does. Finish the operator upgrade, or delete that `Snapshot` if the run is genuinely over.
- **A child parked behind the repository's concurrency cap.** A run holding `RepositorySlotAvailable=False` is queued, not running. Cancelling it would free no capacity and the replacement would immediately queue in its place, so `Replace` behaves like `Forbid` until the pool drains. The schedule records `ReplacementHeld=True` with reason `WaitingForRepositorySlot`, and emits one `WaitingForRepositorySlot` Normal event when it enters the hold, not one per retry. Unlike `ScheduleRunnable=False` this is not a structural gate: it clears on its own and needs no action from you.

#### `schedule.startingDeadlineSeconds`

If a slot is missed by more than this many seconds, for example because the operator was down, skip it instead of firing late. Omit the field for no deadline. `0`, meaning fire only exactly on time, is legitimate.

/// warning | Must be `>= 0`, and it interacts badly with a concurrency cap

A **negative** deadline is not "no deadline". The miss check is `now - slot > deadline`, so a negative value marks every slot expired the instant it fires. The schedule then skips every run forever while reporting itself perfectly healthy. The webhook rejects it, [at admission only](../../upgrade.md#admission-only-jitter-and-deadline-rules-re-apply-only) like the jitter cap, so a stored schedule keeps reconciling, badly but visibly, until it is re-applied.

Separately, a deadline does not know *why* a slot went unfired. Slots held by `Forbid` behind a run [queued on the repository's concurrency cap](../../backups.md#limiting-concurrent-jobs-per-repository), or held by `ReplacementHeld`, keep aging. Any slot that ages past the deadline is permanently skipped with `SkipExpiredSlot` rather than deferred. Combining a cap with a short deadline turns a throughput limit into dropped runs.

///

### `failedJobsHistoryLimit`

How many *failed* `Snapshot` objects from this schedule to keep. The default is `10`, and `0` keeps none.

Each reconcile prunes the oldest failures beyond the limit, keeping the newest by completion time. That bounds failure history, including backups held back by a [`preflight`](snapshot-policy.md#preflight) check.

There is deliberately **no** `successfulJobsHistoryLimit`. Retention of successful snapshots is GFS-driven through the [`SnapshotPolicy`](snapshot-policy.md)'s `retention` block, not a flat count.

## `status`

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` this status reflects, for staleness detection. |
| `lastSchedule` | The most recent firing, with cron and jitter already applied, and the `Snapshot` it produced. |
| `nextSchedule` | The next firing slot the controller computed, plus the `timezone` and `jitter` it was computed with, so a change to either can invalidate and recompute the pinned slot. |
| `lastSuccessfulSchedule` | The most recent firing whose `Snapshot` succeeded. |
| `consecutiveFailures` | How many runs failed back to back; resets on success. Drives alerting. |
| `conditions` | Standard Kubernetes conditions carrying schedule health. |

Each schedule slot is recorded as an `at`, the RFC 3339 instant it fired or is due to fire, plus an optional `snapshotRef` naming the `Snapshot` that slot produced.

All three slots share one schema, so `timezone` and `jitter` appear on each. The controller only ever **writes** them on `nextSchedule`, because they describe a pin it may still have to invalidate rather than a record of a slot that already fired.

If the schedule's effective timezone or jitter later changes, through a `schedule.timezone` or `schedule.jitter` edit or through an inherited repository `scheduleDefaults` change, the controller notices the mismatch and recomputes the pinned slot in the new zone or window. A recorded value that is absent counts as unchanged, so a pin written by an older operator is never churned on upgrade.
