import { describe, expect, it } from "vitest";

import { cssRules, readStyles } from "../testing/css";

describe("whole-row links", () => {
  const rules = cssRules(readStyles());
  const body = (match: (selector: string) => boolean) =>
    rules
      .filter((r) => match(r.selector))
      .map((r) => r.body)
      .join("\n");

  it("stretch the row's link over the whole row", () => {
    expect(body((s) => s.includes("tr:has(.row-link)") && !s.includes(" "))).toMatch(
      /position:\s*relative/,
    );
    const cover = body((s) => s.includes(".row-link::after"));
    expect(cover).toMatch(/position:\s*absolute/);
    expect(cover).toMatch(/inset:\s*0/);
  });

  it("keep the row's other links and buttons above the cover, so they still work", () => {
    expect(body((s) => s.includes(":not(.row-link)"))).toMatch(/z-index:\s*1/);
  });

  it("drop the underline on the row's name — the whole row is the link", () => {
    expect(body((s) => s === ".row-link")).toMatch(/text-decoration:\s*none/);
  });
});
