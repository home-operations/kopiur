import { describe, expect, it } from "vitest";

import type { SnapshotRow } from "../api/types";
import { NO_POLICY, type SizePoint, plotArea, sizeSeries } from "./series";

function row(over: Partial<SnapshotRow> = {}): SnapshotRow {
  return {
    namespace: "media",
    name: "nightly-1",
    phase: "succeeded",
    origin: "scheduled",
    policy: "nightly",
    repository: "media/nas",
    kopiaSnapshotId: "k-1",
    identity: "kopiur@media:/data",
    startTime: "2026-09-09T01:00:00Z",
    endTime: "2026-09-09T01:04:00Z",
    sizeBytes: 1000,
    bytesNew: null,
    filesTotal: 5,
    filesFailed: null,
    pinned: false,
    deletionPolicy: "Delete",
    copiedFrom: null,
    ...over,
  };
}

describe("sizeSeries", () => {
  it("groups by policy and plots oldest first, whatever order the rows arrive in", () => {
    // The ledger is newest-first; a time axis reads the other way.
    const series = sizeSeries([
      row({ name: "c", endTime: "2026-09-09T00:00:00Z", sizeBytes: 300 }),
      row({ name: "a", endTime: "2026-09-07T00:00:00Z", sizeBytes: 100 }),
      row({ name: "b", endTime: "2026-09-08T00:00:00Z", sizeBytes: 200 }),
    ]);
    expect(series).toHaveLength(1);
    expect(series[0]?.policy).toBe("nightly");
    expect(series[0]?.points.map((p) => p.name)).toEqual(["a", "b", "c"]);
  });

  it("keeps one series per policy, in name order", () => {
    const series = sizeSeries([
      row({ name: "w", policy: "weekly" }),
      row({ name: "n", policy: "nightly" }),
    ]);
    expect(series.map((s) => s.policy)).toEqual(["nightly", "weekly"]);
  });

  it("files a snapshot with no policy under its own group rather than dropping it", () => {
    const series = sizeSeries([row({ name: "orphan", policy: null })]);
    expect(series.map((s) => s.policy)).toEqual([NO_POLICY]);
  });

  it("excludes a run with no recorded size and counts it, never plotting it as zero", () => {
    const series = sizeSeries([
      row({ name: "ok", sizeBytes: 500 }),
      row({ name: "running", sizeBytes: null, endTime: null }),
      row({ name: "failed", phase: "failed", sizeBytes: null }),
    ]);
    expect(series[0]?.points.map((p) => p.name)).toEqual(["ok"]);
    expect(series[0]?.excluded).toBe(2);
  });

  it("falls back to the start time for a run that reported no end", () => {
    const series = sizeSeries([
      row({ name: "s", endTime: null, startTime: "2026-09-09T02:00:00Z" }),
    ]);
    expect(series[0]?.points[0]?.at).toBe("2026-09-09T02:00:00Z");
  });

  it("drops a row whose instants cannot be parsed rather than plotting at the epoch", () => {
    const series = sizeSeries([row({ name: "bad", endTime: "not a date", startTime: null })]);
    expect(series).toHaveLength(0);
  });

  it("keeps a zero-byte snapshot, which is a measurement and not an absence", () => {
    const series = sizeSeries([row({ name: "empty", sizeBytes: 0 })]);
    expect(series[0]?.points).toHaveLength(1);
    expect(series[0]?.excluded).toBe(0);
  });
});

describe("sizeSeries over a multi-source policy (#689)", () => {
  // 8 PVCs, 3 runs: one Snapshot per PVC per run, as a pvcSelector policy mints.
  const pvcs = ["a", "b", "c", "d", "e", "f", "g", "h"];
  const days = ["2026-09-07", "2026-09-08", "2026-09-09"];
  const rows = days.flatMap((day, run) =>
    pvcs.map((pvc, i) =>
      row({
        name: `samba-${day}-${pvc}`,
        policy: "samba",
        identity: `samba@samba:/pvc/${pvc}`,
        endTime: `${day}T06:0${String(i)}:00Z`,
        sizeBytes: (i + 1) * 1_000_000_000 + run,
      }),
    ),
  );

  it("is one series per source, never one line through every PVC", () => {
    const series = sizeSeries(rows);
    expect(series).toHaveLength(8);
    for (const s of series) {
      expect(s.policy).toBe("samba");
      expect(s.sourcesInPolicy).toBe(8);
      expect(s.points).toHaveLength(3);
      // Every point of a series is the same volume, so the line is monotone
      // here (each PVC grows by a byte per run) — no saw-tooth.
      const bytes = s.points.map((p) => p.bytes);
      expect([...bytes].sort((x, y) => x - y)).toEqual(bytes);
      expect(new Set(s.points.map((p) => p.name.slice(-1))).size).toBe(1);
    }
    expect(series.map((s) => s.source)).toEqual(pvcs.map((p) => `samba@samba:/pvc/${p}`));
  });

  it("keeps a single-source policy as one series with sourcesInPolicy 1", () => {
    const series = sizeSeries([
      row({ name: "x" }),
      row({ name: "y", endTime: "2026-09-10T00:00:00Z" }),
    ]);
    expect(series).toHaveLength(1);
    expect(series[0]?.sourcesInPolicy).toBe(1);
    expect(series[0]?.source).toBe("kopiur@media:/data");
  });

  it("falls back to one series per policy when rows carry no identity", () => {
    const series = sizeSeries([
      row({ name: "x", identity: null }),
      row({ name: "y", identity: null }),
    ]);
    expect(series).toHaveLength(1);
    expect(series[0]?.source).toBeNull();
  });
});

describe("plotArea", () => {
  const older: SizePoint = {
    name: "a",
    namespace: "media",
    at: "2026-09-07T00:00:00Z",
    ms: 1000,
    bytes: 50,
  };
  const newer: SizePoint = {
    name: "b",
    namespace: "media",
    at: "2026-09-08T00:00:00Z",
    ms: 3000,
    bytes: 100,
  };
  const points: SizePoint[] = [older, newer];

  it("is zero-based, so the height of a line is the size it stands for", () => {
    const area = plotArea(points);
    expect(area.max).toBeGreaterThanOrEqual(100);
    // The lowest point sits above the baseline, not on it.
    expect(area.y(0)).toBeGreaterThan(area.y(50));
  });

  it("maps the oldest point to the left edge and the newest to the right", () => {
    const area = plotArea(points);
    expect(area.x(1000)).toBeLessThan(area.x(3000));
  });

  it("centres a lone point instead of dividing by a zero-wide time span", () => {
    const area = plotArea([older]);
    expect(Number.isFinite(area.x(1000))).toBe(true);
    expect(area.x(1000)).toBeCloseTo((area.left + area.right) / 2, 5);
  });

  it("gives an all-zero series a usable axis rather than dividing by a zero-high range", () => {
    const area = plotArea([{ ...older, bytes: 0 }]);
    expect(Number.isFinite(area.y(0))).toBe(true);
    expect(area.max).toBeGreaterThan(0);
  });
});
