import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { cssRules, readStyles } from "../testing/css";

describe("whole-row links", () => {
  const rules = cssRules(readStyles());
  const body = (match: (selector: string) => boolean) =>
    rules
      .filter((r) => match(r.selector))
      .map((r) => r.body)
      .join("\n");

  /*
   * #583: a `<tr>` is not a containing block in shipping Safari (WebKit bug
   * 240961), and Firefox ESR 115 has no `:has()`. A `.row-link::after` cover
   * stretched over a table row therefore sized against the table, every row's
   * cover filled it, and the last row's won — every row opened the last one.
   * Table rows follow their link from a click handler (`rowLink.ts`) instead,
   * so the stylesheet must not bring the cover back.
   */
  it("never stretches a cover over a table row", () => {
    const covers = rules.filter((r) => r.selector.includes(".row-link::after"));
    expect(covers.length).toBeGreaterThan(0);
    for (const rule of covers) {
      for (const selector of rule.selector.split(",").map((s) => s.trim())) {
        expect(selector, "a ::after cover must be scoped to a grid-div row").toMatch(
          /^\.(attention|activity)-row\b/,
        );
      }
    }
  });

  it("never makes a table row depend on :has() or on being positioned", () => {
    for (const rule of rules) {
      const selectors = rule.selector.split(",").map((s) => s.trim());
      for (const selector of selectors) {
        expect(selector, `${selector} keys a table row on :has()`).not.toMatch(/\btr[^,\s]*:has\(/);
      }
    }
    expect(body((s) => /(^|,\s*)tr\.row--link\s*$/.test(s))).not.toMatch(/position:/);
  });

  it("marks a linked table row with a class: a pointer, and the hover tint", () => {
    expect(body((s) => s === "tr.row--link")).toMatch(/cursor:\s*pointer/);
    expect(body((s) => s.includes("tr.row--link:hover"))).toMatch(
      /background:\s*var\(--bg-hover\)/,
    );
  });

  it("keep the grid rows' other links and buttons above their cover, so they still work", () => {
    for (const row of [".attention-row", ".activity-row"]) {
      expect(
        body((s) => s.includes(row) && s.includes(":not(.row-link)")),
        row,
      ).toMatch(/z-index:\s*1/);
    }
  });

  it("keep an overview time above the cover, so hovering it shows the instant", () => {
    for (const row of [".attention-row", ".activity-row"]) {
      const lifted = rules.filter(
        (r) => r.selector.includes(row) && r.selector.includes("time[title]"),
      );
      expect(lifted.map((r) => r.body).join("\n"), row).toMatch(/z-index:\s*1/);
    }
  });

  it("drop the underline on the row's name — the whole row is the link", () => {
    expect(body((s) => s === ".row-link")).toMatch(/text-decoration:\s*none/);
  });

  it("make a whole attention row its object's link, and show the ring on the row", () => {
    expect(body((s) => s === ".attention-row:has(.row-link)")).toMatch(/position:\s*relative/);
    expect(
      body((s) => s.includes(".attention-row:has(.row-link)") && s.includes(":not(.row-link)")),
    ).toMatch(/z-index:\s*1/);
    expect(body((s) => s.includes(".attention-row:has(.row-link:focus-visible)"))).toMatch(
      /box-shadow:\s*var\(--focus-ring\)/,
    );
  });
});

/*
 * A table whose rows carry a `.row-link` must say so, or its rows are not
 * click targets at all: the stylesheet no longer stretches the link, so a
 * forgotten opt-in is a silently dead row rather than a working one.
 */
describe("tables with row links opt in to the row click", () => {
  function sources(dir: string): string[] {
    return readdirSync(dir).flatMap((entry) => {
      const path = join(dir, entry);
      if (statSync(path).isDirectory()) return sources(path);
      return entry.endsWith(".tsx") && !entry.endsWith(".test.tsx") ? [path] : [];
    });
  }
  const files = sources(join(process.cwd(), "src")).map((path) => ({
    path,
    text: readFileSync(path, "utf8"),
  }));

  it("every ColumnLedger with a row link passes rowLinks", () => {
    const ledgers = files.filter(
      (f) => f.text.includes("<ColumnLedger") && f.text.includes("row-link"),
    );
    expect(ledgers.length).toBeGreaterThan(5);
    for (const file of ledgers) {
      expect(file.text, file.path).toMatch(/\browLinks\b/);
    }
  });

  it("every hand-drawn table row with a row link follows it on click", () => {
    const tables = files.filter(
      (f) =>
        f.text.includes("<table") &&
        f.text.includes("row-link") &&
        !f.path.endsWith("ColumnLedger.tsx"),
    );
    for (const file of tables) {
      expect(file.text, file.path).toMatch(/onClick=\{followRowLink\}/);
    }
  });
});
