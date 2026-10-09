import { Pin } from "lucide-react";
import type { ReactNode } from "react";

import type { SnapshotRow } from "../api/types";
import { EMPTY_CELL, humanAge, humanBytes, humanDuration } from "../util/format";
import { ColumnLedger } from "./ColumnLedger";
import { LampBadge } from "./HealthBadge";
import { KindChip } from "./KindMark";
import { ObjectRef, WireRef } from "./ObjectRef";
import { durationSeconds, originLabel, snapshotPhaseLamp } from "./snapshot";
import { InspectLink } from "./InspectLink";
import type { ColumnSpec } from "./tableColumns";

/**
 * The snapshots ledger: one row per `Snapshot` resource, newest run first.
 *
 * The order is the **server's** (`filter_rows` sorts by the run's start time
 * with the CR name as the tiebreaker, so two runs that started in the same
 * second do not swap places between polls), and this never re-sorts: a table
 * that reordered client-side would disagree with the `offset`/`limit` window
 * the same request produced, and page 2 would repeat rows from page 1.
 *
 * Three columns that are not obvious.
 *
 * **Started, not finished.** Every run has a start; only a terminal one has an
 * end. A "Finished" column would be empty for exactly the rows an operator
 * opens the list to look at. The end time is on the detail, where it is also
 * the instant GFS buckets on.
 *
 * **Files carries its failures.** `filesFailed` is what turns a green
 * "Succeeded" into a partial backup — kopia finished, having skipped files it
 * could not read — so the count rides in the same cell, in the failed lamp's
 * ink, rather than in a column an eye scanning for trouble would not reach.
 *
 * **No "new bytes" column.** `Snapshot.status.stats.bytesNew` is declared on
 * the CRD and written by no controller (`components/unwired.ts`), so the
 * column would be "not reported" in every row of every cluster. The detail
 * says so once, in words, which is the honest place for it.
 */
export interface SnapshotTableProps {
  rows: readonly SnapshotRow[];
  /** The clock ages are measured against. */
  now?: Date | undefined;
  /** Table caption / accessible name. */
  caption?: string | undefined;
}

type SnapshotColumn =
  | "snapshot"
  | "phase"
  | "origin"
  | "policy"
  | "repository"
  | "size"
  | "files"
  | "started"
  | "took";

const SNAPSHOT_COLUMNS: readonly ColumnSpec<SnapshotColumn>[] = [
  { id: "snapshot", label: "Snapshot", width: "auto", min: 200, locked: true, stripe: true },
  { id: "phase", label: "Phase", width: 130, min: 130 },
  { id: "origin", label: "Origin", width: 110, min: 80 },
  { id: "policy", label: "Policy", width: 210, min: 200 },
  {
    id: "repository",
    label: "Repository",
    width: 220,
    min: 210,
    className: "snapshot-table__repository",
  },
  { id: "size", label: "Size", width: 100, min: 96, numeric: true },
  { id: "files", label: "Files", width: 110, min: 80, numeric: true },
  { id: "started", label: "Started", width: 100, min: 90, numeric: true },
  { id: "took", label: "Took", width: 80, min: 70, numeric: true },
];

export function SnapshotTable({
  rows,
  now = new Date(),
  caption = "Snapshots",
}: SnapshotTableProps) {
  return (
    <ColumnLedger
      id="snapshots"
      label={caption}
      className="snapshot-table"
      columns={SNAPSHOT_COLUMNS}
      rows={rows}
      rowKey={(row) => `${row.namespace}/${row.name}`}
      rowProps={() => ({ "data-kind": "snapshot" })}
      cell={(row, id) => snapshotCell(row, id, now)}
    />
  );
}

function snapshotCell(row: SnapshotRow, id: SnapshotColumn, now: Date): ReactNode {
  switch (id) {
    case "snapshot":
      return (
        <div className="table__object">
          <KindChip kind="snapshot" size="sm" />
          <div className="snapshot-table__object">
            <InspectLink
              className="row-link mono snapshot-table__name"
              target={{ kind: "snapshot", namespace: row.namespace, name: row.name }}
            >
              {row.name}
            </InspectLink>
            <span className="snapshot-table__namespace mono">{row.namespace}</span>
            {row.pinned ? (
              <span className="snapshot-table__pin">
                <Pin size={12} strokeWidth={2} aria-hidden="true" />
                pinned
              </span>
            ) : null}
          </div>
        </div>
      );
    case "phase":
      return <LampBadge lamp={snapshotPhaseLamp(row.phase)} />;
    case "origin":
      return originLabel(row.origin);
    case "policy":
      return row.policy !== null && row.policy !== undefined && row.policy.length > 0 ? (
        <ObjectRef
          kind="snapshotPolicy"
          name={row.policy}
          namespace={row.namespace}
          contextNamespace={row.namespace}
        />
      ) : (
        EMPTY_CELL
      );
    case "repository":
      return <WireRef value={row.repository} contextNamespace={row.namespace} />;
    case "size":
      return humanBytes(row.sizeBytes);
    case "files":
      return <FilesCell total={row.filesTotal} failed={row.filesFailed} />;
    case "started":
      return row.startTime !== null && row.startTime !== undefined ? (
        <time dateTime={row.startTime}>{humanAge(row.startTime, now)}</time>
      ) : (
        EMPTY_CELL
      );
    case "took":
      return humanDuration(durationSeconds(row));
    default:
      return id satisfies never;
  }
}

/** A nullable string as itself, or the ledger's empty cell. */
interface FilesCellProps {
  total: number | null | undefined;
  failed: number | null | undefined;
}

/**
 * Files considered, and the ones kopia could not read.
 *
 * A non-zero failure count is the difference between a backup and a backup
 * with holes in it, so it is said in the cell rather than left to the detail.
 * A zero is not rendered: "0 failed" on every healthy row would bury the rows
 * where it is not zero.
 */
function FilesCell({ total, failed }: FilesCellProps) {
  const count = total !== null && total !== undefined ? total.toLocaleString() : EMPTY_CELL;
  if (failed === null || failed === undefined || failed === 0) {
    return <>{count}</>;
  }
  return (
    <>
      {count}{" "}
      <span className="snapshot-table__failed">
        {failed.toLocaleString()} failed
        <span className="visually-hidden"> to read</span>
      </span>
    </>
  );
}
