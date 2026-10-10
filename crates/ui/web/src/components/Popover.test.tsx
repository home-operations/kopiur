import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { Popover } from "./Popover";

function Harness({ align, onEscape }: { align?: "start" | "end"; onEscape?: () => boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Popover
        label="Options"
        open={open}
        onOpenChange={setOpen}
        align={align}
        onEscape={onEscape}
        trigger={(props) => (
          <button type="button" {...props}>
            Open
          </button>
        )}
      >
        {(close) => (
          <>
            <button type="button" disabled>
              Disabled first
            </button>
            <label>
              Name <input />
            </label>
            <button type="button" onClick={close}>
              Done
            </button>
          </>
        )}
      </Popover>
      <p>outside</p>
    </>
  );
}

describe("Popover", () => {
  it("opens a named panel anchored to its trigger, and says so on the trigger", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const trigger = screen.getByRole("button", { name: "Open" });
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(trigger).toHaveAttribute("aria-haspopup", "dialog");
    expect(screen.queryByRole("dialog")).toBeNull();
    await user.click(trigger);
    const panel = screen.getByRole("dialog", { name: "Options" });
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(trigger).toHaveAttribute("aria-controls", panel.id);
    expect(panel).toHaveClass("popover__panel");
    expect(panel).toHaveAttribute("data-align", "start");
    expect(panel.closest(".popover")).toContainElement(trigger);
  });

  it("puts focus on the first control it can take, skipping disabled ones", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Open" }));
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveFocus();
  });

  it("can be told where focus should start instead", () => {
    render(
      <Popover
        label="Picks"
        open
        onOpenChange={() => undefined}
        initialFocus="input[type=checkbox]"
        trigger={(props) => (
          <button type="button" {...props}>
            Open
          </button>
        )}
      >
        <button type="button">Grip</button>
        <label>
          Pick <input type="checkbox" />
        </label>
      </Popover>,
    );
    expect(screen.getByRole("checkbox", { name: "Pick" })).toHaveFocus();
  });

  it("closes on Escape and hands focus back to the trigger", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Open" }));
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByRole("button", { name: "Open" })).toHaveFocus();
  });

  it("lets its content keep Escape for itself", async () => {
    const user = userEvent.setup();
    render(<Harness onEscape={() => true} />);
    await user.click(screen.getByRole("button", { name: "Open" }));
    await user.keyboard("{Escape}");
    expect(screen.getByRole("dialog", { name: "Options" })).toBeInTheDocument();
  });

  it("closes on a click outside, and a second click on the trigger closes it too", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const trigger = screen.getByRole("button", { name: "Open" });
    await user.click(trigger);
    await user.click(screen.getByText("outside"));
    expect(screen.queryByRole("dialog")).toBeNull();
    await user.click(trigger);
    await user.click(trigger);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("hands its content a way to close it that returns focus to the trigger", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Open" }));
    await user.click(screen.getByRole("button", { name: "Done" }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByRole("button", { name: "Open" })).toHaveFocus();
  });

  it("can hang from the trigger's far edge", async () => {
    const user = userEvent.setup();
    render(<Harness align="end" />);
    await user.click(screen.getByRole("button", { name: "Open" }));
    expect(screen.getByRole("dialog")).toHaveAttribute("data-align", "end");
  });

  it("marks which side it opens on", () => {
    render(
      <Popover
        label="Account"
        open
        onOpenChange={() => undefined}
        side="top"
        trigger={(props) => (
          <button type="button" {...props}>
            Me
          </button>
        )}
      >
        menu
      </Popover>,
    );
    expect(screen.getByRole("dialog")).toHaveAttribute("data-side", "top");
  });

  it("escapes a clipping scroll box by pinning itself to the trigger on screen", () => {
    render(
      <Popover
        label="Suspend"
        open
        onOpenChange={() => undefined}
        align="end"
        strategy="fixed"
        trigger={(props) => (
          <button
            type="button"
            {...props}
            ref={(node) => {
              if (node !== null) {
                node.getBoundingClientRect = () => new DOMRect(900, 200, 100, 34);
              }
              const ref = props.ref;
              if (typeof ref === "function") ref(node);
            }}
          >
            Suspend
          </button>
        )}
      >
        question
      </Popover>,
    );
    const panel = screen.getByRole("dialog");
    expect(panel).toHaveAttribute("data-strategy", "fixed");
    expect(panel.style.position).toBe("fixed");
    // Below the trigger (200 + 34 + 6), its end edge on the trigger's end edge.
    expect(panel.style.top).toBe("240px");
    expect(panel.style.right).toBe(`${String(window.innerWidth - 1000)}px`);
  });

  it("takes Escape for itself, so a drawer around it stays open", async () => {
    const user = userEvent.setup();
    const outer = vi.fn();
    // A drawer hears Escape on the document and ignores one already handled.
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.defaultPrevented) outer();
    };
    document.addEventListener("keydown", onKey);
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Open" }));
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(outer).not.toHaveBeenCalled();
    document.removeEventListener("keydown", onKey);
  });
});
