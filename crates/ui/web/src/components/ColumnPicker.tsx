import { ChevronDown, ChevronUp, Columns3, GripVertical, RotateCcw } from "lucide-react";
import {
  type PointerEvent as ReactPointerEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from "react";

import { resetColumns, setHidden, setOrder, useColumnPrefs } from "./columnPrefs";
import {
  type ColumnSpec,
  hiddenCount,
  isVisible,
  movableOrder,
  moveColumn,
  orderedColumns,
} from "./tableColumns";

/** Which control should hold focus once a move has re-rendered the list. */
interface Refocus {
  id: string;
  control: "grip" | "earlier" | "later";
}

/**
 * A table's "Columns" menu: tick a column off to hide it, put the columns in
 * the order you want — drag a row by its grip, use the arrow keys on the grip,
 * or the earlier/later buttons — and reset the table to how it ships.
 *
 * Locked columns (the identity column, a column of buttons) are not listed:
 * there is nothing a person may do with them. The last column on screen
 * cannot be turned off. Every change is said in a status line, since a row
 * jumping place is easy to miss.
 *
 * It behaves like the namespace switcher: a click outside closes it, Escape
 * closes it and hands focus back to its button.
 */
export function ColumnPicker({
  table,
  label,
  columns,
}: {
  table: string;
  /** The table's name, for the panel's. */
  label: string;
  columns: readonly ColumnSpec[];
}) {
  const prefs = useColumnPrefs(table);
  const [open, setOpen] = useState(false);
  const [said, setSaid] = useState("");
  const [refocus, setRefocus] = useState<Refocus | null>(null);
  const button = useRef<HTMLButtonElement | null>(null);
  const panel = useRef<HTMLDivElement | null>(null);
  const list = useRef<HTMLOListElement | null>(null);
  const drag = useRef<string | null>(null);
  const panelId = useId();

  const close = (refocusButton: boolean) => {
    setOpen(false);
    setSaid("");
    if (refocusButton) button.current?.focus();
  };

  useEffect(() => {
    if (!open) return undefined;
    list.current?.querySelector<HTMLInputElement>("input[type=checkbox]")?.focus();
    const onPointer = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!panel.current?.contains(target) && !button.current?.contains(target)) close(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") close(true);
    };
    document.addEventListener("mousedown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // A row that moved was re-inserted, which drops focus; put it back on the
  // control that moved it — or on the grip, if that control is now disabled.
  useLayoutEffect(() => {
    if (refocus === null || list.current === null) return;
    const find = (control: Refocus["control"]) =>
      list.current?.querySelector<HTMLButtonElement>(
        `[data-column="${CSS.escape(refocus.id)}"][data-control="${control}"]`,
      );
    const wanted = find(refocus.control);
    (wanted !== null && wanted !== undefined && !wanted.disabled ? wanted : find("grip"))?.focus();
    setRefocus(null);
  }, [refocus]);

  const order = movableOrder(columns, prefs);
  if (order.length === 0) return null;
  const byId = new Map(columns.map((c) => [c.id, c]));
  const hidden = hiddenCount(columns, prefs);
  const shown = orderedColumns(columns, prefs).filter((c) => isVisible(c, prefs)).length;

  const move = (id: string, to: number, control: Refocus["control"]) => {
    const next = moveColumn(order, id, to);
    if (next.join() === order.join()) return;
    setOrder(table, next);
    setSaid(
      `${byId.get(id)?.label ?? id} moved to position ${String(next.indexOf(id) + 1)} of ${String(next.length)}`,
    );
    setRefocus({ id, control });
  };

  const onGripKey = (event: ReactKeyboardEvent, id: string) => {
    const step = event.key === "ArrowUp" ? -1 : event.key === "ArrowDown" ? 1 : 0;
    if (step === 0) return;
    event.preventDefault();
    move(id, order.indexOf(id) + step, "grip");
  };

  const onGripDown = (event: ReactPointerEvent<HTMLButtonElement>, id: string) => {
    if (event.button !== 0) return;
    event.preventDefault();
    drag.current = id;
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  // Where the pointer is among the other rows: past the middle of a row is
  // past that row.
  const onGripMove = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const id = drag.current;
    if (id === null || list.current === null) return;
    const rows = Array.from(list.current.querySelectorAll<HTMLLIElement>(":scope > li"));
    const to = rows
      .filter((row) => row.dataset.column !== id)
      .filter((row) => {
        const box = row.getBoundingClientRect();
        return box.top + box.height / 2 < event.clientY;
      }).length;
    move(id, to, "grip");
  };

  const onGripUp = (event: ReactPointerEvent<HTMLButtonElement>) => {
    if (drag.current === null) return;
    drag.current = null;
    event.currentTarget.releasePointerCapture(event.pointerId);
  };

  const name = hidden > 0 ? `Columns, ${String(hidden)} hidden` : "Columns";
  return (
    <div className="column-picker">
      <button
        ref={button}
        type="button"
        className="button button--quiet column-picker__button"
        aria-label={name}
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        onClick={() => {
          if (open) close(false);
          else setOpen(true);
        }}
      >
        <Columns3 size={14} strokeWidth={2} aria-hidden="true" />
        Columns
        {hidden > 0 ? <span className="column-picker__count"> · {hidden} hidden</span> : null}
      </button>
      {open ? (
        <div
          id={panelId}
          ref={panel}
          className="column-picker__panel"
          role="dialog"
          aria-label={`Columns of ${label}`}
        >
          <ol ref={list} className="column-picker__list" aria-label="Column order">
            {order.map((id, index) => {
              const spec = byId.get(id);
              if (spec === undefined) return null;
              const visible = isVisible(spec, prefs);
              return (
                <li key={id} className="column-picker__row" data-column={id}>
                  <button
                    type="button"
                    className="column-picker__grip"
                    aria-label={`Reorder ${spec.label}`}
                    data-column={id}
                    data-control="grip"
                    onKeyDown={(event) => {
                      onGripKey(event, id);
                    }}
                    onPointerDown={(event) => {
                      onGripDown(event, id);
                    }}
                    onPointerMove={onGripMove}
                    onPointerUp={onGripUp}
                    onPointerCancel={onGripUp}
                  >
                    <GripVertical size={14} strokeWidth={2} aria-hidden="true" />
                  </button>
                  <label className="column-picker__toggle">
                    <input
                      type="checkbox"
                      checked={visible}
                      disabled={visible && shown <= 1}
                      onChange={(event) => {
                        setHidden(table, id, !event.target.checked);
                        setSaid(`${spec.label} ${event.target.checked ? "shown" : "hidden"}`);
                      }}
                    />
                    {spec.label}
                  </label>
                  <span className="column-picker__moves">
                    <button
                      type="button"
                      className="column-picker__move"
                      aria-label={`Move ${spec.label} earlier`}
                      data-column={id}
                      data-control="earlier"
                      disabled={index === 0}
                      onClick={() => {
                        move(id, index - 1, "earlier");
                      }}
                    >
                      <ChevronUp size={14} strokeWidth={2} aria-hidden="true" />
                    </button>
                    <button
                      type="button"
                      className="column-picker__move"
                      aria-label={`Move ${spec.label} later`}
                      data-column={id}
                      data-control="later"
                      disabled={index === order.length - 1}
                      onClick={() => {
                        move(id, index + 1, "later");
                      }}
                    >
                      <ChevronDown size={14} strokeWidth={2} aria-hidden="true" />
                    </button>
                  </span>
                </li>
              );
            })}
          </ol>
          <div className="column-picker__foot">
            <button
              type="button"
              className="button button--quiet"
              onClick={() => {
                resetColumns(table);
                setSaid("Column layout reset");
              }}
            >
              <RotateCcw size={14} strokeWidth={2} aria-hidden="true" />
              Reset columns
            </button>
            <span className="visually-hidden" role="status">
              {said}
            </span>
          </div>
        </div>
      ) : null}
    </div>
  );
}
