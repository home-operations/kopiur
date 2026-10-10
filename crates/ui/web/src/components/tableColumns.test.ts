import { describe, expect, it } from "vitest";

import {
  type ColumnSpec,
  MAX_COLUMN_WIDTH,
  clampWidth,
  fitWidths,
  columnWidth,
  hiddenCount,
  isVisible,
  movableOrder,
  moveColumn,
  orderedColumns,
  parsePrefs,
  tableMinWidth,
  visibleColumns,
} from "./tableColumns";

const SPECS: ColumnSpec[] = [
  { id: "name", label: "Snapshot", width: 260, min: 200, locked: true },
  { id: "phase", label: "Phase", width: 130, min: 90 },
  { id: "origin", label: "Origin", width: 120, min: 80 },
  { id: "size", label: "Size", width: 100, min: 70, numeric: true },
  { id: "notes", label: "Notes", width: 160, min: 80, defaultHidden: true },
  { id: "run", label: "Run", width: 90, min: 90, locked: true, resizable: false },
];

function spec(id: string): ColumnSpec {
  const found = SPECS.find((s) => s.id === id);
  if (found === undefined) throw new Error(id);
  return found;
}

const ids = (columns: readonly ColumnSpec[]) => columns.map((c) => c.id);

describe("column order", () => {
  it("is the declared order until someone moves a column", () => {
    expect(ids(orderedColumns(SPECS, {}))).toEqual([
      "name",
      "phase",
      "origin",
      "size",
      "notes",
      "run",
    ]);
  });

  it("follows a stored order, with locked columns where they were declared", () => {
    const order = ["size", "phase", "origin", "notes"];
    expect(ids(orderedColumns(SPECS, { order }))).toEqual([
      "name",
      "size",
      "phase",
      "origin",
      "notes",
      "run",
    ]);
  });

  it("ignores a stored order's attempt to move a locked column", () => {
    const order = ["run", "name", "origin", "phase", "size", "notes"];
    expect(ids(orderedColumns(SPECS, { order }))).toEqual([
      "name",
      "origin",
      "phase",
      "size",
      "notes",
      "run",
    ]);
  });

  it("drops ids that no longer exist and repeats, and appends new columns", () => {
    const order = ["gone", "origin", "origin", "phase"];
    expect(ids(orderedColumns(SPECS, { order }))).toEqual([
      "name",
      "origin",
      "phase",
      "size",
      "notes",
      "run",
    ]);
  });

  it("keeps a hidden column's place, so showing it again puts it back there", () => {
    const prefs = { order: ["size", "origin", "phase", "notes"], hidden: { origin: true } };
    expect(ids(visibleColumns(SPECS, prefs))).toEqual(["name", "size", "phase", "run"]);
    expect(ids(visibleColumns(SPECS, { ...prefs, hidden: {} }))).toEqual([
      "name",
      "size",
      "origin",
      "phase",
      "run",
    ]);
  });

  it("lists only the columns that can move, in their current order", () => {
    expect(movableOrder(SPECS, { order: ["size"] })).toEqual(["size", "phase", "origin", "notes"]);
  });

  it("moves one column to a new position and leaves the rest in order", () => {
    const order = ["phase", "origin", "size", "notes"];
    expect(moveColumn(order, "size", 0)).toEqual(["size", "phase", "origin", "notes"]);
    expect(moveColumn(order, "phase", 3)).toEqual(["origin", "size", "notes", "phase"]);
    expect(moveColumn(order, "phase", 99)).toEqual(["origin", "size", "notes", "phase"]);
    expect(moveColumn(order, "missing", 0)).toEqual(order);
  });
});

describe("column visibility", () => {
  it("shows every column but the default-hidden ones", () => {
    expect(ids(visibleColumns(SPECS, {}))).toEqual(["name", "phase", "origin", "size", "run"]);
  });

  it("follows what was stored, either way", () => {
    const prefs = { hidden: { phase: true, notes: false } };
    expect(ids(visibleColumns(SPECS, prefs))).toEqual(["name", "origin", "size", "notes", "run"]);
  });

  it("never hides a locked column, whatever was stored", () => {
    expect(isVisible(spec("name"), { hidden: { name: true } })).toBe(true);
  });

  it("is never empty", () => {
    const loose: ColumnSpec[] = [
      { id: "a", label: "A", width: 100, min: 50 },
      { id: "b", label: "B", width: 100, min: 50 },
    ];
    expect(ids(visibleColumns(loose, { hidden: { a: true, b: true } }))).toEqual(["a"]);
  });

  it("counts the hidden columns a person could show again", () => {
    expect(hiddenCount(SPECS, {})).toBe(1);
    expect(hiddenCount(SPECS, { hidden: { phase: true, name: true } })).toBe(2);
  });
});

describe("column width", () => {
  const phase = spec("phase");
  const name = spec("name");

  it("keeps a width between the column's floor and the ceiling", () => {
    expect(clampWidth(phase, 10)).toBe(90);
    expect(clampWidth(phase, 4000)).toBe(MAX_COLUMN_WIDTH);
    expect(clampWidth(phase, 151.6)).toBe(152);
    expect(clampWidth(phase, Number.NaN)).toBe(90);
  });

  it("is the stored width, else the declared one", () => {
    expect(columnWidth(phase, {})).toBe(130);
    expect(columnWidth(phase, { width: { phase: 200 } })).toBe(200);
    expect(columnWidth(phase, { width: { phase: 5 } })).toBe(90);
    expect(columnWidth(name, {})).toBe(260);
    expect(columnWidth(name, { width: { name: 320 } })).toBe(320);
  });

  it("adds the columns up, the last at its floor, so a wide layout scrolls instead of squeezing", () => {
    const visible = visibleColumns(SPECS, {});
    // name 260 + phase 130 + origin 120 + size 100, and run (last) at its floor 90.
    expect(tableMinWidth(visible, fitWidths(visible, {}, null))).toBe(700);
    const wider = { width: { origin: 300 } };
    expect(tableMinWidth(visible, fitWidths(visible, wider, null))).toBe(880);
  });
});

describe("stored preferences", () => {
  it("round-trips what a table stores", () => {
    const prefs = {
      snapshots: { hidden: { origin: true }, width: { size: 140 }, order: ["size", "phase"] },
    };
    expect(parsePrefs(JSON.stringify(prefs))).toEqual(prefs);
  });

  it("comes back empty from nothing or from junk", () => {
    for (const junk of [null, "", "not json", "[]", "42", '"text"']) {
      expect(parsePrefs(junk), String(junk)).toEqual({});
    }
  });

  it("drops entries of the wrong shape and keeps the rest", () => {
    const raw = JSON.stringify({
      a: { hidden: { x: "yes", y: true }, width: { x: "wide", y: 120 }, order: ["x", 3, "y"] },
      b: "nope",
      c: { order: "x" },
    });
    expect(parsePrefs(raw)).toEqual({
      a: { hidden: { y: true }, width: { y: 120 }, order: ["x", "y"] },
      c: {},
    });
  });
});

describe("fitting the columns to the card", () => {
  const visible = visibleColumns(SPECS, {});
  // name 260/200, phase 130/90, origin 120/80, size 100/70, run 90/90:
  // declared 700, floors 530.

  it("draws the declared widths when there is room, or when the room is not known", () => {
    expect(fitWidths(visible, {}, 2000)).toEqual([260, 130, 120, 100, 90]);
    expect(fitWidths(visible, {}, null)).toEqual([260, 130, 120, 100, 90]);
  });

  it("shrinks the columns nobody sized toward their floors before the table scrolls", () => {
    // 615 is halfway between floors and declared: each gives up half its slack.
    expect(fitWidths(visible, {}, 615)).toEqual([230, 110, 100, 85, 90]);
    expect(fitWidths(visible, {}, 400)).toEqual([200, 90, 80, 70, 90]);
  });

  it("never moves one column because another was sized", () => {
    // Origin was dragged; every other column is drawn exactly as before.
    expect(fitWidths(visible, { width: { origin: 300 } }, 615)).toEqual([230, 110, 300, 85, 90]);
    expect(fitWidths(visible, { width: { origin: 90 } }, 615)).toEqual([230, 110, 90, 85, 90]);
  });
});
