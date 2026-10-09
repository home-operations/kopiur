import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  COLUMNS_KEY,
  commitWidths,
  forgetColumnPrefs,
  resetColumns,
  resetWidth,
  setHidden,
  setOrder,
  setWidth,
  subscribeColumnPrefs,
  tablePrefs,
} from "./columnPrefs";

const stored = () => JSON.parse(window.localStorage.getItem(COLUMNS_KEY) ?? "{}") as unknown;

beforeEach(() => {
  window.localStorage.clear();
  forgetColumnPrefs();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("column preferences", () => {
  it("starts empty, with the same object every time it is asked", () => {
    expect(tablePrefs("snapshots")).toEqual({});
    expect(tablePrefs("snapshots")).toBe(tablePrefs("snapshots"));
    expect(tablePrefs("snapshots")).toBe(tablePrefs("repositories"));
  });

  it("keeps each table to itself", () => {
    setHidden("snapshots", "origin", true);
    expect(tablePrefs("snapshots").hidden).toEqual({ origin: true });
    expect(tablePrefs("repositories")).toEqual({});
  });

  it("hands back a new object only when the table changed", () => {
    setHidden("snapshots", "origin", true);
    const before = tablePrefs("snapshots");
    const other = tablePrefs("repositories");
    setHidden("repositories", "phase", true);
    expect(tablePrefs("snapshots")).toBe(before);
    expect(tablePrefs("repositories")).not.toBe(other);
  });

  it("does not wake anyone for a change that changes nothing", () => {
    setHidden("snapshots", "origin", true);
    const listener = vi.fn();
    const stop = subscribeColumnPrefs(listener);
    setHidden("snapshots", "origin", true);
    expect(listener).not.toHaveBeenCalled();
    setHidden("snapshots", "origin", false);
    expect(listener).toHaveBeenCalledTimes(1);
    stop();
  });

  it("remembers visibility and order at once", () => {
    setHidden("snapshots", "origin", true);
    setOrder("snapshots", ["size", "phase"]);
    expect(stored()).toEqual({ snapshots: { hidden: { origin: true }, order: ["size", "phase"] } });
    forgetColumnPrefs();
    expect(tablePrefs("snapshots")).toEqual({ hidden: { origin: true }, order: ["size", "phase"] });
  });

  it("holds a width in memory while it is dragged, and stores it when the drag ends", () => {
    setWidth("snapshots", "size", 180);
    expect(tablePrefs("snapshots").width).toEqual({ size: 180 });
    expect(stored()).toEqual({});
    commitWidths();
    expect(stored()).toEqual({ snapshots: { width: { size: 180 } } });
  });

  it("puts one column's width back, and stores that", () => {
    setWidth("snapshots", "size", 180);
    setWidth("snapshots", "phase", 140);
    resetWidth("snapshots", "size");
    expect(stored()).toEqual({ snapshots: { width: { phase: 140 } } });
  });

  it("resets one table and leaves the others alone", () => {
    setHidden("snapshots", "origin", true);
    setHidden("repositories", "phase", true);
    resetColumns("snapshots");
    expect(tablePrefs("snapshots")).toEqual({});
    expect(stored()).toEqual({ repositories: { hidden: { phase: true } } });
  });

  it("reads what another tab stored", () => {
    const listener = vi.fn();
    const stop = subscribeColumnPrefs(listener);
    const value = JSON.stringify({ snapshots: { hidden: { phase: true } } });
    window.dispatchEvent(new StorageEvent("storage", { key: COLUMNS_KEY, newValue: value }));
    expect(tablePrefs("snapshots").hidden).toEqual({ phase: true });
    expect(listener).toHaveBeenCalled();
    listener.mockClear();
    window.dispatchEvent(new StorageEvent("storage", { key: "kopiur-ui.theme", newValue: "dark" }));
    expect(listener).not.toHaveBeenCalled();
    stop();
  });

  it("still works, unremembered, when storage is blocked", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    forgetColumnPrefs();
    expect(tablePrefs("snapshots")).toEqual({});
    expect(() => {
      setHidden("snapshots", "origin", true);
      setWidth("snapshots", "size", 200);
      commitWidths();
    }).not.toThrow();
    expect(tablePrefs("snapshots").hidden).toEqual({ origin: true });
  });
});
