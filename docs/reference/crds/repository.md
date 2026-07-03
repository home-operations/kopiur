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

### `moverDefaults`

Base mover configuration — security context, pod security context, resources,
cache, `nodeSelector`/`tolerations`/`affinity`, Job TTL — inherited by **every**
mover this repository spawns (bootstrap, backup, restore, maintenance). Each recipe
can override fields per-mover; the merge is field-wise. See [Movers](../../movers.md).

### `scheduleDefaults`

Repo-level scheduling defaults inherited at reconcile time by consumers that don't
set their own equivalent field: `SnapshotPolicy.spec.verification`,
`RepositoryReplication.spec.schedule`, and `Maintenance.spec.schedule` all fall
back to `scheduleDefaults.timezone` when their own `timezone` is absent (the
consuming cron's own value always wins). `SnapshotSchedule` does not inherit it
yet. See [Repositories → `scheduleDefaults`](../../repositories.md#scheduledefaults--set-the-cron-timezone-once).

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

### `catalog`

Catalog-materialization status: `discoveredBackupCount` (how many `Snapshot` CRs
were materialized from the scan) and `lastRefreshAt` (RFC 3339 timestamp of the last
catalog refresh).

### `server`

Resolved kopia server endpoint/auth, pinned by the reconciler. See
[Server](../../server.md).

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
