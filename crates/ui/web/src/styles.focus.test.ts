/**
 * The focus ring is a `box-shadow`, so anything else that sets one can eat it.
 *
 * `:focus-visible { box-shadow: var(--focus-ring) }` is declared once, near the
 * top of `styles.css`. A later rule of equal specificity that sets its own
 * `box-shadow` — an elevation, a pressed state — wins *even while the element
 * is focused*, and the control silently loses its ring. It is invisible to
 * every other gate: jsdom computes no styles, so the component suites cannot
 * see it, and it looks correct in a screenshot of an unfocused page.
 *
 * It had already happened twice by the time anyone looked in a browser: the
 * skip link (the FIRST control a keyboard user reaches) and whichever theme
 * option was currently selected.
 *
 * So this reads the stylesheet as text. Every selector that sets a non-inset
 * `box-shadow` after the global rule must also carry a `:focus-visible` rule —
 * or be listed below as a shape that can never receive focus.
 */

import { describe, expect, it } from "vitest";

import { cssRules as rules, readStyles } from "./testing/css";

const CSS = readStyles();

/**
 * Selectors that set an elevation and cannot be focused, so they cannot hide a
 * ring. Add to this list only with a reason — if the element can take focus,
 * it needs a `:focus-visible` rule instead.
 */
const NEVER_FOCUSED = new Set([
  // The disabled-control tooltip: a `::after` pseudo-element.
  ".button[data-reason]::after",
  // A floating panel, not a control.
  ".identity__panel",
  // The pressed theme option — covered by its own rule, asserted separately.
  '.theme-switch__option[aria-pressed="true"]',
  // The skip link — covered by its own rule, asserted separately.
  ".skip-link",
]);

function setsOuterBoxShadow(body: string): boolean {
  const declaration = /box-shadow:\s*([^;]+);/.exec(body);
  if (declaration === null) {
    return false;
  }
  const value = (declaration[1] ?? "").trim();
  return value !== "none" && !value.startsWith("inset") && !value.includes("var(--focus-ring)");
}

describe("the focus ring", () => {
  const all = rules(CSS);
  const global = all.find((rule) => rule.selector === ":focus-visible");

  it("is declared once, globally", () => {
    expect(global).toBeDefined();
    expect(global?.body).toContain("var(--focus-ring)");
  });

  it("is not eaten by a later elevation on any focusable selector", () => {
    const after = all.filter(
      (rule) =>
        rule.at > (global?.at ?? 0) &&
        !rule.selector.includes(":focus-visible") &&
        setsOuterBoxShadow(rule.body),
    );
    const focusRules = new Set(
      all.filter((r) => r.selector.includes(":focus-visible")).map((r) => r.selector),
    );
    const unguarded = after
      .map((rule) => rule.selector)
      .filter((selector) => !NEVER_FOCUSED.has(selector))
      .filter((selector) => !focusRules.has(`${selector}:focus-visible`));
    expect(unguarded).toEqual([]);
  });

  it("survives on the skip link, which is the first control a keyboard reaches", () => {
    const rule = all.find((r) => r.selector === ".skip-link:focus-visible");
    expect(rule?.body).toContain("var(--focus-ring)");
  });

  it("survives on the selected theme option, which carries its own elevation", () => {
    const rule = all.find(
      (r) => r.selector === '.theme-switch__option[aria-pressed="true"]:focus-visible',
    );
    expect(rule?.body).toContain("var(--focus-ring)");
  });
});

/**
 * `light-dark()` is a COLOUR function. Handing it anything else makes the whole
 * declaration invalid at computed-value time, and the property silently falls
 * back to its initial value — which for `box-shadow` is `none`. That is how
 * every elevation in this console came to render as nothing while looking
 * perfectly reasonable in the source.
 *
 * A browser reports nothing: the rule parses, it just does not apply.
 */
describe("light-dark()", () => {
  const COLOUR = /^(#|rgba?\(|hsla?\(|oklch\(|oklab\(|color\(|var\(|transparent$|currentcolor$)/i;

  it("is only ever handed colours", () => {
    const bad: string[] = [];
    // Comments out first: this file's own prose names `light-dark()` in
    // sentences, and a sentence is not a colour.
    const bare = CSS.replace(/\/\*[\s\S]*?\*\//g, "");
    const re = /light-dark\(([\s\S]*?)\)(?=\s*[;,)])/g;
    let match: RegExpExecArray | null;
    while ((match = re.exec(bare)) !== null) {
      const args = (match[1] ?? "").trim();
      // Split on the top-level comma: each side must read as a colour.
      let depth = 0;
      let split = -1;
      for (let i = 0; i < args.length; i += 1) {
        const ch = args[i];
        if (ch === "(") depth += 1;
        else if (ch === ")") depth -= 1;
        else if (ch === "," && depth === 0) {
          split = i;
          break;
        }
      }
      if (split === -1) {
        continue;
      }
      for (const side of [args.slice(0, split), args.slice(split + 1)]) {
        const value = side.trim();
        if (value.length > 0 && !COLOUR.test(value)) {
          bad.push(`light-dark(… ${value.slice(0, 44)} …)`);
        }
      }
    }
    expect(bad).toEqual([]);
  });

  it("leaves the shadow tokens as real shadow lists", () => {
    for (const token of ["--shadow-1:", "--shadow-2:"]) {
      const at = CSS.indexOf(token);
      expect(at).toBeGreaterThan(-1);
      const value = CSS.slice(at + token.length, CSS.indexOf(";", at)).trim();
      expect(value.startsWith("light-dark(")).toBe(false);
    }
  });
});
