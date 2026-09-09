/**
 * The two replication kinds, read as one ledger.
 *
 * `GET /api/v1/replications` answers `ReplicationsView { repository, snapshot }`
 * — two arrays with different row types, because the two kinds copy different
 * things: a `RepositoryReplication` syncs a repository's blobs to a bare
 * backend, a `SnapshotReplication` migrates selected snapshots between
 * repository CRs. An operator asking "is anything falling behind?" wants one
 * list, so this projects both onto a common row while keeping every field
 * that only one of them has.
 *
 * The kind is **structural**, not parsed: neither row type carries a kind
 * field, so which array a row came from is the only thing that knows, and it
 * is recorded on the row as both the CRD kind (for display) and the token
 * `ReplicationRunBody.kind` takes.
 */

import type {
  Health,
  ReplicationPhaseView,
  ReplicationsView,
  RepositoryReplicationRow,
  SnapshotReplicationRow,
} from "../api/types";
import { unknownVariant } from "../util/assertNever";
import { EMPTY_CELL, relativeTime } from "../util/format";

/** The token `ReplicationRunBody.kind` accepts for each CRD. */
export type ReplicationKindToken = "replication" | "snapshot-replication";

/** The counters only a `SnapshotReplication` run reports. */
interface SnapshotRunCounters {
  identitiesSelected?: number | null | undefined;
  snapshotsCopied?: number | null | undefined;
  alreadyPresent?: number | null | undefined;
  failed?: number | null | undefined;
  pruned?: number | null | undefined;
}

/** One replication of either kind, as the shared ledger reads it. */
export interface ReplicationRow {
  /** Stable React key, unique across the two kinds. */
  id: string;
  /** The CRD kind, for display. */
  kind: "RepositoryReplication" | "SnapshotReplication";
  /** The token the run action sends. */
  kindToken: ReplicationKindToken;
  namespace: string;
  name: string;
  /** The repository blobs or snapshots are read from. */
  source: string;
  /** Where the copy lands: a backend variant, or another repository. */
  destination: string;
  /**
   * Whether {@link destination} names a repository CR (a snapshot copy) or a
   * bare backend (a blob sync). Only a `SnapshotReplication` can write into a
   * repository — a `RepositoryReplication`'s destination is not a CR at all.
   */
  destinationIsRepository: boolean;
  cron: string;
  suspended: boolean;
  phase: ReplicationPhaseView | null | undefined;
  lastReplicated: string | null | undefined;
  /** `SnapshotReplication` run counters; absent for a blob sync. */
  counters?: SnapshotRunCounters | undefined;
}

/**
 * Both arrays as one list: repository syncs first, then snapshot copies, each
 * block by namespace then name.
 *
 * The order is fixed rather than data-dependent so a poll that returns the
 * same objects never reshuffles the table under the reader's cursor — the same
 * reason the two handlers sort their own arrays.
 */
export function replicationRows(view: ReplicationsView): ReplicationRow[] {
  const repository = [...view.repository].sort(byNamespaceThenName).map(fromRepositoryRow);
  const snapshot = [...view.snapshot].sort(byNamespaceThenName).map(fromSnapshotRow);
  return [...repository, ...snapshot];
}

function byNamespaceThenName(
  a: { namespace: string; name: string },
  b: { namespace: string; name: string },
): number {
  return a.namespace.localeCompare(b.namespace) || a.name.localeCompare(b.name);
}

function fromRepositoryRow(row: RepositoryReplicationRow): ReplicationRow {
  return {
    id: `RepositoryReplication/${row.namespace}/${row.name}`,
    kind: "RepositoryReplication",
    kindToken: "replication",
    namespace: row.namespace,
    name: row.name,
    source: row.source,
    // The handler already falls back to the spec's backend variant when the
    // controller has not mirrored one; only a row with neither lands here.
    destination: nonEmpty(row.destinationBackend) ?? EMPTY_CELL,
    destinationIsRepository: false,
    cron: row.cron,
    suspended: row.suspended,
    phase: row.phase,
    lastReplicated: row.lastReplicated,
  };
}

function fromSnapshotRow(row: SnapshotReplicationRow): ReplicationRow {
  return {
    id: `SnapshotReplication/${row.namespace}/${row.name}`,
    kind: "SnapshotReplication",
    kindToken: "snapshot-replication",
    namespace: row.namespace,
    name: row.name,
    source: row.source,
    destination: row.destination,
    destinationIsRepository: true,
    cron: row.cron,
    suspended: row.suspended,
    phase: row.phase,
    lastReplicated: row.lastReplicated,
    counters: {
      identitiesSelected: row.identitiesSelected,
      snapshotsCopied: row.snapshotsCopied,
      alreadyPresent: row.alreadyPresent,
      failed: row.failed,
      pruned: row.pruned,
    },
  };
}

function nonEmpty(value: string | null | undefined): string | undefined {
  return value !== null && value !== undefined && value.length > 0 ? value : undefined;
}

/**
 * `status.phase` as a word, narrowed in the shape the ui-model doc prescribes
 * (addenda item 11). An absent phase is the empty cell; a phase from a newer
 * operator is that operator's own word.
 */
export function replicationPhaseLabel(phase: ReplicationPhaseView | null | undefined): string {
  if (phase === null || phase === undefined) {
    return EMPTY_CELL;
  }
  if (typeof phase === "string") {
    switch (phase) {
      case "pending":
        return "Pending";
      case "replicating":
        return "Replicating";
      case "succeeded":
        return "Succeeded";
      case "failed":
        return "Failed";
      case "suspended":
        return "Suspended";
      default:
        return unknownVariant(phase, "ReplicationPhaseView");
    }
  }
  return phase.unknown.raw;
}

/**
 * The lamp for a replication.
 *
 * Unlike a repository, a replication row carries no `Health` — the backend
 * computes one only for the topology graph's edges. This mirrors that
 * function exactly (`crates/ui/src/api/graph.rs::replication_edge_health`), so
 * an edge in the topology and a row here cannot disagree about the same
 * object: suspension first, a phase this bundle cannot interpret is unknown,
 * and no absence ever reads as healthy.
 */
export function replicationHealth(
  phase: ReplicationPhaseView | null | undefined,
  suspended: boolean,
): Health {
  if (suspended) {
    return "suspended";
  }
  if (phase === null || phase === undefined) {
    return "unknown";
  }
  if (typeof phase === "string") {
    switch (phase) {
      case "succeeded":
        return "healthy";
      case "replicating":
      case "pending":
        return "pending";
      case "failed":
        return "failed";
      case "suspended":
        return "suspended";
      default:
        unknownVariant(phase, "ReplicationPhaseView");
        return "unknown";
    }
  }
  return "unknown";
}

/**
 * How far behind the copy is: how long ago the last successful replication
 * finished.
 *
 * A replication that has never succeeded says "never", not the empty cell —
 * an absent `lastReplicated` on a scheduled copy is the single most alarming
 * fact this table can carry, and `-` would bury it.
 */
export function replicationLag(
  lastReplicated: string | null | undefined,
  now: Date = new Date(),
): string {
  if (lastReplicated === null || lastReplicated === undefined || lastReplicated.length === 0) {
    return "never";
  }
  return relativeTime(lastReplicated, now);
}

/**
 * What the last run did, for a snapshot copy — or `null` for a blob sync,
 * whose only two counters (`lastReplicatedBytes`, `lastReplicatedBlobs`) are
 * in the wiring ratchet's never-written set. `null` is not "nothing to say":
 * it tells the caller to render "not reported" instead of a summary that
 * would be permanently empty.
 *
 * A counter the run did not report is omitted rather than printed as `0`.
 */
export function lastRunSummary(row: ReplicationRow): string | null {
  const counters = row.counters;
  if (counters === undefined) {
    return null;
  }
  const parts = [
    count(counters.snapshotsCopied, "copied"),
    count(counters.alreadyPresent, "already present"),
    count(counters.failed, "failed"),
    count(counters.pruned, "pruned"),
    counters.identitiesSelected !== null && counters.identitiesSelected !== undefined
      ? `from ${String(counters.identitiesSelected)} identities`
      : null,
  ].filter((part): part is string => part !== null);
  return parts.length === 0 ? EMPTY_CELL : parts.join(", ");
}

function count(value: number | null | undefined, word: string): string | null {
  return value === null || value === undefined ? null : `${String(value)} ${word}`;
}
