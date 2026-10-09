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

/** A row being dragged: which, where it was grabbed, where its copy is. */
interface Drag {
  id: string;
  /** Pointer distance below the row's top edge when it was picked up. */
  grab: number;
  /** The copy's top, in the list's own coordinates. */
  top: number;
}

/** How long a row takes to slide into its new place. */
const SLIDE_MS = 160;

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
 * A drag lifts the row: a copy of it rides under the pointer, its slot stays
 * behind as an outline, and the other rows slide out of the way as it passes
 * them. The new order is saved when it is let go; Escape puts it back. Every
 * other move (a button, an arrow key, a reset) slides too, so a row is never
 * seen to teleport.
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
  const stopDrag = useRef<(() => void) | null>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [preview, setPreview] = useState<string[] | null>(null);
  const tops = useRef(new Map<string, number>());
  // The latest drag handlers, for listeners attached before they existed.
  const dragToRef = useRef<(y: number) => void>(() => undefined);
  const finishRef = useRef<(keep: boolean) => void>(() => undefined);
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
      if (event.key !== "Escape") return;
      if (stopDrag.current !== null) finishRef.current(false);
      else close(true);
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
  const shownOrder = preview ?? order;
  const byId = new Map(columns.map((c) => [c.id, c]));
  const hidden = hiddenCount(columns, prefs);
  const shown = orderedColumns(columns, prefs).filter((c) => isVisible(c, prefs)).length;

  const announce = (id: string, next: readonly string[]) => {
    setSaid(
      `${byId.get(id)?.label ?? id} moved to position ${String(next.indexOf(id) + 1)} of ${String(next.length)}`,
    );
  };

  const move = (id: string, to: number, control: Refocus["control"]) => {
    const next = moveColumn(order, id, to);
    if (next.join() === order.join()) return;
    setOrder(table, next);
    announce(id, next);
    setRefocus({ id, control });
  };

  const onGripKey = (event: ReactKeyboardEvent, id: string) => {
    const step = event.key === "ArrowUp" ? -1 : event.key === "ArrowDown" ? 1 : 0;
    if (step === 0) return;
    event.preventDefault();
    move(id, order.indexOf(id) + step, "grip");
  };

  // Each row that changed place starts from where it was and slides to where
  // it is (measured from layout, so a slide still running does not skew it).
  useLayoutEffect(() => {
    const rows = list.current?.querySelectorAll<HTMLLIElement>(":scope > li") ?? [];
    const still =
      typeof window.matchMedia === "function" &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const seen = new Map<string, number>();
    for (const row of rows) {
      const id = row.dataset.column ?? "";
      const was = tops.current.get(id);
      seen.set(id, row.offsetTop);
      if (was === undefined || was === row.offsetTop || still) continue;
      if (typeof row.animate !== "function") continue;
      row.animate(
        [
          { transform: `translateY(${String(was - row.offsetTop)}px)` },
          { transform: "translateY(0)" },
        ],
        { duration: SLIDE_MS, easing: "cubic-bezier(0.2, 0, 0, 1)" },
      );
    }
    tops.current = seen;
  });

  /** The row slots: their pitch (row plus gap) and how many. */
  const slots = () => {
    const rows = Array.from(list.current?.querySelectorAll<HTMLLIElement>(":scope > li") ?? []);
    const first = rows[0];
    const second = rows[1];
    const pitch =
      first !== undefined && second !== undefined
        ? second.offsetTop - first.offsetTop
        : (first?.offsetHeight ?? 0);
    return { pitch, count: rows.length };
  };

  // The copy follows the pointer, kept inside the list; the slot nearest its
  // top is where the row would land, and the others make room for it there.
  const dragTo = (y: number) => {
    if (drag === null || list.current === null) return;
    const { pitch, count } = slots();
    const listTop = list.current.getBoundingClientRect().top;
    const top = Math.max(0, Math.min((count - 1) * pitch, y - listTop - drag.grab));
    const to = pitch > 0 ? Math.round(top / pitch) : 0;
    setDrag({ ...drag, top });
    setPreview(moveColumn(shownOrder, drag.id, to));
  };

  const finish = (keep: boolean) => {
    stopDrag.current?.();
    if (drag !== null && keep && preview !== null && preview.join() !== order.join()) {
      setOrder(table, preview);
      announce(drag.id, preview);
      setRefocus({ id: drag.id, control: "grip" });
    }
    setDrag(null);
    setPreview(null);
  };

  useLayoutEffect(() => {
    dragToRef.current = dragTo;
    finishRef.current = finish;
  });
  useEffect(
    () => () => {
      stopDrag.current?.();
    },
    [],
  );

  // A drag is followed on the window, not the grip: the release can land
  // anywhere. A move with no button held means the release was missed; the
  // row is dropped where it is.
  const onGripDown = (event: ReactPointerEvent<HTMLButtonElement>, id: string) => {
    if (event.button !== 0 || list.current === null) return;
    event.preventDefault();
    stopDrag.current?.();
    const row = event.currentTarget.closest("li");
    const rowTop = row?.offsetTop ?? 0;
    const listTop = list.current.getBoundingClientRect().top;
    const onMove = (e: PointerEvent) => {
      if ((e.buttons & 1) === 0) {
        finishRef.current(true);
        return;
      }
      dragToRef.current(e.clientY);
    };
    const onUp = () => {
      finishRef.current(true);
    };
    const stop = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
      stopDrag.current = null;
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    stopDrag.current = stop;
    setDrag({ id, grab: event.clientY - listTop - rowTop, top: rowTop });
    setPreview(order);
  };

  if (order.length === 0) return null;
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
          data-dragging={drag !== null ? "true" : undefined}
        >
          <div className="column-picker__stack">
            <ol ref={list} className="column-picker__list" aria-label="Column order">
              {shownOrder.map((id, index) => {
                const spec = byId.get(id);
                if (spec === undefined) return null;
                const visible = isVisible(spec, prefs);
                return (
                  <li
                    key={id}
                    className="column-picker__row"
                    data-column={id}
                    data-dragging={drag?.id === id ? "true" : undefined}
                  >
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
                        disabled={index === shownOrder.length - 1}
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
            {drag !== null ? (
              <div
                className="column-picker__ghost"
                aria-hidden="true"
                style={{ top: `${String(drag.top)}px` }}
              >
                <GripVertical size={14} strokeWidth={2} />
                {byId.get(drag.id)?.label ?? drag.id}
              </div>
            ) : null}
          </div>
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
