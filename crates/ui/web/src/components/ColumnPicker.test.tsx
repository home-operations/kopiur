import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { ColumnPicker } from "./ColumnPicker";
import { COLUMNS_KEY, forgetColumnPrefs, setHidden, tablePrefs } from "./columnPrefs";
import type { ColumnSpec } from "./tableColumns";

const COLUMNS: readonly ColumnSpec[] = [
  { id: "name", label: "Snapshot", width: "auto", min: 200, locked: true },
  { id: "phase", label: "Phase", width: 130, min: 90 },
  { id: "origin", label: "Origin", width: 120, min: 80 },
  { id: "size", label: "Size", width: 100, min: 70 },
  { id: "notes", label: "Notes", width: 160, min: 80, defaultHidden: true },
];

const stored = () => JSON.parse(window.localStorage.getItem(COLUMNS_KEY) ?? "{}") as unknown;

function mount(columns: readonly ColumnSpec[] = COLUMNS) {
  return render(
    <>
      <ColumnPicker table="test" label="Snapshots" columns={columns} />
      <p>outside</p>
    </>,
  );
}

async function open() {
  const user = userEvent.setup();
  mount();
  await user.click(screen.getByRole("button", { name: /^Columns/ }));
  const panel = screen.getByRole("dialog", { name: "Columns of Snapshots" });
  return { user, panel };
}

const order = (panel: HTMLElement) =>
  within(within(panel).getByRole("list", { name: "Column order" }))
    .getAllByRole("checkbox")
    .map((box) => box.closest("label")?.textContent);

beforeEach(() => {
  window.localStorage.clear();
  forgetColumnPrefs();
});

describe("ColumnPicker", () => {
  it("says how many columns are hidden on its button", () => {
    mount();
    expect(screen.getByRole("button", { name: "Columns, 1 hidden" })).toHaveAttribute(
      "aria-expanded",
      "false",
    );
    act(() => {
      setHidden("test", "phase", true);
    });
    expect(screen.getByRole("button", { name: "Columns, 2 hidden" })).toBeInTheDocument();
  });

  it("lists the columns that can change, in order, and leaves out the locked one", async () => {
    const { panel } = await open();
    expect(order(panel)).toEqual(["Phase", "Origin", "Size", "Notes"]);
    expect(within(panel).getByRole("checkbox", { name: "Notes" })).not.toBeChecked();
    expect(within(panel).getByRole("checkbox", { name: "Phase" })).toHaveFocus();
  });

  it("hides and shows a column, and remembers it", async () => {
    const { user, panel } = await open();
    await user.click(within(panel).getByRole("checkbox", { name: "Origin" }));
    await user.click(within(panel).getByRole("checkbox", { name: "Notes" }));
    expect(stored()).toEqual({ test: { hidden: { origin: true, notes: false } } });
    expect(within(panel).getByRole("status")).toHaveTextContent("Notes shown");
  });

  it("keeps the last column on screen", async () => {
    const loose: ColumnSpec[] = [
      { id: "a", label: "A", width: 100, min: 50 },
      { id: "b", label: "B", width: 100, min: 50 },
    ];
    const user = userEvent.setup();
    mount(loose);
    await user.click(screen.getByRole("button", { name: /^Columns/ }));
    await user.click(screen.getByRole("checkbox", { name: "A" }));
    expect(screen.getByRole("checkbox", { name: "B" })).toBeDisabled();
  });

  it("moves a column earlier and later, says where it went, and stops at the ends", async () => {
    const { user, panel } = await open();
    await user.click(within(panel).getByRole("button", { name: "Move Size earlier" }));
    expect(order(panel)).toEqual(["Phase", "Size", "Origin", "Notes"]);
    expect(within(panel).getByRole("status")).toHaveTextContent("Size moved to position 2 of 4");
    expect(tablePrefs("test").order).toEqual(["phase", "size", "origin", "notes"]);
    expect(within(panel).getByRole("button", { name: "Move Phase earlier" })).toBeDisabled();
    expect(within(panel).getByRole("button", { name: "Move Notes later" })).toBeDisabled();
  });

  it("keeps focus in the list when a move reaches the end", async () => {
    const { user, panel } = await open();
    const earlier = within(panel).getByRole("button", { name: "Move Origin earlier" });
    await user.click(earlier);
    expect(order(panel)[0]).toBe("Origin");
    expect(within(panel).getByRole("button", { name: "Reorder Origin" })).toHaveFocus();
  });

  it("reorders from the grip with the arrow keys", async () => {
    const { user, panel } = await open();
    within(panel).getByRole("button", { name: "Reorder Phase" }).focus();
    await user.keyboard("{ArrowDown}{ArrowDown}");
    expect(order(panel)).toEqual(["Origin", "Size", "Phase", "Notes"]);
    expect(within(panel).getByRole("button", { name: "Reorder Phase" })).toHaveFocus();
  });

  it("reorders by dragging the grip over another row", async () => {
    const { panel } = await open();
    const rows = within(panel).getAllByRole("listitem");
    rows.forEach((row, i) => {
      row.getBoundingClientRect = () => new DOMRect(0, i * 40, 200, 40);
    });
    const grip = within(panel).getByRole("button", { name: "Reorder Phase" });
    grip.setPointerCapture = () => undefined;
    grip.releasePointerCapture = () => undefined;
    fireEvent.pointerDown(grip, { button: 0, clientY: 20, pointerId: 1 });
    fireEvent.pointerMove(grip, { clientY: 105, pointerId: 1 });
    fireEvent.pointerUp(grip, { clientY: 105, pointerId: 1 });
    expect(order(panel)).toEqual(["Origin", "Size", "Phase", "Notes"]);
  });

  it("resets this table's layout and no other", async () => {
    const { user, panel } = await open();
    act(() => {
      setHidden("other", "phase", true);
    });
    await user.click(within(panel).getByRole("checkbox", { name: "Origin" }));
    await user.click(within(panel).getByRole("button", { name: "Move Size earlier" }));
    await user.click(within(panel).getByRole("button", { name: "Reset columns" }));
    expect(order(panel)).toEqual(["Phase", "Origin", "Size", "Notes"]);
    expect(stored()).toEqual({ other: { hidden: { phase: true } } });
    expect(within(panel).getByRole("status")).toHaveTextContent("Column layout reset");
  });

  it("closes on Escape and hands focus back to its button", async () => {
    const { user } = await open();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByRole("button", { name: /^Columns/ })).toHaveFocus();
  });

  it("closes on a click outside it", async () => {
    const { user } = await open();
    await user.click(screen.getByText("outside"));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("is not offered when nothing about the table can change", () => {
    mount([{ id: "name", label: "Name", width: "auto", min: 100, locked: true }]);
    expect(screen.queryByRole("button", { name: /^Columns/ })).toBeNull();
  });
});
