import { useRouter, useRouterState } from "@tanstack/react-router";
import { type ReactNode, useEffect, useState } from "react";

import { ColumnHeader } from "./ColumnHeader";
import { ColumnPicker } from "./ColumnPicker";
import { useColumnPrefs } from "./columnPrefs";
import { ROW_LINK_ROW, followRowLink } from "./rowLink";
import {
  type ColumnSpec,
  type SortValue,
  type TableSort,
  fitWidths,
  nextSort,
  parseSort,
  sortParam,
  sortRows,
  tableMinWidth,
  visibleColumns,
} from "./tableColumns";

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
  /**
   * Every row carries an `a.row-link` to its object, and a click anywhere
   * else in the row follows it (`rowLink.ts`). Required wherever a cell
   * renders a `row-link` — the stylesheet no longer stretches the link.
   */
  rowLinks?: boolean | undefined;
  /** One cell's content. A `switch` over `Id`, so a new column cannot go undrawn. */
  cell: (row: Row, id: Id) => ReactNode;
  /**
   * What a sortable column (`ColumnSpec.sort`) orders by in one row — a
   * `switch` over `Id` like `cell`; `null` for none. Without it (or
   * `serverSorted`) no column sorts.
   */
  sortValue?: ((row: Row, id: Id) => SortValue) | undefined;
  /**
   * The rows arrive already in `?sort=` order (the server pages them): the
   * headers show the sort, and the rows are drawn as they came.
   */
  serverSorted?: boolean | undefined;
}

/**
 * A list table whose columns a person can resize, hide and reorder — the
 * layout remembered per table, per browser (`columnPrefs.ts`).
 *
 * The table is laid out fixed: the `colgroup` says how wide each column is,
 * the last one takes what is left, and when the columns add up to more than
 * the card the table scrolls inside it rather than squeezing them. Dragging a
 * column's edge changes that column and nothing else; the ones after it move
 * along, and the last gives or takes the difference. Text
 * in a cell still wraps (names break at `/` and `-`); nothing is cut off.
 *
 * A column that declares a `sort` has its header as a button: the order is
 * the page's `?sort=` (one table per page), so it can be shared and Back
 * undoes it. Changing it drops `offset` — a new order starts at its top.
 */
export function ColumnLedger<Row, Id extends string>({
  id,
  label,
  className,
  columns,
  rows,
  rowKey,
  rowProps,
  rowLinks = false,
  cell,
  sortValue,
  serverSorted = false,
}: ColumnLedgerProps<Row, Id>) {
  const prefs = useColumnPrefs(id);
  const sorts = sortValue !== undefined || serverSorted;
  const sortable = sorts ? columns : [];
  const [sort, setSort] = useTableSort(sortable);
  const shown =
    serverSorted || sortValue === undefined
      ? rows
      : sortRows(rows, sort, (row, column) => sortValue(row, column as Id));
  const visible = visibleColumns(columns, prefs);
  const [card, setCard] = useState<HTMLDivElement | null>(null);
  const widths = fitWidths(visible, prefs, useCardWidth(card));
  const floor = tableMinWidth(visible, widths);
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
          style={{ width: "100%", minWidth: `${String(floor)}px` }}
        >
          <colgroup>
            {visible.map((spec, i) => {
              // The last column has no width of its own: it takes the room
              // the others leave, so the table always meets the card's edge
              // and a drag never pulls on a column it was not made on.
              const last = i === visible.length - 1;
              return (
                <col
                  key={spec.id}
                  style={last ? undefined : { width: `${String(widths[i] ?? spec.width)}px` }}
                />
              );
            })}
          </colgroup>
          <thead>
            <tr>
              {visible.map((spec, i) => (
                <ColumnHeader
                  key={spec.id}
                  table={id}
                  spec={spec}
                  width={widths[i] ?? spec.width}
                  last={i === visible.length - 1}
                  sort={sorts ? (sort?.id === spec.id ? sort : null) : undefined}
                  onSort={() => {
                    setSort(nextSort(sort, spec));
                  }}
                />
              ))}
            </tr>
          </thead>
          <tbody>
            {shown.map((row) => (
              <tr
                key={rowKey(row)}
                {...rowProps?.(row)}
                {...(rowLinks
                  ? { className: ROW_LINK_ROW, onClick: followRowLink, onAuxClick: followRowLink }
                  : {})}
              >
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
 * The page's `?sort=`, read against this table's columns, and how to change
 * it. Each change is a history entry, so Back steps through the sorts; the
 * page keeps its place.
 */
function useTableSort(
  columns: readonly ColumnSpec[],
): [TableSort | null, (next: TableSort | null) => void] {
  const router = useRouter();
  const location = useRouterState({ select: (s) => s.location });
  const search = location.search as Record<string, unknown>;
  const sort = parseSort(search.sort, columns);
  const set = (next: TableSort | null) => {
    const rest = Object.fromEntries(
      Object.entries(search).filter(([key]) => key !== "sort" && key !== "offset"),
    );
    const param = sortParam(next);
    void router.navigate({
      to: location.pathname,
      search: (param === undefined ? rest : { ...rest, sort: param }) as never,
      resetScroll: false,
    });
  };
  return [sort, set];
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
