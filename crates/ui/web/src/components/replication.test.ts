import { describe, expect, it } from "vitest";

import type {
  ReplicationsView,
  RepositoryReplicationRow,
  SnapshotReplicationRow,
} from "../api/types";
import { nth } from "../test-utils";
import { EMPTY_CELL } from "../util/format";
import {
  lastRunSummary,
  replicationHealth,
  replicationLag,
  replicationPhaseLabel,
  replicationRows,
} from "./replication";

function repositoryRow(over: Partial<RepositoryReplicationRow> = {}): RepositoryReplicationRow {
  return {
    namespace: "media",
    name: "blobsync",
    source: "Repository/media/nas",
    destinationBackend: "S3",
    cron: "0 5 * * *",
    suspended: false,
    phase: "succeeded",
    lastReplicated: "2026-09-08T05:12:00Z",
    nextScheduledAt: null,
    lastReplicatedBytes: null,
    lastReplicatedBlobs: null,
    ...over,
  };
}

function snapshotRow(over: Partial<SnapshotReplicationRow> = {}): SnapshotReplicationRow {
  return {
    namespace: "media",
    name: "offsite",
    source: "Repository/media/nas",
    destination: "ClusterRepository/shared",
    cron: "0 3 * * *",
    suspended: false,
    phase: "replicating",
    lastReplicated: "2026-09-08T03:10:00Z",
    identitiesSelected: 3,
    snapshotsCopied: 7,
    alreadyPresent: 120,
    failed: 1,
    pruned: 2,
    ...over,
  };
}

const view: ReplicationsView = { repository: [repositoryRow()], snapshot: [snapshotRow()] };

describe("replicationRows", () => {
  it("puts both kinds in one table, each tagged with the kind it came from", () => {
    const rows = replicationRows(view);
    expect(rows).toHaveLength(2);
    expect(rows.map((r) => r.kind)).toEqual(["RepositoryReplication", "SnapshotReplication"]);
  });

  it("carries the token the run action needs, from the array the row came from", () => {
    // `ReplicationRunBody.kind` takes `replication` or `snapshot-replication`
    // (crates/ui/src/actions/mod.rs::REPLICATION_KINDS). The rows themselves
    // carry no kind field, so it is structural — which array, not a parse.
    const rows = replicationRows(view);
    expect(rows.map((r) => r.kindToken)).toEqual(["replication", "snapshot-replication"]);
  });

  it("says what a repository sync writes to and what a snapshot copy writes into", () => {
    const rows = replicationRows(view);
    expect(nth(rows, 0).destination).toBe("S3");
    expect(nth(rows, 0).destinationIsRepository).toBe(false);
    expect(nth(rows, 1).destination).toBe("ClusterRepository/shared");
    expect(nth(rows, 1).destinationIsRepository).toBe(true);
  });

  it("falls back to the empty cell when even the spec backend is missing", () => {
    const rows = replicationRows({
      repository: [repositoryRow({ destinationBackend: null })],
      snapshot: [],
    });
    expect(nth(rows, 0).destination).toBe(EMPTY_CELL);
  });

  it("sorts stably by kind, then namespace, then name", () => {
    const rows = replicationRows({
      repository: [
        repositoryRow({ namespace: "prod", name: "a" }),
        repositoryRow({ namespace: "media", name: "z" }),
        repositoryRow({ namespace: "media", name: "b" }),
      ],
      snapshot: [],
    });
    expect(rows.map((r) => `${r.namespace}/${r.name}`)).toEqual(["media/b", "media/z", "prod/a"]);
  });

  it("gives every row an id unique across the two kinds", () => {
    const rows = replicationRows({
      repository: [repositoryRow({ name: "same" })],
      snapshot: [snapshotRow({ name: "same" })],
    });
    expect(new Set(rows.map((r) => r.id)).size).toBe(2);
  });
});

describe("replicationPhaseLabel", () => {
  it("titles each unit variant", () => {
    expect(replicationPhaseLabel("pending")).toBe("Pending");
    expect(replicationPhaseLabel("replicating")).toBe("Replicating");
    expect(replicationPhaseLabel("succeeded")).toBe("Succeeded");
    expect(replicationPhaseLabel("failed")).toBe("Failed");
    expect(replicationPhaseLabel("suspended")).toBe("Suspended");
  });

  it("renders a newer operator's phase as the raw word", () => {
    expect(replicationPhaseLabel({ unknown: { raw: "Reconciling" } })).toBe("Reconciling");
  });

  it("is the empty cell with no phase at all", () => {
    expect(replicationPhaseLabel(null)).toBe(EMPTY_CELL);
  });
});

describe("replicationHealth", () => {
  it("mirrors the server's own edge colouring, suspension first", () => {
    // crates/ui/src/api/graph.rs::replication_edge_health
    expect(replicationHealth("succeeded", false)).toBe("healthy");
    expect(replicationHealth("replicating", false)).toBe("pending");
    expect(replicationHealth("pending", false)).toBe("pending");
    expect(replicationHealth("failed", false)).toBe("failed");
    expect(replicationHealth("suspended", false)).toBe("suspended");
    expect(replicationHealth("succeeded", true)).toBe("suspended");
    expect(replicationHealth("failed", true)).toBe("suspended");
  });

  it("never reads an absent or unrecognised phase as healthy", () => {
    expect(replicationHealth(null, false)).toBe("unknown");
    expect(replicationHealth({ unknown: { raw: "Reconciling" } }, false)).toBe("unknown");
  });
});

describe("replicationLag", () => {
  const now = new Date("2026-09-08T06:12:00Z");

  it("is how long ago the last successful copy was", () => {
    expect(replicationLag("2026-09-08T05:12:00Z", now)).toBe("1h ago");
  });

  it("says never rather than an empty cell when nothing has replicated yet", () => {
    expect(replicationLag(null, now)).toBe("never");
  });
});

describe("lastRunSummary", () => {
  it("counts what a snapshot copy actually did", () => {
    const rows = replicationRows({ repository: [], snapshot: [snapshotRow()] });
    expect(lastRunSummary(nth(rows, 0))).toBe(
      "7 copied, 120 already present, 1 failed, 2 pruned, from 3 identities",
    );
  });

  it("omits a counter the run did not report rather than printing a zero", () => {
    const rows = replicationRows({
      repository: [],
      snapshot: [
        snapshotRow({
          identitiesSelected: null,
          snapshotsCopied: 4,
          alreadyPresent: null,
          failed: null,
          pruned: null,
        }),
      ],
    });
    expect(lastRunSummary(nth(rows, 0))).toBe("4 copied");
  });

  it("is the empty cell for a snapshot copy that has not run", () => {
    const rows = replicationRows({
      repository: [],
      snapshot: [
        snapshotRow({
          identitiesSelected: null,
          snapshotsCopied: null,
          alreadyPresent: null,
          failed: null,
          pruned: null,
        }),
      ],
    });
    expect(lastRunSummary(nth(rows, 0))).toBe(EMPTY_CELL);
  });

  it("is null for a repository sync, whose two counters no controller writes", () => {
    // Both `lastReplicatedBytes` and `lastReplicatedBlobs` are in the wiring
    // ratchet's never-written set, so the caller must render "not reported"
    // rather than a summary that would always be the empty cell.
    const rows = replicationRows({ repository: [repositoryRow()], snapshot: [] });
    expect(lastRunSummary(nth(rows, 0))).toBeNull();
  });
});
