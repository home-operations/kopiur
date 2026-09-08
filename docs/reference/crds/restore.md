# Restore

A `Restore` restores data from a repository into a PVC. For the short type-and-default table see the [field reference](../../field-reference.md). For task guidance see [Restores](../../restores.md).

A `Restore` names three things: where to read data **from** (`source`), where to write it **to** (`target`), and what to do when the source snapshot does not exist yet (`policy`). Exactly one `source` mode and exactly one `target` mode are set at a time.

The source is resolved once at admission and pinned to status, so a restore never quietly retargets a snapshot that appears later.

## `spec`

### `source`

Where to read data from. It is a single-key object with exactly three modes:

- **`snapshotRef`** references an existing `Snapshot` object by `name` and optional `namespace`. It works for scheduled, manual and discovered snapshots, which are all the same kind. This is the mode where you point at one specific backup.
- **`fromPolicy`** names a `SnapshotPolicy`, with an optional `namespace`. The restore resolves a snapshot through the policy's identity even when **no** `Snapshot` object exists yet, which is the deploy-or-restore pattern. Pick the snapshot with `offset`, where `0` is the latest, `1` the previous, and so on, defaulting to `0`. Or restore the newest snapshot at or before a point in time with `asOf`, written as RFC 3339.
- **`identity`** is a raw kopia identity, for foreign writers or for snapshots that aged out of the catalog. Give `username` and `hostname`, both required, and optionally `sourcePath`; leaving `sourcePath` out matches any path. Select the snapshot with `snapshotID` (an exact manifest id), `asOf`, or `offset`. The `identity` source **requires** `spec.repository`, because there is no `Snapshot` or `SnapshotPolicy` to derive the repository from.

### `target`

Where to write the restored data. It is **required**, and is a single-key object with exactly three modes:

- **`pvc`** has the operator create the PVC from a template: `name` plus optional `storageClassName`, `capacity` and `accessModes`.
- **`pvcRef`** writes into an existing PVC by `name`, with an optional `namespace`.
- **`populator`** is passive populator mode, written as an empty object, `populator: {}`. There is no workload target when the volume is provisioned; a separate PVC claims the restore through its `spec.dataSourceRef`. It is an empty sub-object today so future populator settings can be added without breaking the API.

A `Restore` with no `target` is invalid and fails admission. `inheritSecurityContextFrom`, under `mover`, means nothing with a `populator` target, because there is no workload pod to copy a security context from, and the validator rejects it.

### `repository`

The repository to read from. It is derived from `source` when you leave it out, and is **required only** with `source.identity`, which has no object to derive it from.

### `options`

Knobs for kopia's restore behavior. Every field defaults to kopia's own default, so leaving one unset lets kopia decide, and leaving the whole `options` block out reproduces plain, additive `kopia snapshot restore` behavior.

The three main ones:

- `enableFileDeletion` deletes files in the target that are absent from the snapshot, making the target an exact mirror. It is off by default, so restores are additive and safe. It drives kopia's `--delete-extra`.
- `ignorePermissionErrors` continues past permission errors. It defaults to true.
- `writeFilesAtomically` writes to a temp file and renames. It defaults to true.

The rest map one-to-one onto `kopia snapshot restore`'s own flags. All of them are three-state booleans (`true`, `false` or absent) except `parallel`: `parallel` (restore parallelism; kopia's default is `8`), `writeSparseFiles`, `skipOwners`, `skipPermissions`, `skipTimes`, `overwriteFiles`, `overwriteDirectories`, `overwriteSymlinks`, `ignoreErrors` and `skipExisting`. See the [field reference](../../field-reference.md) for kopia's default per flag.

### `policy`

How the restore reacts to a missing snapshot.

`onMissingSnapshot` is either `Fail`, which fails closed and is the default for explicit `snapshotRef` and `identity` sources so an explicit restore can never quietly do nothing, or `Continue`, which proceeds with an empty deploy-or-restore volume and is the default for `fromPolicy`.

`waitTimeout`, `5m` for example, bounds how long the restore waits for the source snapshot to appear before giving up. The window opens when the restore can first proceed, meaning its repository is `Ready`, and, for `target.populator`, a PVC has claimed it. That instant is recorded in `status.waitStartedAt`, and it is **not** measured from `metadata.creationTimestamp`.

The window does not open at all while the referenced `Repository` object or the `fromPolicy` `SnapshotPolicy` is missing, and the restore parks at `Pending` with `RestoreReferentMissing`. It does still open for a `snapshotRef` whose `Snapshot` row has not appeared yet, so `onMissingSnapshot` can fire for a reference that never resolves. See [Restores → `waitTimeout`](../../restores.md#waittimeout--wait-before-giving-up).

### `mover`

Per-run mover Job overrides for this restore: resource requests and limits, kopia cache sizing, and the container and pod `securityContext` used to match the workload's UID and GID. It also carries `fsGroup`, which makes a fresh restore volume group-writable.

This is the same surface a backup gets through `SnapshotPolicy.spec.mover`. An elevated context is gated per namespace exactly as a backup's is, and `securityContext` combines with `inheritSecurityContextFrom`: an explicit field wins field by field, and it is also the fallback when no workload pod resolves. See [Security context](../../security-context.md).

### `credentialProjection`

Opt-in and off by default. With `enabled: true`, the operator copies the referenced repository's credential Secrets into the restore mover's namespace, and does nothing when they already live there. That way restoring from a shared `ClusterRepository` into a fresh namespace does not need the Secret created there first.

### `failurePolicy`

Retry and deadline limits for the mover Job, `backoffLimit` and `activeDeadlineSeconds`, mirroring `Snapshot.spec.failurePolicy`. See [Movers](../../movers.md).

## `status`

### `resolved`

The source as resolved and **pinned at admission**, never resolved again. That is what makes a restore deterministic.

`resolution` records the pinned outcome, either `Snapshot`, meaning the source resolved to a concrete kopia snapshot, or `NoSnapshot`, meaning the source matched nothing and `onMissingSnapshot: Continue` chose an empty deploy-or-restore volume. Because the outcome is pinned once, a snapshot that appears later can never quietly retarget a volume that is already provisioned.

The block also carries `kopiaSnapshotID`, the exact manifest id, which is restored on every later reconcile even if newer snapshots appear; `snapshotRef`, the concrete `Snapshot` object where that applies; `repository`; `pinnedAt`, in RFC 3339; and the resolved `identity` as `username@hostname:path`.

### `target`

Resolved target detail: `pvcRef` is the PVC actually written to, whether created or pre-existing, and `pvcPrime` is the populator handshake for the passive and pvc-create modes.

### `failure`

Structured detail about a terminal failure: the kopia error class, a stderr tail, and a retry hint. The mover writes it before exiting non-zero. Read this first when a `Restore` lands in `Failed`.

### Other status fields

| Field | Meaning |
| --- | --- |
| `phase` | Current lifecycle phase: `Pending`, `Resolving`, `Restoring`, `Completed`, or `Failed`. |
| `sourceKind` | The pinned source kind (`SnapshotRef`/`FromPolicy`/`Identity`), behind the `SOURCE` printer column. |
| `observedGeneration` | `metadata.generation` last reconciled, for staleness detection. |
| `timing` | `startTime`/`endTime` (RFC 3339) of the restore run. |
| `progress` | Live `bytesRestored`/`filesRestored` counters, patched periodically by the mover. |
| `conditions` | Standard Kubernetes conditions carrying the human-readable status and reason. |
| `logTail` | The last lines of the run's output, written by the mover when it reaches a terminal state; capped in size, with full logs in the Job pod. |
