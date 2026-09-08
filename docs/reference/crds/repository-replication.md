# RepositoryReplication

A `RepositoryReplication` mirrors a repository's blobs to a second backend on a schedule, using `kopia repository sync-to`. It is the "2" in a 3-2-1 backup strategy.

For the short type-and-default table see the [field reference](../../field-reference.md). For task guidance see [Replication](../../replication.md).

A `RepositoryReplication` is **namespaced**. It lives beside its source repository, the same way [`Maintenance`](maintenance.md) does, and references either a namespaced [`Repository`](repository.md) or a cluster-scoped [`ClusterRepository`](cluster-repository.md). The controller schedules one mover Job per slot, using the same machinery as `Maintenance`: croner, deterministic jitter, single-flight, and a repository-ready gate.

## `spec`

### `sourceRef`

The `Repository` or `ClusterRepository` to mirror *from*. Credentials and connect details come from it.

### `destination`

The backend to mirror *to*. It is exactly one backend, written as the same single-key `Backend` object the repository types use, for example `destination: { s3: {…} }`.

The destination **must differ from the source's backend**. The admission webhook rejects a same-backend mirror, because replicating a backend onto itself means nothing.

The destination's own **access** credentials come from its `auth.secretRef`, or `auth.workloadIdentity`, exactly like a source repository. `sync-to` is a blob-level copy, so the mirror always inherits the source repository's format and encryption password, and there is no separate destination password.

The destination credential `Secret` must live in the `RepositoryReplication`'s own namespace, because the mover loads it with `envFrom`, which only reads the local namespace, and replication does not project credentials. Use the same key names a source Secret would use.

The source and destination may use different credentials. Kopiur delivers the destination Secret under a `KOPIUR_DEST_` environment prefix, so the two sides' keys never collide. See [Destination credentials](../../replication.md#destination-credentials).

### `schedule`

Cron and deterministic jitter for the replication runs, using the same scheduling machinery as `Maintenance`.

### `mover`

Overrides for the replication run's mover Job pod: resources, scheduling and security context. It inherits the source repository's `moverDefaults` underneath.

### `suspend`

Pause this replication from the manifest, default `false`. A suspended `RepositoryReplication` is skipped by its own reconcile, so no sync runs, and the state shows up in a condition.

### `sync`

Tuning for the underlying `kopia repository sync-to` call, from issue #216. Every field is optional. Leaving out the `sync` block, or a field inside it, reproduces kopia's own default for that flag. See [Tuning the sync](../../replication.md#tuning-the-sync).

| Field | kopia flag | Meaning |
| --- | --- | --- |
| `parallel` | `--parallel` | Concurrent blob-copy workers (kopia default `1` — sequential; the main knob for a slow initial seed). |
| `deleteExtra` | `--delete` | Prune destination-only blobs for a true mirror (kopia default `false` — additive sync). **Deletes destination content** — see the safety note in the guide. |
| `mustExist` | `--[no-]must-exist` | Fail instead of initializing the destination's repository-format blob (kopia default `false`). |
| `times` | `--[no-]times` | Synchronize blob modification times to the destination, when supported (kopia default `true`). |
| `update` | `--[no-]update` | Update blobs already present at the destination when the source copy is newer (kopia default `true`). |
| `maxDownloadSpeedBytesPerSecond` | `--max-download-speed` | Cap read throughput from the source, bytes/sec (kopia default: unlimited). |
| `maxUploadSpeedBytesPerSecond` | `--max-upload-speed` | Cap write throughput to the destination, bytes/sec (kopia default: unlimited). |

`parallel` and the two speed caps must be `1` or greater when set, which the admission webhook enforces.

## Out-of-band runs

To trigger a one-off mirror, annotate a `RepositoryReplication` with `kopiur.home-operations.com/run-requested` set to an RFC 3339 timestamp. There is no `run-mode` companion, because a replication has only one kind of run.

The timestamp identifies *which* request the status answers, so re-applying the same value does nothing and a new timestamp starts a new run.

The requested run goes through the same mover, the same gates and the same single-flight rule as a cron slot. Because it stamps `status.lastReplicated` on success, it also re-anchors the next scheduled slot. See [Run it now](../../replication.md#run-it-now). `kubectl kopiur replication run` stamps the annotation for you.

/// warning | A malformed timestamp is refused at admission

The admission webhook rejects a `run-requested` value that is not RFC 3339, naming the bad value and the fix, so in practice a malformed annotation never reaches the controller.

An object annotated while the webhook was down degrades gracefully instead of stalling. The schedule keeps running, and the controller reports `Ready=False` with reason `InvalidRunRequest` on the next pass where **no cron slot is due**. A due slot's own report takes that one `Ready` write, so on a very frequent schedule the message appears once the replication next goes idle.

///

## `status`

### `phase`

The lifecycle phase: `Pending` (admitted, not yet run, which is the default), `Replicating` (a mover Job is in flight), `Succeeded` (the last run completed and it is idle until the next slot), `Failed` (the last run failed; see conditions), or `Suspended` (paused through `spec.suspend`).

### `manualRun`

The state of the most recent [annotation-requested run](#out-of-band-runs): the `requestedAt` value it answers, its `phase`, and the `completedAt` instant it reached a terminal one. It is absent until a run is requested.

| `phase` | Meaning |
| --- | --- |
| `Pending` | Recorded but not started, either because the replication is suspended or because the request is waiting behind an in-flight run. It runs once you unsuspend, or once that run finishes. |
| `Running` | The requested mover Job is in flight. |
| `Succeeded` | The requested run completed. |
| `Failed` | The requested run's Job failed; conditions carry the detail. |

### others

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` last reconciled, for staleness detection and kstatus. |
| `destinationBackend` | The destination backend kind, mirroring the `spec.destination` discriminant, behind the `DESTINATION` print column. |
| `lastReplicated` | RFC 3339 timestamp of the most recent successful replication run, behind the `LAST` print column. |
| `nextScheduledAt` | RFC 3339 timestamp of the next scheduled run, with cron and jitter already applied. |
| `lastReplicatedBytes` | Bytes replicated by the last successful run, read from kopia output where available. |
| `lastReplicatedBlobs` | Blobs replicated by the last successful run, read from kopia output where available. |
| `conditions` | Standard Kubernetes conditions: `Ready`, `Reconciling`, `Stalled`. |
