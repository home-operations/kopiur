# RepositoryReplication

Mirrors a repository's blobs to a second backend on a schedule (`kopia repository sync-to`) — the "2" in 3-2-1 backup. For the terse type/default table see the [field reference](../../field-reference.md); for how-to guidance see [Replication](../../replication.md).

A `RepositoryReplication` is **namespaced**: it lives alongside its source repository (mirroring [`Maintenance`](maintenance.md)) and references either a namespaced [`Repository`](repository.md) or a cluster-scoped [`ClusterRepository`](cluster-repository.md). The controller schedules a per-slot mover Job (croner + deterministic jitter, single-flight, repo-ready gate) exactly like `Maintenance`.

## `spec`

### `sourceRef`

The `Repository` or `ClusterRepository` to mirror *from*. Credentials and connect details are resolved from it.

### `destination`

The backend to mirror *to* — exactly one backend, expressed as the externally-tagged `Backend` enum (e.g. `destination: { s3: {…} }`), reused from the repository types. The destination **must differ from the source's backend**; the admission webhook rejects a same-backend mirror, since replicating a backend onto itself is meaningless.

### `destinationEncryption`

Encryption/password for the destination repository when it needs its own — for example a freshly-created mirror with a distinct password.

**Omit this to mirror.** When absent, the destination uses the **source** repository's password. This is the common case for a true mirror: `sync-to` copies blobs verbatim, so the repository format (including the encryption material) is identical and a separate destination password would be wrong. Set it only when the destination is a distinct repository with its own credentials.

### `schedule`

Cron and deterministic jitter for the replication runs, using the same scheduling kernel as `Maintenance`.

### `mover`

Mover (Job pod) overrides for the replication run — resources, scheduling, security context. Inherits the source repository's `moverDefaults` underneath.

### `suspend`

Pause this replication declaratively (default `false`). A suspended `RepositoryReplication` is skipped by its own reconcile — no sync runs — and the state is surfaced via a condition.

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
