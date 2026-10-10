/**
 * The design tokens are the `kopiur-ui-design` skill's, verbatim.
 *
 * `.claude/skills/kopiur-ui-design/references/tokens.md` is the single source
 * for every colour, size, radius, shadow and motion value; its contrast is
 * checked by the skill's `scripts/check-contrast.py`, which `ui-check` runs.
 * This test is what makes "verbatim" true: a token edited in `styles.css`
 * alone would ship colours nobody checked.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

import { readStyles, stripComments } from "./testing/css";

const TOKENS_MD = join(
  process.cwd(),
  "../../../.claude/skills/kopiur-ui-design/references/tokens.md",
);

/** The first `:root { … }` block, comments stripped, whitespace collapsed. */
function rootBlock(css: string): string | undefined {
  const block = /:root \{[\s\S]*?\n\}/.exec(stripComments(css))?.[0];
  return block?.replace(/\s+/g, " ").trim();
}

describe("design tokens", () => {
  it("styles.css :root is the skill's token block, verbatim", () => {
    const fenced = /```css\n([\s\S]*?)```/.exec(readFileSync(TOKENS_MD, "utf8"))?.[1];
    expect(fenced, "tokens.md has a ```css block").toBeDefined();
    const want = rootBlock(fenced ?? "");
    expect(want, "tokens.md's block opens with :root").toBeDefined();
    expect(rootBlock(readStyles())).toBe(want);
  });
});
