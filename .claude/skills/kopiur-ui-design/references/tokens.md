# Tokens

The CSS block below is the **single source** for every colour, size, radius,
shadow and motion value in the console. `crates/ui/web/src/styles.css` adopts it
verbatim at the top of the sheet; `scripts/check-contrast.py` parses it. Change a
value here first, re-run the script, then copy the block.

Rules that shape the block:

- **One declaration per colour.** Every colour is `light-dark(light, dark)`. There
  is no dark block to keep in sync. `light-dark()` only ever takes colours (hex,
  `rgb()`, `var()`, `transparent`) — `styles.focus.test.ts` enforces this, which is
  why the kind chip tints are explicit hex rather than a per-theme percentage.
- **Shadows are not colours.** `--shadow-1`/`--shadow-2` are offsets that _use_
  `--shadow-ink-*`; they never start with `light-dark(` (also test-enforced).
- **Names are stable.** `--fg`, `--bg-*`, `--line*`, `--accent*`, `--health-*`,
  `--focus-ring` keep the names the SPA and its tests already use. New families:
  `--accent-ink`, `--kind-*`, `--card-border`, `--radius-lg`/`--radius-pill`.
- **The page follows the OS** (`color-scheme: light dark`) until the user picks a
  theme, which sets `data-theme="light"|"dark"` on `<html>`.

```css
:root {
  color-scheme: light dark;

  /* ---- type: the system stack, no web fonts ---- */
  --font-sans: ui-sans-serif, system-ui, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  --font-mono: ui-monospace, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
  --text-xs: 0.6875rem; /* 11px  label caps, kind name, table heads */
  --text-sm: 0.8125rem; /* 13px  table cells, controls, meta lines  */
  --text-md: 0.875rem; /* 14px  body                               */
  --text-lg: 1rem; /* 16px  verdict sentence, section titles   */
  --text-xl: 1.375rem; /* 22px  page title                         */
  --text-2xl: 1.5rem; /* 24px  the number on a kind tile          */
  --weight-regular: 400;
  --weight-medium: 500;
  --weight-semibold: 600;
  --weight-bold: 700;
  --leading-dense: 1.35;
  --leading-prose: 1.5;
  --tracking-caps: 0.08em;

  /* ---- space (4px base) ---- */
  --space-1: 0.25rem;
  --space-2: 0.5rem;
  --space-3: 0.75rem;
  --space-4: 1rem;
  --space-5: 1.5rem;
  --space-6: 2rem;
  --space-7: 3rem;

  /* ---- shape ---- */
  --radius-sm: 7px; /* small kind chip, focus ring on chips      */
  --radius-md: 10px; /* buttons, inputs, kind chip, nav items     */
  --radius-lg: 16px; /* cards, tables, panels, popovers           */
  --radius-pill: 999px; /* status pills only                         */
  --stripe: 4px; /* the kind stripe on a card / row / header  */

  /* ---- layout ---- */
  --sidebar-width: 240px;
  --content-max: 1440px;

  /* ---- motion ---- */
  --ease-out: cubic-bezier(0.16, 1, 0.3, 1);
  --dur-fast: 120ms;
  --dur-base: 180ms;

  /* ---- surfaces and ink ---- */
  --bg-canvas: light-dark(#eef1f7, #0a0e1a);
  --bg-surface: light-dark(#ffffff, #121829);
  --bg-inset: light-dark(#f5f7fb, #0f1424);
  --bg-hover: light-dark(rgb(11 16 32 / 0.04), rgb(232 236 248 / 0.05));
  --bg-active: light-dark(rgb(11 16 32 / 0.08), rgb(232 236 248 / 0.09));
  --fg: light-dark(#0b1020, #e8ecf8);
  --fg-muted: light-dark(#5d6681, #8b93ad);
  --fg-on-accent: light-dark(#ffffff, #0a0e1a);
  --line: light-dark(#e1e5ef, #1f2740);
  --line-strong: light-dark(#c9cfdd, #2c3554);
  --card-border: light-dark(transparent, #1f2740);

  /* ---- accent: selection, current item, focus, primary action — nothing else ---- */
  --accent: light-dark(#6c5ce7, #8b7dff);
  --accent-ink: light-dark(#5a49d6, #a99fff);
  --accent-soft: light-dark(#ece9fd, #221f45);
  --selection: light-dark(#d9d4fb, #34306b);

  /* ---- depth ---- */
  --shadow-ink-1: light-dark(rgb(11 16 32 / 0.08), transparent);
  --shadow-ink-2: light-dark(rgb(11 16 32 / 0.35), rgb(0 0 0 / 0.6));
  --shadow-1: 0 1px 3px var(--shadow-ink-1);
  --shadow-2: 0 16px 40px -12px var(--shadow-ink-2);
  --focus-ring: 0 0 0 2px var(--bg-surface), 0 0 0 4px var(--accent);

  /* ---- status: tinted pill = fg on bg; always with icon + word ---- */
  --health-healthy-fg: light-dark(#166534, #4ade80);
  --health-healthy-bg: light-dark(#dcf5e5, #10291c);
  --health-failed-fg: light-dark(#b91c1c, #fb8a8a);
  --health-failed-bg: light-dark(#fde2e2, #3a1518);
  --health-degraded-fg: light-dark(#8a5208, #fbbf24);
  --health-degraded-bg: light-dark(#fdefcf, #33270b);
  --health-pending-fg: light-dark(#1d4ed8, #7cb0ff);
  --health-pending-bg: light-dark(#e0eafd, #13233f);
  --health-unknown-fg: light-dark(#474f6b, #a3abc4);
  --health-unknown-bg: light-dark(#e8ebf3, #1e2438);
  --health-suspended-fg: var(--health-unknown-fg);
  --health-suspended-bg: var(--health-unknown-bg);

  /* ---- kinds: stripe, chip glyph and small-caps name; -bg is the chip tint ---- */
  --kind-repository: light-dark(#0e7490, #22d3ee);
  --kind-repository-bg: light-dark(#ddecef, #164154);
  --kind-cluster-repository: light-dark(#155e75, #67e8f9);
  --kind-cluster-repository-bg: light-dark(#dee8ec, #254657);
  --kind-maintenance: light-dark(#64748b, #94a3b8);
  --kind-maintenance-bg: light-dark(#e9ecef, #2f3748);
  --kind-snapshot-policy: light-dark(#c026d3, #e879f9);
  --kind-snapshot-policy-bg: light-dark(#f6e1f9, #412d57);
  --kind-snapshot-schedule: light-dark(#9333ea, #c084fc);
  --kind-snapshot-schedule-bg: light-dark(#f0e2fc, #383057);
  --kind-snapshot: light-dark(#c2410c, #fb923c);
  --kind-snapshot-bg: light-dark(#f6e4dd, #45332d);
  --kind-restore: light-dark(#db2777, #f472b6);
  --kind-restore-bg: light-dark(#fae1ec, #442c48);
  --kind-repository-replication: light-dark(#0f766e, #2dd4bf);
  --kind-repository-replication-bg: light-dark(#ddeceb, #18414a);
  --kind-snapshot-replication: light-dark(#4f46e5, #818cf8);
  --kind-snapshot-replication-bg: light-dark(#e6e5fb, #2a3257);

  /* ---- skeleton ---- */
  --skeleton-base: light-dark(#e1e5ef, #1f2740);
  --skeleton-sheen: light-dark(#f2f4f9, #2a3352);
}

:root[data-theme="light"] {
  color-scheme: light;
}
:root[data-theme="dark"] {
  color-scheme: dark;
}
```

## Contrast

Checked with `scripts/check-contrast.py` (WCAG 2.x relative luminance). Text pairs
need 4.5:1, non-text UI (focus ring, kind stripe, chip glyph) 3:1. Every pair below
passes in both themes; the script exits non-zero if one stops passing.

| Pair                                                         | Need | Light       | Dark        |
| ------------------------------------------------------------ | ---- | ----------- | ----------- |
| `--fg` on canvas / surface                                   | 4.5  | 16.7 / 18.9 | 16.3 / 15.0 |
| `--fg-muted` on canvas / surface                             | 4.5  | 5.0 / 5.7   | 6.3 / 5.8   |
| `--accent-ink` on canvas / surface                           | 4.5  | 5.5 / 6.3   | 8.3 / 7.7   |
| `--fg-on-accent` on `--accent` (primary button)              | 4.5  | 4.9         | 5.9         |
| `--accent-ink` on `--accent-soft` (soft button, current nav) | 4.5  | 5.3         | 6.7         |
| `--accent` focus ring on surface                             | 3    | 4.9         | 5.5         |
| each `--health-*-fg` on its `-bg` (pill), lowest             | 4.5  | 5.3         | 6.7         |
| `--health-failed-fg` as loud text on surface                 | 4.5  | 6.5         | 7.7         |
| each `--kind-*` as small-caps text on surface, lowest        | 4.5  | 4.6         | 5.9         |
| each `--kind-*` glyph on its `-bg` chip, lowest              | 3    | 3.7         | 4.2         |
| each `--kind-*` stripe on canvas, lowest                     | 3    | 4.1         | 6.5         |

The exact figures are whatever the script prints; this table is a summary, so
re-run the script rather than trusting it after a change.

Three light kind hues were stepped one shade darker on their own ramp to clear
4.5:1 as text (Repository, Snapshot, RepositoryReplication), and ClusterRepository
moved with Repository to stay distinct. Do not "brighten" them back for the stripe:
one value serves stripe, glyph and label.

## Spacing and density in use

| Where                    | Value                                                                     |
| ------------------------ | ------------------------------------------------------------------------- |
| Card padding             | `14px 16px`                                                               |
| Table head cell          | `9px 14px`, `--text-xs` caps, `--fg-muted`                                |
| Table body cell          | `11px 14px` (~44px row)                                                   |
| Gap between cards        | `--space-3` (12px)                                                        |
| Gap between page regions | `--space-5` (24px)                                                        |
| Button                   | 34px tall, `0 14px`, `--radius-md`, `--text-sm` semibold                  |
| Kind chip                | 28px (22px in tables and references), `--radius-md` (`--radius-sm` small) |
| Status pill              | `2px 9px 2px 6px`, `--radius-pill`, `--text-xs`–`--text-sm` semibold      |

Light mode lifts cards with `--shadow-1` and no border; dark mode has no shadow
and draws `--card-border`. Write both as `box-shadow: var(--shadow-1); border: 1px
solid var(--card-border);` and the tokens do the rest.

## Migrating from the old sheet

The previous "Vault Ledger" tokens this block drops, and what replaces them:

| Old                                     | New                                                                       |
| --------------------------------------- | ------------------------------------------------------------------------- |
| `--fg-faint`                            | `--fg-muted` (two ink strengths now, not three)                           |
| `--fg-disabled`                         | `--fg-muted` on a dashed `--line-strong` outline (see primitives: Button) |
| `--bg-rail`                             | `--bg-surface` (the sidebar is a surface)                                 |
| `--radius-md` 6px                       | `--radius-md` 10px; cards use `--radius-lg` 16px                          |
| `--rail-width` 232px, `--header-height` | `--sidebar-width` 240px; there is no header bar                           |
| `--text-lg` 1.0625rem                   | `--text-lg` 1rem; new `--text-2xl`                                        |
