import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { PickerField } from "./PickerField";

const PHASES = [
  { value: "running", label: "Running" },
  { value: "failed", label: "Failed" },
  { value: "succeeded", label: "Succeeded" },
];

function Single({ initial = "", clearable = true }: { initial?: string; clearable?: boolean }) {
  const [value, setValue] = useState(initial);
  return (
    <>
      <PickerField
        id="phase"
        label="Phase"
        value={value}
        onChange={setValue}
        options={PHASES}
        emptyLabel={clearable ? "any phase" : undefined}
      />
      <output aria-label="value">{value}</output>
    </>
  );
}

function Multiple({ initial = [] }: { initial?: string[] }) {
  const [value, setValue] = useState(initial);
  return (
    <>
      <PickerField
        id="phase"
        label="Phase"
        multiple
        value={value}
        onChange={setValue}
        options={PHASES}
        emptyLabel="any phase"
      />
      <output aria-label="value">{value.join(",")}</output>
    </>
  );
}

const choices = () =>
  Array.from(screen.getByRole("dialog").querySelectorAll(".picker__option")).map(
    (b) => b.textContent,
  );

describe("PickerField", () => {
  it("lists only real choices; choosing nothing is a Clear button at the top", async () => {
    const user = userEvent.setup();
    render(<Single initial="failed" />);
    await user.click(screen.getByRole("button", { name: "Phase: Failed" }));
    const panel = screen.getByRole("dialog", { name: "Choose the phase" });
    expect(within(panel).queryByRole("searchbox")).toBeNull();
    expect(choices()).toEqual(["Running", "Failed", "Succeeded"]);
    await user.click(within(panel).getByRole("button", { name: "Clear" }));
    expect(screen.getByLabelText("value")).toBeEmptyDOMElement();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByRole("button", { name: "Phase: any phase" })).toHaveAttribute(
      "data-empty",
      "true",
    );
  });

  it("has no Clear button when the field must hold a value", async () => {
    const user = userEvent.setup();
    render(<Single initial="running" clearable={false} />);
    await user.click(screen.getByRole("button", { name: "Phase: Running" }));
    expect(within(screen.getByRole("dialog")).queryByRole("button", { name: "Clear" })).toBeNull();
  });

  it("opens on the current choice and walks with the arrow keys", async () => {
    const user = userEvent.setup();
    render(<Single initial="running" />);
    await user.click(screen.getByRole("button", { name: "Phase: Running" }));
    const panel = screen.getByRole("dialog");
    expect(within(panel).getByRole("button", { name: /^Running/ })).toHaveFocus();
    await user.keyboard("{ArrowDown}{Enter}");
    expect(screen.getByLabelText("value")).toHaveTextContent("failed");
    expect(screen.getByRole("button", { name: "Phase: Failed" })).toHaveFocus();
  });

  it("toggles several values when multiple, staying open, and says them all", async () => {
    const user = userEvent.setup();
    render(<Multiple />);
    await user.click(screen.getByRole("button", { name: "Phase: any phase" }));
    const panel = screen.getByRole("dialog");
    expect(within(panel).getByText("none chosen")).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: "Clear" })).toBeDisabled();
    await user.click(within(panel).getByRole("button", { name: /^Failed/ }));
    await user.click(within(panel).getByRole("button", { name: /^Running/ }));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: /^Failed/ })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(within(panel).getByText("2 chosen")).toBeInTheDocument();
    expect(screen.getByLabelText("value")).toHaveTextContent("failed,running");
    // The button shows the first and how many more; its name says every one.
    const button = screen.getByRole("button", { name: "Phase: Failed, Running" });
    expect(button).toHaveTextContent("Failed +1");

    // A second pick of one unticks it.
    await user.click(within(panel).getByRole("button", { name: /^Failed/ }));
    expect(screen.getByLabelText("value")).toHaveTextContent("running");
  });

  it("clears every value at once and keeps the list open with focus in it", async () => {
    const user = userEvent.setup();
    render(<Multiple initial={["failed", "running"]} />);
    await user.click(screen.getByRole("button", { name: "Phase: Failed, Running" }));
    const panel = screen.getByRole("dialog");
    await user.click(within(panel).getByRole("button", { name: "Clear" }));
    expect(screen.getByLabelText("value")).toBeEmptyDOMElement();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: /^Running/ })).toHaveFocus();
  });
});
