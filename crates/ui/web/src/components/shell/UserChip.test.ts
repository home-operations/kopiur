import { describe, expect, it } from "vitest";

import { cssRules, readStyles } from "../../testing/css";

describe("UserChip panel", () => {
  const rule = (selector: string) =>
    cssRules(readStyles()).find((r) => r.selector === selector)?.body ?? "";

  it("opens above the chip, from its left edge", () => {
    const panel = rule(".identity__panel");
    expect(panel).toMatch(/position:\s*absolute/);
    expect(panel).toMatch(/bottom:\s*calc\(100% \+/);
    expect(panel).toMatch(/left:\s*0/);
  });

  it("is wider than the sidebar, so it spills out over the page", () => {
    expect(rule(".identity__panel")).toMatch(/width:\s*min\(44rem/);
  });

  it("is not clipped: the sidebar does not scroll, only its nav does", () => {
    expect(rule(".sidebar")).not.toMatch(/overflow/);
    expect(rule(".sidebar__nav")).toMatch(/overflow-y:\s*auto/);
  });

  it("is a rectangle: identity on the left, capabilities beside it", () => {
    expect(rule(".identity__panel")).toMatch(/grid-template-columns:[^;]*minmax\(0,\s*1fr\)/);
    expect(rule(".identity__panel > .cap-list")).toMatch(/grid-column:\s*2/);
  });
});
