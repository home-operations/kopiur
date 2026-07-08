# RepositoryReplication

Mirrors a repository's blobs to a second backend on a schedule (`kopia repository sync-to`) — the "2" in 3-2-1 backup. For the terse type/default table see the [field reference](../../field-reference.md); for how-to guidance see [Replication](../../replication.md).

A `RepositoryReplication` is **namespaced**: it lives alongside its source repository (mirroring [`Maintenance`](maintenance.md)) and references either a namespaced [`Repository`](repository.md) or a cluster-scoped [`ClusterRepository`](cluster-repository.md). The controller schedules a per-slot mover Job (croner + deterministic jitter, single-flight, repo-ready gate) exactly like `Maintenance`.

## `spec`

### `sourceRef`

The `Repository` or `ClusterRepository` to mirror *from*. Credentials and connect details are resolved from it.

### `destination`

The backend to mirror *to* — exactly one backend, expressed as the externally-tagged `Backend` enum (e.g. `destination: { s3: {…} }`), reused from the repository types. The destination **must differ from the source's backend**; the admission webhook rejects a same-backend mirror, since replicating a backend onto itself is meaningless.

The destination's own backend **access** credentials are supplied by its `auth.secretRef` (or `auth.workloadIdentity`), just like a source repository. `sync-to` is a blob-level copy, so the mirror always inherits the source repository's format and encryption password — there is no separate destination password. The destination credential `Secret` must live in the `RepositoryReplication`'s own namespace (the mover loads it with namespace-local `envFrom`; replication does not project credentials) and use the same key names a source Secret would. The source and destination may use different credentials — Kopiur delivers the destination Secret under a `KOPIUR_DEST_` env prefix so the two sides' keys never collide. See [Destination credentials](../../replication.md#destination-credentials).

### `schedule`

Cron and deterministic jitter for the replication runs, using the same scheduling kernel as `Maintenance`.

### `mover`

Mover (Job pod) overrides for the replication run — resources, scheduling, security context. Inherits the source repository's `moverDefaults` underneath.

### `suspend`

Pause this replication declaratively (default `false`). A suspended `RepositoryReplication` is skipped by its own reconcile — no sync runs — and the state is surfaced via a condition.

### `sync`

Tuning knobs for the underlying `kopia repository sync-to` invocation (issue #216). Every field is optional; an absent `sync` block, or an absent field within it, reproduces kopia's own default for that flag — see [Tuning the sync](../../replication.md#tuning-the-sync).

| Field | kopia flag | Meaning |
| --- | --- | --- |
| `parallel` | `--parallel` | Concurrent blob-copy workers (kopia default `1` — sequential; the main knob for a slow initial seed). |
| `deleteExtra` | `--delete` | Prune destination-only blobs for a true mirror (kopia default `false` — additive sync). **Deletes destination content** — see the safety note in the guide. |
| `mustExist` | `--[no-]must-exist` | Fail instead of initializing the destination's repository-format blob (kopia default `false`). |
| `times` | `--[no-]times` | Synchronize blob modification times to the destination, when supported (kopia default `true`). |
| `update` | `--[no-]update` | Update blobs already present at the destination when the source copy is newer (kopia default `true`). |
| `maxDownloadSpeedBytesPerSecond` | `--max-download-speed` | Cap read throughput from the source, bytes/sec (kopia default: unlimited). |
| `maxUploadSpeedBytesPerSecond` | `--max-upload-speed` | Cap write throughput to the destination, bytes/sec (kopia default: unlimited). |

`parallel` and the two speed caps must be `>= 1` when set (admission-webhook enforced).

## `status`

### `phase`

Lifecycle phase: `Pending` (admitted, not yet run — the default), `Replicating` (a mover Job is in flight), `Succeeded` (last run completed, idle until the next slot), `Failed` (last run failed; see conditions), or `Suspended` (paused via `spec.suspend`).

### others

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` last reconciled, for staleness detection / kstatus. |
| `destinationBackend` | The destination backend kind (mirror of the `spec.destination` discriminant), backing the `DESTINATION` print column. |
| `lastReplicated` | RFC3339 timestamp of the most recent successful replication run, backing the `LAST` print column. |
| `nextScheduledAt` | RFC3339 timestamp of the next scheduled run (cron + jitter, pinned). |
| `lastReplicatedBytes` | Bytes replicated by the last successful run (best-effort from kopia output). |
| `lastReplicatedBlobs` | Blobs replicated by the last successful run (best-effort). |
| `conditions` | Standard Kubernetes conditions (`Ready`, `Reconciling`, `Stalled`). |
