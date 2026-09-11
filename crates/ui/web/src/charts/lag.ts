/**
 * Replication lag: how long ago each copy last ran.
 *
 * Pure, and separate from the SVG, for the same reason `series.ts` is — the
 * decisions that can misinform a reader are the ones about which rows get a
 * bar at all, and those are testable without a DOM.
 *
 * **"Never" is not a very long bar.** A replication that has never succeeded
 * has no age to draw; giving it the longest bar would put a measurement on
 * the screen that nobody took, and giving it a zero-length one would file the
 * most alarming row on the page under "fine". Those rows leave the plot and
 * are named in words above it.
 *
 * **A future instant is clamped to zero, not drawn backwards.** The cluster's
 * clock and the reader's browser are two different clocks; a few seconds of
 * skew must not draw a bar out of the left of the plot.
 *
 * **There is no "overdue" line on this chart**, and that is a data limit, not
 * an oversight. Overdue means "later than the next scheduled run", and no
 * controller writes one: `RepositoryReplication.status.nextScheduledAt` is in
 * the never-written set and `SnapshotReplication` has no such field at all.
 * Deriving it would mean evaluating the cron a second time, in a second
 * language, against a timezone this bundle does not have — so the chart shows
 * the age it can measure and the screen says the rest in words.
 */

import type { ReplicationRow } from "../components/replication";

/** One copy's measured staleness. */
export interface LagBar {
  /** `ReplicationRow.id` — stable across polls. */
  id: string;
  kind: string;
  namespace: string;
  name: string;
  /** `namespace/name`, the label beside the bar. */
  label: string;
  /** The RFC3339 instant measured from, for a `<time>` element. */
  lastReplicated: string;
  /** Milliseconds between that instant and now; never negative. */
  ageMs: number;
  /** The schedule is stopped on purpose — the age is real, the alarm is not. */
  suspended: boolean;
}

/** A copy with no age to plot. */
export interface NeverReplicated {
  id: string;
  kind: string;
  label: string;
  suspended: boolean;
}

/** Everything the chart and its table need. */
export interface LagSeries {
  /** Stalest first: the reader's question is which copy is furthest behind. */
  bars: LagBar[];
  neverReplicated: NeverReplicated[];
  /** Rows whose `lastReplicated` could not be read as an instant. */
  unreadable: number;
  /** The top of the axis, in milliseconds. Always positive. */
  max: number;
}

/** A floor for the axis, so a fleet copied minutes ago still draws bars. */
const MIN_AXIS_MS = 60 * 60 * 1000;

export function lagSeries(rows: readonly ReplicationRow[], now: Date = new Date()): LagSeries {
  const bars: LagBar[] = [];
  const neverReplicated: NeverReplicated[] = [];
  let unreadable = 0;

  for (const row of rows) {
    const label = `${row.namespace}/${row.name}`;
    const last = row.lastReplicated;
    if (last === null || last === undefined || last.length === 0) {
      neverReplicated.push({ id: row.id, kind: row.kind, label, suspended: row.suspended });
      continue;
    }
    const ms = new Date(last).getTime();
    if (Number.isNaN(ms)) {
      unreadable += 1;
      continue;
    }
    bars.push({
      id: row.id,
      kind: row.kind,
      namespace: row.namespace,
      name: row.name,
      label,
      lastReplicated: last,
      ageMs: Math.max(0, now.getTime() - ms),
      suspended: row.suspended,
    });
  }

  bars.sort((a, b) => b.ageMs - a.ageMs || a.label.localeCompare(b.label));
  const peak = Math.max(0, ...bars.map((bar) => bar.ageMs));
  return { bars, neverReplicated, unreadable, max: Math.max(MIN_AXIS_MS, peak * 1.08) };
}

/**
 * The plot's box, in the SVG's own user units. The height is not here: it
 * grows with the number of bars, so the chart computes it.
 */
export const LAG = {
  width: 560,
  /** Room for a `namespace/name` label to the left of the track. */
  plotLeft: 190,
  plotRight: 548,
  top: 6,
  /** Space under the last bar for the two axis words. */
  bottom: 22,
  rowHeight: 22,
  barHeight: 14,
} as const;

/**
 * A bar's drawn width. Clamped to the track, and given a visible minimum so a
 * copy made seconds ago is a sliver rather than nothing at all — a bar of zero
 * width reads as "no data", which is a different fact.
 */
export function lagWidth(ageMs: number, max: number): number {
  const track = LAG.plotRight - LAG.plotLeft;
  if (max <= 0) {
    return 2;
  }
  return Math.max(2, Math.min(track, (ageMs / max) * track));
}
