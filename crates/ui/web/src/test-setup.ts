// vitest setup, run once per test file (see vite.config.ts `test.setupFiles`).
import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

afterEach(() => {
  cleanup();
});

// jsdom has no modal dialogs. Stand in for the two calls the side panel makes:
// opening toggles the `open` attribute (which is what makes it visible to the
// accessibility tree), closing removes it and fires `close`. The top layer,
// the backdrop and the inert page behind it are browser behaviour, checked
// live, not here.
const dialog = HTMLDialogElement.prototype as Partial<HTMLDialogElement>;
if (typeof dialog.showModal !== "function") {
  dialog.showModal = function (this: HTMLDialogElement) {
    this.setAttribute("open", "");
  };
  dialog.close = function (this: HTMLDialogElement) {
    if (!this.hasAttribute("open")) return;
    this.removeAttribute("open");
    this.dispatchEvent(new Event("close"));
  };
}
