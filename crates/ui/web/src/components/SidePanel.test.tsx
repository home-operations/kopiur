import { fireEvent, render, screen } from "@testing-library/react";
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

  it("floats at the drawer width over a scrim, and fills a phone", () => {
    expect(rule(".side-panel")).toMatch(/--drawer-width/);
    expect(rule(".side-panel::backdrop")).toMatch(/var\(--scrim\)/);
    expect(rule(".side-panel")).toMatch(/width:\s*100vw/);
  });

  it("wraps a long name rather than widening the panel", () => {
    expect(rule(".side-panel__title")).toMatch(/overflow-wrap:\s*anywhere/);
  });
});
