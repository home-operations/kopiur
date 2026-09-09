import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { ActionButton } from "./ActionButton";

const reason =
  "Delete snapshots is not permitted for alice in namespace prod. Ask a cluster admin to bind kopiur-ui-user or kopiur-ui-editor to your user or group.";

describe("ActionButton", () => {
  it("is an ordinary button when allowed: named by its label, clickable, undescribed", async () => {
    const onClick = vi.fn();
    render(<ActionButton onClick={onClick}>Delete snapshot</ActionButton>);
    const button = screen.getByRole("button", { name: "Delete snapshot" });
    expect(button).not.toHaveAttribute("aria-disabled");
    expect(button).not.toHaveAttribute("data-reason");
    expect(button).not.toHaveAccessibleDescription();
    await userEvent.click(button);
    expect(onClick).toHaveBeenCalledOnce();
  });

  it("stays visible and focusable when blocked, and the reason is its accessible description", async () => {
    const onClick = vi.fn();
    render(
      <ActionButton onClick={onClick} disabledReason={reason}>
        Delete snapshot
      </ActionButton>,
    );
    // Never hidden: the control is still there, still named by its action.
    const button = screen.getByRole("button", { name: "Delete snapshot" });
    // Disabled for assistive technology through aria-disabled, not `disabled`,
    // so it can still take focus and the reason can be reached by keyboard.
    expect(button).toHaveAttribute("aria-disabled", "true");
    expect(button).not.toBeDisabled();
    await userEvent.tab();
    expect(button).toHaveFocus();
    // The reason is associated with the control, not merely stored on it.
    expect(button).toHaveAccessibleDescription(reason);
    expect(button).toHaveAttribute("data-reason", reason);
  });

  it("swallows a click while blocked", async () => {
    const onClick = vi.fn();
    render(
      <ActionButton onClick={onClick} disabledReason={reason}>
        Delete snapshot
      </ActionButton>,
    );
    await userEvent.click(screen.getByRole("button", { name: "Delete snapshot" }));
    expect(onClick).not.toHaveBeenCalled();
  });

  it("treats an empty reason as allowed", async () => {
    const onClick = vi.fn();
    render(
      <ActionButton onClick={onClick} disabledReason="">
        Retry
      </ActionButton>,
    );
    const button = screen.getByRole("button", { name: "Retry" });
    expect(button).not.toHaveAttribute("aria-disabled");
    await userEvent.click(button);
    expect(onClick).toHaveBeenCalledOnce();
  });

  it("carries its variant class", () => {
    const { container } = render(<ActionButton variant="primary">Snapshot now</ActionButton>);
    expect(container.querySelector("button")).toHaveClass("button", "button--primary");
  });
});
