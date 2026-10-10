// vitest setup, run once per test file (see vite.config.ts `test.setupFiles`).
import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

import { browsed } from "./components/browse/browsed";
import { toasts } from "./components/toast/toasts";

afterEach(() => {
  cleanup();
  // The toast list is module state; one test's answers must not reach the next.
  toasts.clear();
  // So is what Browse is on, and the tab's storage behind it.
  browsed.reset();
  window.sessionStorage.clear();
});

// jsdom has no modal dialogs. Stand in for the two calls the side panel makes:
// opening toggles the `open` attribute (which is what makes it visible to the
// accessibility tree), closing removes it and fires `close` — on a later task,
// as browsers do, so a stale `close` can land after the dialog reopened. The
// top layer, the backdrop and the inert page behind it are browser behaviour,
// checked live, not here.
const dialog = HTMLDialogElement.prototype as Partial<HTMLDialogElement>;
if (typeof dialog.showModal !== "function") {
  dialog.showModal = function (this: HTMLDialogElement) {
    this.setAttribute("open", "");
  };
  dialog.close = function (this: HTMLDialogElement) {
    if (!this.hasAttribute("open")) return;
    this.removeAttribute("open");
    setTimeout(() => this.dispatchEvent(new Event("close")), 0);
  };
}

// Nor pointer capture, which the side panel's resize handle takes while dragging.
const element = Element.prototype as Partial<Element>;
if (typeof element.setPointerCapture !== "function") {
  element.setPointerCapture = () => undefined;
  element.releasePointerCapture = () => undefined;
}
