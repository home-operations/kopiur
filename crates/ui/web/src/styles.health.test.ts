/**
 * The Lettered Lamp Rule, enforced against the stylesheet.
 *
 * `DESIGN.md`: *a health colour never appears without its icon and its word*.
 * The reason is not decorative. This is backup software; an operator opens the
 * console at three in the morning to find out whether their data is safe, and
 * a red/green board tells a colour-blind reader nothing at all. Contrast does
 * not save it either — every lamp in here clears AA in both schemes, and a
 * deuteranope still cannot tell the lit cell from the calm one beside it.
 *
 * It rotted quietly. Four surfaces grew a `color: var(--health-failed-fg)`
 * class of their own — `never verified`, `3 failed runs`, `never run`, a bare
 * failure count — and each was locally reasonable: the failed lamp's ink is
 * exactly the right ink, and the author was reaching for the right *colour*.
 * What nobody notices at the point of writing is that a colour reached
 * directly is a colour reached without the component that guarantees the rest
 * of the lamp.
 *
 * So the guard is about **who may reach the token**, not about what any
 * particular screen renders today. A test that pinned today's markup would
 * pass forever while the fifth surface was being written. Every use of a
 * `--health-*` colour must fall into one of three shapes that cannot be a hue
 * alone, or be named below with a reason:
 *
 *   1. it *is* a lamp (`.health`, `.verdict__lamp`), which renders icon and
 *      word by construction — pinned by `HealthBadge.test.tsx`;
 *   2. the colour lands on an icon, which is itself the non-hue carrier;
 *   3. it tints a container — a border or a field, never the text — and the
 *      words and the icon are inside it.
 *
 * Anything else is text wearing a health colour, and has to earn its place on
 * `INK_ON_TEXT` in front of whoever reviews it. The list is asserted in both
 * directions: an entry that stops using a health colour fails too, so it
 * cannot linger as dead permission for the next author to file under.
 */

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { cssRules, readStyles, type CssRule } from "./testing/css";

const CSS = readStyles();

/** Every non-test component source, for counting who applies a class. */
function tsxSources(dir = join(process.cwd(), "src")): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...tsxSources(path));
    } else if (entry.name.endsWith(".tsx") && !entry.name.includes(".test.")) {
      out.push(readFileSync(path, "utf8"));
    }
  }
  return out;
}

/** Every lamp token, `-fg` and `-bg` alike. */
const HEALTH_TOKEN = /var\(--health-[a-z]+-(?:fg|bg)\)/;

/**
 * Selectors that are a lamp. These are the two components that render colour,
 * icon and word together and cannot render one without the others.
 */
const LAMP = /^\.(?:health|verdict__lamp)\b/;

/**
 * The colour lands on an icon rather than on text: an `__icon` element, or a
 * descendant `svg`. An icon is a shape, so it carries meaning without its hue.
 */
const ICON = /(?:__icon|__warning-icon|\bsvg)\b[^\s]*$/;

/**
 * A blessed use of a health colour on text: why it is not a violation, and how
 * many places may say so.
 */
interface Exception {
  reason: string;
  /**
   * How many TSX call sites apply this class, pinned.
   *
   * Reuse is how the rot actually spread: `.policy-table__never` was written
   * for one absence in the policy ledger, and the policy *detail* later
   * borrowed it to tint a raw `failed` phase — a second, unreviewed meaning
   * under a name review had already blessed. A count that has to be edited is
   * a second review moment. `undefined` where the selector is not a plain
   * class, or is composed at run time and so has no literal to count.
   */
  callSites?: number;
}

/**
 * Text that wears a health colour without being a lamp — every one of them,
 * with the reason it is not a violation. **Adding to this list is the review
 * moment.** If the answer is "the colour is what tells you it is bad", the
 * entry does not belong here: route it through `LampBadge` (see `loudLamp` in
 * `components/health.ts`, which keeps the fact's own wording and adds the
 * icon).
 */
const INK_ON_TEXT: ReadonlyMap<string, Exception> = new Map<string, Exception>([
  [
    ".button--danger",
    {
      reason:
        "A verb, not a verdict. The colour marks what the button will do; its " +
        "own label says what that is, and the confirmation panel says it again " +
        "in a sentence. No object's health is being reported.",
      // Composed as `button--${variant}` by ActionButton, so there is no
      // literal to count; the variant union is the type-level equivalent.
    },
  ],
  [
    '.controls__hint[data-invalid="true"]',
    {
      reason:
        "The hint text IS the validation message — the element swaps its prose " +
        "for the error. The input beside it carries `aria-invalid`, which is " +
        "what a screen reader announces; the colour emphasises a sentence.",
    },
  ],
  [
    ".replication-table__never",
    {
      reason:
        "A lag cell reading `never`, on a row whose State column already carries " +
        "a real `HealthBadge` for the same copy. Lamping it would say the same " +
        "thing twice in one row. Left deliberately; see task-10's audit.",
      callSites: 1,
    },
  ],
  [
    ".browse-session__lapsed",
    {
      reason:
        "`<time>` reading `… — it may already be gone`. The clause is the " +
        "meaning, and a lapsed browse session is the console's own lease " +
        "expiring, not a health the operator published about anything.",
      callSites: 1,
    },
  ],
  [
    ".snapshot-table__failed",
    {
      reason:
        "The cell's own text is `N failed` plus a visually-hidden `to read`. " +
        "The word `failed` is already in the sentence the colour emphasises. " +
        "Two call sites: the ledger cell and the detail's twin of it.",
      callSites: 2,
    },
  ],
  [
    '.action__prose p[data-destructive="true"]',
    {
      reason:
        "The clause naming an irreversible consequence, inside a confirmation " +
        "the reader has already opened. The sentence is the warning; the ink " +
        "stops it being skimmed.",
    },
  ],
  [
    ".policy-table__never",
    {
      reason:
        "`names no repository` — a whole clause naming a configuration defect. " +
        "The absences that were only a word (`never verified`, `never " +
        "succeeded`) are lamps now.",
      callSites: 1,
    },
  ],
  [
    ".schedule-table__never",
    {
      reason: "`nothing — it names no policy and sets no selector`, on the same terms.",
      callSites: 1,
    },
  ],
]);

/** Whether the rule paints TEXT with a health token (as opposed to a field). */
function inksText(rule: CssRule): boolean {
  const declaration = /(?:^|;)\s*color:\s*([^;]+)/.exec(rule.body);
  return declaration !== null && HEALTH_TOKEN.test(declaration[1] ?? "");
}

/** Every rule that reaches for a lamp token at all. */
function usesHealthToken(rule: CssRule): boolean {
  // The declarations of the tokens themselves are `--health-…: light-dark(…)`,
  // which is the palette, not a use of it.
  return HEALTH_TOKEN.test(rule.body) && !rule.selector.includes(":root");
}

describe("the Lettered Lamp Rule", () => {
  const all = cssRules(CSS);
  const uses = all.filter(usesHealthToken);

  it("finds the health colours in use at all, so a silent rename cannot pass this file", () => {
    // If `--health-*` were renamed, every assertion below would vacuously
    // pass. This is the canary: the console has six lamps and uses most of
    // them in several places.
    expect(uses.length).toBeGreaterThan(15);
  });

  it("never lets a health colour reach text that is not a lamp, an icon, or listed", () => {
    const unlettered = uses
      .filter(inksText)
      .map((rule) => rule.selector)
      .filter((selector) => !LAMP.test(selector))
      .filter((selector) => !ICON.test(selector))
      .filter((selector) => !INK_ON_TEXT.has(selector));
    // A failure here is not a styling nit. It means a surface is telling an
    // operator that something is wrong using nothing but a hue. Either render
    // it through `LampBadge` — `loudLamp(word)` keeps the wording and adds the
    // icon — or add it to `INK_ON_TEXT` with the reason it already says so in
    // words.
    expect(unlettered).toEqual([]);
  });

  it("keeps no stale permission on the list", () => {
    // An entry whose selector has gone, or stopped using a health colour, is
    // dead permission: it reads as review having blessed something, and the
    // next author files a new violation under an old name.
    const inked = new Set(uses.filter(inksText).map((rule) => rule.selector));
    const stale = [...INK_ON_TEXT.keys()].filter((selector) => !inked.has(selector));
    expect(stale).toEqual([]);
  });

  it("gives every listed exception a reason a reviewer can disagree with", () => {
    const thin = [...INK_ON_TEXT.entries()]
      .filter(([, exception]) => exception.reason.trim().length < 40)
      .map(([selector]) => selector);
    expect(thin).toEqual([]);
  });

  it("does not let a blessed ink class quietly acquire a second meaning", () => {
    // The fourth violation was this exact move: `.policy-table__never` was
    // written for one absence in the policy ledger, and the policy detail
    // later reached for it to tint a raw `failed` phase. The class was already
    // on the blessed side of every check, so nothing objected — and a phase
    // that is a lamp one screen away became a hue on this one.
    const drift = [...INK_ON_TEXT.entries()]
      .filter(([, exception]) => exception.callSites !== undefined)
      .map(([selector]) => {
        const cls = selector.slice(1);
        const found = tsxSources().filter((src) => src.includes(`"${cls}"`)).length;
        return `${selector} x${found}`;
      });
    const pinned = [...INK_ON_TEXT.entries()]
      .filter(([, exception]) => exception.callSites !== undefined)
      .map(([selector, exception]) => `${selector} x${String(exception.callSites)}`);
    // A new call site means someone is about to tell an operator that
    // something is wrong in a place review has not looked at. Read the reason
    // above, decide whether the new cell says it in words too, and either
    // reach for `loudLamp` or bump the count deliberately.
    expect(drift).toEqual(pinned);
  });

  it("tints containers with a field and a border, never by inking their text", () => {
    // The other safe shape: `.problem`, `.receipt`, `.topo-node[data-missing]`
    // and friends wear a lamp's field, and the icon and prose live inside.
    // Such a rule must not ALSO ink its own text — that is the violation
    // wearing a container's clothes.
    const containers = uses.filter(
      (rule) => /(?:^|;)\s*(?:background|border)/.test(rule.body) && !LAMP.test(rule.selector),
    );
    const both = containers
      .filter(inksText)
      .map((rule) => rule.selector)
      .filter((selector) => !ICON.test(selector))
      .filter((selector) => !INK_ON_TEXT.has(selector));
    expect(both).toEqual([]);
  });
});

describe("the failed lamp", () => {
  it("is not reachable from the stylesheet under a second name", () => {
    // The token is the chokepoint the rule above guards. A literal copy of its
    // value elsewhere would route around the whole file.
    const declaration = /--health-failed-fg:\s*light-dark\(([^)]+)\)/.exec(CSS);
    expect(declaration).not.toBeNull();
    const [light = "", dark = ""] = (declaration?.[1] ?? "").split(",").map((s) => s.trim());
    expect(light).not.toBe("");
    expect(dark).not.toBe("");
    // Each hex appears exactly once: in the token's own declaration.
    for (const hex of [light, dark]) {
      const occurrences = CSS.split(hex).length - 1;
      expect(`${hex} x${occurrences}`).toBe(`${hex} x1`);
    }
  });
});
