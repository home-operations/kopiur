import { describe, expect, it } from "vitest";

import type { OverviewView } from "../api/types";
import { navTally } from "./navTally";

const overview: OverviewView = {
  snapshotWindowHours: 24,
  generatedAt: "2026-10-08T12:00:00Z",
  kinds: [
    {
      kind: "repository",
      total: 2,
      byHealth: [
        { health: "failed", count: 1 },
        { health: "healthy", count: 1 },
      ],
      refused: false,
    },
    {
      kind: "clusterRepository",
      total: 1,
      byHealth: [{ health: "healthy", count: 1 }],
      refused: false,
    },
    { kind: "maintenance", total: 1, byHealth: [{ health: "healthy", count: 1 }], refused: false },
    {
      kind: "snapshotPolicy",
      total: 2,
      byHealth: [{ health: "healthy", count: 2 }],
      refused: false,
    },
    { kind: "snapshotSchedule", total: 0, byHealth: [], refused: false },
    {
      kind: "snapshot",
      total: 4,
      byHealth: [
        { health: "healthy", count: 2 },
        { health: "failed", count: 1 },
        { health: "unknown", count: 1 },
      ],
      refused: false,
    },
    { kind: "restore", total: 0, byHealth: [], refused: false },
    {
      kind: "repositoryReplication",
      total: 1,
      byHealth: [{ health: "pending", count: 1 }],
      refused: false,
    },
    {
      kind: "snapshotReplication",
      total: 1,
      byHealth: [{ health: "healthy", count: 1 }],
      refused: false,
    },
  ],
};

describe("navTally", () => {
  it("merges the two repository kinds and the two replication kinds", () => {
    expect(navTally(overview, "/repositories")?.total).toBe(3);
    expect(navTally(overview, "/replications")?.total).toBe(2);
    expect(navTally(overview, "/maintenance")?.total).toBe(1);
  });

  it("orders the parts worst first and says them in words", () => {
    const snapshots = navTally(overview, "/snapshots");
    expect(snapshots?.parts).toEqual([
      { key: "failed", n: 1 },
      { key: "unknown", n: 1 },
      { key: "healthy", n: 2 },
    ]);
    expect(snapshots?.failing).toBe(1);
    expect(snapshots?.words).toBe("4 in the last 24h: 1 failed, 1 unknown, 2 ok");
    expect(navTally(overview, "/policies")?.words).toBe("2: 2 ok");
  });

  it("says an empty kind is empty", () => {
    const schedules = navTally(overview, "/schedules");
    expect(schedules?.total).toBe(0);
    expect(schedules?.parts).toEqual([]);
    expect(schedules?.words).toBe("none in scope");
  });

  it("has nothing for the sections that list no kind", () => {
    for (const to of ["/", "/topology", "/doctor"] as const) {
      expect(navTally(overview, to)).toBeNull();
    }
  });

  it("has nothing when the overview has not been read", () => {
    expect(navTally(undefined, "/repositories")).toBeNull();
  });

  it("draws no count for a section whose kinds the caller may not list", () => {
    const refused: OverviewView = {
      ...overview,
      kinds: overview.kinds.map((k) =>
        k.kind === "restore" ? { ...k, total: 0, byHealth: [], refused: true } : k,
      ),
    };
    const tally = navTally(refused, "/restores");
    expect(tally?.refused).toBe(true);
    expect(tally?.words).toBe("not permitted to list here");
  });

  it("counts what it may read when only one of a section's kinds is refused", () => {
    const refused: OverviewView = {
      ...overview,
      kinds: overview.kinds.map((k) =>
        k.kind === "repositoryReplication" ? { ...k, total: 0, byHealth: [], refused: true } : k,
      ),
    };
    const tally = navTally(refused, "/replications");
    expect(tally?.refused).toBe(false);
    expect(tally?.total).toBe(1);
    expect(tally?.words).toBe("1: 1 ok; some not permitted to list here");
  });
});
