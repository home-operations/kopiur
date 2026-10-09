import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { bodyRows, nth } from "../test-utils";
import { ColumnLedger } from "./ColumnLedger";
import { COLUMNS_KEY, forgetColumnPrefs, setHidden } from "./columnPrefs";
import type { ColumnSpec } from "./tableColumns";

type Id = "name" | "phase" | "size" | "run";

const COLUMNS: readonly ColumnSpec<Id>[] = [
  { id: "name", label: "Snapshot", width: "auto", min: 200, locked: true, stripe: true },
  { id: "phase", label: "Phase", width: 130, min: 90 },
  { id: "size", label: "Size", width: 100, min: 70, numeric: true },
  { id: "run", label: "Run", width: 90, min: 90, locked: true, resizable: false },
];

interface Row {
  name: string;
  phase: string;
  size: string;
}

const ROWS: Row[] = [
  { name: "photos-1", phase: "Succeeded", size: "2 KiB" },
  { name: "photos-2", phase: "Failed", size: "—" },
];

function cell(row: Row, id: Id) {
  switch (id) {
    case "name":
      return row.name;
    case "phase":
      return row.phase;
    case "size":
      return row.size;
    case "run":
      return <button type="button">Run</button>;
  }
}

function mount() {
  render(
    <ColumnLedger
      id="test"
      label="Test table"
      className="test-table"
      columns={COLUMNS}
      rows={ROWS}
      rowKey={(r) => r.name}
      rowProps={() => ({ "data-kind": "snapshot" })}
      cell={cell}
    />,
  );
  return screen.getByRole("table", { name: "Test table" });
}

const stored = () => JSON.parse(window.localStorage.getItem(COLUMNS_KEY) ?? "{}") as unknown;
const headers = (table: HTMLElement) =>
  within(table)
    .getAllByRole("columnheader")
    .map((th) => th.querySelector(".ledger__label")?.textContent);

beforeEach(() => {
  window.localStorage.clear();
  forgetColumnPrefs();
});

describe("ColumnLedger", () => {
  it("draws the declared columns in order, the first with the kind stripe", () => {
    const table = mount();
    expect(table).toHaveClass("ledger", "ledger--fixed", "test-table");
    expect(headers(table)).toEqual(["Snapshot", "Phase", "Size", "Run"]);
    const row = nth(bodyRows(table), 0);
    expect(row).toHaveAttribute("data-kind", "snapshot");
    const cells = Array.from(row.querySelectorAll("td"));
    expect(cells.map((td) => td.textContent)).toEqual(["photos-1", "Succeeded", "2 KiB", "Run"]);
    expect(nth(cells, 0)).toHaveClass("has-stripe");
    expect(nth(cells, 2)).toHaveClass("num");
  });

  it("sizes each column through a colgroup, leaving the flexible one free", () => {
    const table = mount();
    const cols = Array.from(table.querySelectorAll<HTMLElement>("colgroup col"));
    expect(cols.map((c) => c.style.width)).toEqual(["", "130px", "100px", "90px"]);
    // Every column at its width, the flexible one at its floor: 200 + 130 + 100 + 90.
    expect(table.style.minWidth).toBe("520px");
  });

  it("drops a hidden column from the header and every row together", () => {
    const table = mount();
    act(() => {
      setHidden("test", "phase", true);
    });
    expect(headers(table)).toEqual(["Snapshot", "Size", "Run"]);
    for (const row of bodyRows(table)) {
      expect(row.querySelectorAll("td")).toHaveLength(3);
    }
    expect(table.querySelectorAll("colgroup col")).toHaveLength(3);
  });

  it("gives each resizable column a handle named for it, in pixels", () => {
    mount();
    const handle = screen.getByRole("separator", { name: "Resize Phase" });
    expect(handle).toHaveAttribute("aria-orientation", "vertical");
    expect(handle).toHaveAttribute("aria-valuenow", "130");
    expect(handle).toHaveAttribute("aria-valuemin", "90");
    expect(handle).toHaveAttribute("aria-valuetext", "130 pixels");
    expect(screen.queryByRole("separator", { name: "Resize Run" })).toBeNull();
  });

  it("resizes with the arrow keys and remembers it", async () => {
    const table = mount();
    const handle = screen.getByRole("separator", { name: "Resize Phase" });
    const user = userEvent.setup();
    handle.focus();
    expect(handle).toHaveFocus();
    await user.keyboard("{ArrowRight}{ArrowRight}");
    expect(handle).toHaveAttribute("aria-valuenow", "162");
    expect(stored()).toEqual({ test: { width: { phase: 162 } } });
    expect(nth(Array.from(table.querySelectorAll<HTMLElement>("col")), 1).style.width).toBe(
      "162px",
    );
    await user.keyboard("{ArrowLeft}{ArrowLeft}{ArrowLeft}{ArrowLeft}{ArrowLeft}");
    expect(handle).toHaveAttribute("aria-valuenow", "90");
  });

  it("goes back to its declared width on Home or a double click", async () => {
    mount();
    const handle = screen.getByRole("separator", { name: "Resize Size" });
    const user = userEvent.setup();
    handle.focus();
    await user.keyboard("{ArrowRight}");
    expect(handle).toHaveAttribute("aria-valuenow", "116");
    await user.keyboard("{Home}");
    expect(handle).toHaveAttribute("aria-valuenow", "100");
    await user.keyboard("{ArrowRight}");
    await user.dblClick(handle);
    expect(handle).toHaveAttribute("aria-valuenow", "100");
    expect(stored()).toEqual({});
  });

  it("follows a drag from where it started, and stores it once the drag ends", () => {
    mount();
    const handle = screen.getByRole("separator", { name: "Resize Phase" });
    handle.setPointerCapture = () => undefined;
    handle.releasePointerCapture = () => undefined;
    fireEvent.pointerDown(handle, { button: 0, clientX: 300, pointerId: 1 });
    fireEvent.pointerMove(handle, { clientX: 340, pointerId: 1 });
    fireEvent.pointerMove(handle, { clientX: 360, pointerId: 1 });
    expect(handle).toHaveAttribute("aria-valuenow", "190");
    expect(stored()).toEqual({});
    fireEvent.pointerUp(handle, { clientX: 360, pointerId: 1 });
    expect(stored()).toEqual({ test: { width: { phase: 190 } } });
  });

  it("opens with the widths it was left at", () => {
    window.localStorage.setItem(COLUMNS_KEY, JSON.stringify({ test: { width: { size: 150 } } }));
    forgetColumnPrefs();
    mount();
    expect(screen.getByRole("separator", { name: "Resize Size" })).toHaveAttribute(
      "aria-valuenow",
      "150",
    );
  });
});
