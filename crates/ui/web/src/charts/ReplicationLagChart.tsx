import { useState } from "react";

import { Finding } from "../components/Finding";
import { healthLamp } from "../components/health";
import type { ReplicationRow } from "../components/replication";
import { formatTimestamp, humanDuration } from "../util/format";
import { LAG, type LagBar, type LagSeries, lagSeries, lagWidth } from "./lag";

/**
 * How far behind each copy is, as one bar per replication.
 *
 * The dataviz procedure, in its own order.
 *
 * **Form.** The job is "compare magnitude, low to high" across a handful of
 * named things, and the names are `namespace/name` — long. That is a
 * *horizontal* bar chart, not a line: there is one measurement per
 * replication, not a history, so there is nothing to draw a trend through.
 *
 * **Colour.** One series, one hue. The bar is the console's accent at a
 * single step and the track behind it is the inset surface; nothing here
 * encodes state in colour, because the one state that matters — suspended —
 * is a word in the table, and this design system does not let health ride on
 * hue. There is deliberately no red "overdue" band: see below.
 *
 * **Marks.** A 2px surface gap between the bar and its track, rounded data
 * ends, a recessive axis, and a direct label only on the bar the pointer is
 * on. Every bar's number is in the table regardless.
 *
 * **Interaction.** The readout sits above the plot rather than floating over
 * it, matching `SnapshotSizeChart` — an absolutely positioned layer changes
 * its container's scroll geometry even while hidden, which is a defect a
 * visual round already found once on these screens.
 *
 * **The spoken twin.** The SVG is `aria-hidden`; every bar is also a row of a
 * real table. An operator on a screen reader still needs the number.
 *
 * **There is no overdue marker, and the chart says so.** Overdue means "past
 * the next scheduled run", and no controller writes one —
 * `RepositoryReplication.status.nextScheduledAt` is declared and never set,
 * and `SnapshotReplication` has no such field on the CRD at all. Drawing a
 * threshold would mean evaluating each cron a second time, in a second
 * language, against a timezone this bundle is not given. The chart shows the
 * age it can measure and names the gap in words.
 */
export interface ReplicationLagChartProps {
  rows: readonly ReplicationRow[];
  /** The clock ages are measured against. */
  now?: Date | undefined;
}

export function ReplicationLagChart({ rows, now = new Date() }: ReplicationLagChartProps) {
  const series = lagSeries(rows, now);
  const { bars, neverReplicated, unreadable } = series;

  return (
    <div className="charts">
      {bars.length > 0 ? <LagPlot series={series} /> : null}

      {neverReplicated.length > 0 ? (
        <ul className="finding-list">
          {neverReplicated.map((never) => (
            <li key={never.id}>
              <Finding
                title={`${never.label} has never been copied`}
                what={`${never.kind} ${never.label} has recorded no successful replication, so there is no age to plot and no second copy to fall back on.`}
                why={
                  never.suspended
                    ? "Its schedule is suspended, so it is not going to run on its own."
                    : "A scheduled copy that has never succeeded has either never fired or never finished."
                }
                fix="open the replication and run it once, then check the run's own counters"
                lamp={healthLamp(never.suspended ? "suspended" : "failed")}
              />
            </li>
          ))}
        </ul>
      ) : null}

      {bars.length === 0 && neverReplicated.length === 0 ? (
        <p className="page__section-note">
          No replication has recorded a copy, so there is no lag to chart. A replication records{" "}
          <span className="mono">status.lastReplicated</span> when a run finishes.
        </p>
      ) : null}

      {unreadable > 0 ? (
        <p className="page__section-note">
          {unreadable} replication{unreadable === 1 ? "" : "s"} recorded a time this build could not
          read, and {unreadable === 1 ? "is" : "are"} not plotted.
        </p>
      ) : null}

      <p className="page__section-note">
        Every bar is the age of the last copy. None of them says <em>overdue</em>, because no
        controller writes a next-run time to compare against — the paragraph below the ledger says
        which field is missing on each kind.
      </p>
    </div>
  );
}

function LagPlot({ series }: { series: LagSeries }) {
  const { bars, max } = series;
  const [hovered, setHovered] = useState<string | null>(null);
  const shown = bars.find((bar) => bar.id === hovered) ?? bars[0];
  const height = LAG.top + bars.length * LAG.rowHeight + LAG.bottom;

  return (
    <figure className="chart">
      <figcaption className="chart__caption">
        <span className="chart__title">Time since the last copy</span>
        <span className="chart__readout" role="status">
          {shown !== undefined ? (
            <>
              <span className="chart__readout-value">{humanDuration(shown.ageMs / 1000)}</span>{" "}
              <span className="mono chart__readout-meta">
                {shown.label}
                {hovered === null ? " (stalest)" : ""}
              </span>
            </>
          ) : null}
        </span>
      </figcaption>

      <div className="chart__scroll">
        <svg
          className="chart__plot"
          viewBox={`0 0 ${String(LAG.width)} ${String(height)}`}
          preserveAspectRatio="xMidYMid meet"
          aria-hidden="true"
          focusable="false"
          onMouseLeave={() => {
            setHovered(null);
          }}
        >
          {bars.map((bar, index) => {
            const y = LAG.top + index * LAG.rowHeight;
            return (
              <Bar
                key={bar.id}
                bar={bar}
                y={y}
                width={lagWidth(bar.ageMs, max)}
                current={bar.id === shown?.id}
                onEnter={() => {
                  setHovered(bar.id);
                }}
              />
            );
          })}
          <text className="chart__axis" x={LAG.plotLeft} y={height - 6} textAnchor="start">
            just now
          </text>
          <text className="chart__axis" x={LAG.plotRight} y={height - 6} textAnchor="end">
            {humanDuration(max / 1000)} ago
          </text>
        </svg>
      </div>

      <div className="ledger-scroll">
        <table className="ledger" aria-label="Replication lag">
          <thead>
            <tr>
              <th scope="col">Replication</th>
              <th scope="col">Last copied</th>
              <th scope="col" className="num">
                Age
              </th>
            </tr>
          </thead>
          <tbody>
            {bars.map((bar) => (
              <tr key={bar.id}>
                <td className="mono">
                  {bar.label}
                  {bar.suspended ? <span className="lag__suspended"> suspended</span> : null}
                </td>
                <td className="mono">
                  <time dateTime={bar.lastReplicated}>{formatTimestamp(bar.lastReplicated)}</time>
                </td>
                <td className="num">{humanDuration(bar.ageMs / 1000)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </figure>
  );
}

interface BarProps {
  bar: LagBar;
  y: number;
  width: number;
  current: boolean;
  onEnter: () => void;
}

/** One copy's age: a label, a track, and the bar drawn over it. */
function Bar({ bar, y, width, current, onEnter }: BarProps) {
  return (
    <g onMouseEnter={onEnter}>
      <rect
        className="chart__hit"
        x={0}
        y={y}
        width={LAG.width}
        height={LAG.rowHeight}
        data-bar={bar.label}
      />
      <text className="chart__axis" x={LAG.plotLeft - 6} y={y + LAG.barHeight} textAnchor="end">
        {bar.label}
      </text>
      <rect
        className="lag__track"
        x={LAG.plotLeft}
        y={y + 2}
        width={LAG.plotRight - LAG.plotLeft}
        height={LAG.barHeight}
        rx={2}
      />
      <rect
        className="lag__bar"
        x={LAG.plotLeft}
        y={y + 2}
        width={width}
        height={LAG.barHeight}
        rx={2}
        data-current={current ? "true" : undefined}
        data-suspended={bar.suspended ? "true" : undefined}
      />
    </g>
  );
}
