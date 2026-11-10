import { useState } from "react";

import type { SnapshotRow } from "../api/types";
import { NotReported } from "../components/NotReported";
import { formatTimestamp, humanBytes } from "../util/format";
import {
  CHART,
  type SizeSeries,
  type SizePoint,
  areaPath,
  linePath,
  plotArea,
  sizeSeries,
} from "./series";

/**
 * Snapshot size over time, one small multiple per (`SnapshotPolicy`, source).
 *
 * **What is plotted** is each snapshot's LOGICAL size (`status.stats.sizeBytes`
 * — the size of the tree kopia captured, before deduplication), one line per
 * SOURCE. A policy that backs up several PVCs mints one `Snapshot` per PVC per
 * run, and those are different volumes: drawing them as one line made a
 * saw-tooth between unrelated sizes (#689). Each source (kopia identity) is
 * its own series, and the caption says which one and how many the policy has.
 *
 * Five decisions, in the order the dataviz procedure asks them.
 *
 * **Form.** Change over time for a magnitude: a line. One chart *per policy*
 * rather than one chart with a line per policy, because two policies' sizes
 * are not comparable quantities — one covers a 2 TiB media volume and the
 * other a 40 MiB config directory, and drawing them on one axis makes the
 * second a flat line on the floor. Small multiples are also what keeps this
 * honest as the policy count grows: a categorical hue is never cycled.
 *
 * **Colour.** One series per chart, so there is no categorical palette to
 * assign: the line is the body ink the rest of the console uses, and the
 * accent — this design system's *one* current marker — marks the point the
 * pointer is on. No area fill: an accent wash is forbidden by the design
 * system, and a filled area under a line exaggerates the magnitude anyway.
 *
 * **Marks.** A 2px line, 3px dots with an invisible 9px hit target, a
 * zero-based axis with three recessive gridlines, and only two x labels (the
 * first and last instants) so nothing collides. The newest value is direct-
 * labelled; no other point carries a number.
 *
 * **Interaction.** A readout above the plot, not a floating tooltip. A tooltip
 * positioned over a chart is the bug Task 5 found in a ledger — an absolutely
 * positioned descendant changes its container's scroll geometry even while
 * hidden — and a readout in the flow cannot do that. It shows the newest point
 * by default and the hovered point while the pointer is on one.
 *
 * **The spoken twin.** The SVG is `aria-hidden` and every point it draws is
 * also a row of a real table beneath it, per the design system's rule that a
 * picture is never the only copy of a fact. That table is also the "table
 * view" an accessible chart owes its reader.
 *
 * **There is no "new bytes" series**, though the brief asks for one:
 * `Snapshot.status.stats.bytesNew` is declared on the CRD and written by no
 * controller (`components/unwired.ts`), so the series would be empty in every
 * cluster. A chart with an invisible second line would be worse than saying
 * so, which is what the footnote does.
 */
export interface SnapshotSizeChartProps {
  rows: readonly SnapshotRow[];
  /** How many (policy, source) series to draw before naming the rest instead. */
  maxSeries?: number | undefined;
  /**
   * The snapshot this chart is about (a drawer's own): its SOURCE's series is
   * drawn first, and its point is the one the readout shows until the pointer
   * moves, instead of the newest.
   */
  focus?: { namespace: string; name: string } | undefined;
}

export function SnapshotSizeChart({ rows, maxSeries = 4, focus }: SnapshotSizeChartProps) {
  const series = focusFirst(sizeSeries(rows), focus);
  if (series.length === 0) {
    return (
      <p className="page__section-note">
        None of these snapshots recorded a size yet; only successful runs record one.
      </p>
    );
  }
  const drawn = series.slice(0, maxSeries);
  const hidden = series.length - drawn.length;

  return (
    <div className="charts">
      {drawn.map((policySeries) => (
        <PolicyChart
          key={`${policySeries.policy}/${policySeries.source ?? ""}`}
          series={policySeries}
          focus={focus}
        />
      ))}
      {hidden > 0 ? (
        <p className="page__section-note">
          {hidden} more {hidden === 1 ? "series is" : "series are"} in this list and not drawn —
          each source a policy backs up is charted on its own.
        </p>
      ) : null}
      <p className="page__section-note">
        New bytes after deduplication is not charted:{" "}
        <span className="mono">Snapshot.status.stats.bytesNew</span> is{" "}
        <NotReported field="snapshotBytesNew" />, so the series would be empty in every cluster.
      </p>
    </div>
  );
}

/**
 * The series the focused snapshot belongs to, moved to the front so a drawer
 * capped at one chart draws the snapshot's OWN source — never a sibling PVC's.
 * Every other series keeps its order.
 */
function focusFirst(series: SizeSeries[], focus: SnapshotSizeChartProps["focus"]): SizeSeries[] {
  if (focus === undefined) return series;
  const index = series.findIndex((s) =>
    s.points.some((p) => p.name === focus.name && p.namespace === focus.namespace),
  );
  if (index <= 0) return series;
  const own = series[index];
  if (own === undefined) return series;
  return [own, ...series.slice(0, index), ...series.slice(index + 1)];
}

/** The chart's title: the quantity, then whose it is. */
function ChartTitle({ series }: { series: SizeSeries }) {
  return (
    <span className="chart__title">
      Logical size per snapshot · <span className="mono">{series.policy}</span>
      {series.source !== null ? (
        <>
          {" "}
          · <span className="mono">{series.source}</span>
        </>
      ) : null}
    </span>
  );
}

/**
 * Says this line is ONE of several sources, so nobody reads one PVC's size as
 * the policy's total.
 */
function SourcesNote({ series }: { series: SizeSeries }) {
  if (series.sourcesInPolicy <= 1) return null;
  return (
    <p className="page__section-note">
      One of {series.sourcesInPolicy} sources this policy backs up. Each source is charted on its
      own: their sizes are different volumes, not one series.
    </p>
  );
}

interface PolicyChartProps {
  series: SizeSeries;
  focus?: SnapshotSizeChartProps["focus"];
}

function PolicyChart({ series, focus }: PolicyChartProps) {
  const { points } = series;
  const [hovered, setHovered] = useState<number | null>(null);
  // One point is not a trend, and "over time" needs two. A lone dot in an
  // empty frame, under an axis top invented from a single value, is a picture
  // of nothing — the dataviz form heuristic calls for a stat tile instead.
  // The table stays either way, because the number is the point.
  if (points.length < 2) {
    return <PolicyTile series={series} />;
  }
  const area = plotArea(points);
  const latestIndex = points.length - 1;
  const focusIndex =
    focus === undefined
      ? -1
      : points.findIndex((p) => p.name === focus.name && p.namespace === focus.namespace);
  const restIndex = focusIndex === -1 ? latestIndex : focusIndex;
  const shownIndex = hovered ?? restIndex;
  const shown = points[shownIndex];
  const first = points[0];
  const last = points[latestIndex];
  const gridlines = [0, area.max / 2, area.max];

  return (
    <figure className="chart">
      <figcaption className="chart__caption">
        <ChartTitle series={series} />
        <span className="chart__readout" role="status">
          {shown !== undefined ? (
            <>
              <span className="chart__readout-value">{humanBytes(shown.bytes)}</span>{" "}
              <span className="mono chart__readout-meta">
                {shown.name} · {formatTimestamp(shown.at)}
                {hovered === null ? (focusIndex === -1 ? " (latest)" : " (this snapshot)") : ""}
              </span>
            </>
          ) : null}
        </span>
      </figcaption>

      <div className="chart__scroll">
        <svg
          className="chart__plot"
          viewBox={`0 0 ${String(CHART.width)} ${String(CHART.height)}`}
          preserveAspectRatio="xMidYMid meet"
          aria-hidden="true"
          focusable="false"
          onMouseLeave={() => {
            setHovered(null);
          }}
        >
          {gridlines.map((value) => (
            <g key={value}>
              <line
                className="chart__grid"
                x1={area.left}
                x2={area.right}
                y1={area.y(value)}
                y2={area.y(value)}
              />
              <text
                className="chart__axis"
                x={area.left - 6}
                y={area.y(value) + 4}
                textAnchor="end"
              >
                {humanBytes(Math.round(value))}
              </text>
            </g>
          ))}

          <path className="chart__area" d={areaPath(points, area)} />
          <path className="chart__line" d={linePath(points, area)} />

          {points.map((point, index) => (
            <Marker
              key={`${point.namespace}/${point.name}`}
              point={point}
              x={area.x(point.ms)}
              y={area.y(point.bytes)}
              current={index === shownIndex}
              onEnter={() => {
                setHovered(index);
              }}
            />
          ))}

          {first !== undefined ? (
            <text className="chart__axis" x={area.left} y={CHART.bottom + 18} textAnchor="start">
              {formatTimestamp(first.at)}
            </text>
          ) : null}
          {last !== undefined && points.length > 1 ? (
            <text className="chart__axis" x={area.right} y={CHART.bottom + 18} textAnchor="end">
              {formatTimestamp(last.at)}
            </text>
          ) : null}
        </svg>
      </div>

      <SourcesNote series={series} />
      <details className="chart__data">
        <summary>{tableSummary(series)}</summary>
        <SizeTable series={series} />
      </details>
    </figure>
  );
}

/**
 * One measurement, stated rather than plotted.
 *
 * The value leads; the run that produced it and the fact that there is only
 * one are the two things that stop it being mistaken for a trend. The table
 * is still here, unfolded — with a single row there is nothing to disclose.
 */
function PolicyTile({ series }: PolicyChartProps) {
  const point = series.points[0];
  if (point === undefined) {
    return null;
  }
  return (
    <figure className="chart" aria-label={`Logical snapshot size for ${seriesName(series)}`}>
      <figcaption className="chart__caption">
        <ChartTitle series={series} />
      </figcaption>
      <p className="chart__figure">
        <span className="chart__figure-value">{humanBytes(point.bytes)}</span>{" "}
        <span className="mono chart__readout-meta">
          {point.name} · {formatTimestamp(point.at)}
        </span>
      </p>
      <p className="page__section-note">
        Only one run recorded a size for this source; a second successful run makes this a chart.
        {series.excluded > 0 ? ` ${excludedText(series.excluded)}` : ""}
      </p>
      <SourcesNote series={series} />
      <SizeTable series={series} />
    </figure>
  );
}

/** The series' name for an accessible label: policy, and source when known. */
function seriesName(series: SizeSeries): string {
  return series.source !== null ? `${series.policy} (${series.source})` : series.policy;
}

/** The disclosure's own line: how many points, and what was left out. */
function tableSummary(series: SizeSeries): string {
  const { length } = series.points;
  const head = `${String(length)} ${length === 1 ? "measurement" : "measurements"} as a table`;
  return series.excluded > 0 ? `${head} · ${excludedText(series.excluded)}` : head;
}

function excludedText(excluded: number): string {
  const runs = `${String(excluded)} run${excluded === 1 ? "" : "s"}`;
  return `${runs} recorded no size and ${excluded === 1 ? "is" : "are"} not plotted.`;
}

/** Every plotted point as a row — the number a picture cannot be read for. */
function SizeTable({ series }: PolicyChartProps) {
  return (
    <div className="ledger-scroll">
      <table className="ledger" aria-label={`Logical snapshot sizes for ${seriesName(series)}`}>
        <thead>
          <tr>
            <th scope="col">Snapshot</th>
            <th scope="col">Backed up</th>
            <th scope="col" className="num">
              Logical size
            </th>
          </tr>
        </thead>
        <tbody>
          {[...series.points].reverse().map((point) => (
            <tr key={`${point.namespace}/${point.name}`}>
              <td className="mono">{point.name}</td>
              <td className="mono">
                <time dateTime={point.at}>{formatTimestamp(point.at)}</time>
              </td>
              <td className="num">{humanBytes(point.bytes)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

interface MarkerProps {
  point: SizePoint;
  x: number;
  y: number;
  current: boolean;
  onEnter: () => void;
}

/**
 * One measurement: a 3px dot with an invisible 9px hit target, so a pointer
 * does not have to land on a 6px circle to read a value.
 */
function Marker({ point, x, y, current, onEnter }: MarkerProps) {
  return (
    <g data-point={point.name} onMouseEnter={onEnter}>
      <circle className="chart__hit" cx={x} cy={y} r={9} />
      <circle
        className="chart__dot"
        cx={x}
        cy={y}
        r={3}
        data-current={current ? "true" : undefined}
      />
    </g>
  );
}
