import { useEffect, useLayoutEffect, useRef, useState } from "react";

import { commitWidths, resetWidth, setWidth } from "./columnPrefs";
import { COLUMN_STEP, type ColumnSpec, MAX_COLUMN_WIDTH, clampWidth } from "./tableColumns";

/**
 * One column's header, with the handle on its right edge that resizes it.
 *
 * The handle is a focusable separator like the drawer's grip and the
 * overview's split: drag it, or focus it and use the arrow keys; Home or a
 * double click puts the declared width back. A drag is measured from where
 * it started (the width then plus how far the pointer has gone), so a column
 * never creeps; the width is stored when the drag ends, and on each key.
 *
 * The flexible column has no width of its own until someone drags it, so a
 * drag starts from the width it is drawn at.
 */
export function ColumnHeader({
  table,
  spec,
  width: fitted,
}: {
  table: string;
  spec: ColumnSpec;
  /** What the table draws it at; `null` for the flexible column left alone. */
  width: number | null;
}) {
  const header = useRef<HTMLTableCellElement | null>(null);
  const handle = useRef<HTMLDivElement | null>(null);
  const [drawn, setDrawn] = useState<number | null>(null);
  const width = fitted ?? drawn ?? spec.min;
  const widthRef = useRef(width);
  useLayoutEffect(() => {
    widthRef.current = width;
  }, [width]);
  const resizable = spec.resizable !== false;

  // The flexible column's drawn width, so the handle can say what it is.
  useLayoutEffect(() => {
    const node = header.current;
    if (node === null || fitted !== null) return;
    const measured = Math.round(node.getBoundingClientRect().width);
    if (measured > 0 && measured !== drawn) setDrawn(measured);
  }, [fitted, drawn]);

  useEffect(() => {
    const node = handle.current;
    if (node === null) return;
    // A focusable separator is a widget the a11y lint's role table does not
    // yet know; the tab stop is set here, as on the drawer's grip.
    node.tabIndex = 0;
    const resize = (px: number) => {
      setWidth(table, spec.id, clampWidth(spec, px));
    };
    // Where a change starts from is the width on screen now — the window or
    // the sidebar may have moved it since anything was measured — falling
    // back to what the table asked for where nothing is laid out.
    const current = () => {
      const measured = Math.round(header.current?.getBoundingClientRect().width ?? 0);
      return measured > 0 ? measured : widthRef.current;
    };
    let start: { x: number; width: number } | null = null;
    const onDown = (event: PointerEvent) => {
      if (event.button !== 0) return;
      event.preventDefault();
      event.stopPropagation();
      start = { x: event.clientX, width: current() };
      node.setPointerCapture(event.pointerId);
      node.dataset.dragging = "true";
    };
    const onMove = (event: PointerEvent) => {
      if (start === null) return;
      resize(start.width + (event.clientX - start.x));
    };
    const onUp = (event: PointerEvent) => {
      if (start === null) return;
      start = null;
      node.releasePointerCapture(event.pointerId);
      delete node.dataset.dragging;
      commitWidths();
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Home") {
        event.preventDefault();
        resetWidth(table, spec.id);
        return;
      }
      const step =
        event.key === "ArrowRight" ? COLUMN_STEP : event.key === "ArrowLeft" ? -COLUMN_STEP : 0;
      if (step === 0) return;
      event.preventDefault();
      resize(current() + step);
      commitWidths();
    };
    const onDouble = () => {
      resetWidth(table, spec.id);
    };
    node.addEventListener("pointerdown", onDown);
    node.addEventListener("pointermove", onMove);
    node.addEventListener("pointerup", onUp);
    node.addEventListener("pointercancel", onUp);
    node.addEventListener("keydown", onKey);
    node.addEventListener("dblclick", onDouble);
    return () => {
      node.removeEventListener("pointerdown", onDown);
      node.removeEventListener("pointermove", onMove);
      node.removeEventListener("pointerup", onUp);
      node.removeEventListener("pointercancel", onUp);
      node.removeEventListener("keydown", onKey);
      node.removeEventListener("dblclick", onDouble);
    };
  }, [table, spec, resizable]);

  return (
    <th ref={header} scope="col" className={spec.numeric === true ? "num" : undefined}>
      <span className="ledger__label" title={spec.label}>
        {spec.label}
      </span>
      {resizable ? (
        <div
          ref={handle}
          className="ledger__resize"
          role="separator"
          aria-orientation="vertical"
          aria-label={`Resize ${spec.label}`}
          aria-valuemin={spec.min}
          aria-valuemax={MAX_COLUMN_WIDTH}
          aria-valuenow={width}
          aria-valuetext={`${String(width)} pixels`}
        />
      ) : null}
    </th>
  );
}
