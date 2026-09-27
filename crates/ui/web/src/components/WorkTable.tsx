import type { ReactNode } from "react";

import type { Health } from "../api/types";
import { EMPTY_CELL, humanAge } from "../util/format";
import { HealthBadge } from "./HealthBadge";

/**
 * One row of work: an object, its state as a lamp, a detail line and an age.
 *
 * The shape is deliberately route-agnostic so the overview (stalled objects),
 * the snapshot and restore lists, and a detail drawer's "recent runs" can all
 * feed it: the caller maps its wire type onto a `WorkRow` and, when a detail
 * route exists for the object, supplies the name as a link — the table knows
 * nothing about routes.
 */
export interface WorkRow {
  /** Stable React key: `kind/namespace/name` or similar. */
  id: string;
  /** The CRD kind, rendered as the label strip's KIND. */
  kind: string;
  name: string;
  /** Absent for a cluster-scoped object. */
  namespace?: string | null | undefined;
  health: Health;
  /**
   * The lamp's word when the state has a better name than the health —
   * "Stalled", "Running", "Restoring". The icon and colour stay the lamp's.
   */
  stateWord?: string | undefined;
  /** What is going on: a condition message, a failure, a phase note. */
  detail?: string | null | undefined;
  /** RFC3339 instant the age is measured from; absent renders the empty cell. */
  at?: string | null | undefined;
  /** The name rendered as a link into a detail route, when one exists. */
  nameLink?: ReactNode;
}

export interface WorkTableProps {
  /** The table's accessible name: "Stalled objects", "Recent restores". */
  caption: string;
  rows: readonly WorkRow[];
  /** The clock ages are measured against — the server's `now` when known. */
  now?: Date | undefined;
  /** Rendered instead of the table when there are no rows. */
  empty?: ReactNode;
  /** Label for the age column, e.g. "Since" or "Started". */
  ageLabel?: string | undefined;
}

/**
 * The ledger every list of work inherits: label strip, lamp, detail, age.
 *
 * The age column appears only when at least one row has an instant to
 * measure — a column of empty cells says nothing and costs the detail its
 * width. On a narrow viewport the ledger scrolls inside its wrapper rather
 * than starving the object column to nothing.
 */
export function WorkTable({
  caption,
  rows,
  now = new Date(),
  empty,
  ageLabel = "Age",
}: WorkTableProps) {
  if (rows.length === 0) {
    return <>{empty}</>;
  }
  const showAge = rows.some((row) => row.at !== undefined && row.at !== null);
  return (
    <div className="ledger-scroll">
      <table className="ledger work-table" aria-label={caption}>
        <thead>
          <tr>
            <th scope="col">Object</th>
            <th scope="col">State</th>
            <th scope="col">Detail</th>
            {showAge ? (
              <th scope="col" className="num">
                {ageLabel}
              </th>
            ) : null}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.id}>
              <td>
                <div className="work-table__object">
                  <span className="label-strip">
                    <span className="label-strip__kind">{row.kind}</span>
                    <span className="label-strip__name">{row.nameLink ?? row.name}</span>
                  </span>
                  {row.namespace !== undefined &&
                  row.namespace !== null &&
                  row.namespace.length > 0 ? (
                    <span className="work-table__namespace mono">{row.namespace}</span>
                  ) : null}
                </div>
              </td>
              <td>
                <HealthBadge health={row.health} label={row.stateWord} />
              </td>
              <td className="work-table__detail">
                {row.detail !== undefined && row.detail !== null && row.detail.length > 0
                  ? row.detail
                  : EMPTY_CELL}
              </td>
              {showAge ? (
                <td className="num">
                  {row.at !== undefined && row.at !== null ? (
                    <time dateTime={row.at}>{humanAge(row.at, now)}</time>
                  ) : (
                    EMPTY_CELL
                  )}
                </td>
              ) : null}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
