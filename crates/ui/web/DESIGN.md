---
name: kopiur-ui
description: The vault ledger — an operator's console for Kopia-native Kubernetes backups, quiet when healthy, specific when not.
colors:
  accent-indigo: "light-dark(#4f55d2, #8b90f0)"
  accent-soft: "light-dark(rgba(79, 85, 210, 0.12), rgba(139, 144, 240, 0.16))"
  selection: "light-dark(#d9dbf8, #34397a)"
  ink: "light-dark(#1a1d21, #e6e8eb)"
  ink-muted: "light-dark(#4f5761, #a7aeb8)"
  ink-faint: "light-dark(#636c78, #858d98)"
  ink-on-accent: "light-dark(#ffffff, #0f1114)"
  slate-canvas: "light-dark(#f3f4f6, #131519)"
  slate-surface: "light-dark(#ffffff, #1b1e24)"
  slate-rail: "light-dark(#e9ebef, #0f1114)"
  slate-inset: "light-dark(#edeff2, #22262d)"
  hairline: "light-dark(#d5d9df, #2c313a)"
  hairline-strong: "light-dark(#b8bec7, #3b414c)"
  lamp-healthy: "light-dark(#0b6e4f, #5fd3a6)"
  lamp-healthy-field: "light-dark(#ddf3ea, #10281f)"
  lamp-degraded: "light-dark(#7a4e00, #f2b84b)"
  lamp-degraded-field: "light-dark(#fbefd3, #2e2410)"
  lamp-failed: "light-dark(#a6320b, #f28b6b)"
  lamp-failed-field: "light-dark(#fbe3d9, #331a12)"
  lamp-pending: "light-dark(#0b5c8f, #6fbbef)"
  lamp-pending-field: "light-dark(#dceef8, #10222e)"
  lamp-suspended: "light-dark(#4f5761, #a7aeb8)"
  lamp-suspended-field: "light-dark(#e7e9ed, #23272e)"
typography:
  title:
    fontFamily: "ui-sans-serif, system-ui, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "1.375rem"
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: "-0.01em"
  headline:
    fontFamily: "ui-sans-serif, system-ui, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "1.0625rem"
    fontWeight: 600
    lineHeight: 1.25
  body:
    fontFamily: "ui-sans-serif, system-ui, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: 1.35
    fontFeature: "tabular-nums"
  ledger:
    fontFamily: "ui-sans-serif, system-ui, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "0.8125rem"
    fontWeight: 400
    lineHeight: 1.35
  label:
    fontFamily: "ui-sans-serif, system-ui, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "0.6875rem"
    fontWeight: 500
    letterSpacing: "0.06em"
  identifier:
    fontFamily: "ui-monospace, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace"
    fontSize: "0.8125rem"
    fontWeight: 400
rounded:
  sm: "3px"
  md: "6px"
spacing:
  "1": "0.25rem"
  "2": "0.5rem"
  "3": "0.75rem"
  "4": "1rem"
  "5": "1.5rem"
  "6": "2rem"
  "7": "3rem"
components:
  button:
    backgroundColor: "{colors.slate-surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0 0.75rem"
    height: "2rem"
  button-primary:
    backgroundColor: "{colors.accent-indigo}"
    textColor: "{colors.ink-on-accent}"
    rounded: "{rounded.md}"
    padding: "0 0.75rem"
    height: "2rem"
  button-quiet:
    backgroundColor: "transparent"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0 0.75rem"
    height: "2rem"
  health-lamp:
    backgroundColor: "{colors.lamp-suspended-field}"
    textColor: "{colors.lamp-suspended}"
    rounded: "{rounded.sm}"
    padding: "0.1em 0.5em 0.1em 0.4em"
  nav-item:
    backgroundColor: "transparent"
    textColor: "{colors.ink-muted}"
    padding: "0 1rem"
    height: "2.125rem"
  nav-item-current:
    backgroundColor: "{colors.slate-inset}"
    textColor: "{colors.ink}"
    padding: "0 1rem"
    height: "2.125rem"
  panel:
    backgroundColor: "{colors.slate-surface}"
    rounded: "{rounded.md}"
    padding: "1rem"
  problem:
    backgroundColor: "{colors.lamp-failed-field}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0.75rem 1rem"
---

# Design System: kopiur-ui

## Overview

**Creative North Star: "The Vault Ledger"**

kopiur-ui is read the way a tape librarian reads the vault: rows of labelled
media, each with a kind, a name and a lettered retention ring, on an anodised
slate chassis that says nothing when all is well. It is an Operate surface —
dense, calm, legible at a glance — opened many times a day and sometimes at
3am, so both lights are first-class and neither is a default: the daylit vault
and the 3am vault are the same console under different lamps.

Every colour is declared once with `light-dark()` in `src/styles.css`; the
page follows the OS until the user overrides it through `data-theme` on
`<html>`, which `color-scheme` turns into the other set of values. Health is
never a hue alone: six lamps, each an icon and a word on a tinted field, and
the accent is reserved by law for selection, the current marker and the
primary action. Where healthy is quiet, a failure is loud and specific — what
happened, why, and the fix on its own plate.

**Key Characteristics:**

- Slate neutrals, cool in both lights; one indigo accent; six Okabe–Ito-derived health lamps
- One system sans for everything; monospace only for identifiers and paths; tabular numerals throughout
- 1px hairlines and 6px chamfers; one level of enclosure, never a card inside a card
- Label strips (small-caps KIND + mono name) and lettered lamps as the two recurring marks
- Skeleton loading, teaching empty states, and what / why / fix errors with the fix set apart

## Colors

A restrained palette: cool slate neutrals carrying the page, one accent, and a
health vocabulary that never speaks by colour alone.

### Primary

- **Accent Indigo** (`{colors.accent-indigo}`): the current nav marker, focus rings, the primary button, links and the "FIX" label. Chosen because it sits outside every health hue, so a lit lamp and a selected control can never be confused. Never used as decoration.
- **Accent Soft** and **Selection**: the accent at low opacity for text selection and soft fills.

### Neutral

- **Slate Canvas** (`{colors.slate-canvas}`): the page ground behind everything.
- **Slate Surface** (`{colors.slate-surface}`): the header, panels, the identity panel — the working plane.
- **Slate Rail** (`{colors.slate-rail}`): the second neutral layer for the navigation rail; slightly darker (light) or deeper (dark) than the canvas so the frame reads as chassis.
- **Slate Inset** (`{colors.slate-inset}`): table headers, the theme switch track, skeleton base — recessed regions.
- **Ink / Ink Muted / Ink Faint** (`{colors.ink}`, `{colors.ink-muted}`, `{colors.ink-faint}`): three text strengths; ink faint is the floor and still clears 4.5:1 on every surface it is used on.
- **Hairline / Hairline Strong** (`{colors.hairline}`, `{colors.hairline-strong}`): the only dividers; strong for a control's edge or a table's head rule.

### Health lamps

- **Healthy** (`{colors.lamp-healthy}` on `{colors.lamp-healthy-field}`): CircleCheck + "Healthy".
- **Degraded** (`{colors.lamp-degraded}` on `{colors.lamp-degraded-field}`): TriangleAlert + "Degraded"; also the not-permitted banner's field.
- **Failed** (`{colors.lamp-failed}` on `{colors.lamp-failed-field}`): OctagonX + "Failed"; also the problem banner's field and the danger button's ink.
- **Pending** (`{colors.lamp-pending}` on `{colors.lamp-pending-field}`): Clock + "Pending".
- **Suspended / Unknown** (`{colors.lamp-suspended}` on `{colors.lamp-suspended-field}`): CirclePause + "Suspended"; CircleHelp + "Unknown". The two share a neutral field and differ by icon and word — an unknown value must never read as healthy.

### Named Rules

**The Lettered Lamp Rule.** A health colour never appears without its icon and its word. `HealthBadge` (a `Health` value) and `LampBadge` (a `Lamp` that carries its own icon, such as the dashed circle of a dangling reference) are the only two ways to render one; `HealthBadge` is `LampBadge` with the lamp looked up.

**The Accent By Law Rule.** Indigo marks selection, the current item, focus and the primary action. It is never a health colour, never a border for emphasis, never a background wash.

**The One Declaration Rule.** Every colour is written once as `light-dark(light, dark)`. There is no dark block to keep in sync.

**The Browser Floor.** `light-dark()` needs Chrome 123, Firefox 120 or Safari 17.5 (all 2024); that is the console's supported floor. Below it every colour token resolves to `unset` and the page degrades to the UA's own light/dark canvas — readable, and the lamps still differ by icon and word — while a single `@supports not (color: light-dark(#000, #fff))` block in `styles.css` pins static light values for `--fg`, `--bg-canvas`, `--bg-surface` and `--focus-ring`, so keyboard focus stays visible. Do not add per-token fallbacks beyond those four; raise the floor instead.

## Typography

**Display Font:** none — this is an Operate surface; the page title is the body family at 22px.
**Body Font:** the system UI stack (`ui-sans-serif, system-ui, …`)
**Label/Mono Font:** the system monospace stack (`ui-monospace, "SF Mono", Menlo, …`)

**Character:** one workhorse sans carrying everything from the page title to a
table cell, with monospace reserved for the things an operator copies —
object names, namespaces, paths, users. Numbers are tabular everywhere so
columns of sizes and ages align.

### Hierarchy

- **Title** (600, 1.375rem, 1.2): the page title in the header, one per page.
- **Headline** (600, 1.0625rem, 1.25): section and state titles (`h2`).
- **Body** (400, 0.875rem, 1.35; prose 1.5): running text, at most 65ch.
- **Ledger** (400, 0.8125rem, 1.35): table rows, controls, the identity strip — the density the console lives at.
- **Label** (500, 0.6875rem, 0.06em tracking, uppercase): column headers, definition terms, the KIND half of a label strip, the "FIX" tag. Always beside or above data, never above a heading.
- **Identifier** (mono, 0.8125rem): names, namespaces, paths, users.

### Named Rules

**The Mono For Identifiers Rule.** Monospace is for what gets copied — names, paths, users — never a costume for "technical".

**The Rank By Weight Rule.** Five sizes only. Rank inside a row is carried by weight, case and colour, not by another size.

## Layout

A two-column shell: a 232px sticky rail (`--rail-width`) and a content column
with a 48px sticky header (`--header-height`); content is padded 1.5rem and
capped at 1440px (`--content-max`). Spacing is a 4px scale (0.25rem to 3rem,
`--space-1` … `--space-7`); tight inside a group (0.5rem), generous between
regions (1.5rem), more above a heading than below it.

Responsive behaviour is structural, not fluid: below 900px the rail becomes a
horizontally scrolling strip above the header (its trailing edge fades to say
"more"), the header stops sticking, the capability list drops to one column,
and below 560px the theme switch and identity source show icons only. Type
sizes never scale with the viewport.

## Elevation & Depth

Depth is mostly tonal: the rail, canvas, surface and inset are four steps of
the same slate, and hairlines do the separating. Shadows appear only on
things that float — the identity panel and the disabled-reason tooltip — and
always carry an offset and a soft blur. In the dark theme the same tokens
deepen rather than lighten.

### Shadow Vocabulary

- **Rest** (`--shadow-1`: `0 1px 2px rgba(26,29,33,0.08)` / dark `0 1px 2px rgba(0,0,0,0.5)`): the pressed theme-switch option.
- **Float** (`--shadow-2`: `0 6px 16px -4px rgba(26,29,33,0.18), 0 2px 4px …` / dark `0 8px 20px -6px rgba(0,0,0,0.7)`): the identity panel and the reason tooltip.
- **Focus** (`--focus-ring`: `0 0 0 2px surface, 0 0 0 4px accent`): every focusable element; inset on rail links.

### Named Rules

**The Hairlines Only Rule.** Regions are divided by 1px hairlines, never by shadow or by a second box. A card inside a card is always wrong.

## Shapes

Small chamfers, like a chassis: 3px (`--radius-sm`) on chips, lamps and focus
rings; 6px (`--radius-md`) on controls, panels, the banner and the identity
panel. No pills, no large radii. Borders are 1px hairlines; the only wider
stroke is the 3px accent bar that marks the current nav row, the one current
marker the system has.

## Components

### Buttons

- **Shape:** chamfered (6px), 2rem tall, 0.75rem horizontal padding, 0.8125rem medium.
- **Default:** surface fill, strong hairline border, ink text.
- **Primary:** accent fill, on-accent ink, accent border.
- **Quiet:** transparent, no border; the dismiss control.
- **Danger:** failed-lamp ink and border on a surface fill.
- **Hover / Active:** hover and active tints from the neutral overlay tokens; 120ms ease-out.
- **Disabled with a reason:** `aria-disabled` (still focusable) at 55% opacity, with the reason in a floating tooltip on hover and keyboard focus and in the accessible name. Never hidden.

### Health lamp

- **Style:** icon (14px, 2px stroke) + word on a tinted field, 3px chamfer, 0.8125rem medium.
- **State:** keyed by `data-health`; six lamps; unknown carries the raw word.

### Label strip

- **Style:** KIND in label caps (ink faint) beside the name in monospace (ink), one line, name truncates.

### Panels

- **Corner Style:** 6px.
- **Background:** surface.
- **Shadow Strategy:** none at rest (tonal).
- **Border:** 1px hairline; a hairline under the header row.
- **Internal Padding:** 1rem.

### Ledger (table)

- **Head:** label caps in ink faint over a strong hairline.
- **Rows:** 0.5rem × 0.75rem cells, hairline between rows, hover overlay; numeric cells right-aligned in tabular figures.

### Navigation

- **Rail:** slate rail with a hairline right edge; brand row (mark + wordmark + "console"); ten rows of icon (16px) + label, 2.125rem tall, ink muted; hover overlay; current row gets the inset tint and a single 3px accent bar.
- **Mobile:** a scrolling strip with the bar under the current row and a fade at the trailing edge.

### Problem

- **Style:** grid of icon / body / dismiss on the failed-lamp field with a 1px failed-lamp border (degraded field and border for a 403); what in semibold, why in muted, the fix on a surface plate led by an accent "FIX" label, metadata in faint mono beneath.
- **Banner variant:** full width under the header, square corners, hairline bottom only.

### Verdict

- **Style:** an `h2` row under the header: a lamp plate (icon + word on the lamp's field, 6px chamfer, the only lamp set at body size) beside one sentence in headline weight, metadata faint mono at the far end, a hairline beneath.
- **Rule:** never the healthy lamp while any source is loading or refused, and never over an empty scope — "cannot tell" and "no repositories in scope" are unknown, not green. Used by the overview and the doctor summary (`components/verdict.ts`).

### Health strip

- **Style:** the fleet counted per lamp: a row of chamfered surface tiles, each a tabular count in headline weight beside its `HealthBadge`, worst first, every lamp present and dimmed at zero. Each tile is a link into the filtered list. Never a stat tile with a big number and a small label.

### Finding

- **Style:** what in semibold, why in muted prose, the fix on an inset plate led by the accent "FIX" label, metadata faint mono beneath; an optional lamp icon leads it. The same plate the problem banner uses, for things that are not request errors (a failing doctor check, a fired gate). A one-line finding renders as one line.

### Work ledger

- **Style:** the ledger with a label strip (KIND + mono name, wrapping — never an ellipsis on the name — with the namespace faint beneath), a lamp whose word may be the state ("Stalled"), a muted detail, and an age in tabular figures only when a row has one. Wide ledgers scroll inside `.ledger-scroll` on narrow viewports rather than starving a column.

### Controls

- **Style:** a surface bar of labelled fields (label caps above a mono input on the canvas ground), a faint hint or a failed-lamp error beneath, one primary action at the end. Validation repeats the server's rule so a refused value never leaves the form.

### Board (topology)

- **Style:** a scrolling frame on the inset ground with a hairline edge and a 6px chamfer, holding two layers: one SVG of routed orthogonal lines, and ordinary HTML plates positioned over it. Plates are 232px wide and at least 64px tall — the label strip's two lines (kind in label caps, name in mono) over the lamp — on the surface with a strong hairline, so they read as raised off the chassis. The current plate takes the accent border and inset ring, the one current marker the system has.
- **Lines:** direction is the arrow; _kind_ is the dash and the end marker; _health_ is the stroke colour **and**, at the middle of the line, the word — a failing replication reads "0 5 \* \* \* · Failed", never a red line alone. Five strokes, defined once in `topology/model.ts` (`edgeStyle`) and drawn from that one source by both the board and the legend.
- **Rule:** the SVG is `aria-hidden` and everything it draws is also said in words — each plate is a button, each relationship is in the drawer for either end, and the whole edge set is listed for assistive technology beneath the board. A picture is never the only copy of a fact.

### Ghost plate

- **Style:** a 2px dashed failed-lamp border on the failed-lamp field, with a dashed-circle lamp reading "referenced but not found" — the one lamp on the console allowed to wrap, because truncating it would turn a dangling reference back into an ordinary failure.
- **Rule:** a node the server marked `missing` is a backup destination that does not exist. It is the only structurally different shape on the board, so it is legible before a single word is read, and its drawer states what / why / fix.

### Edge legend

- **Style:** a three-column grid of sample stroke, relationship name, and one clause of meaning, drawn with the same dash, width and marker the board uses, in neutral ink — the legend is about kind, not health. One column with the sample above the words below 900px.
- **Rule:** always on the page while a board is on it. Never a hover tooltip, never behind a disclosure: a dash vocabulary the reader has to uncover is not a vocabulary.

### Drawer

- **Style:** a panel in the column beside the board (a block beneath it below 900px), sticky to the top while the board scrolls, never an overlay — the plate you selected has to stay visible while you read about it. Header is the label strip and a quiet close; body is one clause saying what the kind is, a definition list of facts, any findings, the relationships at both ends grouped by direction, and a link to the section that lists the object.
- **Rule:** opening it moves focus into the panel and closing it (button or Escape) hands focus back to the plate. Its links stop at the section: a repository's URL segment is the server's `kindPath` and is never guessed from the display kind.

### States

- **Loading:** skeleton ledger rows (1.4s sheen, disabled under reduced motion), announced as busy.
- **Empty:** icon + headline that says what would appear, body that says how it comes to exist, optional action.
- **Error / Not permitted:** headline + the problem; a 403 swaps to the shield icon, the degraded field and a sentence about RBAC, and offers no retry. Two sections fed by one refused read say it once, across their width, not twice side by side.

## Do's and Don'ts

### Do:

- **Do** render every health value through `HealthBadge` — icon and word, never a coloured dot.
- **Do** declare a new colour once, as `light-dark(light, dark)`, and check both pairs clear 4.5:1.
- **Do** set identifiers in monospace and numbers in tabular figures.
- **Do** disable an action the user may not perform and say why (`ActionButton.disabledReason` from `capabilityReason`).
- **Do** render a problem's what, why and fix from the server's own text, fix set apart.
- **Do** give a drawn relationship a word as well as a stroke — the dash says which kind, the legend says what the dash means, and a stroke colour is always joined by a word on the line, on the plate or in the drawer.

### Don't:

- **Don't** put a label, kicker or eyebrow above a heading; label caps sit beside data, in table heads and definition terms only.
- **Don't** use the accent for a health state, a border for emphasis, or a background wash.
- **Don't** nest a panel in a panel or divide regions with shadows; use a hairline.
- **Don't** use a spinner; loading is a skeleton of the rows to come.
- **Don't** let an unknown or unrecognised value fall through to the healthy lamp.
- **Don't** draw a graph whose only reading is visual: a picture must have a spoken twin — a button, a drawer, or a list beneath it.
