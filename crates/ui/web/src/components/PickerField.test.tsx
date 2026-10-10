import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { PickerField } from "./PickerField";

const PHASES = [
  { value: "running", label: "Running" },
  { value: "failed", label: "Failed" },
];

function Harness({ initial = "", empty = true }: { initial?: string; empty?: boolean }) {
  const [value, setValue] = useState(initial);
  return (
    <>
      <PickerField
        id="phase"
        label="Phase"
        value={value}
        onChange={setValue}
        options={PHASES}
        emptyLabel={empty ? "any phase" : undefined}
      />
      <output aria-label="value">{value}</output>
    </>
  );
}

describe("PickerField without search", () => {
  it("is the same dropdown with no filter field, listing the empty choice first", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Phase: any phase" }));
    const panel = screen.getByRole("dialog", { name: "Choose the phase" });
    expect(within(panel).queryByRole("searchbox")).toBeNull();
    expect(
      within(panel)
        .getAllByRole("button")
        .map((b) => b.textContent),
    ).toEqual(["any phase", "Running", "Failed"]);
  });

  it("opens on the current choice, walks with the arrow keys and says the label it picked", async () => {
    const user = userEvent.setup();
    render(<Harness initial="running" />);
    const button = screen.getByRole("button", { name: "Phase: Running" });
    await user.click(button);
    const panel = screen.getByRole("dialog");
    expect(within(panel).getByRole("button", { name: /^Running/ })).toHaveFocus();
    await user.keyboard("{ArrowDown}");
    expect(within(panel).getByRole("button", { name: /^Failed/ })).toHaveFocus();
    await user.keyboard("{Enter}");
    expect(screen.getByLabelText("value")).toHaveTextContent("failed");
    expect(screen.getByRole("button", { name: "Phase: Failed" })).toHaveFocus();
  });

  it("has no empty choice when the field must hold a value", async () => {
    const user = userEvent.setup();
    render(<Harness initial="running" empty={false} />);
    await user.click(screen.getByRole("button", { name: "Phase: Running" }));
    expect(
      within(screen.getByRole("dialog"))
        .getAllByRole("button")
        .map((b) => b.textContent),
    ).toEqual(["Running", "Failed"]);
  });
});
