import { describe, expect, it } from "vitest";

import type {
  MaintenanceRow,
  PolicyRow,
  RepositoryReplicationRow,
  RepositorySummary,
  RestoreRow,
  ScheduleRow,
  SnapshotReplicationRow,
  SnapshotRow,
} from "../api/types";
import { cardFacts } from "./objectCard";

const NOW = new Date("2026-10-08T12:00:00Z");

const repo: RepositorySummary = {
  kind: "Repository",
  kindPath: "repository",
  name: "nas-offsite",
  namespace: "media",
  phase: "failed",
  health: "failed",
  backend: "S3",
  mode: "ReadWrite",
  serverBacked: false,
  suspended: false,
  snapshotCount: 41,
  totalSizeBytes: 19_756_849_152,
  lastObservedAt: "2026-10-08T09:00:00Z",
};
const policy: PolicyRow = {
  namespace: "kopiur-dev",
  name: "app-data",
  repositories: ["Repository/kopiur-dev/dev-repo"],
  multiRepo: false,
  suspended: false,
  lastSuccessfulSnapshot: "2026-10-08T11:56:00Z",
  activeSnapshotCount: 4,
};
const schedule: ScheduleRow = {
  namespace: "kopiur-dev",
  name: "app-data-frequent",
  policy: "app-data",
  cron: "H * * * *",
  suspended: false,
  nextFire: "2026-10-08T12:39:00Z",
  lastFire: "2026-10-08T11:39:00Z",
  consecutiveFailures: 0,
};
const snapshot: SnapshotRow = {
  namespace: "kopiur-dev",
  name: "app-data-manual",
  phase: "succeeded",
  origin: "manual",
  pinned: true,
  sizeBytes: 2_097_152,
  filesTotal: 10,
  startTime: "2026-10-08T11:00:00Z",
  endTime: "2026-10-08T11:00:04Z",
};
const restore: RestoreRow = {
  namespace: "kopiur-dev",
  name: "app-data-restore",
  phase: "completed",
  targetKind: "Pvc",
  bytesRestored: 2_097_152,
  filesRestored: 10,
  startTime: "2026-10-08T11:00:00Z",
  endTime: "2026-10-08T11:00:16Z",
  claims: [{ pvc: "app-data-restored", phase: "Populated" }],
};
const maintenance: MaintenanceRow = {
  namespace: "kopiur-dev",
  name: "dev-repo",
  repository: "Repository/kopiur-dev/dev-repo",
  managedByRepository: true,
  quick: { lastRunAt: "2026-10-08T06:00:00Z", consecutiveFailures: 2 },
  full: { consecutiveFailures: 0 },
};
const repoRepl: RepositoryReplicationRow = {
  namespace: "kopiur-dev",
  name: "dev-repo-offsite",
  source: "Repository/kopiur-dev/dev-repo",
  destinationBackend: "S3",
  cron: "0 3 * * *",
  suspended: false,
  phase: "replicating",
  lastReplicated: "2026-10-07T03:00:00Z",
  lastReplicatedBytes: 1_288_490_189,
  lastReplicatedBlobs: 3104,
};
const snapRepl: SnapshotReplicationRow = {
  namespace: "kopiur-dev",
  name: "app-data-to-cluster",
  source: "Repository/kopiur-dev/dev-repo",
  destination: "ClusterRepository/dev-cluster-repo",
  cron: "15 * * * *",
  suspended: false,
  phase: "succeeded",
  snapshotsCopied: 3,
  alreadyPresent: 1,
  failed: 0,
};

const labels = (stats: readonly { label: string }[]) => stats.map((s) => s.label);

describe("cardFacts — each kind carries the three facts that say whether it is doing its job", () => {
  it("repository: snapshots, stored, last observed; links to its detail", () => {
    const f = cardFacts({ kind: "repository", row: repo }, NOW);
    expect(labels(f.stats)).toEqual(["Snapshots", "Stored", "Last observed"]);
    expect(f.meta).toBe("S3 · ReadWrite");
    expect(f.lamp.key).toBe("failed");
    expect(f.to).toBe("/repositories/repository/nas-offsite?namespace=media");
  });

  it("cluster repository: says which namespaces it admits, never a sentinel", () => {
    const f = cardFacts(
      {
        kind: "clusterRepository",
        row: {
          ...repo,
          kind: "ClusterRepository",
          kindPath: "cluster-repository",
          namespace: null,
          admits: "all",
        },
      },
      NOW,
    );
    expect(f.meta).toContain("admits all namespaces");
    expect(f.to).toBe("/repositories/cluster-repository/nas-offsite");
  });

  it("policy: a policy never verified says so loudly", () => {
    const f = cardFacts({ kind: "snapshotPolicy", row: policy }, NOW);
    expect(labels(f.stats)).toEqual(["Live snapshots", "Last success", "Last verified"]);
    expect(f.stats[2].value).toEqual({ absent: "loud", text: "never verified" });
    expect(f.lamp.word).toBe("Active");
  });

  it("schedule: next fire, last fire, failures; a failing one wears its failure count", () => {
    const f = cardFacts(
      { kind: "snapshotSchedule", row: { ...schedule, consecutiveFailures: 3 } },
      NOW,
    );
    expect(labels(f.stats)).toEqual(["Next fire", "Last fire", "Failures"]);
    expect(f.lamp).toMatchObject({ key: "failed", word: "3 failed runs" });
    expect(f.to).toBeUndefined();
  });

  it("snapshot: size, files, took — and an unrecognised phase is its raw word, never healthy", () => {
    const f = cardFacts({ kind: "snapshot", row: snapshot }, NOW);
    expect(labels(f.stats)).toEqual(["Size", "Files", "Took"]);
    expect(f.stats[2].value).toBe("4s");
    expect(f.meta).toContain("pinned");
    const odd = cardFacts(
      { kind: "snapshot", row: { ...snapshot, phase: { unknown: { raw: "Archiving" } } } },
      NOW,
    );
    expect(odd.lamp).toMatchObject({ key: "unknown", word: "Archiving" });
  });

  it("restore: restored, files, took", () => {
    const f = cardFacts({ kind: "restore", row: restore }, NOW);
    expect(labels(f.stats)).toEqual(["Restored", "Files", "Took"]);
    expect(f.stats[2].value).toBe("16s");
    expect(f.meta).toContain("app-data-restored");
  });

  it("maintenance: a full track that never ran is loud; failures make the pill", () => {
    const f = cardFacts({ kind: "maintenance", row: maintenance }, NOW);
    expect(labels(f.stats)).toEqual(["Quick", "Full", "Reclaimed"]);
    expect(f.stats[1].value).toEqual({ absent: "loud", text: "never run" });
    expect(f.lamp).toMatchObject({ key: "failed", word: "2 failed runs" });
    expect(f.to).toBeUndefined();
  });

  it("replications: what was copied, per kind", () => {
    expect(labels(cardFacts({ kind: "repositoryReplication", row: repoRepl }, NOW).stats)).toEqual([
      "Last replicated",
      "Copied",
      "Blobs",
    ]);
    expect(labels(cardFacts({ kind: "snapshotReplication", row: snapRepl }, NOW).stats)).toEqual([
      "Copied",
      "Already there",
      "Failed",
    ]);
  });
});
