import { Link } from "@tanstack/react-router";
import { Pin } from "lucide-react";

import type { SnapshotRow } from "../api/types";
import { EMPTY_CELL, humanAge, humanBytes, humanDuration } from "../util/format";
import { LampBadge } from "./HealthBadge";
import { durationSeconds, originLabel, snapshotPhaseLamp } from "./snapshot";

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

export function SnapshotTable({
  rows,
  now = new Date(),
  caption = "Snapshots",
}: SnapshotTableProps) {
  return (
    <div className="ledger-scroll">
      <table className="ledger snapshot-table" aria-label={caption}>
        <thead>
          <tr>
            <th scope="col">Snapshot</th>
            <th scope="col">Phase</th>
            <th scope="col">Origin</th>
            <th scope="col">Policy</th>
            <th scope="col">Repository</th>
            <th scope="col" className="num">
              Size
            </th>
            <th scope="col" className="num">
              Files
            </th>
            <th scope="col" className="num">
              Started
            </th>
            <th scope="col" className="num">
              Took
            </th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={`${row.namespace}/${row.name}`}>
              <td>
                <div className="snapshot-table__object">
                  <Link
                    className="mono snapshot-table__name"
                    to="/snapshots/$namespace/$name"
                    params={{ namespace: row.namespace, name: row.name }}
                  >
                    {row.name}
                  </Link>
                  <span className="snapshot-table__namespace mono">{row.namespace}</span>
                  {row.pinned ? (
                    <span className="snapshot-table__pin">
                      <Pin size={12} strokeWidth={2} aria-hidden="true" />
                      pinned
                    </span>
                  ) : null}
                </div>
              </td>
              <td>
                <LampBadge lamp={snapshotPhaseLamp(row.phase)} />
              </td>
              <td>{originLabel(row.origin)}</td>
              <td className="mono">{orDash(row.policy)}</td>
              <td className="mono snapshot-table__repository">{orDash(row.repository)}</td>
              <td className="num">{humanBytes(row.sizeBytes)}</td>
              <td className="num">
                <FilesCell total={row.filesTotal} failed={row.filesFailed} />
              </td>
              <td className="num">
                {row.startTime !== null && row.startTime !== undefined ? (
                  <time dateTime={row.startTime}>{humanAge(row.startTime, now)}</time>
                ) : (
                  EMPTY_CELL
                )}
              </td>
              <td className="num">{humanDuration(durationSeconds(row))}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** A nullable string as itself, or the ledger's empty cell. */
function orDash(value: string | null | undefined): string {
  return value !== null && value !== undefined && value.length > 0 ? value : EMPTY_CELL;
}

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
