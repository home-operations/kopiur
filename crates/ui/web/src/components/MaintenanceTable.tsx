import type { ReactNode } from "react";

import type { MaintenanceRow, RunStatusView } from "../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../util/format";
import { ColumnLedger } from "./ColumnLedger";
import { healthLamp, loudLamp } from "./health";
import { LampBadge } from "./HealthBadge";
import { NotReported } from "./NotReported";
import { KindChip } from "./KindMark";
import { WireRef } from "./ObjectRef";
import { InspectLink } from "./InspectLink";
import type { ColumnSpec } from "./tableColumns";

/**
 * Every `Maintenance` in scope as a ledger: one row per object, its two run
 * tracks across the columns, any manual run, and the control that asks for
 * another.
 *
 * # Managed and hand-authored look identical, and the difference matters
 *
 * kopiur projects a `Maintenance` from a repository's `spec.maintenance`, and
 * honors a hand-authored one it finds instead. Their specs are
 * indistinguishable — the difference is a controller `ownerReference` — so
 * `managedByRepository` is the field that tells a reader whether editing this
 * object will stick or be reconciled away, and it is said under every name
 * (the repository that projected it is the Repository column).
 *
 * # The next run is a column nobody fills yet
 *
 * `Maintenance.status.{quick,full}.nextScheduledAt` is declared on the CRD and
 * written by no controller (`components/unwired.ts`), so both next-run
 * columns would read "not reported" on every row. They are there, off by
 * default, and say so in words when turned on; the value is read first, so
 * the day a controller writes it the number simply appears.
 *
 * Presentation only: the run control is built by the route, which owns the
 * hooks.
 */
export interface MaintenanceTableProps {
  rows: readonly MaintenanceRow[];
  /** The "run now" control for one row, built by the route. */
  renderAction?: ((row: MaintenanceRow) => ReactNode) | undefined;
  now?: Date | undefined;
}

type MaintenanceColumn =
  | "maintenance"
  | "repository"
  | "quickLast"
  | "quickNext"
  | "quickFailures"
  | "fullLast"
  | "fullNext"
  | "fullFailures"
  | "reclaimed"
  | "manual"
  | "action";

const MAINTENANCE_COLUMNS: readonly ColumnSpec<MaintenanceColumn>[] = [
  { id: "maintenance", label: "Maintenance", width: 240, min: 220, locked: true, stripe: true },
  { id: "repository", label: "Repository", width: 220, min: 210 },
  { id: "quickLast", label: "Quick last run", width: 140, min: 140 },
  { id: "quickNext", label: "Quick next run", width: 140, min: 140, defaultHidden: true },
  { id: "quickFailures", label: "Quick failures", width: 130, min: 130 },
  { id: "fullLast", label: "Full last run", width: 130, min: 130 },
  { id: "fullNext", label: "Full next run", width: 140, min: 130, defaultHidden: true },
  { id: "fullFailures", label: "Full failures", width: 120, min: 120 },
  { id: "reclaimed", label: "Reclaimed", width: 110, min: 100, numeric: true },
  { id: "manual", label: "Manual run", width: 200, min: 120 },
];

/** With the route's run control: a column of buttons, kept last. */
const MAINTENANCE_COLUMNS_WITH_ACTION: readonly ColumnSpec<MaintenanceColumn>[] = [
  ...MAINTENANCE_COLUMNS,
  { id: "action", label: "Run", width: 110, min: 110, locked: true, resizable: false },
];

export function MaintenanceTable({ rows, renderAction, now = new Date() }: MaintenanceTableProps) {
  return (
    <ColumnLedger
      id="maintenance"
      label="Maintenance"
      className="maintenance-table"
      columns={renderAction === undefined ? MAINTENANCE_COLUMNS : MAINTENANCE_COLUMNS_WITH_ACTION}
      rows={rows}
      rowKey={(row) => `${row.namespace}/${row.name}`}
      rowProps={() => ({ "data-kind": "maintenance" })}
      cell={(row, id) => maintenanceCell(row, id, now, renderAction)}
    />
  );
}

function maintenanceCell(
  row: MaintenanceRow,
  id: MaintenanceColumn,
  now: Date,
  renderAction: MaintenanceTableProps["renderAction"],
): ReactNode {
  switch (id) {
    case "maintenance":
      return (
        <div className="table__object">
          <KindChip kind="maintenance" size="sm" />
          <div className="maintenance-table__object">
            <InspectLink
              className="row-link mono"
              target={{ kind: "maintenance", namespace: row.namespace, name: row.name }}
            >
              {row.name}
            </InspectLink>
            <span className="maintenance-table__namespace mono">{row.namespace}</span>
            <span className="maintenance-table__note">
              {row.managedByRepository
                ? "operator-managed; edits here are reconciled away"
                : "user-authored; the operator never rewrites it"}
            </span>
          </div>
        </div>
      );
    case "repository":
      return <WireRef value={row.repository} contextNamespace={row.namespace} />;
    case "quickLast":
      return <LastRun track={row.quick} now={now} />;
    case "quickNext":
      return <NextRun track={row.quick} now={now} field="maintenanceQuickNextRun" />;
    case "quickFailures":
      return <Failures track={row.quick} />;
    case "fullLast":
      return <LastRun track={row.full} now={now} />;
    case "fullNext":
      return <NextRun track={row.full} now={now} field="maintenanceFullNextRun" />;
    case "fullFailures":
      return <Failures track={row.full} />;
    case "reclaimed": {
      // Full maintenance is what drops unreachable content; a quick run's
      // figure, when one is written, is shown if the full track has none.
      const bytes = row.full.lastContentReclaimedBytes ?? row.quick.lastContentReclaimedBytes;
      return bytes !== null && bytes !== undefined ? humanBytes(bytes) : EMPTY_CELL;
    }
    case "manual":
      return <ManualRun row={row} now={now} />;
    case "action":
      return renderAction?.(row) ?? null;
    default:
      return id satisfies never;
  }
}

function LastRun({ track, now }: { track: RunStatusView; now: Date }) {
  return track.lastRunAt !== null && track.lastRunAt !== undefined ? (
    <time dateTime={track.lastRunAt} title={track.lastRunAt}>
      {relativeTime(track.lastRunAt, now)}
    </time>
  ) : (
    <LampBadge lamp={loudLamp("never run")} />
  );
}

function NextRun({
  track,
  now,
  field,
}: {
  track: RunStatusView;
  now: Date;
  field: "maintenanceQuickNextRun" | "maintenanceFullNextRun";
}) {
  return track.nextScheduledAt !== null && track.nextScheduledAt !== undefined ? (
    <time dateTime={track.nextScheduledAt}>{relativeTime(track.nextScheduledAt, now)}</time>
  ) : (
    <NotReported field={field} />
  );
}

/**
 * Failures since the last success. The one place a bare number carried the
 * alarm: a red "2" beside a plain "0" differed by hue and nothing else, and
 * with the icon `aria-hidden` a screen reader heard only the digit. The count
 * stays the visible word, and the lamp's own word is spoken after it.
 */
function Failures({ track }: { track: RunStatusView }) {
  return track.consecutiveFailures > 0 ? (
    <LampBadge lamp={healthLamp("failed")} label={String(track.consecutiveFailures)} />
  ) : (
    "0"
  );
}

/**
 * The manual run, when there is one.
 *
 * `requestedAt` is the token the operator echoes back on the run it produced,
 * which is how a previous run's outcome is told apart from the one just asked
 * for — so it is shown rather than collapsed into "a run was requested".
 */
function ManualRun({ row, now }: { row: MaintenanceRow; now: Date }) {
  const manual = row.manualRun;
  if (manual === null || manual === undefined) {
    return EMPTY_CELL;
  }
  return (
    <span>
      <span className="mono">{manual.mode ?? "run"}</span> — {manual.phase ?? "requested"}
      {manual.requestedAt !== null && manual.requestedAt !== undefined ? (
        <>
          , asked for{" "}
          <time dateTime={manual.requestedAt} title={manual.requestedAt}>
            {relativeTime(manual.requestedAt, now)}
          </time>
        </>
      ) : null}
      {manual.completedAt !== null && manual.completedAt !== undefined ? (
        <>
          , finished{" "}
          <time dateTime={manual.completedAt}>{relativeTime(manual.completedAt, now)}</time>
        </>
      ) : null}
    </span>
  );
}
