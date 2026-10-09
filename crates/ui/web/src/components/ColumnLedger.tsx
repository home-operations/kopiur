import type { ReactNode } from "react";

import { ColumnHeader } from "./ColumnHeader";
import { useColumnPrefs } from "./columnPrefs";
import { type ColumnSpec, columnWidth, tableMinWidth, visibleColumns } from "./tableColumns";

/** Attributes a row may carry: its kind for the stripe, a state for styling. */
type RowAttributes = Record<`data-${string}`, string | undefined>;

export interface ColumnLedgerProps<Row, Id extends string> {
  /** Names the table's stored layout; stable across releases. */
  id: string;
  /** The table's accessible name. */
  label: string;
  /** The table's own class, beside `ledger`. */
  className?: string | undefined;
  columns: readonly ColumnSpec<Id>[];
  rows: readonly Row[];
  rowKey: (row: Row) => string;
  rowProps?: ((row: Row) => RowAttributes) | undefined;
  /** One cell's content. A `switch` over `Id`, so a new column cannot go undrawn. */
  cell: (row: Row, id: Id) => ReactNode;
}

/**
 * A list table whose columns a person can resize, hide and reorder — the
 * layout remembered per table, per browser (`columnPrefs.ts`).
 *
 * The table is laid out fixed: the `colgroup` says how wide each column is,
 * the flexible one takes what is left, and when the columns add up to more
 * than the card the table scrolls inside it rather than squeezing them. Text
 * in a cell still wraps (names break at `/` and `-`); nothing is cut off.
 */
export function ColumnLedger<Row, Id extends string>({
  id,
  label,
  className,
  columns,
  rows,
  rowKey,
  rowProps,
  cell,
}: ColumnLedgerProps<Row, Id>) {
  const prefs = useColumnPrefs(id);
  const visible = visibleColumns(columns, prefs);
  const classes = ["ledger", "ledger--fixed", className].filter(Boolean).join(" ");
  return (
    <div className="ledger-scroll">
      <table
        className={classes}
        aria-label={label}
        style={{ minWidth: `${String(tableMinWidth(visible, prefs))}px` }}
      >
        <colgroup>
          {visible.map((spec) => {
            const width = columnWidth(spec, prefs);
            return (
              <col
                key={spec.id}
                style={width === null ? undefined : { width: `${String(width)}px` }}
              />
            );
          })}
        </colgroup>
        <thead>
          <tr>
            {visible.map((spec) => (
              <ColumnHeader key={spec.id} table={id} spec={spec} prefs={prefs} />
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={rowKey(row)} {...rowProps?.(row)}>
              {visible.map((spec) => (
                <td key={spec.id} className={cellClass(spec)}>
                  {cell(row, spec.id)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function cellClass(spec: ColumnSpec): string | undefined {
  const classes = [
    spec.stripe === true ? "has-stripe" : null,
    spec.numeric === true ? "num" : null,
    spec.className ?? null,
  ].filter((c): c is string => c !== null);
  return classes.length > 0 ? classes.join(" ") : undefined;
}
