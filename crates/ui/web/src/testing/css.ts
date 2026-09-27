/**
 * Reading `styles.css` as text, for the guards that cannot be written any
 * other way.
 *
 * Two defects this console has actually shipped are invisible to every other
 * gate: a later `box-shadow` silently eating a focus ring, and a health colour
 * reaching text with no icon or word beside it. jsdom computes no styles, so
 * the component suites cannot see either, and both look perfectly reasonable
 * in the source. The only place the truth is written down is the stylesheet
 * itself, so the guards read it.
 *
 * Test-only, like `test-utils.tsx`, and deliberately side-effect free so a
 * guard can import it without dragging React or a fetch mock along.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

/** One declaration block, with where it sits in source order. */
export interface CssRule {
  selector: string;
  body: string;
  /** Byte offset of the block, so "declared after X" is answerable. */
  at: number;
}

/**
 * The stylesheet off disk.
 *
 * Read rather than imported: vitest stubs CSS modules to an empty string
 * (`test.css` is off), so `import "./styles.css?raw"` silently yields `""` and
 * every assertion built on it would pass against nothing.
 */
export function readStyles(): string {
  return readFileSync(join(process.cwd(), "src/styles.css"), "utf8");
}

/** The stylesheet with its comments removed. */
export function stripComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//g, "");
}

/**
 * Every declaration block in source order.
 *
 * Comments come out FIRST, before anything is matched: this stylesheet's own
 * prose quotes CSS (`.ledger-scroll … { content: none }`), and a brace inside
 * a comment derails a brace-counting parse. `@media` needs no special case —
 * its body contains braces, so the inner rules are what match.
 */
export function cssRules(css: string): CssRule[] {
  const bare = stripComments(css);
  const out: CssRule[] = [];
  const re = /([^{}]+)\{([^{}]*)\}/g;
  let match: RegExpExecArray | null;
  while ((match = re.exec(bare)) !== null) {
    const selector = (match[1] ?? "").trim();
    if (selector.length === 0 || selector.startsWith("@")) {
      continue;
    }
    out.push({ selector, body: match[2] ?? "", at: match.index });
  }
  return out;
}
