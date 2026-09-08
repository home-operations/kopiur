# Snapshot

A `Snapshot` is one kopia snapshot represented as a Kubernetes object, so one backup invocation. For the short type-and-default table see the [field reference](../../field-reference.md). For task guidance see [Backups & schedules](../../backups.md).

A `Snapshot` comes into being in one of three ways, recorded in `status.origin`:

- **`scheduled`** means a [`SnapshotSchedule`](snapshot-schedule.md) created it, and its spec carries `policyRef`.
- **`manual`** means `kubectl create` or your own automation created it, and its spec carries `policyRef`.
- **`discovered`** means the catalog scan materialized it for a kopia snapshot Kopiur did not produce. **A discovered Snapshot has an empty spec**, because every spec field is optional and absent, and all its data lives in `status`.

## `spec`

For `scheduled` and `manual` backups the spec carries `policyRef` plus optional per-run overrides. For `discovered` backups the spec is empty.

### `policyRef`

The [`SnapshotPolicy`](snapshot-policy.md) recipe to run, which says what to back up. It is absent for `discovered` snapshots, which Kopiur did not create.

### `tags`

Key/value tags applied to the kopia snapshot, such as `reason: scheduled-nightly`. They flow through to kopia and show up when you list snapshots.

### `failurePolicy`

Retry and deadline limits for this run's mover Job: how many times the Job retries, and the deadlines that bound a single attempt and the run overall. See [Movers](../../movers.md).

### `deletionPolicy`

What happens to the underlying kopia snapshot when you delete this object.

The default depends on the origin. Scheduled and manual snapshots default to deleting the kopia content. Discovered snapshots are forced to retain it, because Kopiur did not create that data and never reclaims it. Set `Orphan` to drop the object without touching kopia. See [Backups & schedules](../../backups.md).

### `pin`

Exempt this snapshot from GFS retention. With `true`, the reconciler applies a kopia snapshot pin and the GFS pruner never selects it for deletion. Use it for a pre-migration or compliance hold. Clearing the field removes the pin. It defaults to `false`.

### `description`

Free-form text recorded on the kopia snapshot manifest, through `snapshot create --description`, up to 1024 characters.

It is per-invocation by nature, so a `SnapshotSchedule`'s children and `discovered` backups never set it. Use it on a manual `Snapshot`, or through `kubectl kopiur snapshot now --description`, to annotate one-off runs, for example `pre-upgrade snapshot`.

## `status`

The status is rich, and `discovered` snapshots fill it in without any spec.

### `phase`

The current lifecycle phase, one of:

- `Pending`: admitted, not started.
- `Running`: the mover Job is in flight.
- `Succeeded`.
- `Failed`: retries exhausted.
- `Deleting`: the finalizer is reclaiming the kopia snapshot.
- `Discovered`: the catalog materialized it.
- `Unchanged`: the backup ran, but nothing had changed since the previous snapshot, so kopia created none. See [`files.ignoreIdenticalSnapshots`](snapshot-policy.md#files).

`Unchanged` is a **success**. The source was read and hashed, and the previous snapshot still protects it. It advances the policy's last-backup timestamp and health, and `kubectl wait --for=condition=Ready` passes.

What an `Unchanged` run does *not* have is a `status.snapshot`. There is no new kopia manifest, so you cannot restore *from this object*. Restore from the previous one, which is still the live restore point. It also takes no retention slot, so it can never displace a real restore point.

### `origin`

The canonical origin: `scheduled`, `manual` or `discovered`. It is also mirrored to the `kopiur.home-operations.com/origin` label, and it decides the `deletionPolicy` default.

### `snapshot`

Identifies the kopia snapshot this object owns. `kopiaSnapshotID` is the handle the finalizer uses to delete the content, and `identity` is the resolved `username@hostname:path` recorded for it.

### `stats`

Byte and file counts parsed from kopia's JSON output: `sizeBytes`, `bytesNew` (uploaded after dedup and compression), `filesNew`, `filesModified` and `filesUnchanged`.

#### `stats.filesFailed`

How many source entries kopia could **not** read and therefore left **out** of the snapshot, which makes the backup incomplete.

It is present, and greater than zero, only when an `ignoreFileErrors` or `ignoreDirErrors` policy let the snapshot complete despite unreadable files. That is the partial-backup case that would otherwise be silent, so Kopiur raises a warning condition and an Event for it.

It usually means the mover and the workload disagree on UID or GID. See [Security context](../../security-context.md).

### `conditions`

Standard Kubernetes conditions carrying run health, such as `SourcesQuiesced` and `SnapshotCreated`. Use these for kstatus-based readiness checks.

### `logTail`

The last lines of the run's output, written by the mover when it reaches a terminal state. On success that is the `Snapshot created: <id>` line. On failure it is the actionable error plus a tail of kopia's stderr. It is capped in size; the full logs live in the mover Job's pod.

### `failure`

Structured detail about a terminal failure: the kopia error class, a stderr tail, and a retry hint. The mover writes it before exiting non-zero. Read this first when a `Snapshot` lands in `Failed`. See [Troubleshooting](../../troubleshooting.md).

### `staged`

The CSI staging objects the run created when the source was captured with `copyMethod: Snapshot` or `Clone`. That is the `VolumeSnapshot` and staged `PVC` the mover mounted in place of the live source, plus whether the stage is `ready`.

It is absent for `Direct` and for NFS, which mount the live source with no staging. See [Copy methods](../../copy-methods.md).

### Other status fields

| Field | Meaning |
| --- | --- |
| `observedGeneration` | `metadata.generation` last reconciled, for staleness detection. |
| `timing` | `startTime`, `endTime`, `durationSeconds` of the run. |
| `job` | The mover Job (`name`, `attempts`) behind a scheduled or manual run; absent for discovered. |
| `resolved` | Recipe values pinned when the run started: the `repository` targeted, the concrete `sources` (PVCs plus kopia paths) backed up, and the `credentialProjection` opt-in that was in force. They exist so the cleanup finalizer keeps working after the `SnapshotPolicy` is deleted: it needs to reach the repository to delete the kopia snapshot, and the recipe is where all three normally live. An absent `credentialProjection` means the run predates the pin, not that projection was off. |
| `pinned` | The pin state seen on the kopia side: `true` if pinned, `false` if unpinned, absent before any pin reconcile. |
| `hooks` | Completion timestamps, so each hook list (`beforeSnapshot` and `afterSnapshot`) runs exactly once per Snapshot. |
