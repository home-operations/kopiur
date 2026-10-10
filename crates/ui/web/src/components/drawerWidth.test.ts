import { afterEach, describe, expect, it, vi } from "vitest";

import {
  DRAWER_MIN_WIDTH,
  DRAWER_WIDTH_KEY,
  clampDrawerWidth,
  readDrawerWidth,
  saveDrawerWidth,
} from "./drawerWidth";

afterEach(() => {
  window.localStorage.clear();
  vi.restoreAllMocks();
});

describe("drawer width", () => {
  it("opens at half the window when nothing is remembered", () => {
    expect(readDrawerWidth(1600)).toBe(800);
  });

  it("never grows past 85% of the window, nor shrinks below the minimum", () => {
    expect(clampDrawerWidth(5000, 1000)).toBe(850);
    expect(clampDrawerWidth(10, 1000)).toBe(DRAWER_MIN_WIDTH);
    expect(clampDrawerWidth(600, 1000)).toBe(600);
  });

  it("gives a window narrower than the minimum all of 85%, not more than the window", () => {
    expect(clampDrawerWidth(10, 300)).toBe(255);
  });

  it("remembers the width it was dragged to, clamped to the window it reopens in", () => {
    saveDrawerWidth(700);
    expect(window.localStorage.getItem(DRAWER_WIDTH_KEY)).toBe("700");
    expect(readDrawerWidth(1600)).toBe(700);
    expect(readDrawerWidth(800)).toBe(680);
  });

  it("ignores a remembered value that is not a width", () => {
    for (const junk of ["", "wide", "-40", "NaN", "Infinity"]) {
      window.localStorage.setItem(DRAWER_WIDTH_KEY, junk);
      expect(readDrawerWidth(1600), junk).toBe(800);
    }
  });

  it("still opens when storage is unavailable", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(readDrawerWidth(1600)).toBe(800);
    expect(() => {
      saveDrawerWidth(700);
    }).not.toThrow();
  });
});
