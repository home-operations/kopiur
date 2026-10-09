import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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

/**
 * jsdom lays nothing out: give each row a 40px slot by its place in the list,
 * so a drag and a slide have positions to work from.
 */
function layOut() {
  Object.defineProperty(HTMLLIElement.prototype, "offsetTop", {
    configurable: true,
    get(this: HTMLLIElement) {
      return Array.from(this.parentElement?.children ?? []).indexOf(this) * 40;
    },
  });
  Object.defineProperty(HTMLLIElement.prototype, "offsetHeight", {
    configurable: true,
    get: () => 40,
  });
}

beforeEach(() => {
  window.localStorage.clear();
  forgetColumnPrefs();
  layOut();
});

afterEach(() => {
  // The prototype getters jsdom inherits from HTMLElement come back.
  Reflect.deleteProperty(HTMLLIElement.prototype, "offsetTop");
  Reflect.deleteProperty(HTMLLIElement.prototype, "offsetHeight");
  Reflect.deleteProperty(HTMLElement.prototype, "animate");
  vi.restoreAllMocks();
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
    const grip = within(panel).getByRole("button", { name: "Reorder Phase" });
    fireEvent.pointerDown(grip, { button: 0, clientY: 20, pointerId: 1 });
    fireEvent.pointerMove(grip, { clientY: 105, pointerId: 1, buttons: 1 });
    fireEvent.pointerUp(grip, { clientY: 105, pointerId: 1 });
    expect(order(panel)).toEqual(["Origin", "Size", "Phase", "Notes"]);
    expect(tablePrefs("test").order).toEqual(["origin", "size", "phase", "notes"]);
    expect(within(panel).getByRole("status")).toHaveTextContent("Phase moved to position 3 of 4");
  });

  it("lifts the row while it is dragged, and saves nothing until it is let go", async () => {
    const { panel } = await open();
    const grip = within(panel).getByRole("button", { name: "Reorder Phase" });
    fireEvent.pointerDown(grip, { button: 0, clientY: 20, pointerId: 1 });
    fireEvent.pointerMove(grip, { clientY: 105, pointerId: 1, buttons: 1 });
    // The rows already make room…
    expect(order(panel)).toEqual(["Origin", "Size", "Phase", "Notes"]);
    // …the row's own slot stays behind as an outline…
    expect(panel.querySelector('li[data-column="phase"]')).toHaveAttribute("data-dragging", "true");
    // …and a copy of it rides under the pointer, grabbed where it was grabbed.
    const ghost = panel.querySelector<HTMLElement>(".column-picker__ghost");
    expect(ghost).toHaveTextContent("Phase");
    expect(ghost).toHaveAttribute("aria-hidden", "true");
    expect(ghost?.style.top).toBe("85px");
    expect(tablePrefs("test").order).toBeUndefined();
    fireEvent.pointerUp(grip, { clientY: 105, pointerId: 1 });
    expect(panel.querySelector(".column-picker__ghost")).toBeNull();
    expect(panel.querySelector("li[data-dragging]")).toBeNull();
  });

  it("keeps the copy inside the list, however far the pointer goes", async () => {
    const { panel } = await open();
    const grip = within(panel).getByRole("button", { name: "Reorder Phase" });
    fireEvent.pointerDown(grip, { button: 0, clientY: 20, pointerId: 1 });
    fireEvent.pointerMove(grip, { clientY: 900, pointerId: 1, buttons: 1 });
    expect(panel.querySelector<HTMLElement>(".column-picker__ghost")?.style.top).toBe("120px");
    expect(order(panel)).toEqual(["Origin", "Size", "Notes", "Phase"]);
  });

  it("puts everything back on Escape mid-drag, and keeps the menu open", async () => {
    const { user, panel } = await open();
    const grip = within(panel).getByRole("button", { name: "Reorder Phase" });
    fireEvent.pointerDown(grip, { button: 0, clientY: 20, pointerId: 1 });
    fireEvent.pointerMove(grip, { clientY: 105, pointerId: 1, buttons: 1 });
    await user.keyboard("{Escape}");
    expect(screen.getByRole("dialog", { name: "Columns of Snapshots" })).toBeInTheDocument();
    expect(order(panel)).toEqual(["Phase", "Origin", "Size", "Notes"]);
    expect(panel.querySelector(".column-picker__ghost")).toBeNull();
    fireEvent.pointerUp(grip, { clientY: 105, pointerId: 1 });
    expect(tablePrefs("test").order).toBeUndefined();
  });

  it("ends a drag released away from the grip, so hovering later moves nothing", async () => {
    const { panel } = await open();
    const grip = within(panel).getByRole("button", { name: "Reorder Phase" });
    fireEvent.pointerDown(grip, { button: 0, clientY: 20, pointerId: 1 });
    // The release landed on a label, off the grip.
    fireEvent.pointerUp(within(panel).getByText("Size"), { clientY: 20, pointerId: 1 });
    fireEvent.pointerMove(grip, { clientY: 140, pointerId: 1, buttons: 0 });
    expect(order(panel)).toEqual(["Phase", "Origin", "Size", "Notes"]);
  });

  it("slides the rows that move to their new places", async () => {
    const animate = vi.fn(() => ({}) as Animation);
    HTMLElement.prototype.animate = animate;
    const { user, panel } = await open();
    await user.click(within(panel).getByRole("button", { name: "Move Size earlier" }));
    const slides = animate.mock.calls.map((call) => {
      const [frames] = call as unknown as [Keyframe[]];
      return frames[0]?.transform;
    });
    // Size came up a slot and Origin went down one; each starts where it was.
    expect(slides).toEqual(expect.arrayContaining(["translateY(40px)", "translateY(-40px)"]));
    expect(slides).toHaveLength(2);
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
