# Snapshot

A single kopia snapshot represented as a Kubernetes object — one backup invocation. For the terse type/default table see the [field reference](../../field-reference.md); for how-to guidance see [Backups & schedules](../../backups.md).

A `Snapshot` comes to exist in one of three ways, recorded in `status.origin`:

- **`scheduled`** — created by a [`SnapshotSchedule`](snapshot-schedule.md); its spec carries `policyRef`.
- **`manual`** — created by `kubectl create` or external automation; its spec carries `policyRef`.
- **`discovered`** — materialized by the catalog scan for a kopia snapshot Kopiur did not produce. **A discovered Snapshot has an empty spec** — every spec field is optional and absent — and its data lives entirely in `status`.

## `spec`

For `scheduled`/`manual` backups the spec carries `policyRef` plus optional per-run overrides. For `discovered` backups the spec is empty.

### `policyRef`

The [`SnapshotPolicy`](snapshot-policy.md) recipe to run — what to back up. Absent for `discovered` snapshots, which Kopiur did not create.

### `tags`

Arbitrary key/value tags applied to the kopia snapshot (e.g. `reason: scheduled-nightly`). They flow through to kopia and are visible when listing snapshots.

### `failurePolicy`

Mover Job retry and deadline limits for this run — how many times the mover Job retries, and the deadlines that bound a single attempt and overall execution. See [Movers](../../movers.md).

### `deletionPolicy`

What happens to the underlying kopia snapshot when this CR is deleted. The default is origin-aware: scheduled/manual snapshots default to deleting the kopia content, while `discovered` snapshots are forced to retain it (Kopiur did not create that data, so it never reclaims it). Set `Orphan` to drop the CR without touching kopia. See [Backups & schedules](../../backups.md).

### `pin`

Exempt this snapshot from GFS retention. When `true` the reconciler applies a kopia snapshot pin and the GFS pruner never selects it for deletion — useful for pre-migration or compliance holds. Clearing it removes the pin. Defaults to `false`.

### `description`

Free-form text recorded on the kopia snapshot manifest (`snapshot create --description`), up to 1024 characters. Per-invocation by nature — a `SnapshotSchedule`'s children and `discovered` backups never set this; use it on a manual `Snapshot` (or `kubectl kopiur snapshot now --description`) to annotate one-off runs, e.g. `pre-upgrade snapshot`.

## `status`

The status is rich; `discovered` snapshots populate it without any spec.

### `phase`

Current lifecycle phase — one of `Pending` (admitted, not started), `Running` (mover Job in flight), `Succeeded`, `Failed` (retries exhausted), `Deleting` (finalizer reclaiming the kopia snapshot), or `Discovered` (catalog-materialized).

### `origin`

The canonical origin (`scheduled`, `manual`, or `discovered`), also mirrored to the `kopiur.home-operations.com/origin` label. Origin drives the `deletionPolicy` default.

### `snapshot`

Identifies the kopia snapshot this CR owns: `kopiaSnapshotID` (the handle the finalizer uses to delete content) and the resolved `username@hostname:path` `identity` recorded for it.

### `stats`

Byte and file counts parsed from kopia's JSON output: `sizeBytes`, `bytesNew` (uploaded after dedup/compression), `filesNew`, `filesModified`, `filesUnchanged`.

#### `stats.filesFailed`

The count of source entries kopia could **not** read and therefore **excluded** from the snapshot, making the backup *incomplete*. This is present (and `> 0`) only when an `ignoreFileErrors`/`ignoreDirErrors` policy let the snapshot complete despite unreadable files — the otherwise-silent partial-backup case. Kopiur surfaces it as a warning condition and Event. It usually means a UID/GID mismatch between the mover and the workload; see [Security context](../../security-context.md).

### `conditions`

Standard Kubernetes conditions surfacing run health (e.g. `SourcesQuiesced`, `SnapshotCreated`). Use these for kstatus-based readiness checks.

### `logTail`

The last lines of the run's output, written by the mover at the terminal transition — on success the `Snapshot created: <id>` line, on failure the actionable error plus a kopia stderr tail. It is capped in size; the full logs live in the mover Job's pod.

### `failure`

Structured terminal-failure detail (kopia error class, stderr tail, retry hint), written by the mover before it exits non-zero. Read this first when a `Snapshot` lands in `Failed`. See [Troubleshooting](../../troubleshooting.md).

### `staged`

The CSI staging objects the run created when the source was captured via `copyMethod: Snapshot`/`Clone` (the `VolumeSnapshot` and/or staged `PVC` the mover mounted in place of the live source, and whether the stage is `ready`). Absent for `Direct` and NFS, which mount the live source with no staging. See [Copy methods](../../copy-methods.md).

### Other status fields

| Field | Meaning |
| --- | --- |
| `observedGeneration` | `metadata.generation` last reconciled, for staleness detection. |
| `timing` | `startTime`, `endTime`, `durationSeconds` of the run. |
| `job` | The mover Job (`name`, `attempts`) backing a scheduled/manual run; absent for discovered. |
| `resolved` | Frozen recipe values pinned at run time — the `repository` targeted, the concrete `sources` (PVCs + kopia paths) backed up, and the `credentialProjection` opt-in that was in force. These exist so the cleanup finalizer keeps working after the `SnapshotPolicy` is deleted: it needs to reach the repository to delete the kopia snapshot, and the recipe is where all three normally live. Absent `credentialProjection` means the run predates the pin, not that projection was off. |
| `pinned` | The observed kopia-side pin state: `true` if pinned, `false` if unpinned, absent before any pin reconcile. |
| `hooks` | Completion timestamps so each hook list (`beforeSnapshot`/`afterSnapshot`) runs exactly once per Snapshot. |
