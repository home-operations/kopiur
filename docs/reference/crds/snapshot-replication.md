# SnapshotReplication

A `SnapshotReplication` copies selected **snapshots**, meaning their manifests and the content they reference, from one repository into another on a schedule. It uses `kopia snapshot migrate`.

For the short type-and-default table see the [field reference](../../field-reference.md#snapshotreplication). For task guidance see [Snapshot replication](../../snapshot-replication.md).

A `SnapshotReplication` is **namespaced** and lives beside its source, the same way [`Maintenance`](maintenance.md) and [`RepositoryReplication`](repository-replication.md) do. Both ends are **real repository objects** with their own passwords and formats. That is the difference from `RepositoryReplication`'s passive blob mirror: here the destination stays a first-class repository that can also take direct backups.

Each copied snapshot becomes a [`Snapshot`](snapshot.md) object in this object's namespace, with `origin: replicated` and `spec.repository` pinned to the destination. The copies carry **no ownerReference** back to the `SnapshotReplication`, so deleting the `SnapshotReplication` never deletes them.

## `spec`

### `sourceRef` / `destinationRef`

The repositories to copy *from* and *into*. Each is a reference to a [`Repository`](repository.md) or a [`ClusterRepository`](cluster-repository.md), and `kind` defaults to `Repository`. A `ClusterRepository` destination is the main consolidation setup.

The two must name **different** repositories. Self-replication is rejected at admission, and so is a pair that resolves to the same backend target.

The source is opened **read-only**. The destination must be `Ready` and writable when the run starts.

### `schedule`

Cron and deterministic jitter for the replication runs, using the same scheduling machinery as `Maintenance`, including Jenkins-style `H`. There is no on-demand trigger here.

### `selection`

Which snapshots to copy. Leaving it out copies every identity's full history, which is `kopia snapshot migrate --all`.

| Field | Meaning |
| --- | --- |
| `identities.include` / `identities.exclude` | Lists of matchers over the kopia identity triple. Each matcher sets any of `username` / `hostname` / `sourcePath` (at least one required — webhook-enforced); every **set** component must match. Components match with anchored globs: `*` = any run of characters, `?` = exactly one. A snapshot is selected when it matches any `include` (empty = everything) and no `exclude` — exclude wins. |
| `latestOnly` | `true` = copy only each selected identity's most recent snapshot (a cheap seed); default `false` = full history. |

Matching zero identities is a successful no-op, not an error. Source snapshots that are incomplete, meaning interrupted, are never copied.

### `migrate`

Tuning for the underlying `kopia snapshot migrate`:

| Field | Meaning |
| --- | --- |
| `parallel` | Snapshots migrated concurrently (kopia default `1` — sequential; must be `>= 1` when set, webhook-enforced). The main knob for large first runs. |
| `policies` | Whether kopia **policy** objects ride along: `none` (default — Kopiur pins retention CR-side, so imported kopia policies are usually unwanted), `copy` (copy where absent), `copyOverwrite` (copy and overwrite). |
| `throttle.source` / `throttle.destination` | Bandwidth and ops caps for this replication's runs, one block **per side**. Each is a [`throttle`](shared-types.md#moverdefaults) block holding `uploadBytesPerSecond`, `downloadBytesPerSecond`, `readOpsPerSecond` and `writeOpsPerSecond`; every value you set must be `>= 1`, which the webhook enforces. Each side **overrides that side's repository `moverDefaults.throttle` one field at a time**: a value set here wins, and a value left unset keeps the repository's own. Leave both out and each side uses its repository's defaults. See [Throttling a replication](../../snapshot-replication.md#throttling-a-replication). |

`kopia snapshot migrate` has **no speed flags of its own**, so `throttle` is not passed to the migrate command. Kopiur applies each side with `kopia repository throttle set` on that side's connection *before* the migrate runs, and kopia stores the limits in that connection's config for the migrate to pick up.

Two repositories means two connections and two independent blocks, so the source cap says nothing about the destination and the reverse. Capping the read-only source connection works, because kopia accepts `throttle set` there, so a `mode: ReadOnly` source is throttled like any other.

### `pruning`

What happens to copies you already made when later runs go through. It is a single-key object with exactly three modes, and **leaving it out means `none`**.

Pruning only ever considers copies **this replication created**. It never touches snapshots written directly to the destination.

| Mode | Meaning |
| --- | --- |
| `none: {}` *(default)* | Never prune; copies accumulate until deleted by hand. |
| `mirrorSource: {}` | Delete a copy whose `(identity, startTime)` has vanished from the source. Kopiur deliberately classifies this as an **external** deletion, so the destination's [mass-deletion breaker](repository.md) holds a bulk source-side vanish. Ransomware at the source therefore cannot empty the off-site copy in one wave. |
| `retention: { keepDaily: …, … }` | Independent GFS retention over the copies at the destination, using the same shape as a `SnapshotPolicy`'s. A block that keeps nothing is rejected. Kopiur stamps these as operator prunes, so they bypass the breaker. |

### `mover`

Overrides for the mover Job pod: resources, scheduling and security context. It inherits the source repository's `moverDefaults`.

`inheritSecurityContextFrom` is rejected here, because there is no workload to inherit from.

Replication movers run under the dedicated `kopiur-snapshot-replication-mover` ServiceAccount, because they create, patch and delete the copy `Snapshot` objects. The ordinary mover deliberately lacks that grant.

### `credentialProjection`

Opt in to [credential projection](../../movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) so the operator copies a `ClusterRepository` source's or destination's Secret into this namespace for the run.

Both repositories' credentials are delivered independently, with the destination's under a `KOPIUR_DEST_` environment prefix, so the two repositories may use entirely different passwords and backends.

### `suspend`

Pause replication from the manifest, default `false`, without deleting the object.

## Out-of-band runs

To trigger a one-off copy pass, annotate a `SnapshotReplication` with `kopiur.home-operations.com/run-requested` set to an RFC 3339 timestamp. There is no `run-mode` companion, because a replication has only one kind of run.

The timestamp identifies *which* request the status answers, so re-applying the same value does nothing and a new timestamp starts a new run.

The requested run goes through the same mover, the same gates and the same single-flight rule as a cron slot. The gates are: both repositories `Ready`, the destination writable, and `IdentityOverlap`. Because it stamps `status.lastReplicated` on success, it also re-anchors the next scheduled slot. See [Run it now](../../snapshot-replication.md#run-it-now). `kubectl kopiur replication run` stamps the annotation for you.

/// warning | A malformed timestamp is refused at admission

The admission webhook rejects a `run-requested` value that is not RFC 3339, naming the bad value and the fix, so in practice a malformed annotation never reaches the controller.

An object annotated while the webhook was down degrades gracefully instead of stalling. The schedule keeps running, and the controller reports `Ready=False` with reason `InvalidRunRequest` on the next pass where **no cron slot is due**. A due slot's own report takes that one `Ready` write, so on a very frequent schedule the message appears once the replication next goes idle.

///

## `status`

### `phase`

The lifecycle phase: `Pending` (admitted, not yet run), `Replicating` (a mover Job is in flight), `Succeeded` (the last run completed), `Failed` (the last run failed; see conditions), or `Suspended`.

### `manualRun`

The state of the most recent [annotation-requested run](#out-of-band-runs): the `requestedAt` value it answers, its `phase`, and the `completedAt` instant it reached a terminal phase. It is absent until a run is requested.

The phase is `Pending` while the replication is suspended, or while the request waits behind an in-flight run. It then moves to `Running`, and finally to `Succeeded` or `Failed`.

### others

| Field | Meaning |
| --- | --- |
| `observedGeneration` | The `metadata.generation` last reconciled, for staleness detection and kstatus. |
| `lastReplicated` | RFC 3339 timestamp of the most recent successful run, behind the `LAST` print column. |
| `lastRun` | Counters from the most recent run: `identitiesSelected`, `snapshotsCopied`, `alreadyPresent` (idempotent skips), `failed`, `pruned`. |
| `conditions` | Standard `Ready`/`Reconciling`/`Stalled` for `kubectl wait`, plus gates like `WaitingForSourceRepository` / `WaitingForDestinationRepository` / `DestinationReadOnly` and the `IdentityOverlap` runtime guard. |
