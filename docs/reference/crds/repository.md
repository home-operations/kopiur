# Repository

A `Repository` is one kopia repository owned by a single namespace. It holds the storage backend, the credentials, the encryption password, and optional limits on how many discovered snapshots become `Snapshot` objects. Many `SnapshotPolicy` and `Restore` resources can point at the same one.

For the short type-and-default table see the [field reference](../../field-reference.md). For task guidance see [Repositories](../../repositories.md).

## `spec`

### `backend`

Exactly one storage backend. You write it as a single-key object, such as `backend: { s3: {...} }`. That shape is what makes "two backends at once" impossible to express. See [Backends](../../backends/index.md) for the fields each backend takes.

### `encryption`

The repository password, always given as a reference to a Secret. It is a sub-object, `encryption.passwordSecretRef`, rather than a bare field.

### `create`

What to do when the repository does not exist yet in the backing storage.

Leave `create` out, or disable it, and the repository must already exist. The operator then only connects to it. Enable it and the operator creates the repository using the encryption, splitter, hash and ECC algorithms you give.

Those `create.*` algorithm choices are **immutable after creation**. The apiserver and the webhook reject a change to them on an existing repository, because kopia writes them into the repository format. The `encryption` password Secret reference is not locked, so you can rename the Secret as long as the password value stays the same.

### `seed`

Initialize this repository from an existing replica the **first** time it bootstraps. This is the disaster-recovery counterpart of `RepositoryReplication`.

`seed.from` takes exactly one of two sources:

- `backend` is blob mode. Kopiur runs `kopia repository sync-to` from a bare mirror backend, and the new repository inherits the mirror's format and password.
- `repository` is migrate mode. Kopiur runs `kopia snapshot migrate` from another `Repository` or `ClusterRepository`, and the new repository gets its own format and password.

Both modes preserve snapshot identities and times.

A seed only runs while `status.uniqueId` is unset **and** the mover's first connect finds the backend uninitialized. On an already-initialized repository it does nothing and reports `Seeded=True` with reason `AlreadyInitialized`, so it is safe to leave in a GitOps manifest forever.

Tuning that belongs to one mode is rejected when you pair it with the other mode's source: `sync` belongs to blob mode, `migrate` and `credentialProjection` belong to migrate mode. `allowEmptySource` (default `false`) is the only way to accept a source that holds zero snapshots. While a seed runs, `failurePolicy.activeDeadlineSeconds` defaults to **86400** seconds, or 24 hours, instead of the 120 seconds a routine connect gets. See [Repositories → `seed`](../../repositories.md#seed--initialize-a-new-repository-from-a-replica).

How you cap the copy depends on the mode.

Blob mode uses `sync.maxDownloadSpeedBytesPerSecond` and `sync.maxUploadSpeedBytesPerSecond`, because `kopia repository sync-to` has real speed flags.

Migrate mode has no such flags. `kopia snapshot migrate` accepts none, so Kopiur applies `migrate.throttle` with `kopia repository throttle set` on each connection before the copy starts, one side at a time. `throttle.source` caps the replica's read connection. `throttle.destination` caps this repository's write connection. Each is a [`throttle`](shared-types.md#moverdefaults) block holding `uploadBytesPerSecond`, `downloadBytesPerSecond`, `readOpsPerSecond` and `writeOpsPerSecond`. Every value you set must be `1` or greater, which the webhook enforces.

Each side overrides **that side's** repository `moverDefaults.throttle` one field at a time: a value set here wins, and a value left unset keeps the repository's own. The override applies only while the seed is armed. Later connects are capped by `moverDefaults.throttle` alone. Two repositories means two connections and two independent blocks, so the cap on the replica says nothing about the cap on this repository, and the reverse. Capping the read-only replica connection works, because kopia accepts `throttle set` there. See [Throttling a seed](../../repositories.md#throttling-a-seed).

### `moverDefaults`

Base mover configuration inherited by **every** mover this repository starts: bootstrap, backup, restore and maintenance. It covers security context, pod security context, resources, cache, `nodeSelector`, `tolerations`, `affinity`, `podLabels`, `podAnnotations` and the Job TTL. Each recipe can override any field, and the two are merged one field at a time. See [Movers](../../movers.md) and [MoverDefaults](shared-types.md#moverdefaults).

### `scheduleDefaults`

Repository-wide scheduling defaults. Consumers that do not set their own equivalent field inherit them at reconcile time. That covers `SnapshotPolicy.spec.verification`, `RepositoryReplication.spec.schedule`, `SnapshotReplication.spec.schedule`, `Maintenance.spec.schedule` and `SnapshotSchedule.spec.schedule`, each of which falls back to `scheduleDefaults.timezone` and `scheduleDefaults.jitter` when its own value is missing. The cron's own value always wins.

Below `timezone` the fallback is UTC. Below `jitter` there is no fallback, so a value missing at both levels means no spread at all.

A `SnapshotSchedule` resolves the repository defaults of the policy it targets and records both in `status.nextSchedule.timezone` and `status.nextSchedule.jitter`. A watch on the repository re-triggers the schedule when either default changes, and the pinned slot is recomputed. `jitter` is capped at 24 hours at admission. See [Repositories → `scheduleDefaults`](../../repositories.md#scheduledefaults--set-the-cron-timezone-and-jitter-once) and [ScheduleDefaults](shared-types.md#scheduledefaults).

### `concurrency`

Limits on how many mover Jobs this repository runs at once. `maxConcurrentJobs` is the ceiling. Leave it out, or set `0`, for unlimited, which is the default and matches every release before the field existed.

Backups, restores and the source side of both replication kinds share ONE pool. Maintenance, verification, pin, batched snapshot deletions, bootstrap and catalog scans, and browse sessions are all outside it. A restore is always admitted, but it still occupies a slot while it runs. See [ConcurrencySpec](shared-types.md#concurrencyspec) and [Backups → limiting concurrent jobs per repository](../../backups.md#limiting-concurrent-jobs-per-repository).

### `catalog`

Limits how many `Snapshot` objects with `origin: discovered` Kopiur creates from the kopia catalog. Use it to keep the etcd footprint sane on a large repository.

### `server`

An optional kopia web UI server, published through a `Service` in this repository's own namespace. Adding the block turns it on. See [Server](../../server.md).

### `maintenance`

Maintenance control, managed by default. When this block is absent, or sets `enabled: true`, the reconciler creates and owns a `Maintenance` object for this repository in this namespace. A `Maintenance` you write yourself is always honored and never duplicated. See [Maintenance](../../maintenance.md).

### `onNamespaceDelete`

What happens to this repository's snapshots when a namespace that uses it is deleted. `Orphan`, the default, keeps the snapshot history and gives up ownership. `Delete` cascades to each `Snapshot`'s own `deletionPolicy`. Because `Orphan` is the default, `kubectl delete ns` does not destroy snapshots.

### `mode`

Access mode: `ReadWrite`, the default, or `ReadOnly`. A `ReadOnly` repository serves restores only. The reconciler refuses backup Jobs and skips maintenance projection. Use it when decommissioning or migrating and you want no risk of a write. See [Access modes](../../access-modes.md).

### `suspend`

Pause this repository from the manifest, default `false`. A suspended repository skips connect, bootstrap and maintenance projection, and reports the state in a condition.

### `health`

Thresholds for the warnings the reconciler raises about a repository that is degrading but still usable.

- `health.indexBlobWarnThreshold` is the index-blob count above which the reconciler raises the `IndexBlobHealth` condition and a Warning event, meaning maintenance is not compacting fast enough. Leave it out to use the built-in default of 1000. Set `0` to turn the warning off. A negative value is rejected by the admission webhook.

### `parameters`

Kopia repository parameters that can change. Kopiur re-applies them whenever they drift from what the repository reports. These are different from [`create`](#create), whose settings are fixed when the repository is made and immutable afterwards. `parameters` describes a live repository and is what `kopia repository set-parameters` exists for.

`parameters.epoch` tunes the epoch manager. Kopia cannot compact an index blob until its epoch closes, and an epoch cannot close before `minDuration` no matter how many blobs it holds. On a busy repository that gate, not the maintenance schedule, is what keeps the index-blob count high.

- `epoch.minDuration` is the minimum age an epoch reaches before it may advance. Kopia's default is `24h`. Write it as a Go-style duration. This is the gate to lower, to `6h` for example, when blob counts stay high even though maintenance is running.
- `epoch.refreshFrequency` is how often clients re-read epoch state. Kopia's default is `20m`.
- `epoch.advanceOnCount` is the number of index blobs that triggers an advance once `minDuration` has passed. Kopia's default is `20`.
- `epoch.advanceOnSizeMiB` is the index size that triggers an advance. Kopia's default is `10`. The unit is **mebibytes**, so `10` means 10485760 bytes, even though kopia's own log prints "MB".
- `epoch.checkpointFrequency` is how many epochs pass between full index checkpoints. Kopia's default is `7`.
- `epoch.deleteParallelism` is the parallelism used for epoch cleanup deletions. Kopia's default is `4`.

Every field is optional and Kopiur adds no defaults of its own. Leaving a field out leaves kopia's current value alone, and removing a value you set earlier does not restore kopia's default. The whole block is rejected at admission on a `mode: ReadOnly` repository, because `set-parameters` is a repository-wide write that kopia refuses on a read-only connection. See [Maintenance → index-blob health](../../maintenance.md#index-blob-health).

## `status`

### `storageStats`

Repository-wide storage figures from the last catalog scan.

- `snapshotCount` is the total number of snapshots in the repository, across all identities.
- `totalSize` is the total on-disk size in human-readable form, such as `412Gi`.
- `lastObservedAt` is the RFC 3339 timestamp when these figures were last seen.
- `indexBlobCount` is the number of content-index blobs seen at the last bootstrap. Kopia compacts these during maintenance. A count that climbs without limit means maintenance is not keeping up, and crossing `spec.health.indexBlobWarnThreshold` raises the `IndexBlobHealth` warning. It is also the `IndexBlobs` print column.

### `parameters`

The kopia repository parameters actually **seen** at the last bootstrap. This is what the repository reports, not what `spec.parameters` asked for, so a value you declared that failed to apply shows up here as a mismatch instead of silence.

`parameters.epoch` mirrors the full epoch set: `enabled`, `minDuration`, `refreshFrequency`, `cleanupSafetyMargin`, `advanceOnCount`, `advanceOnSizeMiB`, `checkpointFrequency` and `deleteParallelism`. Durations use the same Go-style grammar `spec` uses, so you can compare the two directly.

`cleanupSafetyMargin` appears here but you deliberately cannot set it. It is the grace window that stops kopia deleting index blobs a concurrent writer still needs.

### `catalog`

Catalog status. `discoveredBackupCount` is how many `Snapshot` objects the scan created, and `lastRefreshAt` is the RFC 3339 timestamp of the last catalog refresh.

### `server`

The resolved kopia server endpoint and auth, pinned by the reconciler. See [Server](../../server.md).

### `seed`

What the last seed attempt did.

`startedAt` is stamped before the seeding Job is created and never cleared. It is the durable marker that lets an interrupted seed resume instead of being mistaken for an ordinary adoption. `seededAt` is set once, when the seed completes. `mode` is `blob` or `migrate`. `source` is the rendered source, either a backend discriminant or `Kind/name`, and never a credential or a bucket path. `snapshotCount` is what the seed saw at the source. `snapshotsCopied` applies to migrate mode only and is **cumulative**: it counts what is present after the run, including anything an interrupted earlier attempt had already moved. The matching condition is `Seeded`.

### Other status fields

- `phase` is the lifecycle phase: `Pending`, `Initializing`, `Ready`, `Degraded` (the repository is reachable but a sub-operation is failing, so read the conditions), or `Failed` (connect or create failed, and the conditions name the reason).
- `observedGeneration` is the `metadata.generation` of the `spec` last reconciled. It drives staleness detection.
- `resolvedCredentialVersion` is the `resourceVersion` of the password Secret seen at the last connect attempt. Editing the Secret's content therefore re-triggers a connect instead of leaving the repository parked at `Failed`.
- `uniqueId` is the kopia repository's unique ID. It is pinned on the **first** successful bootstrap and never rewritten by a health probe. Its presence is what makes auto-create a one-time event: `spec.create.enabled` governs the first bootstrap only, and once this field is set Kopiur will never create a fresh empty repository at this backend, however empty the backend becomes. A wiped backend therefore parks at `Failed` with reason `RepositoryReinitializeBlocked` instead of being re-created silently. See [`allow-reinitialize`](#annotations) below.
- `backend` mirrors the `spec.backend` discriminant for the print column.
- `conditions` holds the standard Kubernetes conditions, such as `Connected` and `MaintenanceOwned`.

## Annotations

- `kopiur.home-operations.com/allow-reinitialize` tells Kopiur that you meant to **re-initialize** a repository whose backend was wiped.

  Set the value to the repository's current `status.uniqueId`, copied exactly. Kopiur honors the annotation only while the two match, and that is what makes it expire on its own: a successful re-initialize mints a new ID, the old value stops matching, and a copy left behind in a GitOps manifest can never authorize a second wipe.

  It also does nothing on a healthy repository. The acknowledgement becomes permission to create only after the repository has left `Ready`, so it can never turn a routine health probe into a re-create.

  A value that does not match the pin is ignored. While the repository is not `Ready`, a mismatch also raises a Warning event, `InvalidReinitializeAck`, naming the value Kopiur expects. Kopiur never writes, rewrites or removes this annotation. Full procedure: [Deliberately re-initialize a wiped repository](../../repository-health.md#deliberately-re-initialize-a-wiped-repository).

- `kopiur.home-operations.com/allow-mass-deletion` acknowledges a pending mass-deletion wave. See [the mass-deletion circuit breaker](../../repositories.md#deletionprotection--the-mass-deletion-circuit-breaker).
