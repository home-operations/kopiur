import { describe, expect, it } from "vitest";

import type { ReplicationRow } from "../components/replication";
import { lagSeries } from "./lag";

const NOW = new Date("2026-09-10T12:00:00Z");

function row(over: Partial<ReplicationRow> = {}): ReplicationRow {
  return {
    id: "replication/media/nas-to-offsite",
    kind: "RepositoryReplication",
    kindToken: "replication",
    namespace: "media",
    name: "nas-to-offsite",
    source: "media/nas",
    destination: "b2",
    destinationIsRepository: false,
    cron: "0 4 * * *",
    suspended: false,
    phase: "succeeded",
    lastReplicated: "2026-09-10T04:00:00Z",
    ...over,
  };
}

describe("lagSeries", () => {
  it("measures each copy's age from its last replication", () => {
    const series = lagSeries([row()], NOW);
    expect(series.bars).toHaveLength(1);
    expect(series.bars[0]?.ageMs).toBe(8 * 60 * 60 * 1000);
    expect(series.bars[0]?.label).toBe("media/nas-to-offsite");
  });

  it("puts the stalest copy first, because that is the question", () => {
    const series = lagSeries(
      [
        row({ id: "a", name: "fresh", lastReplicated: "2026-09-10T11:00:00Z" }),
        row({ id: "b", name: "stale", lastReplicated: "2026-09-03T11:00:00Z" }),
        row({ id: "c", name: "middling", lastReplicated: "2026-09-09T11:00:00Z" }),
      ],
      NOW,
    );
    expect(series.bars.map((b) => b.name)).toEqual(["stale", "middling", "fresh"]);
  });

  it("never draws a copy that has never run, because it has no age to draw", () => {
    // Plotting "never" as the longest bar would be a number nobody measured.
    // It is the worst fact on the screen and it is stated in words instead.
    const series = lagSeries([row({ id: "a", name: "never-ran", lastReplicated: null })], NOW);
    expect(series.bars).toHaveLength(0);
    expect(series.neverReplicated.map((r) => r.label)).toEqual(["media/never-ran"]);
  });

  it("keeps a suspended copy, and marks it — the clock stopped on purpose", () => {
    const series = lagSeries([row({ suspended: true })], NOW);
    expect(series.bars[0]?.suspended).toBe(true);
  });

  it("drops an unparseable instant rather than drawing it at an invented age", () => {
    const series = lagSeries([row({ lastReplicated: "not a date" })], NOW);
    expect(series.bars).toHaveLength(0);
    expect(series.unreadable).toBe(1);
  });

  it("treats a future instant as no lag rather than a negative bar", () => {
    // Clock skew between the cluster and the reader's browser is real, and a
    // bar of negative length draws backwards out of the plot.
    const series = lagSeries([row({ lastReplicated: "2026-09-10T13:00:00Z" })], NOW);
    expect(series.bars[0]?.ageMs).toBe(0);
  });

  it("scales the axis to the stalest copy, with a floor so a fresh fleet still plots", () => {
    const series = lagSeries([row({ lastReplicated: "2026-09-10T11:59:59Z" })], NOW);
    expect(series.max).toBeGreaterThan(0);
    expect(Number.isFinite(series.max)).toBe(true);
  });

  it("is empty, not broken, when there are no replications at all", () => {
    const series = lagSeries([], NOW);
    expect(series.bars).toHaveLength(0);
    expect(series.neverReplicated).toHaveLength(0);
    expect(series.max).toBeGreaterThan(0);
  });
});
