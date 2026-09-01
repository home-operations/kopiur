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

When **absent**, the schedule inherits its target policy's repository [`scheduleDefaults.jitter`](repository.md#scheduledefaults), resolved at slot-computation time exactly as `timezone` is (following `policyRef` or each `policySelector` match). Unlike `timezone` there is no built-in fallback: absent at both levels means no spread. The resolved window is recorded in [`status.nextSchedule.jitter`](#status), and a change at either level re-triggers the schedule (referent watch) and recomputes the pinned slot in the new window. Matched policies whose repositories **disagree** on the window resolve to no jitter and log the candidate windows, recommending an explicit `schedule.jitter`.

/// warning | Capped at 24h, at admission only

A window over 24h is rejected by the webhook (`jitter of 25h exceeds the 24h maximum`) — jitter is a spread *within* a cron period, not a schedule offset. The rule is **admission-only**: it tightens a field that already shipped, so a stored schedule carrying an over-cap window keeps reconciling rather than being bricked by an upgrade. The next apply that touches it must satisfy the cap. Same treatment for the verification, maintenance and replication jitter windows.

///

#### `schedule.timezone`

The IANA timezone the cron is evaluated in (e.g. `America/Los_Angeles`). When set,
it wins outright. When **absent**, the schedule inherits its target policy's
repository [`scheduleDefaults.timezone`](repository.md#scheduledefaults) (resolved
at slot-computation time, following `policyRef` or each `policySelector` match),
else UTC. The resolved zone is recorded in [`status.nextSchedule.timezone`](#status);
a change to the repository default re-triggers the schedule (referent watch) and
recomputes the pinned slot. A `policySelector` schedule whose matched policies'
repositories disagree on the zone falls back to UTC and raises a
`TimezoneDefaultAmbiguous` condition recommending an explicit `schedule.timezone`.

#### `schedule.runOnCreate`

Whether to fire immediately when the schedule is created. Defaults to `false` — the GitOps-friendly choice, so applying a manifest doesn't trigger an unexpected backup. This default materializes into the stored object and `kubectl explain`.

#### `schedule.suspend`

When `true`, skip future firings. In-flight and completed runs are untouched.

#### `schedule.concurrencyPolicy`

What to do when a slot fires while a prior run from this schedule is still in flight:

- **`Forbid`** (default) — skip the new run and surface a condition, rather than let runs pile up.
- **`Allow`** — start the new run alongside the in-flight one.
- **`Replace`** — cancel the in-flight run(s) and start the new one in their place.

This default also materializes into the stored object and `kubectl explain`.

##### What `Replace` actually does

When a slot comes due and this schedule still has unfinished children (phase
`Pending`, `Running`, or not yet stamped), the controller — in this order, per
victim — deletes the run's **mover Job** (stopping the pod deterministically,
rather than racing ownership garbage collection), then annotates the `Snapshot`
CR with `kopiur.home-operations.com/pruned-by: replaced-run` and deletes it.
Only then does it mint the new slot's `Snapshot`, in the same reconcile. One
Normal event, `ReplacedActiveRun`, lists every run that was cancelled.

The `replaced-run` stamp marks the deletion as an **operator prune**, so it is
exempt from the repository's [mass-deletion breaker](repository.md) — without
it, every `Replace` fire would look like an external mass deletion and a busy
schedule would trip its own breaker.

/// warning | What is and isn't reclaimed

`deletionPolicy` governs **committed** kopia snapshots. A run cancelled mid-flight
has not committed one (`status.snapshot` is unset), so there is nothing for the
finalizer to delete in the repository — the CR is simply released. What the
killed mover had already written (data blobs, and an incomplete manifest if it
checkpointed) is reclaimed by kopia's blob garbage collection during
[maintenance](maintenance.md), not by the finalizer.

Each victim is re-read live and skipped if it has already finished, so the
selection cannot cancel a completed backup. In the residual sub-millisecond race
where a run commits its snapshot as the delete lands, that snapshot is
deliberately **kept**: you asked to cancel an in-flight run, not to destroy a
finished backup. It then exists as an unreferenced kopia snapshot that Kopiur no
longer tracks — and reclaiming it is **not** automatic. Nothing re-scans the
repository on a timer unless [`catalog.periodicRefresh`](repository.md) is
enabled (it is off by default); otherwise the catalog is rescanned on a
repository spec change (re-bootstrap), a failure re-probe, or an explicit
on-demand scan request. Only after such a scan does the snapshot reappear as a
`Discovered` row that adoption and GFS retention can govern.

///

Two situations make `Replace` decline to replace anything, and in both it waits
rather than firing:

- **A child at an unrecognized phase.** If a run sits at a `status.phase` this
  operator build does not know — almost always a newer operator wrote it — the
  schedule refuses to delete what it cannot classify and instead raises
  `ScheduleRunnable=False` with reason `BlockedOnUnreadableRun`, exactly as
  `Forbid` does. Finish the operator upgrade, or delete that `Snapshot` if the
  run is genuinely over.
- **A child parked behind the repository's concurrency cap.** A run holding
  `RepositorySlotAvailable=False` is *queued*, not running. Cancelling it would
  free no capacity and the replacement would immediately queue in its place, so
  `Replace` degrades to `Forbid`-like behavior until the pool drains. The
  schedule records `ReplacementHeld=True` (reason `WaitingForRepositorySlot`)
  and emits one `WaitingForRepositorySlot` Normal event on entering the hold —
  not one per retry. Unlike `ScheduleRunnable=False` this is not a structural
  gate: it clears on its own and needs no action.

#### `schedule.startingDeadlineSeconds`

If a slot is missed by more than this many seconds (e.g. the operator was down), skip it instead of firing late. Omit the field for no deadline; `0` (fire only exactly on time) is legitimate.

/// warning | Must be `>= 0`, and it interacts badly with a concurrency cap

A **negative** deadline is not "no deadline". The miss check is `now - slot > deadline`, so a negative value marks every slot expired the instant it fires: the schedule then skips every run forever while reporting itself perfectly healthy. The webhook rejects it — [admission-only](../../upgrade.md#admission-only-jitter-and-deadline-rules-re-apply-only), like the jitter cap, so a stored schedule keeps reconciling (badly, but visibly) until it is re-applied.

Separately: a deadline does not know *why* a slot went unfired. Slots held by `Forbid` behind a run [queued on the repository's concurrency cap](../../backups.md#limiting-concurrent-jobs-per-repository) — or by `ReplacementHeld` — keep aging, and any that ages past the deadline is permanently skipped (`SkipExpiredSlot`), not deferred. Combining a cap with a short deadline turns a throughput limit into dropped runs.

///

### `failedJobsHistoryLimit`

The maximum number of *failed* `Snapshot` CRs from this schedule to retain (default `10`; `0` keeps none). The oldest failures beyond the limit are pruned each reconcile (newest kept, by completion time) — bounding failure history, including backups held back by a [`preflight`](snapshot-policy.md#preflight) check. There is deliberately **no** `successfulJobsHistoryLimit`: retention of successful snapshots is GFS-driven on the [`SnapshotPolicy`](snapshot-policy.md)'s `retention` block, not a flat count.

## `status`

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` this status reflects, for staleness detection. |
| `lastSchedule` | The most recent firing (cron + jitter, pinned), and the `Snapshot` it produced. |
| `nextSchedule` | The next firing slot the controller has computed, plus the `timezone` and `jitter` it was computed with (so a change to either can invalidate and recompute the pinned slot). |
| `lastSuccessfulSchedule` | The most recent firing whose `Snapshot` succeeded. |
| `consecutiveFailures` | Count of back-to-back failed runs; resets on success. Drives alerting. |
| `conditions` | Standard Kubernetes conditions surfacing schedule health. |

Each schedule slot is recorded as an `at` (the RFC3339 instant it fired or is scheduled to) plus an optional `snapshotRef` naming the `Snapshot` CR that slot produced. All three slots share one schema, so `timezone` and `jitter` appear on each — but the controller only ever **writes** them on `nextSchedule`: they describe a pin it may still have to invalidate, not a record of a slot that already fired. If the schedule's effective timezone or jitter later changes (a `schedule.timezone`/`schedule.jitter` edit, or an inherited repository `scheduleDefaults` change), the controller detects the mismatch and recomputes the pinned slot in the new zone/window. An absent recorded value is treated as "unchanged", so a pin written by an older operator is never churned on upgrade.
