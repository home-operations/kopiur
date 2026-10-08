# Shell

Sidebar only. **There is no top bar**: the page title lives in the content.

```
┌───────────────┬──────────────────────────────────────────────┐
│ ▣ kopiur      │ Overview  all namespaces · as of now          │
│ [* all ns   ▾]│                                              │
│ [⌕ Find… /  ] │  (page content, max --content-max)           │
│ ⌂ Overview    │                                              │
│ ⧉ Topology    │                                              │
│ STORAGE       │                                              │
│ ▤ Repositories│                                              │
│ ⚒ Maintenance │                                              │
│ ⇄ Replications│                                              │
│ PROTECTION    │                                              │
│ ▦ Policies    │                                              │
│ ◷ Schedules   │                                              │
│ DATA          │                                              │
│ ◉ Snapshots   │                                              │
│ ▣ Restores    │                                              │
│ ───────────   │                                              │
│ ♡ Doctor      │                                              │
│ (k) kopiur-dev│                                              │
│     anonymous ▴                                              │
└───────────────┴──────────────────────────────────────────────┘
```

## Sidebar

`--sidebar-width` (240px), `--bg-surface`, a `--line` right edge, sticky full
height, `padding: 14px 12px`. Top to bottom:

1. **Brand** — 26px accent mark + "kopiur".
2. **Namespace switcher** — a full-width button: a 24px avatar tile (`*` for all
   namespaces, else the namespace's first letter on `--accent-soft`), the scope in
   mono over "namespace scope", a chevron. Opens a popover (`--shadow-2`,
   `--radius-lg`) with a **filter field** first, then "all namespaces" and every
   namespace with its object count on the right; the current one on
   `--accent-soft`. Choosing one sets the shared `?namespace=` search param.
   Capabilities (`/me`) are re-fetched on every change.
3. **Object search** — full-width field "Find an object…" with a `/` key hint.
   Searches names across kinds; results are object cards in summary form.
4. **Navigation** — Overview, Topology; group label **Storage**: Repositories,
   Maintenance, Replications; **Protection**: Policies, Schedules; **Data**:
   Snapshots, Restores; a hairline; Doctor. Group labels are `--text-xs` caps in
   `--fg-muted`. Each item: a 26px chip (the kind's tint and glyph for kind
   sections; neutral for Overview/Topology/Doctor) + label, `--radius-md`. The
   current item: `--accent-soft` fill, `--accent-ink` text, `aria-current="page"`.
   Gates stays off-nav (reached from Doctor).
5. **User chip** (pinned to the bottom, `margin-top: auto`) — compact: 28px avatar
   tile (initial on `--accent-soft`), username (semibold) over **role** (the
   identity source — "anonymous", or the trusted-headers group the proxy sent),
   chevron. Clicking opens a popover **upward** with:
   - avatar, user, "signed in as <source> · groups: …", email if any;
   - "In <scope> you can · N of 13", the capability list in two columns
     (`Check` in healthy-fg / `X` in failed-fg with the label struck through,
     plus visually-hidden "permitted"/"not permitted");
   - **Theme**: System / Light / Dark segmented switch (`aria-pressed`).
     Loading shows a skeleton; a failed `/me` reads "identity unavailable", never
     "not permitted".

Only one popover is open at a time; Escape closes it and returns focus to its
button.

## Content area

`--bg-canvas`, padded `--space-5`, capped at `--content-max`. Every page starts
with the **page title** (`--text-xl`, bold) and the scope in mono beside it, then
the page's own content. A resource's details are never a page: they open in the
side panel over the page (`composites.md` → Resource drawer).

A global problem banner (a request error not tied to a region) is a finding that
spans the content width above the page title.

## Responsive

Below 900px the sidebar becomes an off-canvas drawer opened from a compact bar at
the top of the content (brand + menu button + the namespace button); the drawer
holds the same five parts. Type sizes never scale with the viewport. Card grids
step 4 → 2 → 1 columns; stat strips go 4 → 2 columns; the side panel fills the
screen below 560px.

## Skip link

The first focusable element is "Skip to content", moving focus to `#main`.
