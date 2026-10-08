import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { cssRules, readStyles } from "../testing/css";
import { SidePanel } from "./SidePanel";

function rule(selector: string): string {
  return cssRules(readStyles())
    .filter((r) => r.selector === selector)
    .map((r) => r.body)
    .join("\n");
}

const noop = () => undefined;

describe("SidePanel", () => {
  it("is an open dialog named by its title", () => {
    render(
      <SidePanel label="app-data" kind="snapshotPolicy" onClose={noop}>
        body
      </SidePanel>,
    );
    const dialog = screen.getByRole("dialog", { name: "app-data" });
    expect(dialog).toHaveAttribute("open");
    expect(dialog).toHaveAttribute("data-kind", "snapshot-policy");
    expect(dialog).toHaveClass("has-stripe");
    expect(dialog).toHaveTextContent("SnapshotPolicy");
  });

  it("carries no kind stripe when it shows something that is not a resource", () => {
    render(
      <SidePanel label="s3://bucket" kindWord="Backend" onClose={noop}>
        body
      </SidePanel>,
    );
    const dialog = screen.getByRole("dialog", { name: "s3://bucket" });
    expect(dialog).not.toHaveAttribute("data-kind");
    expect(dialog).not.toHaveClass("has-stripe");
    expect(dialog).toHaveTextContent("Backend");
  });

  it("closes on Escape and on its close button", async () => {
    const onClose = vi.fn();
    render(
      <SidePanel label="nas" onClose={onClose}>
        body
      </SidePanel>,
    );
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    await userEvent.click(screen.getByRole("button", { name: "Close details" }));
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("leaves an Escape alone when something inside it already handled it", () => {
    const onClose = vi.fn();
    render(
      <SidePanel label="nas" onClose={onClose}>
        <button
          type="button"
          onKeyDown={(e) => {
            if (e.key === "Escape") e.stopPropagation();
          }}
        >
          Cancel
        </button>
      </SidePanel>,
    );
    fireEvent.keyDown(screen.getByRole("button", { name: "Cancel" }), { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("closes on a click on the backdrop, not on a click inside", async () => {
    const onClose = vi.fn();
    render(
      <SidePanel label="nas" onClose={onClose}>
        <p>inside</p>
      </SidePanel>,
    );
    await userEvent.click(screen.getByText("inside"));
    expect(onClose).not.toHaveBeenCalled();
    // A click on the ::backdrop lands on the dialog element itself.
    fireEvent.click(screen.getByRole("dialog"));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("takes focus when it opens and hands it back when it goes", () => {
    const trigger = document.createElement("button");
    document.body.append(trigger);
    trigger.focus();
    const { unmount } = render(
      <SidePanel label="nas" onClose={noop}>
        body
      </SidePanel>,
    );
    expect(screen.getByRole("dialog").contains(document.activeElement)).toBe(true);
    unmount();
    expect(document.activeElement).toBe(trigger);
    trigger.remove();
  });

  it("stays open through a remount, ignoring the close its first mount queued", async () => {
    // StrictMode mounts, unmounts and mounts again; the browser delivers the
    // first unmount's `close` after the second mount has reopened the dialog.
    const onClose = vi.fn();
    render(
      <StrictMode>
        <SidePanel label="nas" onClose={onClose}>
          body
        </SidePanel>
      </StrictMode>,
    );
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog")).toHaveAttribute("open");
  });

  it("follows the browser when it closes the dialog itself", async () => {
    const onClose = vi.fn();
    render(
      <SidePanel label="nas" onClose={onClose}>
        body
      </SidePanel>,
    );
    screen.getByRole<HTMLDialogElement>("dialog").close();
    await waitFor(() => {
      expect(onClose).toHaveBeenCalledTimes(1);
    });
  });

  it("gives its body to the content to divide when laid out to fill", () => {
    const { container, rerender } = render(
      <SidePanel label="nas" onClose={noop}>
        body
      </SidePanel>,
    );
    expect(container.ownerDocument.querySelector(".side-panel__body")).not.toHaveAttribute(
      "data-layout",
    );
    rerender(
      <SidePanel label="nas" onClose={noop} layout="fill">
        body
      </SidePanel>,
    );
    expect(container.ownerDocument.querySelector(".side-panel__body")).toHaveAttribute(
      "data-layout",
      "fill",
    );
    expect(rule('.side-panel__body[data-layout="fill"]')).toMatch(/padding:\s*0/);
  });

  it("shows a footer only when given one", () => {
    const { container, rerender } = render(
      <SidePanel label="nas" onClose={noop}>
        body
      </SidePanel>,
    );
    expect(container.querySelector(".side-panel__foot")).toBeNull();
    rerender(
      <SidePanel label="nas" onClose={noop} footer={<a href="/x">Open full page</a>}>
        body
      </SidePanel>,
    );
    expect(screen.getByRole("link", { name: "Open full page" })).toBeInTheDocument();
  });

  it("opens at half the window, with a resize handle that says so", () => {
    window.localStorage.clear();
    render(
      <SidePanel label="nas" onClose={noop}>
        body
      </SidePanel>,
    );
    const dialog = screen.getByRole("dialog");
    const half = Math.round(window.innerWidth / 2);
    expect(dialog.style.getPropertyValue("--drawer-width")).toBe(`${String(half)}px`);
    const grip = screen.getByRole("separator", { name: "Resize details" });
    expect(grip).toHaveAttribute("aria-orientation", "vertical");
    expect(grip).toHaveAttribute("aria-valuenow", String(half));
    expect(grip).toHaveAttribute("aria-valuemax", String(Math.floor(window.innerWidth * 0.85)));
    expect(grip).toHaveAttribute("tabindex", "0");
  });

  it("resizes from the keyboard and remembers the width", async () => {
    window.localStorage.clear();
    render(
      <SidePanel label="nas" onClose={noop}>
        body
      </SidePanel>,
    );
    const half = Math.round(window.innerWidth / 2);
    const grip = screen.getByRole("separator", { name: "Resize details" });
    grip.focus();
    await userEvent.keyboard("{ArrowLeft}");
    expect(grip).toHaveAttribute("aria-valuenow", String(half + 24));
    await userEvent.keyboard("{ArrowRight}{ArrowRight}");
    expect(grip).toHaveAttribute("aria-valuenow", String(half - 24));
    expect(window.localStorage.getItem("kopiur-ui.drawer-width")).toBe(String(half - 24));
    await userEvent.keyboard("{End}");
    expect(grip).toHaveAttribute("aria-valuenow", String(Math.floor(window.innerWidth * 0.85)));
  });

  it("resizes by dragging its left edge, and reopens at the dragged width", () => {
    window.localStorage.clear();
    const { unmount } = render(
      <SidePanel label="nas" onClose={noop}>
        body
      </SidePanel>,
    );
    const half = Math.round(window.innerWidth / 2);
    const grip = screen.getByRole("separator", { name: "Resize details" });
    fireEvent.pointerDown(grip, { clientX: 500, pointerId: 1, button: 0 });
    fireEvent.pointerMove(grip, { clientX: 400, pointerId: 1 });
    expect(grip).toHaveAttribute("aria-valuenow", String(half + 100));
    fireEvent.pointerUp(grip, { clientX: 400, pointerId: 1 });
    // A drag never reaches the backdrop: the panel stays open.
    expect(screen.getByRole("dialog")).toHaveAttribute("open");
    unmount();
    render(
      <SidePanel label="nas" onClose={noop}>
        body
      </SidePanel>,
    );
    expect(screen.getByRole("separator")).toHaveAttribute("aria-valuenow", String(half + 100));
  });

  it("slides out before it goes, and says when it has", () => {
    const onExited = vi.fn();
    const style = vi.spyOn(window, "getComputedStyle");
    style.mockImplementation(
      (el) =>
        ({
          animationName: el instanceof HTMLDialogElement ? "side-panel-out" : "none",
        }) as CSSStyleDeclaration,
    );
    const { rerender } = render(
      <SidePanel label="nas" onClose={noop} onExited={onExited}>
        body
      </SidePanel>,
    );
    rerender(
      <SidePanel label="nas" onClose={noop} onExited={onExited} leaving>
        body
      </SidePanel>,
    );
    const dialog = screen.getByRole("dialog");
    expect(dialog).toHaveAttribute("data-leaving");
    expect(onExited).not.toHaveBeenCalled();
    fireEvent.animationEnd(dialog, { animationName: "side-panel-out" });
    expect(onExited).toHaveBeenCalledTimes(1);
    style.mockRestore();
  });

  it("goes at once when there is no exit animation to wait for (reduced motion)", () => {
    const onExited = vi.fn();
    const { rerender } = render(
      <SidePanel label="nas" onClose={noop} onExited={onExited}>
        body
      </SidePanel>,
    );
    rerender(
      <SidePanel label="nas" onClose={noop} onExited={onExited} leaving>
        body
      </SidePanel>,
    );
    expect(onExited).toHaveBeenCalledTimes(1);
  });

  it("does not ask to close again while it is already leaving", () => {
    const onClose = vi.fn();
    render(
      <SidePanel label="nas" onClose={onClose} leaving>
        body
      </SidePanel>,
    );
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("hangs its resize handle outside its left edge, always visible", () => {
    expect(rule(".side-panel__grip")).toMatch(/left:\s*calc\(-1/);
    expect(rule(".side-panel__grip::after")).not.toMatch(/opacity:\s*0/);
    expect(rule(".side-panel[data-leaving]")).toMatch(/side-panel-out/);
  });

  it("floats at the drawer width over a scrim, and fills a phone", () => {
    expect(rule(".side-panel")).toMatch(/--drawer-width/);
    expect(rule(".side-panel")).toMatch(/--drawer-max/);
    expect(rule(".side-panel__grip")).toMatch(/display:\s*none/);
    expect(rule(".side-panel::backdrop")).toMatch(/var\(--scrim\)/);
    expect(rule(".side-panel")).toMatch(/width:\s*100vw/);
  });

  it("wraps a long name rather than widening the panel", () => {
    expect(rule(".side-panel__title")).toMatch(/overflow-wrap:\s*anywhere/);
  });
});
