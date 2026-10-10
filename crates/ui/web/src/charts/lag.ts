/**
 * Replication lag: how long ago each copy last ran, drawn as a bar under each
 * row's "Last replicated" in the replications ledger, all on one axis so the
 * stalest copy stands out down the column.
 *
 * Pure, and separate from the table, because the decisions that can
 * misinform a reader are the ones about which rows get a bar at all, and
 * those are testable without a DOM.
 *
 * **"Never" is not a very long bar.** A replication that has never succeeded
 * has no age to draw; giving it the longest bar would put a measurement on
 * the screen that nobody took, and giving it a zero-length one would file the
 * most alarming row on the page under "fine". Those rows leave the plot and
 * keep the loud word "never" in their cell instead.
 *
 * **A future instant is clamped to zero, not drawn backwards.** The cluster's
 * clock and the reader's browser are two different clocks; a few seconds of
 * skew must not draw a bar out of the left of the plot.
 *
 * **There is no "overdue" mark on these bars**, and that is a data limit, not
 * an oversight. Overdue means "later than the next scheduled run", and no
 * controller writes one: `RepositoryReplication.status.nextScheduledAt` is in
 * the never-written set and `SnapshotReplication` has no such field at all.
 * Deriving it would mean evaluating the cron a second time, in a second
 * language, against a timezone this bundle does not have — so the bar shows
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
 * A copy's age as a share of the axis, for the bar under it in the ledger.
 * Clamped to the track, with a visible minimum so a copy made seconds ago is
 * a sliver rather than nothing at all — an empty bar reads as "no data",
 * which is a different fact.
 */
export function lagShare(ageMs: number, max: number): number {
  if (max <= 0) return MIN_SHARE;
  return Math.max(MIN_SHARE, Math.min(1, ageMs / max));
}

const MIN_SHARE = 0.02;
