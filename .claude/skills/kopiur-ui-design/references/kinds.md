# The nine kinds

Every CRD the console shows has a fixed identity: a colour token, a lucide glyph,
a navigation group, and a fixed set of facts its card carries. This file is the
lookup table; `primitives.md` says how the mark is drawn.

Never invent a tenth identity for a non-CRD thing (a PVC, a namespace, a Job, a
backend). Those are plain mono identifiers with no stripe and no chip.

## Identity

| Kind (CRD)            | Token slug (`--kind-…`)  | Glyph (lucide-react) | Nav group                           | Detail route                                               |
| --------------------- | ------------------------ | -------------------- | ----------------------------------- | ---------------------------------------------------------- |
| Repository            | `repository`             | `Database`           | Storage                             | `/repositories/repository/$name?namespace=`                |
| ClusterRepository     | `cluster-repository`     | `Server`             | Storage (listed under Repositories) | `/repositories/cluster-repository/$name`                   |
| Maintenance           | `maintenance`            | `Wrench`             | Storage                             | none — list rows only (also embedded in repository detail) |
| RepositoryReplication | `repository-replication` | `Copy`               | Storage (Replications)              | none — list rows only                                      |
| SnapshotReplication   | `snapshot-replication`   | `ArrowLeftRight`     | Storage (Replications)              | none — list rows only                                      |
| SnapshotPolicy        | `snapshot-policy`        | `ScrollText`         | Protection                          | `/policies/$ns/$name`                                      |
| SnapshotSchedule      | `snapshot-schedule`      | `CalendarClock`      | Protection                          | none — list rows only (also embedded in policy detail)     |
| Snapshot              | `snapshot`               | `Camera`             | Data                                | `/snapshots/$ns/$name`                                     |
| Restore               | `restore`                | `ArchiveRestore`     | Data                                | `/restores/$ns/$name`                                      |

Use the repository's `kindPath` from the wire for its URL segment; never derive it
from the display kind. A reference to a kind with no detail route is a mini card
that is **not a link** (see `primitives.md` → Object reference).

The kind name is always written exactly as the CRD kind (`SnapshotPolicy`, not
"Policy") in the small-caps label, because it is the word an operator types into
`kubectl get`. Navigation and page titles may use the plural short form
("Policies").

## Status source

Only some kinds publish a health. The pill on every other kind is built from a
**fact**, worded as the fact, never as an invented verdict.

| Kind                                       | Pill comes from                               | Words                                                                                   |
| ------------------------------------------ | --------------------------------------------- | --------------------------------------------------------------------------------------- |
| Repository, ClusterRepository              | `RepositorySummary.health: Health`            | Healthy · Degraded · Failed · Pending · Suspended · Unknown                             |
| Snapshot                                   | `SnapshotRow.phase: SnapshotPhaseView`        | Pending · Running · Succeeded · Failed · Deleting · Discovered · Unchanged · _raw word_ |
| Restore                                    | `RestoreRow.phase: RestorePhaseView`          | Pending · Resolving · Restoring · Completed · Failed · _raw word_                       |
| RepositoryReplication, SnapshotReplication | `phase: ReplicationPhaseView`                 | Pending · Replicating · Succeeded · Failed · Suspended · _raw word_                     |
| SnapshotPolicy                             | `suspended`, gates                            | Active · Suspended · (error gate ⇒ failed pill with the gate's reason)                  |
| SnapshotSchedule                           | `suspended`, `consecutiveFailures`            | Active · Suspended · "N failed runs" (failed pill)                                      |
| Maintenance                                | `quick/full.consecutiveFailures`, `lastRunAt` | OK · "N failed runs" · "never run" (failed pill)                                        |

Phase → pill state mapping:

| Pill state | Phases                                                                                  |
| ---------- | --------------------------------------------------------------------------------------- |
| healthy    | Succeeded, Completed, Unchanged, Active, OK                                             |
| pending    | Pending, Running, Resolving, Restoring, Replicating, Initializing, Deleting             |
| failed     | Failed, any "N failed runs", "never run", "never verified"                              |
| degraded   | Degraded                                                                                |
| suspended  | Suspended                                                                               |
| unknown    | Unknown, Discovered (not ours to judge), **any `{unknown: {raw}}` — show the raw word** |

Icon per state: healthy `CircleCheck` · failed `OctagonX` · degraded
`TriangleAlert` · pending `Clock` (in-flight phases use `CirclePlay`) · suspended
`CirclePause` · unknown `CircleHelp`.

## What each card shows

The three stat-strip numbers are the facts that answer "is this object doing its
job?" for that kind. Field names are the camelCase wire names in
`crates/ui-model/src/views.rs`.

| Kind                           | Meta line                                                                          | Stat 1                               | Stat 2                                            | Stat 3                                               |
| ------------------------------ | ---------------------------------------------------------------------------------- | ------------------------------------ | ------------------------------------------------- | ---------------------------------------------------- |
| Repository / ClusterRepository | `backend` · `mode` (· "admits all namespaces" / "admits N namespaces" for cluster) | Snapshots `snapshotCount`            | Stored `totalSizeBytes`                           | Last observed `lastObservedAt`                       |
| SnapshotPolicy                 | retention summary from `RetentionView` (only the set rules)                        | Live snapshots `activeSnapshotCount` | Last success `lastSuccessfulSnapshot`             | Last verified `lastVerified` (absent ⇒ loud _never_) |
| SnapshotSchedule               | `cron` (mono) · `timezone`                                                         | Next fire `nextFire`                 | Last fire `lastFire`                              | Failures `consecutiveFailures`                       |
| Snapshot                       | `origin` · pinned · `deletionPolicy` (only when set)                               | Size `sizeBytes`                     | Files `filesTotal`                                | Took (end − start)                                   |
| Restore                        | target: "into new claim `name`" / "into `pvc`"                                     | Restored `bytesRestored`             | Files `filesRestored`                             | Took (end − start)                                   |
| Maintenance                    | "managed by Repository `name`" when `managedByRepository`                          | Quick `quick.lastRunAt`              | Full `full.lastRunAt` (absent ⇒ loud _never run_) | Reclaimed `lastContentReclaimedBytes`                |
| RepositoryReplication          | "every blob → `destinationBackend`" · `cron`                                       | Last replicated `lastReplicated`     | Copied `lastReplicatedBytes`                      | Blobs `lastReplicatedBlobs`                          |
| SnapshotReplication            | "selected snapshots" · `cron`                                                      | Copied `snapshotsCopied`             | Already there `alreadyPresent`                    | Failed `failed`                                      |

`ClusterRepository.allowedNamespaceCount` is a count only when the repository
lists namespaces; when it admits all namespaces say **"admits all namespaces"**.
(The previous console printed "admits -1 namespaces" — never render the sentinel.)

## Relationships

Used by the flow lanes (`composites.md`), the detail-page trail, and card
"from / to" references.

| Kind                  | Upstream (from)                                                                            | Downstream (to)                                                                                                        |
| --------------------- | ------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------- |
| Repository            | —                                                                                          | policies writing here (`RepositoryDetail.policies`), replications out (`replicationsOut`), maintenance (`maintenance`) |
| ClusterRepository     | —                                                                                          | the same, across admitted namespaces                                                                                   |
| SnapshotPolicy        | schedules that fire it (`PolicyDetail.schedules`)                                          | repositories (`PolicyRow.repositories`), recent snapshots                                                              |
| SnapshotSchedule      | —                                                                                          | the policy (`policy`) or "policies matching `policySelector`"                                                          |
| Snapshot              | policy (`policy`), repository (`repository`), source copy (`lineage.copiedFromRepository`) | copies (`lineage.copies`), restores that read it                                                                       |
| Restore               | snapshot (`source.snapshot`) or policy/identity source                                     | target PVC (plain mono, not a kind)                                                                                    |
| Maintenance           | —                                                                                          | its repository (`repository`)                                                                                          |
| RepositoryReplication | source repository                                                                          | destination backend (plain mono)                                                                                       |
| SnapshotReplication   | source repository                                                                          | destination repository                                                                                                 |

Trail for a detail header, top-down: Repository › SnapshotPolicy › Snapshot ›
Restore. Show only the hops the object actually has.
