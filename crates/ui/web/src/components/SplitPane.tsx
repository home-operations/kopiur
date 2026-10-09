import { type CSSProperties, type ReactNode, useEffect, useRef, useState } from "react";

import { SPLIT_DEFAULT, SPLIT_STEP, clampSplit, readSplit, saveSplit } from "./splitRatio";

/**
 * Two sides with a handle between them: drag the 8px gutter (its three
 * stacked dots are the grip), or focus it and use the arrow keys — Home and
 * End for either end, a double click for half and half. Where it was left is
 * remembered under `storageKey` (`splitRatio.ts`).
 *
 * When the pane is too narrow for both sides at their minimum, they stack,
 * start first — a container query, so the sidebar's width counts too — and
 * the handle goes, since there is nothing to divide. The DOM order is the
 * stacked order, so reading and tabbing follow what is seen.
 */
export function SplitPane({
  label,
  storageKey,
  start,
  end,
}: {
  /** What the two sides are, for the group. */
  label: string;
  storageKey: string;
  start: ReactNode;
  end: ReactNode;
}) {
  const grid = useRef<HTMLDivElement | null>(null);
  const handle = useRef<HTMLDivElement | null>(null);
  const [ratio, setRatio] = useState(() => readSplit(storageKey));
  const ratioRef = useRef(ratio);

  // Listeners go on the handle directly, as the drawer's grip does: a drag
  // leaves the 8px gutter at once, so it captures the pointer, and the share
  // is saved once, when the drag ends.
  useEffect(() => {
    const node = handle.current;
    const pane = grid.current;
    if (node === null || pane === null) return;
    // A focusable separator is a widget the a11y lint's role table does not
    // yet know; the tab stop is set here.
    node.tabIndex = 0;
    const width = () => pane.getBoundingClientRect().width;
    const move = (next: number) => {
      const clamped = clampSplit(next, width());
      ratioRef.current = clamped;
      setRatio(clamped);
      return clamped;
    };
    let dragging = false;
    const onDown = (event: PointerEvent) => {
      if (event.button !== 0) return;
      event.preventDefault();
      dragging = true;
      node.setPointerCapture(event.pointerId);
    };
    const onMove = (event: PointerEvent) => {
      if (!dragging) return;
      const box = pane.getBoundingClientRect();
      if (box.width <= 0) return;
      move((event.clientX - box.left) / box.width);
    };
    const onUp = (event: PointerEvent) => {
      if (!dragging) return;
      dragging = false;
      node.releasePointerCapture(event.pointerId);
      saveSplit(storageKey, ratioRef.current);
    };
    const onKey = (event: KeyboardEvent) => {
      const current = ratioRef.current;
      const next =
        event.key === "ArrowRight"
          ? current + SPLIT_STEP
          : event.key === "ArrowLeft"
            ? current - SPLIT_STEP
            : event.key === "Home"
              ? 0
              : event.key === "End"
                ? 1
                : null;
      if (next === null) return;
      event.preventDefault();
      saveSplit(storageKey, move(next));
    };
    const onDouble = () => {
      saveSplit(storageKey, move(SPLIT_DEFAULT));
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
  }, [storageKey]);

  const style = {
    "--split-start": `${String(ratio)}fr`,
    "--split-end": `${String(1 - ratio)}fr`,
  } as CSSProperties;
  return (
    <div className="split" role="group" aria-label={label}>
      <div ref={grid} className="split__grid" style={style}>
        <div className="split__side">{start}</div>
        <div
          ref={handle}
          className="split__gutter"
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize the two columns"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(ratio * 100)}
        />
        <div className="split__side">{end}</div>
      </div>
    </div>
  );
}
