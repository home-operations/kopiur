# Restore

Restore data from a repository into a PVC. For the terse type/default table see the [field reference](../../field-reference.md); for how-to guidance see [Restores](../../restores.md).

A `Restore` names three things: where to read data **from** (`source`), where to write it **to** (`target`), and how to behave when the source snapshot does not exist yet (`policy`). Exactly one `source` mode and exactly one `target` mode are present at a time. The source is resolved once at admission and pinned to status, so a restore never silently retargets a later-appearing snapshot.

## `spec`

### `source`

Where to read data from — exactly one of three externally-tagged modes:

- **`snapshotRef`** — a reference (`name`, optional `namespace`) to an existing `Snapshot` CR, whether scheduled, manual, or discovered (all the same kind). This is the explicit, point-at-a-specific-backup mode.
- **`fromPolicy`** — name (and optional `namespace`) of a `SnapshotPolicy`; the restore resolves a snapshot through the policy's identity even when **no** `Snapshot` CR exists yet (deploy-or-restore). Pick the snapshot with `offset` (`0` = latest, `1` = previous, …; defaults to `0`) or restore the newest snapshot at or before a point in time with `asOf` (RFC3339).
- **`identity`** — a raw kopia identity for foreign writers or snapshots that aged out of the catalog. Specify `username` and `hostname` (both required), optionally `sourcePath` (absent matches any path), and select the snapshot with `snapshotID` (an exact manifest id), `asOf`, or `offset`. The `identity` source **requires** `spec.repository` to be set, since there is no `Snapshot`/`SnapshotPolicy` to derive the repository from.

### `target`

Where to write the restored data — **required**, exactly one of three externally-tagged modes:

- **`pvc`** — the operator creates the PVC from a template (`name` plus optional `storageClassName`, `capacity`, and `accessModes`).
- **`pvcRef`** — write into an existing PVC by `name` (optional `namespace`).
- **`populator`** — passive populator mode, written as an empty object `populator: {}`. No workload target exists at provision time; the restore is claimed by a separate PVC's `spec.dataSourceRef`. It is an empty sub-object today so future populator knobs can slot in without an API break.

A `Restore` with no `target` is invalid and fails admission. `inheritSecurityContextFrom` (under `mover`) is meaningless with a `populator` target — there is no workload pod to copy a security context from — and is rejected by the validator.

### `repository`

The repository to read from. Derived from `source` when omitted; **required only** with `source.identity`, which has no CR to derive it from.

### `options`

kopia restore-behavior knobs; every field's default is kopia's own default (unset ⇒ kopia decides), so an absent `options` block reproduces plain, additive `kopia snapshot restore` behavior. The core three: `enableFileDeletion` (delete files in the target absent from the snapshot, making the target an exact mirror — off by default, so restores are additive and safe; drives kopia's `--delete-extra`), `ignorePermissionErrors` (continue past permission errors, default true), and `writeFilesAtomically` (temp-file + rename, default true).

The rest mirror `kopia snapshot restore`'s own flags one-to-one and are all tri-state (`true`/`false`/absent) booleans except `parallel`: `parallel` (restore parallelism; kopia default `8`), `writeSparseFiles`, `skipOwners`, `skipPermissions`, `skipTimes`, `overwriteFiles`, `overwriteDirectories`, `overwriteSymlinks`, `ignoreErrors`, and `skipExisting`. See the [field reference](../../field-reference.md) for kopia's per-flag default.

### `policy`

How the restore reacts to a missing snapshot. `onMissingSnapshot` is `Fail` (fail-closed; the default for explicit `snapshotRef`/`identity` sources, so an explicit restore can never silently no-op) or `Continue` (proceed with an empty, deploy-or-restore volume; the default for `fromPolicy`). `waitTimeout` (e.g. `5m`) bounds how long the restore waits for the source snapshot to appear before giving up.

### `mover`

Per-run mover-Job overrides for this restore — resource requests/limits, kopia cache sizing, and the container/pod `securityContext` used to match the workload's UID/GID (and `fsGroup`, to make a fresh restore volume group-writable). This is the same surface a backup gets via `SnapshotPolicy.spec.mover`; an elevated context is namespace-gated exactly like a backup's, and `securityContext` combines with `inheritSecurityContextFrom` (explicit wins field-wise, and is the fallback when no workload pod resolves). See [Security context](../../security-context.md).

### `credentialProjection`

Opt-in (off by default). When `enabled: true`, the operator copies the referenced repository's credential Secret(s) into the restore mover's namespace (a no-op when they already live there) — so restoring from a shared `ClusterRepository` into a fresh namespace need not pre-create the Secret there.

### `failurePolicy`

Mover Job retry/deadline limits (`backoffLimit`, `activeDeadlineSeconds`), mirroring `Snapshot.spec.failurePolicy`. See [Movers](../../movers.md).

## `status`

### `resolved`

The source resolved and **pinned at admission**, never re-resolved — this is what makes a restore deterministic. `resolution` records the pinned outcome: `Snapshot` (the source resolved to a concrete kopia snapshot) or `NoSnapshot` (the source matched nothing and `onMissingSnapshot: Continue` chose an empty deploy-or-restore volume). Because the outcome is pinned once, a snapshot that appears later can never silently retarget an already-provisioned volume. The block also carries `kopiaSnapshotID` (the exact manifest id; restored on every subsequent reconcile even if newer snapshots appear), `snapshotRef` (the concrete `Snapshot` CR, when applicable), `repository`, `pinnedAt` (RFC3339), and the resolved `identity` (`username@hostname:path`).

### `target`

Resolved target details: `pvcRef` (the PVC actually written to, created or pre-existing) and `pvcPrime` (the populator handshake for passive / pvc-create modes).

### `failure`

Structured terminal-failure detail (kopia error class, stderr tail, retry hint), written by the mover before it exits non-zero. Read this first when a `Restore` lands in `Failed`.

### Other status fields

| Field | Meaning |
| --- | --- |
| `phase` | Current lifecycle phase: `Pending`, `Resolving`, `Restoring`, `Completed`, or `Failed`. |
| `sourceKind` | The pinned source kind (`SnapshotRef`/`FromPolicy`/`Identity`); backs the `SOURCE` printer column. |
| `observedGeneration` | `metadata.generation` last reconciled, for staleness detection. |
| `timing` | `startTime`/`endTime` (RFC3339) of the restore run. |
| `progress` | Live `bytesRestored`/`filesRestored` counters, patched periodically by the mover. |
| `conditions` | Standard Kubernetes conditions carrying the human-readable status/reason. |
| `logTail` | The last lines of the run's output, written by the mover at the terminal transition; capped in size, with full logs in the Job pod. |
