import { describe, expect, it } from "vitest";

import type { MaintenanceRow, RestoreRow, ReplicationsView, SnapshotRow } from "../api/types";
import { activity } from "./activity";

const snapshot = (name: string, over: Partial<SnapshotRow> = {}): SnapshotRow => ({
  namespace: "media",
  name,
  phase: "succeeded",
  policy: "photos",
  repository: "Repository/media/nas",
  startTime: "2026-10-08T10:00:00Z",
  endTime: "2026-10-08T10:00:30Z",
  sizeBytes: 2048,
  pinned: false,
  ...over,
});

const restore: RestoreRow = {
  namespace: "media",
  name: "bring-back",
  phase: "failed",
  targetKind: "pvc",
  repository: "Repository/media/nas",
  startTime: "2026-10-08T11:00:00Z",
  claims: [],
};

const replications: ReplicationsView = {
  repository: [
    {
      namespace: "kopiur-dev",
      name: "offsite-blobs",
      source: "Repository/kopiur-dev/dev-repo",
      destinationBackend: "s3",
      cron: "0 */6 * * *",
      suspended: false,
      phase: "succeeded",
      lastReplicated: "2026-10-08T09:00:00Z",
    },
  ],
  snapshot: [
    {
      namespace: "kopiur-dev",
      name: "to-cluster",
      source: "Repository/kopiur-dev/dev-repo",
      destination: "ClusterRepository/shared-list",
      cron: "0 */6 * * *",
      suspended: false,
      phase: null,
      lastReplicated: null,
    },
  ],
};

const maintenance: MaintenanceRow = {
  namespace: "media",
  name: "nas",
  repository: "Repository/media/nas",
  managedByRepository: true,
  quick: { lastRunAt: "2026-10-08T08:00:00Z", consecutiveFailures: 0 },
  full: { lastRunAt: "2026-10-07T03:00:00Z", consecutiveFailures: 2 },
};

describe("activity", () => {
  it("merges every kind of run into one timeline, newest first", () => {
    const items = activity({
      snapshots: [snapshot("photos-1")],
      restores: [restore],
      replications,
      maintenance: [maintenance],
    });
    expect(items.map((i) => [i.target.kind, i.target.name, i.at])).toEqual([
      ["restore", "bring-back", "2026-10-08T11:00:00Z"],
      ["snapshot", "photos-1", "2026-10-08T10:00:30Z"],
      ["repositoryReplication", "offsite-blobs", "2026-10-08T09:00:00Z"],
      ["maintenance", "nas", "2026-10-08T08:00:00Z"],
      ["maintenance", "nas", "2026-10-07T03:00:00Z"],
    ]);
  });

  it("says each run's outcome in its own word", () => {
    const items = activity({
      snapshots: [snapshot("photos-1")],
      restores: [restore],
      maintenance: [maintenance],
    });
    const words = Object.fromEntries(items.map((i) => [`${i.target.kind}:${i.what}`, i.lamp.word]));
    expect(items.find((i) => i.target.kind === "restore")?.lamp.key).toBe("failed");
    expect(items.find((i) => i.target.kind === "snapshot")?.lamp.key).toBe("healthy");
    expect(words["maintenance:Quick maintenance of nas"]).toBe("Succeeded");
    expect(words["maintenance:Full maintenance of nas"]).toBe("Failed");
  });

  it("describes a snapshot by what it backed up, where to, and how big", () => {
    const [item] = activity({ snapshots: [snapshot("photos-1")] });
    expect(item?.what).toBe("photos → nas · 2.0 KiB · took 30s");
  });

  it("leaves out what has never run", () => {
    const items = activity({
      snapshots: [snapshot("never", { startTime: null, endTime: null })],
      replications,
      maintenance: [
        { ...maintenance, quick: { consecutiveFailures: 0 }, full: { consecutiveFailures: 0 } },
      ],
    });
    expect(items.map((i) => i.target.name)).toEqual(["offsite-blobs"]);
  });
});
