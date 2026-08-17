# SnapshotReplication

Copies selected **snapshots** — manifests and the content they reference — from one repository into another on a schedule (`kopia snapshot migrate`). For the terse type/default table see the [field reference](../../field-reference.md#snapshotreplication); for how-to guidance see [Snapshot replication](../../snapshot-replication.md).

A `SnapshotReplication` is **namespaced**: it lives alongside its source (mirroring [`Maintenance`](maintenance.md) and [`RepositoryReplication`](repository-replication.md)), and both ends are **real repository CRs** with their own passwords and formats — unlike `RepositoryReplication`'s passive blob mirror, the destination stays a first-class repository that can also take direct backups. Each copied snapshot is materialized as a [`Snapshot`](snapshot.md) CR (`origin: replicated`, pinned to the destination via `spec.repository`) in this CR's namespace, with **no ownerReference** back to it — deleting the `SnapshotReplication` never deletes the copies.

## `spec`

### `sourceRef` / `destinationRef`

The repositories to copy *from* and *into* — each a [`Repository`](repository.md) or [`ClusterRepository`](cluster-repository.md) ref (`kind` defaults to `Repository`; a `ClusterRepository` destination is the flagship consolidation setup). The two must name **different** repositories (self-replication is rejected at admission, as is a pair resolving to the same backend target). The source is opened **read-only**; the destination must be `Ready` and writable at run time.

### `schedule`

Cron and deterministic jitter for the replication runs — the same scheduling kernel as `Maintenance` (Jenkins-style `H` supported). There is no on-demand trigger.

### `selection`

Which snapshots to copy. Absent = every identity's full history (`kopia snapshot migrate --all`).

| Field | Meaning |
| --- | --- |
| `identities.include` / `identities.exclude` | Lists of matchers over the kopia identity triple. Each matcher sets any of `username` / `hostname` / `sourcePath` (at least one required — webhook-enforced); every **set** component must match. Components match with anchored globs: `*` = any run of characters, `?` = exactly one. A snapshot is selected when it matches any `include` (empty = everything) and no `exclude` — exclude wins. |
| `latestOnly` | `true` = copy only each selected identity's most recent snapshot (a cheap seed); default `false` = full history. |

Matching zero identities is a successful no-op, not an error. Incomplete (interrupted) source snapshots are never copied.

### `migrate`

Tuning for the underlying `kopia snapshot migrate`:

| Field | Meaning |
| --- | --- |
| `parallel` | Snapshots migrated concurrently (kopia default `1` — sequential; must be `>= 1` when set, webhook-enforced). The main knob for large first runs. |
| `policies` | Whether kopia **policy** objects ride along: `none` (default — Kopiur pins retention CR-side, so imported kopia policies are usually unwanted), `copy` (copy where absent), `copyOverwrite` (copy and overwrite). |

### `pruning`

What happens to already-made copies on later runs — exactly one of three externally-tagged modes; **absent = `none`**. Pruning only ever considers copies **this replication created**, never the destination's own directly-written snapshots.

| Mode | Meaning |
| --- | --- |
| `none: {}` *(default)* | Never prune; copies accumulate until deleted by hand. |
| `mirrorSource: {}` | Delete a copy whose `(identity, startTime)` has vanished from the source. Deliberately classified as **external** deletion so the destination's [mass-deletion breaker](repository.md) holds a bulk source-side vanish (ransomware at the source cannot empty the off-site copy in one wave). |
| `retention: { keepDaily: …, … }` | Independent GFS retention over the copies at the destination (same shape as a `SnapshotPolicy`'s; a keeps-nothing block is rejected). Stamped as an operator prune — bypasses the breaker. |

### `mover`

Mover (Job pod) overrides — resources, scheduling, security context. Inherits the source repository's `moverDefaults`. `inheritSecurityContextFrom` is rejected (there is no workload to inherit from). Replication movers run under the dedicated `kopiur-snapshot-replication-mover` ServiceAccount (they create/patch/delete the copy `Snapshot` CRs — a grant the ordinary mover deliberately lacks).

### `credentialProjection`

Opt in to [credential projection](../../movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) so the operator copies a `ClusterRepository` source's/destination's Secret into this namespace for the run. Both repositories' credentials are delivered independently (the destination's under a `KOPIUR_DEST_` env prefix) — the two repositories may use entirely different passwords and backends.

### `suspend`

Pause replication declaratively (default `false`) without deleting the CR.

## Out-of-band runs

Annotating a `SnapshotReplication` with `kopiur.home-operations.com/run-requested` (an RFC3339 timestamp) triggers a one-off copy pass. There is no `run-mode` companion — a replication has exactly one kind of run. The timestamp pins *which* request the status answers, so re-applying the same value is a no-op and a new timestamp starts a new run. The requested run flows through the same mover, gates (both repositories `Ready`, destination writable, `IdentityOverlap`) and single-flight rule as a cron slot, and — because it stamps `status.lastReplicated` on success — re-anchors the next scheduled slot. See [Run it now](../../snapshot-replication.md#run-it-now); `kubectl kopiur replication run` stamps the annotation for you.

/// warning | A malformed timestamp is refused at admission

The admission webhook rejects a `run-requested` value that is not RFC3339, naming the offending value and the fix — so in practice a malformed annotation never reaches the controller. An object annotated while the webhook was down degrades gracefully instead of stalling: the schedule keeps running, and the controller reports `Ready=False` with reason `InvalidRunRequest` on the next pass where **no cron slot is due** (a due slot's own report takes that one `Ready` write, so on a very frequent schedule the message appears once the replication next goes idle).

///

## `status`

### `phase`

Lifecycle phase: `Pending` (admitted, not yet run), `Replicating` (a mover Job is in flight), `Succeeded` (last run completed), `Failed` (last run failed; see conditions), or `Suspended`.

### `manualRun`

State of the most recent [annotation-requested run](#out-of-band-runs): the `requestedAt` value it answers, its `phase` (`Pending` while the replication is suspended, then `Running` → `Succeeded`/`Failed`), and the `completedAt` instant it reached a terminal phase. Absent until a run is requested.

### others

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` last reconciled, for staleness detection / kstatus. |
| `lastReplicated` | RFC3339 timestamp of the most recent successful run, backing the `LAST` print column. |
| `lastRun` | Counters from the most recent run: `identitiesSelected`, `snapshotsCopied`, `alreadyPresent` (idempotent skips), `failed`, `pruned`. |
| `conditions` | Standard `Ready`/`Reconciling`/`Stalled` for `kubectl wait`, plus gates like `WaitingForSourceRepository` / `WaitingForDestinationRepository` / `DestinationReadOnly` and the `IdentityOverlap` runtime guard. |
