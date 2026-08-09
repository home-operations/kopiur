# Snapshot replication

A **`SnapshotReplication`** copies selected **snapshots** — the manifests and the content they reference — from one repository into another on a schedule, wrapping `kopia snapshot migrate`. Unlike [repository replication](replication.md), which mirrors a repository's raw **blobs** to a passive destination backend, snapshot replication's source and destination are **both real repository CRs** (`Repository` or `ClusterRepository`), each with its own password, its own format, and — crucially — its own life: the destination can keep taking direct backups of its own while also receiving copies.

/// tip | When to reach for it

- **One off-site repository that is both a mirror target and a live backup target.** `RepositoryReplication` (`kopia repository sync-to`) copies blobs verbatim, so its destination must be a passive byte-for-byte mirror with exactly one writer — it cannot simultaneously be a repository other policies back up into (the sync would fight the direct writes over indexes and epochs). Snapshot replication writes through the destination's own front door, so the destination stays an ordinary first-class repository.
- **Consolidation.** Copy several team repositories into one shared off-site `ClusterRepository`.
- **Seeding.** Populate a new repository from an old one (optionally `latestOnly: true` for a cheap seed).
- **Selective copies.** Only some identities (e.g. every `pg-*` policy), or excluding scratch paths.

If all you want is a passive, byte-identical off-site mirror of one repository — same password, restore-ready as-is — [`RepositoryReplication`](replication.md) is simpler and cheaper. Reach for `SnapshotReplication` when the destination must be a repository in its own right.

///

## How it works

- **Namespaced**, living alongside its source (like `Maintenance` and `RepositoryReplication`). `sourceRef` and `destinationRef` each name a `Repository` or `ClusterRepository`; a `ClusterRepository` destination is the flagship consolidation setup. Replicating a repository into itself is rejected at admission.
- On each cron slot the controller launches a **mover Job** (`<name>-srepl-<unix>`; croner + deterministic jitter, single-flight — the same scheduling kernel `Maintenance` uses). The mover connects to the **source read-only**, connects to the destination normally, and runs `kopia snapshot migrate`.
- **Identity is preserved.** A copied snapshot keeps its `username@hostname:path`, its start/end times, and its description — at the destination it looks exactly like the snapshot it is a copy of.
- **Idempotent and incremental.** kopia keys migration on `(identity, startTime)`: a snapshot already present at the destination is skipped (`status.lastRun.alreadyPresent`), and unchanged content is deduplicated against what the destination already stores. Re-running is always safe.
- **Every run is post-verified.** `kopia snapshot migrate` exits 0 even when individual sources failed to migrate, so the mover independently re-lists the destination and **fails the run loudly** if any selected snapshot did not arrive — a green `Succeeded` phase means the copies are really there.
- **Each copy becomes a `Snapshot` CR** (`origin: replicated`) in the replication's namespace, pinned to the destination repository (`spec.repository`), with `deletionPolicy: Delete` — so the copies are first-class: visible to `kubectl get snapshots`, restorable, deletable through the CR like any other snapshot. The destination's catalog scan recognizes replicated rows and does **not** duplicate them as `discovered`.
- The copy CRs carry **no ownerReference** back to the `SnapshotReplication`: deleting the replication CR never deletes the copies. Only `spec.pruning` — or deleting the copy `Snapshot` CRs yourself — removes replicated data.

## Try it / minimal manifest

The apply-ready example is [`deploy/examples/39-snapshot-replication.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/39-snapshot-replication.yaml) — a destination `Repository` (off-site S3) with its **own** password Secret, plus the replication CR:

```yaml
--8<-- "deploy/examples/39-snapshot-replication.yaml:snapshot-replication"
```

Watch it:

```console
$ kubectl -n billing get snapshotreplications
NAME                     SOURCE        DESTINATION   SCHEDULE    PHASE       LAST   AGE
nas-primary-to-offsite   nas-primary   offsite-s3    0 6 * * *   Succeeded   2m     1d

$ kubectl -n billing get snapshots -l kopiur.home-operations.com/origin=replicated
```

`status.lastRun` carries the per-run counters: `identitiesSelected`, `snapshotsCopied`, `alreadyPresent`, `failed`, `pruned`. `kubectl kopiur status` and `kubectl kopiur doctor` render them too, and `kubectl kopiur snapshots list` shows where a replicated row was copied from (`status.copiedFrom`).

## The fields you'll change

| Field | What it does |
| --- | --- |
| `sourceRef` / `destinationRef` | The two repositories (`kind` defaults to `Repository`; both must exist, differ, and be `Ready`; the destination must be writable). |
| `schedule.cron` / `jitter` | When replication runs (Jenkins-style `H` supported). Run it **after** your backup window so each night's snapshots are there to copy. |
| `selection.identities.include` / `exclude` | Which kopia identities to copy — see [Selecting what to copy](#selecting-what-to-copy). Omit `selection` entirely to copy **every** identity's full history. |
| `selection.latestOnly` | `true` = only each identity's most recent snapshot (cheap seed); default `false` = full history. |
| `migrate.parallel` | Snapshots migrated concurrently (kopia default: 1, sequential) — the main knob for large first runs. |
| `migrate.policies` | Whether kopia **policies** ride along: `none` (default — a Kopiur-managed destination keeps retention CR-driven), `copy`, or `copyOverwrite`. |
| `pruning` | What happens to already-made copies on later runs: exactly one of `none` / `mirrorSource` / `retention` — see [Pruning](#pruning-the-copies). |
| `mover` | Per-run mover overrides (resources, scheduling, security context). Inherits the source repository's `moverDefaults`. `inheritSecurityContextFrom` is rejected here, as for `RepositoryReplication` — there is no workload to inherit from. |
| `credentialProjection` | Opt in to [credential projection](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) for a `ClusterRepository` source/destination whose Secret lives elsewhere. |
| `suspend` | Pause replication without deleting the CR. |

## Selecting what to copy

`selection.identities` takes `include` and `exclude` lists of **matchers**. Each matcher sets any of the three identity components — `username`, `hostname`, `sourcePath` — and every **set** component must match for the matcher to match (an unset component matches anything). At least one component must be set per matcher (webhook-enforced).

Components are matched with **anchored globs**: `*` matches any run of characters (including none), `?` matches exactly one — against the *whole* component, so `pg-*` matches `pg-main` but plain `pg` does not match `pg-main`. A snapshot is copied when it matches **any** `include` (an empty/absent `include` means "everything") **and no** `exclude` — exclude always wins.

```yaml
selection:
    identities:
        include:
            - username: "pg-*" # every postgres policy…
            - hostname: "media" # …plus everything from the media namespace
        exclude:
            - sourcePath: "/scratch/*" # …but never scratch paths
    latestOnly: false
```

Matching zero identities is a **successful no-op** (`NoIdentitiesMatched`), not an error — a fresh source simply has nothing to copy yet. Incomplete (interrupted) source snapshots are never copied.

## Pruning the copies

`pruning` is exactly one of three modes; **absent means `none`**. Whatever the mode, pruning only ever considers snapshots **this replication created** (the copy CRs it labels) — never the destination's own directly-written snapshots, and never another replication's copies.

| Mode | Behavior |
| --- | --- |
| `none` *(default)* | Never prune. Copies accumulate until you delete them (or the CR they became). |
| `mirrorSource` | Delete a copy when its `(identity, startTime)` has **vanished from the source** — the destination tracks the source's own retention. |
| `retention` | Independent GFS retention over the copies at the destination (`keepDaily`, `keepWeekly`, … — the same shape as a `SnapshotPolicy`'s), regardless of what the source still holds. A `retention:` block that keeps nothing is rejected at admission, exactly as on a policy. |

/// warning | `mirrorSource` meets the mass-deletion breaker — by design

`mirrorSource` deletes at the destination whatever disappeared at the source. To stop that from becoming an attack path, mirror-source deletes are deliberately **not** stamped as operator prunes: they count as **external** deletions against the destination repository's [mass-deletion breaker](repositories.md#deletionprotection--the-mass-deletion-circuit-breaker) (`deletionProtection.threshold`, default 10). A *bulk* source-side vanish — ransomware emptying the source, a fat-fingered mass delete — is therefore **held** at the destination instead of cascading into the off-site copy in one wave. The hold shows up as `DeletionHeld` on the affected copy `Snapshot` CRs and `MassDeletionHeld` on the destination repository, and is released with the breaker's normal timestamp acknowledgement once you've confirmed the deletions are intended.

`retention`-mode prunes, by contrast, **are** operator prunes (stamped `pruned-by: replication-retention`) and bypass the breaker — they are bounded, GFS-selected, and initiated by your own spec.

///

## Operational notes

- **Copies survive the CR.** No ownerReferences: deleting the `SnapshotReplication` leaves every copy `Snapshot` CR (and its data) in place. To remove copies, delete those `Snapshot` CRs (their `deletionPolicy: Delete` cascades to the destination repository, subject to its breaker) — or let a `pruning` mode do it.
- **Both repositories must be `Ready`, and the destination writable.** The controller holds runs with `WaitingForSourceRepository` / `WaitingForDestinationRepository` conditions, and a `mode: ReadOnly` destination stalls with `DestinationReadOnly`.
- **Different passwords are fine — and expected.** The source is opened read-only with its own credentials; the destination with its own. Nothing about the two repositories needs to match (different backends, formats, passwords all work).
- **Identity overlap with destination-side policies is guarded.** If a `SnapshotPolicy` writing *directly* into the destination produces the **same kopia identity** as a copied snapshot, the two histories would interleave. The webhook denies that combination outright when `pruning: mirrorSource` is set (the prune would eat the policy's own snapshots) and warns otherwise; at runtime the controller re-checks each pass and surfaces an `IdentityOverlap` condition (skipping the run under `mirrorSource`).
- **A dedicated mover ServiceAccount.** Replication movers create/patch/delete `Snapshot` CRs (the copies), which the ordinary backup mover must never be able to do — so they run as the dedicated `kopiur-snapshot-replication-mover` ServiceAccount with its own narrowly-scoped Role, generated alongside the rest of the [RBAC](rbac.md).
- **No destination maintenance behind your back.** The mover always disables kopia's auto-maintenance; the destination's own [`Maintenance`](maintenance.md) remains the only compaction that runs there.
- **Size the first run.** A full-history first replication of a large repository moves everything once (idempotent thereafter). Raise `migrate.parallel`, consider `latestOnly: true` for seeding, and note the mover Job's deadline can be tuned via `mover`/`failurePolicy` knobs on big estates.

## See also

- [Repository replication](replication.md) — the blob-level mirror, when the destination is a passive copy.
- [Multi-repository fan-out](backups.md#repositories--one-recipe-several-repositories-fan-out) — backing up into N repositories *directly* from one `SnapshotPolicy` (and why hooks + fan-out points you back here).
- [Repositories & backends](repositories.md) — the catalog, `deletionProtection`, `identityDefaults`.
- [Disaster recovery scenario](scenarios/disaster-recovery.md)
