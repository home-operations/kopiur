---
name: kopiur-ui-design
description: The canonical design system for Kopiur's web console (kopiur-ui, crates/ui/web). Use for ANY visual or UI work in crates/ui/web — new or changed routes, components, styles.css, charts, topology, empty/loading/error states, actions and dialogs — and whenever you need to know how an object (Repository, ClusterRepository, SnapshotPolicy, SnapshotSchedule, Snapshot, Restore, Maintenance, RepositoryReplication, SnapshotReplication) should look. Encodes the "Mission dashboard" direction, the light-dark() token set with verified contrast, the per-kind identity (colour, glyph, stripe + tinted chip), the primitives (status pill, kind mark, object reference, button with refusal, stat strip, attention row, split pane, table, side panel, tabs), the composites (overview, resource drawer, finding, inline action/receipt, page states, charts), the sidebar-only shell, and the non-negotiable behaviour rules with the tests that enforce them. Replaces crates/ui/web/DESIGN.md. Backend/CRD work belongs to kopiur-design instead.
---

# Kopiur console design system

This skill **is** the design system for `crates/ui/web`. There is no other design
doc; `PRODUCT.md` says who the console is for and what each screen answers, this
says how it looks and behaves. When the SPA and this skill disagree, the SPA is
wrong (or this skill needs an explicit update in the same change).

## The direction: "Mission dashboard"

Bold, scannable, dark-first but with both themes first-class. Every object is
recognisably its kind at a glance, the fleet's state is legible in seconds, and a
failure is loud and specific while a healthy fleet is calm.

- **Palette:** violet accent on cool navy neutrals. The accent marks selection,
  the current item, focus and the primary action — nothing else.
- **Status:** tinted pills, always icon + word. Six states.
- **Kinds:** nine kinds, each with its own colour and glyph, shown as a **4px
  stripe + tinted icon chip + small-caps kind name**. Square chip = kind, round
  pill = status.
- **Type:** the system stack, no web fonts. Monospace only for things you copy.
- **Shape and density:** spacious — 16px cards, 10px buttons, ~44px table rows;
  light lifts cards with a soft shadow, dark draws a 1px line.
- **Shell:** sidebar only (namespace switcher, object search, grouped nav, compact
  user chip at the bottom). No top bar.

## How to use this skill

1. **Read `references/rules.md` first.** The 17 rules are what a redesign may not
   break; most are enforced by tests in `crates/ui/web/src`.
2. **Find the primitive** in `references/primitives.md` (atoms and per-object
   building blocks) or the pattern in `references/composites.md` (overview,
   relationships, findings, actions, states, charts) or `references/shell.md`.
   Build from those; do not style one-off markup in a route.
3. **Look up the object** in `references/kinds.md` — its token slug, glyph, status
   source, what its card shows and what it relates to. All field names there are
   the real wire fields from `crates/ui-model`.
4. **Use tokens only** from `references/tokens.md`. Never write a raw colour,
   size or radius in a component. Changing a token means changing `tokens.md`,
   re-running the contrast check, then copying the block into `styles.css`.
5. **Check it visually** against `references/gallery.html` (open it in a browser —
   it is the approved look in both themes; its class names are scaffolding, not
   API) and in the running console (`mise run //crates/e2e:ui-dev-stack`, then the
   backend + `mise run ui-dev`; see `crates/ui/web/README.md`).

## Choosing quickly

| You need to show…                        | Use                                                             |
| ---------------------------------------- | --------------------------------------------------------------- |
| a state (health, phase, a failing fact)  | status pill (`HealthBadge` / `LampBadge`)                       |
| which kind an object is                  | kind mark: stripe + chip + kind name                            |
| one object pointing at another           | object reference (mini card with the target's pill)             |
| many objects of one kind                 | table on a card, stripe on the first cell                       |
| objects of mixed kinds that need someone | attention row (one per object, the fix on its plate)            |
| what has run lately, across kinds        | activity row (one per run, newest first)                        |
| two regions whose widths trade off       | split pane (8px gutter, three-dot grip, stacks when narrow)     |
| a handful of facts about one object      | stat strip                                                      |
| a missing value                          | the right absence: loud _never …_, faint _not reported_, or `—` |
| a problem and how to fix it              | finding (what / why / FIX plate)                                |
| an action                                | button in the action row → inline confirm → receipt             |
| something the user may not do            | the button, `aria-disabled`, with the reason beside it          |
| a trend                                  | one-series line chart with a table twin                         |
| nothing / loading / failed / forbidden   | the matching page state (skeleton, never a spinner)             |

## Checks to run

```bash
python3 .claude/skills/kopiur-ui-design/scripts/check-contrast.py   # every token pair, both themes
mise run ui-check     # tsc + eslint + format + vitest (incl. styles.health / styles.focus ratchets) + types drift
mise run ui-build     # bundle budget (entry ≤ 480 kB)
```

## References

- `references/rules.md` — the non-negotiable behaviour rules and their enforcing tests
- `references/tokens.md` — the token block (single source) and measured contrast
- `references/kinds.md` — the nine kinds: identity, status source, card facts, relationships
- `references/primitives.md` — status pill, kind mark, reference, button, stat strip, time, attention row, split pane, table, side panel, tabs
- `references/composites.md` — overview, resource drawer, finding, action/confirm/receipt, page states, charts, derived screens
- `references/shell.md` — sidebar, namespace switcher, search, nav, user chip, responsive
- `references/gallery.html` — the approved mockups, light and dark
- `scripts/check-contrast.py` — WCAG check over `tokens.md`
