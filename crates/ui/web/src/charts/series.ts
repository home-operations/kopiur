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

/**
 * One SOURCE's plottable history under one policy.
 *
 * The unit of a series is the (policy, source) pair, never the policy alone
 * (#689): a policy that backs up eight PVCs mints eight `Snapshot`s per run,
 * one per PVC, and joining all of them into one line drew a saw-tooth that
 * jumped between unrelated volumes' sizes — a 90 GiB share, then a 2 MiB one,
 * then the 90 GiB share again — which reads as a backup collapsing and
 * recovering on every run. A source is the snapshot's kopia identity
 * (`user@host:/path`), which is stable for one PVC across runs.
 */
export interface SizeSeries {
  policy: string;
  /**
   * The kopia identity every point in this series was written under, or
   * `null` when the rows carry none (they are then one series per policy,
   * which is the best the data allows).
   */
  source: string | null;
  /** Oldest first — a time axis reads left to right. */
  points: SizePoint[];
  /** Rows in this series with no size or no usable instant. */
  excluded: number;
  /**
   * How many plottable sources the same policy has — `1` for the common
   * single-source policy; more says this series is one volume of several.
   */
  sourcesInPolicy: number;
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

function nonEmpty(value: string | null | undefined): string | null {
  return value !== null && value !== undefined && value.length > 0 ? value : null;
}

/**
 * The rows grouped into one series per (policy, source), each oldest first.
 *
 * A series whose rows are all unplottable is dropped entirely rather than
 * drawn as an empty frame; a series with some plottable rows keeps its
 * `excluded` count so the chart can say what is missing. Ordered by policy,
 * then source, so a poll that returns the same rows never reorders the charts.
 */
export function sizeSeries(rows: readonly SnapshotRow[]): SizeSeries[] {
  const groups = new Map<string, SizeSeries>();
  for (const row of rows) {
    const policy = nonEmpty(row.policy) ?? NO_POLICY;
    const source = nonEmpty(row.identity);
    // NUL cannot occur in a policy name or a kopia identity, so the key is
    // unambiguous.
    const key = `${policy}\u0000${source ?? ""}`;
    let group = groups.get(key);
    if (group === undefined) {
      group = { policy, source, points: [], excluded: 0, sourcesInPolicy: 1 };
      groups.set(key, group);
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
  const plotted = [...groups.values()].filter((group) => group.points.length > 0);
  const perPolicy = new Map<string, number>();
  for (const group of plotted) {
    perPolicy.set(group.policy, (perPolicy.get(group.policy) ?? 0) + 1);
  }
  return plotted
    .map((group) => ({
      ...group,
      sourcesInPolicy: perPolicy.get(group.policy) ?? 1,
      points: [...group.points].sort((a, b) => a.ms - b.ms || a.name.localeCompare(b.name)),
    }))
    .sort(
      (a, b) => a.policy.localeCompare(b.policy) || (a.source ?? "").localeCompare(b.source ?? ""),
    );
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
/**
 * The line's path closed down to the baseline: the fill beneath it. The y
 * domain is zero-based, so `area.bottom` is zero bytes, not an arbitrary floor.
 */
export function areaPath(points: readonly SizePoint[], area: PlotArea): string {
  const first = points[0];
  const last = points[points.length - 1];
  if (first === undefined || last === undefined) return "";
  const bottom = String(area.bottom);
  return `${linePath(points, area)} L${String(area.x(last.ms))} ${bottom} L${String(area.x(first.ms))} ${bottom} Z`;
}

export function linePath(points: readonly SizePoint[], area: PlotArea): string {
  return points
    .map(
      (point, index) =>
        `${index === 0 ? "M" : "L"}${String(area.x(point.ms))} ${String(area.y(point.bytes))}`,
    )
    .join(" ");
}
