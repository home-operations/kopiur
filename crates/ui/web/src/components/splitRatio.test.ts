import { afterEach, describe, expect, it, vi } from "vitest";

import { SPLIT_MIN_PANE, clampSplit, readSplit, saveSplit } from "./splitRatio";

const KEY = "kopiur-ui.test-split";

afterEach(() => {
  window.localStorage.clear();
  vi.restoreAllMocks();
});

describe("split ratio", () => {
  it("starts at half and half", () => {
    expect(readSplit(KEY)).toBe(0.5);
  });

  it("keeps each side at least its minimum width", () => {
    // 1008px across: 8px of gutter leaves 1000, and each side keeps its minimum.
    expect(clampSplit(0.05, 1008)).toBeCloseTo(SPLIT_MIN_PANE / 1000);
    expect(clampSplit(0.99, 1008)).toBeCloseTo(1 - SPLIT_MIN_PANE / 1000);
    expect(clampSplit(0.52, 1008)).toBe(0.52);
  });

  it("holds a width it cannot measure to a fifth either way", () => {
    expect(clampSplit(0, 0)).toBe(0.2);
    expect(clampSplit(1, 0)).toBe(0.8);
  });

  it("remembers where it was left", () => {
    saveSplit(KEY, 0.35);
    expect(readSplit(KEY)).toBe(0.35);
  });

  it("ignores a remembered value that is not a share", () => {
    for (const junk of ["", "wide", "-0.2", "1.5", "NaN"]) {
      window.localStorage.setItem(KEY, junk);
      expect(readSplit(KEY), junk).toBe(0.5);
    }
  });

  it("still works when storage is unavailable", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(readSplit(KEY)).toBe(0.5);
    expect(() => {
      saveSplit(KEY, 0.4);
    }).not.toThrow();
  });
});
