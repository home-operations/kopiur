/**
 * A whole table row that opens its object, without a stretched link.
 *
 * Every list row carries one real link to its object — `a.row-link`, the
 * name — and that anchor is the keyboard and screen-reader path: it is the
 * only thing in the row that takes focus or is announced as a link. A pointer
 * click anywhere else in the row is forwarded to it here.
 *
 * Why not CSS: the old pattern stretched the link's `::after` over a
 * `position: relative` `<tr>`. Shipping Safari does not make a table row a
 * containing block (WebKit bug 240961) and Firefox ESR 115 has no `:has()`, so
 * in those browsers every row's cover filled the whole table and the LAST
 * row's won — every row opened the last object (#583). A click handler works
 * the same in every engine. The grid-div rows of the overview
 * (`.attention-row`, `.activity-row`) are positioned blocks, not table rows,
 * and keep their CSS cover.
 *
 * Put `className={ROW_LINK_ROW}`, `onClick={followRowLink}` and
 * `onAuxClick={followRowLink}` on the `<tr>` (`ColumnLedger`'s `rowLinks` does
 * it for its rows). Never give the row a `tabindex` or a `role`.
 */

import type { MouseEvent as ReactMouseEvent } from "react";

/** The class a row that follows its link carries: the pointer and the hover tint. */
export const ROW_LINK_ROW = "row--link";

/** Anything in a row that answers a click of its own, and so is never hijacked. */
const INTERACTIVE = [
  "a[href]",
  "button",
  "summary",
  "details",
  "input",
  "select",
  "textarea",
  "label",
  "[contenteditable]:not([contenteditable='false'])",
  "[role='button']",
  "[role='link']",
  "[role='checkbox']",
  "[role='switch']",
  "[role='menuitem']",
  "[role='tab']",
  "[role='separator']",
].join(", ");

/**
 * Follow the row's own `a.row-link` for a click on a plain part of the row.
 *
 * Left alone: a click already handled; one inside a link, button or other
 * control (it keeps its own meaning); one inside something the row portals
 * out of the table (a popover's clicks still bubble through React); and one
 * that ends a text selection, so selecting a name to copy never navigates.
 *
 * A primary click is re-dispatched on the link with the same modifier keys,
 * so the router's `Link` handles it exactly as a click on the name: a plain
 * click opens the drawer in the page, and ctrl/⌘/shift/alt-click are left to
 * the browser's own new-tab / new-window / download behaviour for that link.
 * A middle-click (`auxclick`, button 1) opens the link's address in a new tab,
 * as a middle-click on the name would — a synthetic middle click is not
 * honoured consistently across browsers, so it is not re-dispatched.
 */
export function followRowLink(event: ReactMouseEvent<HTMLElement>): void {
  const row = event.currentTarget;
  const target = event.target;
  if (event.defaultPrevented || !(target instanceof Element) || !row.contains(target)) {
    return;
  }
  const control = target.closest(INTERACTIVE);
  if (control !== null && row.contains(control)) {
    return;
  }
  if ((window.getSelection()?.toString() ?? "") !== "") {
    return;
  }
  const link = row.querySelector<HTMLAnchorElement>("a.row-link[href]");
  if (link === null) {
    return;
  }
  if (event.type === "auxclick") {
    if (event.button === 1) {
      event.preventDefault();
      window.open(link.href, "_blank", "noopener");
    }
    return;
  }
  if (event.button !== 0) {
    return;
  }
  link.dispatchEvent(
    new MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      button: 0,
      ctrlKey: event.ctrlKey,
      metaKey: event.metaKey,
      shiftKey: event.shiftKey,
      altKey: event.altKey,
    }),
  );
}
