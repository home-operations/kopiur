# SnapshotPolicy

The backup recipe — what to back up, identity, retention, policy, hooks. A
`SnapshotPolicy` is idempotent and runs nothing on its own; a `Snapshot`
invocation or a `SnapshotSchedule` drives it. For the terse type/default table see
the [field reference](../../field-reference.md); for how-to guidance see
[Backups & schedules](../../backups.md).

## `spec`

### `repository`

Discriminated reference to the repository this recipe backs up to — either a
namespaced `Repository` or a cluster-scoped `ClusterRepository`. The wire shape
carries a `kind` so the referent kind is explicit.

### `identity`

Identity overrides — what kopia records as `username@hostname:path`. Resolved at
admission and pinned to `status.resolved.identity`, never re-rendered afterward.
Leave it unset to take the operator's defaults.

### `sources`

What to back up. At least one source is required (the webhook enforces it). Each
entry is a single PVC by name, a label/namespace `pvcSelector` matching many PVCs,
or an inline `nfs` export — exactly one of the three per source (also
webhook-enforced). Both the `sourcePathOverride` and `sourcePathStrategy` siblings
apply per source.

Per-source fields:

- `pvc` — a single `PersistentVolumeClaim` by name, in the policy's namespace.
- `pvcSelector` — a Kubernetes `labelSelector` plus an optional `namespaceSelector`
  (its `matchNames` restricts the search; absent means the policy's own namespace).
- `nfs` — an inline NFS export backed up directly, with no PVC; mounted read-only.
- `sourcePathOverride` — what kopia records as the source path. Defaults to
  `/pvc/<name>` for a PVC, or the NFS export's `path` for an NFS source.
- `sourcePathStrategy` — for `pvcSelector` sources only, how each matched PVC's
  source path is derived: `PvcName` (default) uses the name alone;
  `PvcNamespacedName` uses `<namespace>/<name>` to disambiguate same-named PVCs
  across namespaces.

### `copyMethod`

How the source volume is captured before kopia reads it. `Snapshot` (the
default) takes a point-in-time CSI volume snapshot, giving a crash-consistent
capture decoupled from the app's node; it needs the CSI snapshot stack and a
`VolumeSnapshotClass` for the source's driver. `Clone` takes a CSI volume
clone, mounted read-only, for drivers that support cloning but not
snapshotting. `Direct` reads the live PVC with no intermediate snapshot or
clone — it works on **any** storage with no CSI snapshot stack required, but
gives no point-in-time guarantee (the mover co-locates on the volume's node
for RWO); set it explicitly for non-CSI/static sources or clusters without the
snapshot stack. See [Copy methods](../../copy-methods.md).

### `volumeSnapshotClassName`

The `VolumeSnapshotClass` used when `copyMethod` is `Snapshot` or `Clone`.

### `groupBy`

Multi-PVC consistency grouping. `VolumeGroupSnapshot` (the default for multi-PVC
sources) takes one consistent group snapshot across all PVCs; `None` opts into
independent per-PVC snapshots. `None` must be set **explicitly** — there is no
silent per-PVC fallback, because that would produce inconsistent backups.

!!! note "Single-PVC today"
    Group snapshotting is not yet fully wired; multi-PVC group consistency is a
    work in progress. For the current single-PVC behavior this field has no
    observable effect.

### `retention`

GFS (grandfather-father-son) retention, enforced by the operator pruning the
`Snapshot` CRs produced from this recipe. See [Backups & schedules](../../backups.md).

### `defaultDeletionPolicy`

The default `deletionPolicy` stamped onto `Snapshot` CRs created against this
recipe (`Delete`, `Retain`, or `Orphan`), controlling whether deleting the CR also
deletes its kopia snapshot.

### `compression`

Compression policy. `compressor` names a kopia compressor (e.g. `zstd`); absent
leaves kopia's default. `neverCompress` is a list of filename globs to leave
uncompressed (e.g. already-compressed media).

### `files`

File-ignore policy. `ignoreRules` are filename/path globs to exclude from the
snapshot (e.g. `*.tmp`, `*/cache/*`, `lost+found`); `ignoreCacheDirs` honors
`CACHEDIR.TAG`; `ignoreIdenticalSnapshots` skips taking a new snapshot when the
source is identical to the previous one.

### `extraArgs`

An escape hatch for kopia flags not yet modeled as first-class fields.

### `errorHandling`

Backup-side error handling — lets a snapshot complete-with-errors instead of
failing outright. Each flag defaults `false` (kopia's fail-on-error default):
`ignoreFileErrors` (`--ignore-file-errors`) continues past unreadable files,
`ignoreDirErrors` (`--ignore-dir-errors`) past unreadable directories, and
`ignoreUnknownTypes` (`--ignore-unknown-types`) past entries of unknown type.

### `upload`

Upload parallelism (kopia's upload policy). `maxParallelSnapshots`
(`--max-parallel-snapshots`) is how many sources snapshot concurrently;
`maxParallelFileReads` (`--max-parallel-file-reads`) is file-read concurrency
within a snapshot. Absent knobs leave kopia's default.

### `verification`

First-class backup verification that proves snapshots are **restorable**, not just
that maintenance ran. Opt-in: when absent, no verification runs. Two tiers:

- `quick` — a schedule for the frequent blob-level `kopia snapshot verify`; absent
  means no quick verification. `verifyFilesPercent` (`--verify-files-percent`) tunes
  how many files it verifies fully (absent leaves kopia's default).
- `deep` — a schedule plus knobs for the rarer scratch-restore restorability test,
  which restores the latest snapshot into an ephemeral volume, sanity-checks it,
  then discards it. `deep.schedule` is its cron+jitter (e.g. weekly);
  `deep.capacity` sizes the ephemeral scratch PVC (e.g. `10Gi`) — absent falls
  back to a node-ephemeral `emptyDir`; `deep.storageClassName` picks the scratch
  PVC's StorageClass (absent uses the cluster default, and only applies when
  `capacity` is set).
- `successExpr` — a CEL pass/fail predicate over the verify result, applied to both
  tiers. The environment exposes `stats{files,bytes,errors}`, `snapshot`, and (deep
  only) `restored{files,checksumMatches}`; returning `false` fails the run, killing
  the silent "0 files" success. Example: `"stats.files > 0 && stats.errors == 0"`.

### `preflight`

User-declared **CEL preconditions** a backup must satisfy before its mover Job
launches — generalizing the built-in repository-readiness gate. Opt-in: when absent,
no preflight runs.

- `checks` — a list of `{ name, expr, message? }`. Each `expr` is a CEL **bool**
  predicate; **all** must pass (AND) for the backup to launch. `name` is unique and
  identifies the check in the `Snapshot`'s status; `message` is an optional human hint.
- `timeout` — how long to hold the `Snapshot` in `Pending` while a check is unsatisfied
  before failing it (Go-style duration; default `10m`; `0` holds indefinitely). The
  clock starts when a check first fails with the repository `Ready`, so a slow-to-connect
  repository doesn't consume the budget. The resulting `Failed` Snapshots are bounded by
  the schedule's `failedJobsHistoryLimit`.

The CEL environment exposes
`repository.{phase,ready,backendReachable,snapshotCountKnown,snapshotCount,
indexBlobCountKnown,indexBlobCount,sizeBytesKnown,sizeBytes,lastHealthyKnown,
lastHealthyAgeSeconds,lastReverifyKnown,lastReverifyAgeSeconds}` and
`maintenance.{hasRun,lastSuccessAgeSeconds}`, validated at admission. Unobserved values are
`i64::MAX`: a freshness (`< N`) check fails closed, but a count/size (`> N`) check fails
*open*, so always pair it with the `*Known`/`hasRun` companion. Example:
`"maintenance.hasRun && maintenance.lastSuccessAgeSeconds < 604800"`. See
[Repository health → Backup preflight](../../repository-health.md#backup-preflight-opt-in).

### `suspend`

Pause this recipe declaratively. A suspended `SnapshotPolicy` is skipped by
schedules and by its own reconcile — no retention prune, no backup creation —
and is surfaced via the `SUSPENDED` condition/column.

### `hooks`

Pre/post snapshot hooks that run in the workload, not the mover. `beforeSnapshot`
hooks run in order before the snapshot is taken (e.g. quiescing a database);
`afterSnapshot` hooks run in order after it completes (e.g. resuming the workload).
Each hook is exactly one of three forms:

- `workloadExec` — a `kubectl exec`-style command into a matched workload
  pod/container (the default form). Carries the pod/container selector, `command`,
  and `timeout`.
- `runJob` — a full Kubernetes `JobSpec` run as a one-shot Job (the k8up
  `PreBackupPod` analog).
- `httpRequest` — a typed HTTP request for cross-system orchestration, with `url`,
  `method` (default `POST`), optional `body`, and `timeout`.

A hook failure aborts the backup by default; set `continueOnFailure: true` on any
hook form to let the backup proceed past a failed hook. Timeouts are Go duration
strings (e.g. `2m`).

### `mover`

Per-recipe mover overrides — resources, cache, security context — layered over the
repository's `moverDefaults`. See [Movers](../../movers.md) and
[Security context](../../security-context.md).

### `credentialProjection`

Opt-in credential-Secret projection for this recipe's backup movers (default off).
When `enabled: true`, the operator copies the referenced repository's credential
Secret(s) into the namespace where each backup mover runs (a no-op when they
already live there) — so a workload backing up to a shared `ClusterRepository`
need not pre-create the Secret in its own namespace. Inherited by `Snapshot`s
produced from this recipe.

## `status`

### `resolved`

The recipe as kopia would see it, pinned at admission and never re-rendered:
`resolved.identity` is the resolved `username@hostname` identity, and
`resolved.sources` is the concrete list of PVCs and source paths after selector
expansion (each entry pairs a `namespace/name` `pvc` with the `sourcePath` kopia
records for it).

### `retention`

Summary of the most recent GFS retention prune: `activeSnapshotCount` (CRs
currently inside the GFS window), `lastPruneAt` (RFC3339 timestamp of the last
prune pass), and `lastPruneDeleted` (number of `Snapshot` CRs deleted by it).

### Other status fields

- `observedGeneration` — `metadata.generation` last reconciled, for staleness
  detection.
- `lastSuccessfulSnapshot` — RFC3339 timestamp of the most recent successful child
  `Snapshot` from this recipe (backs the `LAST-SNAPSHOT` column).
- `lastVerified` — RFC3339 timestamp of the most recent successful verification of
  any tier (backs the `LAST-VERIFIED` column).
- `conditions` — standard Kubernetes conditions (e.g. `RepositoryReachable`,
  `GroupSnapshotSupported`).
