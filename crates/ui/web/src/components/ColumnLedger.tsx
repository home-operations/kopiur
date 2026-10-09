import { type ReactNode, useEffect, useState } from "react";

import { ColumnHeader } from "./ColumnHeader";
import { ColumnPicker } from "./ColumnPicker";
import { useColumnPrefs } from "./columnPrefs";
import { type ColumnSpec, fitWidths, tableMinWidth, visibleColumns } from "./tableColumns";

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
  const [card, setCard] = useState<HTMLDivElement | null>(null);
  const widths = fitWidths(visible, prefs, useCardWidth(card));
  const floor = tableMinWidth(visible, widths);
  // Fixed layout spreads any room the columns leave over across all of them,
  // so a column would stop following its handle. With a flexible column that
  // column takes it; with none, the table is exactly as wide as its columns.
  const flexible = widths.some((w) => w === null);
  const classes = ["ledger", "ledger--fixed", className].filter(Boolean).join(" ");
  return (
    <div className="ledger-frame">
      <div className="ledger-tools">
        <ColumnPicker table={id} label={label} columns={columns} />
      </div>
      <div ref={setCard} className="ledger-scroll">
        <table
          className={classes}
          aria-label={label}
          style={
            flexible
              ? { width: "100%", minWidth: `${String(floor)}px` }
              : { width: `${String(floor)}px` }
          }
        >
          <colgroup>
            {visible.map((spec, i) => {
              const width = widths[i] ?? null;
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
              {visible.map((spec, i) => (
                <ColumnHeader key={spec.id} table={id} spec={spec} width={widths[i] ?? null} />
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
    </div>
  );
}

/**
 * How wide the card is inside its border, kept current as the window or the
 * sidebar changes it; `null` until it is laid out (or where nothing can
 * measure it, as under test).
 */
function useCardWidth(node: HTMLDivElement | null): number | null {
  const [width, setWidth] = useState<number | null>(null);
  useEffect(() => {
    if (node === null || typeof ResizeObserver === "undefined") return undefined;
    const observer = new ResizeObserver(() => {
      const next = node.clientWidth;
      setWidth(next > 0 ? next : null);
    });
    observer.observe(node);
    return () => {
      observer.disconnect();
    };
  }, [node]);
  return width;
}

function cellClass(spec: ColumnSpec): string | undefined {
  const classes = [
    spec.stripe === true ? "has-stripe" : null,
    spec.numeric === true ? "num" : null,
    spec.className ?? null,
  ].filter((c): c is string => c !== null);
  return classes.length > 0 ? classes.join(" ") : undefined;
}
