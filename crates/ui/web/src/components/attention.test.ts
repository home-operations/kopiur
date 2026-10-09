import { describe, expect, it } from "vitest";

import type {
  DoctorCheckView,
  DoctorObjectView,
  MaintenanceRow,
  PolicyRow,
  RepositorySummary,
  ScheduleRow,
} from "../api/types";
import { cssRules, readStyles } from "../testing/css";
import { attention, splitFix } from "./attention";

const NOW = "2026-09-08T12:00:00Z";

const repo = (name: string, health: RepositorySummary["health"]): RepositorySummary => ({
  kind: "Repository",
  kindPath: "repository",
  name,
  namespace: "media",
  health,
  mode: "ReadWrite",
  serverBacked: false,
  suspended: false,
  phase: health === "failed" ? "failed" : "ready",
});

const policy = (name: string, over: Partial<PolicyRow> = {}): PolicyRow => ({
  namespace: "media",
  name,
  repositories: ["Repository/media/nas"],
  multiRepo: false,
  suspended: false,
  lastSuccessfulSnapshot: NOW,
  lastVerified: NOW,
  ...over,
});

const object = (over: Partial<DoctorObjectView>): DoctorObjectView => ({
  kind: "snapshot",
  namespace: "media",
  name: "nightly-1",
  failing: true,
  message: "something broke",
  ...over,
});

const check = (over: Partial<DoctorCheckView>): DoctorCheckView => ({
  check: "recent-failures",
  title: "no recent failed snapshots/restores",
  outcome: "Fail",
  scope: "namespace",
  what: "1 failed",
  why: "the backup did not happen",
  fix: "check the logs",
  objects: [],
  ...over,
});

describe("splitFix", () => {
  it("separates the operator's own fix from what went wrong", () => {
    expect(splitFix("source PVC missing; the backup waits. Fix: recreate the PVC")).toEqual({
      what: "source PVC missing; the backup waits.",
      fix: "recreate the PVC",
    });
  });

  it("leaves a message without a fix whole", () => {
    expect(splitFix("kopia could not connect")).toEqual({ what: "kopia could not connect" });
  });

  it("does not split on a word that merely ends in fix", () => {
    expect(splitFix("the prefix: abc is wrong")).toEqual({ what: "the prefix: abc is wrong" });
  });
});

describe("attention", () => {
  it("names each object once, however many sources report it", () => {
    const result = attention({
      repositories: [repo("cold", "failed")],
      stalled: [{ kind: "Snapshot", object: "media/nightly-1", message: "stalled. Fix: wait" }],
      checks: [
        check({
          objects: [object({ message: "the mover failed. Fix: re-run it", at: NOW })],
        }),
        check({
          check: "repositories-ready",
          title: "repositories ready",
          objects: [
            object({
              kind: "repository",
              name: "cold",
              message: "not Ready (Failed: bucket gone)",
            }),
          ],
        }),
      ],
    });
    const names = result.items.map((i) => `${i.target.kind}/${i.target.name}`);
    expect(names).toEqual(["snapshot/nightly-1", "repository/cold"]);
    // The doctor's finding is the most specific, so it is the one shown.
    expect(result.items[0]?.problems).toEqual([{ what: "the mover failed.", fix: "re-run it" }]);
    expect(result.items[0]?.at).toBe(NOW);
    expect(result.items[1]?.problems).toEqual([{ what: "not Ready (Failed: bucket gone)" }]);
    expect(result.checks).toEqual([]);
  });

  it("says what is wrong in the state word, never whether the object is suspended", () => {
    const result = attention({
      policies: [
        policy("ledger", { lastSuccessfulSnapshot: null, lastVerified: null }),
        policy("app-data", { lastVerified: null }),
        policy("paused", { suspended: true, lastSuccessfulSnapshot: null }),
      ],
    });
    expect(result.items.map((i) => [i.target.name, i.health, i.state])).toEqual([
      ["ledger", "degraded", "Never succeeded"],
      ["app-data", "degraded", "Never verified"],
    ]);
  });

  it("puts failures before warnings, and stalled work beside the doctor's", () => {
    const schedules: ScheduleRow[] = [
      {
        namespace: "media",
        name: "nightly",
        cron: "0 3 * * *",
        suspended: false,
        consecutiveFailures: 3,
      },
    ];
    const maintenance: MaintenanceRow[] = [
      {
        namespace: "media",
        name: "cold",
        repository: "Repository/media/cold",
        managedByRepository: true,
        quick: { lastRunAt: NOW, consecutiveFailures: 2 },
        full: { lastRunAt: NOW, consecutiveFailures: 0 },
      },
    ];
    const result = attention({
      policies: [policy("ledger", { lastSuccessfulSnapshot: null })],
      schedules,
      maintenance,
      stalled: [{ kind: "Restore", object: "media/of-nothing", message: null }],
    });
    expect(result.items.map((i) => [i.target.kind, i.health, i.state])).toEqual([
      ["restore", "failed", "Stalled"],
      ["snapshotSchedule", "failed", "Failing"],
      ["maintenance", "failed", "Failing"],
      ["snapshotPolicy", "degraded", "Never succeeded"],
    ]);
    expect(result.items[0]?.problems[0]?.what).toMatch(/did not say why/);
    expect(result.items[1]?.problems[0]?.what).toBe("3 runs in a row failed.");
    expect(result.items[2]?.problems[0]?.what).toBe("2 quick maintenance runs in a row failed.");
  });

  it("keeps a failing check that names no object as a line of its own", () => {
    const result = attention({
      checks: [
        check({
          check: "crds-installed",
          title: "CRDs installed",
          what: "2 CRDs missing",
          fix: "helm upgrade",
        }),
        check({ check: "webhook-running", outcome: "Warn", what: "skipped" }),
        check({ objects: [object({ failing: false })] }),
      ],
    });
    expect(result.checks).toEqual([
      {
        check: "crds-installed",
        title: "CRDs installed",
        what: "2 CRDs missing",
        fix: "helm upgrade",
      },
      {
        check: "recent-failures",
        title: "no recent failed snapshots/restores",
        what: "1 failed",
        fix: "check the logs",
      },
    ]);
    // An object reported only as history does not need anyone.
    expect(result.items).toEqual([]);
  });

  it("reads a stalled row's kind and coordinates, and drops one it cannot place", () => {
    const result = attention({
      stalled: [
        { kind: "SnapshotPolicy", object: "media/app", message: "gated" },
        { kind: "Gizmo", object: "media/x", message: "?" },
        { kind: "ClusterRepository", object: "shared", message: "down" },
      ],
    });
    expect(result.items.map((i) => i.target)).toEqual([
      { kind: "snapshotPolicy", namespace: "media", name: "app" },
      { kind: "clusterRepository", name: "shared" },
    ]);
  });

  it("hides nothing: every object that needs someone is a row", () => {
    const many = Array.from({ length: 30 }, (_, i) => repo(`r${String(i)}`, "failed"));
    const result = attention({ repositories: many });
    expect(result.items).toHaveLength(30);
    expect(result).not.toHaveProperty("more");
  });

  it("runs the length of the page: no inner scroller, nothing cut short", () => {
    const list = cssRules(readStyles())
      .filter((r) => r.selector === ".attention")
      .map((r) => r.body)
      .join("\n");
    expect(list).not.toMatch(/max-height:/);
    expect(list).not.toMatch(/overflow(-y)?:/);
  });
});
