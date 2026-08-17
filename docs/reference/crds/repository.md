# Repository

A namespaced kopia repository — credentials, backend, encryption, and optional
catalog-materialization bounds — owned by one namespace and referenced by many
`SnapshotPolicy` and `Restore` resources. For the terse type/default table see the
[field reference](../../field-reference.md); for how-to guidance see
[Repositories](../../repositories.md).

## `spec`

### `backend`

Exactly one storage backend. The wire shape is a single-key object
(`backend: { s3: {...} }`), so an invalid "two backends at once" state is
unrepresentable. See [Backends](../../backends/index.md) for the per-backend fields.

### `encryption`

The repository password, always given as a Secret reference. It is a sub-object
(`encryption.passwordSecretRef`) rather than a bare field.

### `create`

What to do when the repository does not yet exist in the backing storage. When
absent (or disabled) the repository must already exist and the operator only
connects. When enabled, the operator creates it with the given
encryption/splitter/hash/ECC algorithms. Those `create.*` algorithm choices are
**immutable after creation** — the apiserver and webhook reject changing them on an
existing repository, because kopia fixes them into the repository format. (The
`encryption` password Secret reference itself is *not* locked: renaming the Secret
with identical content is allowed.)

### `seed`

Initialize this repository from an existing replica on its **first** bootstrap —
the disaster-recovery counterpart of `RepositoryReplication`. `seed.from` is a
single-key one-of: `backend` (blob mode, `kopia repository sync-to` from a bare
mirror backend, inheriting the mirror's format and password) or `repository`
(migrate mode, `kopia snapshot migrate` from another `Repository`/`ClusterRepository`,
which creates a local repository with its own format and password). Both preserve
snapshot identities and times.

Armed only while `status.uniqueId` is unset **and** the mover's first connect finds
the backend uninitialized; on an already-initialized repository it is a no-op
(`Seeded=True`, reason `AlreadyInitialized`), so it is safe to leave standing in a
GitOps manifest. Mode-specific tuning (`sync` for blob, `migrate` and
`credentialProjection` for migrate) is rejected when paired with the other mode's
source. `allowEmptySource` (default `false`) is the only way to accept a source
holding zero snapshots. `failurePolicy.activeDeadlineSeconds` defaults to **86400**
(24 h) while seeding, versus the 120 s a routine connect gets. See
[Repositories → `seed`](../../repositories.md#seed--initialize-a-new-repository-from-a-replica).

### `moverDefaults`

Base mover configuration — security context, pod security context, resources,
cache, `nodeSelector`/`tolerations`/`affinity`, Job TTL — inherited by **every**
mover this repository spawns (bootstrap, backup, restore, maintenance). Each recipe
can override fields per-mover; the merge is field-wise. See [Movers](../../movers.md).

### `scheduleDefaults`

Repo-level scheduling defaults inherited at reconcile time by consumers that don't
set their own equivalent field: `SnapshotPolicy.spec.verification`,
`RepositoryReplication.spec.schedule`, `Maintenance.spec.schedule`, and
`SnapshotSchedule.spec.schedule` all fall back to `scheduleDefaults.timezone` when
their own `timezone` is absent (the consuming cron's own value always wins). A
`SnapshotSchedule` resolves its target policy's repository default, records the
zone in `status.nextSchedule.timezone`, and is re-triggered by a repository
referent watch when the default changes.
See [Repositories → `scheduleDefaults`](../../repositories.md#scheduledefaults--set-the-cron-timezone-once).

### `catalog`

Bounds materialization of `origin: discovered` `Snapshot` CRs from the kopia
catalog, keeping the etcd footprint sane for large repositories.

### `server`

Optional kopia web-UI server, exposed via a `Service` in this Repository's own
namespace. Presence of the block enables it. See [Server](../../server.md).

### `maintenance`

Maintenance control. Default-managed: when absent or `enabled: true`, the reconciler
creates and owns a `Maintenance` CR for this repository in this namespace. An
externally-authored `Maintenance` is always honored and never duplicated. See
[Maintenance](../../maintenance.md).

### `onNamespaceDelete`

What happens to this repository's snapshots when a consuming namespace is deleted.
`Orphan` (default) keeps the snapshot history and releases ownership; `Delete`
cascades per-`Snapshot` `deletionPolicy`. The default means `kubectl delete ns` does
not destroy snapshots.

### `mode`

Access mode: `ReadWrite` (default) or `ReadOnly`. A `ReadOnly` repository serves
restores only — the reconciler refuses backup Jobs and skips maintenance projection
— useful for decommissioning or migration without write risk. See
[Access modes](../../access-modes.md).

### `suspend`

Pause this repository declaratively (default `false`). A suspended repository skips
connect/bootstrap and maintenance projection, and surfaces the state via a condition.

### `health`

Repository health thresholds — tuning for the warnings the reconciler raises about a
degrading-but-still-usable repository.

- `health.indexBlobWarnThreshold` — the index-blob count above which the reconciler
  raises the `IndexBlobHealth` condition plus a Warning event (maintenance isn't
  compacting fast enough). Absent uses the built-in default (1000). `0` disables the
  warning entirely; a negative value is rejected by the admission webhook.

### `parameters`

Mutable kopia repository parameters, re-applied whenever they drift from what the
repository reports. Distinct from [`create`](#create), whose knobs are fixed at
creation and immutable afterwards — these describe a live repository and are the
point of `kopia repository set-parameters`.

`parameters.epoch` tunes the epoch manager. kopia cannot compact an index blob until
its epoch closes, and an epoch cannot close before `minDuration` regardless of how
many blobs it holds — so on a busy repository this gate, not the maintenance
schedule, is what pins the index-blob count high.

- `epoch.minDuration` — minimum epoch age before it may advance (kopia default `24h`).
  A Go-style duration. **The gate**; lower it (e.g. `6h`) when blobs stay high despite
  maintenance running.
- `epoch.refreshFrequency` — how often clients re-read epoch state (kopia default `20m`).
- `epoch.advanceOnCount` — index blobs that trigger an advance once past `minDuration`
  (kopia default `20`).
- `epoch.advanceOnSizeMiB` — index size that triggers an advance (kopia default `10`).
  **Mebibytes**: `10` is 10485760 bytes, even though kopia's own log renders it as "MB".
- `epoch.checkpointFrequency` — epochs between full index checkpoints (kopia default `7`).
- `epoch.deleteParallelism` — parallelism for epoch cleanup deletions (kopia default `4`).

Every field is optional and kopiur supplies no defaults of its own: **absent leaves
kopia's current value untouched**, and removing a value you previously set does not
restore kopia's default. Rejected at admission on a `mode: ReadOnly` repository —
`set-parameters` is a repository-wide write kopia refuses on a read-only connection.
See [Maintenance → index-blob health](../../maintenance.md#index-blob-health).

## `status`

### `storageStats`

Aggregate repository storage figures from the last catalog scan:

- `snapshotCount` — total snapshots present in the repository (across all identities).
- `totalSize` — human-readable total on-disk size (e.g. `412Gi`).
- `lastObservedAt` — RFC 3339 timestamp these stats were last observed.
- `indexBlobCount` — number of content-index blobs observed at the last bootstrap.
  kopia compacts these during maintenance; an unbounded climb means maintenance
  isn't keeping up, and crossing `spec.health.indexBlobWarnThreshold` raises the
  `IndexBlobHealth` warning. Also surfaced as the `IndexBlobs` print column.

### `parameters`

The kopia repository parameters actually **observed** at the last bootstrap — what the
repository reports, not what `spec.parameters` asked for, so a declared value that
failed to apply shows up here as a mismatch rather than as silence.

`parameters.epoch` mirrors the full epoch set (`enabled`, `minDuration`,
`refreshFrequency`, `cleanupSafetyMargin`, `advanceOnCount`, `advanceOnSizeMiB`,
`checkpointFrequency`, `deleteParallelism`), with durations rendered in the same
Go-style grammar `spec` uses so the two are directly comparable.

`cleanupSafetyMargin` appears here but is deliberately **not** settable: it is the grace
window that stops kopia deleting index blobs a concurrent writer still needs.

### `catalog`

Catalog-materialization status: `discoveredBackupCount` (how many `Snapshot` CRs
were materialized from the scan) and `lastRefreshAt` (RFC 3339 timestamp of the last
catalog refresh).

### `server`

Resolved kopia server endpoint/auth, pinned by the reconciler. See
[Server](../../server.md).

### `seed`

What the last seed attempt did. `startedAt` is the durable seed-attempt marker
(stamped before the seeding Job is created, never cleared — it is what lets an
interrupted seed resume instead of being mistaken for an ordinary adoption);
`seededAt` is set once, when the seed completes. `mode` is `blob`/`migrate`,
`source` is the rendered source (a backend discriminant, or `Kind/name`) and never
a credential or a bucket path. `snapshotCount` is what the seed observed at the
source; `snapshotsCopied` is migrate-only and **cumulative** — what is present
after the run, including anything an interrupted earlier attempt had already
moved. The matching condition is `Seeded`.

### Other status fields

- `phase` — lifecycle phase: `Pending`, `Initializing`, `Ready`, `Degraded`
  (reachable but a sub-operation is failing — see conditions), or `Failed`
  (connect/create failed — see conditions for the actionable reason).
- `observedGeneration` — `metadata.generation` of the `spec` last reconciled; drives
  staleness detection.
- `resolvedCredentialVersion` — `resourceVersion` of the password Secret observed at
  the last connect attempt; editing the Secret's content re-triggers a connect
  rather than parking the repository `Failed` forever.
- `uniqueId` — the kopia repository's unique ID.
- `backend` — mirror of `spec.backend`'s discriminant for the print column.
- `conditions` — standard Kubernetes conditions (e.g. `Connected`,
  `MaintenanceOwned`).
