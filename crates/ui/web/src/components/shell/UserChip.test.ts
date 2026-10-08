import { describe, expect, it } from "vitest";

import { cssRules, readStyles } from "../../testing/css";

describe("UserChip panel", () => {
  const rule = (selector: string) =>
    cssRules(readStyles()).find((r) => r.selector === selector)?.body ?? "";

  it("flies out of the sidebar — fixed, not clipped by the sidebar's own scroll", () => {
    const panel = rule(".identity__panel");
    expect(panel).toMatch(/position:\s*fixed/);
    expect(panel).toMatch(/left:\s*calc\(var\(--sidebar-width\)/);
  });

  it("is a rectangle: identity on the left, capabilities beside it", () => {
    expect(rule(".identity__panel")).toMatch(/grid-template-columns:[^;]*minmax\(0,\s*1fr\)/);
    expect(rule(".identity__panel > .cap-list")).toMatch(/grid-column:\s*2/);
  });
});
