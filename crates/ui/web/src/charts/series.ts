/**
 * Turning snapshot rows into a plottable series, and a series into geometry.
 *
 * Pure and separate from the SVG so the two decisions that can misinform a
 * reader are testable on their own:
 *
 * **An absent size is never a zero.** A running, failed or unchanged run
 * records no `sizeBytes`, and plotting it at the baseline would draw a cliff
 * where nothing happened — the chart would say a backup collapsed to nothing
 * on the night it merely failed. Those rows are excluded and *counted*, and
 * the chart says how many it left out. A real `0` is kept: that is a
 * measurement.
 *
 * **The axis starts at zero.** The question is "how big are my backups", which
 * is a magnitude, so the height of the line is the size it stands for. A
 * truncated axis would turn a 2% week-on-week drift into a cliff.
 */

import type { SnapshotRow } from "../api/types";

/** The group a snapshot naming no `SnapshotPolicy` is filed under. */
export const NO_POLICY = "No policy";

/** One plotted snapshot. */
export interface SizePoint {
  name: string;
  namespace: string;
  /** The RFC3339 instant plotted, for a `<time>` element. */
  at: string;
  /** That instant in epoch milliseconds — the x value. */
  ms: number;
  /** `status.stats.sizeBytes` — the y value. */
  bytes: number;
}

/** One policy's plottable history. */
export interface PolicySeries {
  policy: string;
  /** Oldest first — a time axis reads left to right. */
  points: SizePoint[];
  /** Rows in this policy with no size or no usable instant. */
  excluded: number;
}

/**
 * The instant a row is plotted at: when the backup finished, falling back to
 * when it started for a run that never recorded an end.
 *
 * The same precedence GFS bucketing uses, so a point on this chart sits where
 * retention thinks the snapshot sits.
 */
function instantOf(row: SnapshotRow): { at: string; ms: number } | null {
  for (const value of [row.endTime, row.startTime]) {
    if (value === null || value === undefined || value.length === 0) {
      continue;
    }
    const ms = new Date(value).getTime();
    if (!Number.isNaN(ms)) {
      return { at: value, ms };
    }
  }
  return null;
}

/**
 * The rows grouped into one series per policy, each oldest first.
 *
 * A policy whose rows are all unplottable is dropped entirely rather than
 * drawn as an empty frame; a policy with some plottable rows keeps its
 * `excluded` count so the chart can say what is missing.
 */
export function sizeSeries(rows: readonly SnapshotRow[]): PolicySeries[] {
  const groups = new Map<string, PolicySeries>();
  for (const row of rows) {
    const policy =
      row.policy !== null && row.policy !== undefined && row.policy.length > 0
        ? row.policy
        : NO_POLICY;
    let group = groups.get(policy);
    if (group === undefined) {
      group = { policy, points: [], excluded: 0 };
      groups.set(policy, group);
    }
    const instant = instantOf(row);
    const { sizeBytes } = row;
    if (instant === null || sizeBytes === null || sizeBytes === undefined) {
      group.excluded += 1;
      continue;
    }
    group.points.push({
      name: row.name,
      namespace: row.namespace,
      at: instant.at,
      ms: instant.ms,
      bytes: sizeBytes,
    });
  }
  return [...groups.values()]
    .filter((group) => group.points.length > 0)
    .map((group) => ({
      ...group,
      points: [...group.points].sort((a, b) => a.ms - b.ms || a.name.localeCompare(b.name)),
    }))
    .sort((a, b) => a.policy.localeCompare(b.policy));
}

/** The chart's drawing box, in the SVG's own user units. */
export const CHART = {
  width: 560,
  height: 148,
  left: 62,
  right: 548,
  top: 12,
  bottom: 112,
} as const;

/** The two scales and the domain they were built from. */
export interface PlotArea {
  left: number;
  right: number;
  top: number;
  bottom: number;
  /** The top of the y domain — always above every point, never below. */
  max: number;
  x: (ms: number) => number;
  y: (bytes: number) => number;
}

/**
 * Scales for one series.
 *
 * Two degenerate cases are handled rather than allowed to divide by zero: a
 * single point (or several at the same instant) is centred, and an all-zero
 * series still gets a usable axis. Both otherwise produce `NaN` coordinates,
 * which render as an invisible line — a chart that silently shows nothing.
 */
export function plotArea(points: readonly SizePoint[]): PlotArea {
  const { left, right, top, bottom } = CHART;
  const times = points.map((p) => p.ms);
  const first = Math.min(...times);
  const last = Math.max(...times);
  const span = last - first;
  const peak = Math.max(0, ...points.map((p) => p.bytes));
  // Headroom so the newest point is never welded to the frame, and a floor so
  // an all-zero series has a range to divide by.
  const max = peak > 0 ? peak * 1.08 : 1;

  return {
    left,
    right,
    top,
    bottom,
    max,
    x: (ms) => (span === 0 ? (left + right) / 2 : left + ((ms - first) / span) * (right - left)),
    y: (bytes) => bottom - (bytes / max) * (bottom - top),
  };
}

/** The `d` of a polyline through every point, oldest first. */
export function linePath(points: readonly SizePoint[], area: PlotArea): string {
  return points
    .map(
      (point, index) =>
        `${index === 0 ? "M" : "L"}${String(area.x(point.ms))} ${String(area.y(point.bytes))}`,
    )
    .join(" ");
}
