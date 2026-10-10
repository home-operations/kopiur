import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { ActionPanel } from "./ActionPanel";

/**
 * Focus, which no other test in this suite watches.
 *
 * The confirmation is a disclosure, not a modal — a popover anchored to its
 * trigger (or inline, for a long form), with the rest of the page still
 * readable and usable — so focus is **managed**,
 * not trapped. Three obligations follow from that, and all three were missing
 * until a keyboard walk in a real browser found them: focus moves into the
 * panel when it opens, Escape closes it, and closing it puts focus back where
 * it came from rather than dropping the reader on `<body>`.
 */
function panel({
  onConfirm = vi.fn(),
  presentation,
}: { onConfirm?: () => void; presentation?: "inline" | "popover" } = {}) {
  return (
    <ActionPanel
      label="Delete"
      confirmLabel="Delete it"
      running={false}
      onConfirm={onConfirm}
      presentation={presentation}
    >
      <p>This asks the operator to delete the snapshot.</p>
    </ActionPanel>
  );
}

describe("ActionPanel focus", () => {
  it("opens in a popover anchored to its trigger, and moves focus into it", async () => {
    const user = userEvent.setup();
    render(panel());
    const trigger = screen.getByRole("button", { name: "Delete" });
    await user.click(trigger);
    const question = screen.getByRole("dialog", { name: "Delete" });
    expect(question).toHaveClass("popover__panel");
    expect(question.closest(".popover")).toContainElement(trigger);
    // Entering a named dialog announces its name before its first control.
    expect(question).toContainElement(document.activeElement as HTMLElement);
  });

  it("opens inline instead, for a question too long to float", async () => {
    const user = userEvent.setup();
    render(panel({ presentation: "inline" }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const group = screen.getByRole("group", { name: "Delete" });
    expect(group).toHaveFocus();
  });

  it("closes on Escape and gives focus back to the trigger", async () => {
    const user = userEvent.setup();
    render(panel());
    const trigger = screen.getByRole("button", { name: "Delete" });
    await user.click(trigger);
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "Delete" })).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
    expect(trigger).toHaveAttribute("aria-expanded", "false");
  });

  it("gives focus back after Cancel, rather than dropping the reader on the body", async () => {
    const user = userEvent.setup();
    render(panel());
    const trigger = screen.getByRole("button", { name: "Delete" });
    await user.click(trigger);
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(trigger).toHaveFocus();
  });

  it("gives focus back after the action is confirmed", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    render(panel({ onConfirm }));
    const trigger = screen.getByRole("button", { name: "Delete" });
    await user.click(trigger);
    await user.click(screen.getByRole("button", { name: "Delete it" }));
    expect(onConfirm).toHaveBeenCalledOnce();
    expect(trigger).toHaveFocus();
  });

  it("does not trap: Tab leaves the panel for the rest of the page", async () => {
    // A trap here would be a defect, not a feature — nothing is modal, and a
    // reader must be able to go back and re-read the page before confirming.
    const user = userEvent.setup();
    render(
      <>
        {panel()}
        <a href="/elsewhere">Somewhere else</a>
      </>,
    );
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const link = screen.getByRole("link", { name: "Somewhere else" });
    // Past the confirm and Cancel buttons, Tab carries on into the page.
    for (let i = 0; i < 4 && document.activeElement !== link; i += 1) {
      await user.tab();
    }
    expect(link).toHaveFocus();
  });

  it("ignores Escape when nothing is open, so it cannot swallow a page-level key", async () => {
    const user = userEvent.setup();
    render(panel());
    const trigger = screen.getByRole("button", { name: "Delete" });
    trigger.focus();
    await user.keyboard("{Escape}");
    expect(trigger).toHaveFocus();
  });
});
