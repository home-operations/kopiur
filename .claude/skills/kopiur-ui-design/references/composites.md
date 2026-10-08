# Composites

Page-level patterns built from `primitives.md`. Spacing between regions is
`--space-5`; a region's title is `--text-xs` label caps in `--fg-muted` with an
optional count ("Needs attention · 3").

---

## Overview

The first screen. Answers "is my data safe?" in this order:

1. **Page title** (`Overview`) with the scope beside it in mono
   (`all namespaces · as of now`).
2. **Verdict line** — the worst state as a status pill plus one sentence naming the
   object and the cause ("1 of 3 repositories is failing — `media/nas-offsite`
   cannot reach its bucket."). `components/verdict.ts` decides it. Never healthy
   while any source is loading or refused, or the scope is empty.
3. **Fleet by kind** — a grid of kind tiles, four across on desktop:
   Repositories, Policies, Schedules, Snapshots · 24h, Restores, Replications,
   Maintenance. Each tile: stripe, chip, label, count (`--text-2xl`), a segmented
   status bar, and the breakdown in words with icons ("1 failed · 2 ok"). A tile
   with any failure gets a 1.5px inset ring in `--health-failed-fg` at 45%. Each
   tile links to its list page (filtered to the failing state when it has one).
4. **Needs attention · N** — the objects that need someone, as object cards with
   their stat strips, three across, worst first. Empty state: "Nothing needs you"
   with the doctor link.

The status bar is decoration over the words beside it: it is `aria-hidden`, and
the breakdown is the text. Segment colours are the `--health-*-fg` tokens; a zero
segment is not drawn.

```css
.kind-tile {
  display: grid;
  gap: 9px;
  padding: 12px 14px;
  border-radius: var(--radius-lg);
  background: var(--bg-surface);
  box-shadow: var(--shadow-1);
  border: 1px solid var(--card-border);
  border-left: var(--stripe) solid var(--kc);
  color: inherit;
  text-decoration: none;
}
.kind-tile[data-failing] {
  box-shadow:
    var(--shadow-1),
    inset 0 0 0 1.5px color-mix(in srgb, var(--health-failed-fg) 45%, transparent);
}
.kind-tile__count {
  margin-left: auto;
  font-size: var(--text-2xl);
  font-weight: var(--weight-bold);
}
.status-bar {
  display: flex;
  gap: 3px;
  height: 6px;
}
.status-bar > span {
  border-radius: 3px;
}
```

---

## Relationships (flow lanes)

On a repository's (and policy's) detail page: what feeds it and what it feeds,
left to right — **Fired by → Written by → This repository → Copies to** — as
lanes of summary cards with an arrow between lanes. The centre card is the
object itself with its stat strip. An empty lane says why in a muted line
("Nothing copies this repository."). A gap that is a problem is loud
("No schedule fires `app-data-weekly`" as a loud absence under the policy card).

Lanes collapse to a vertical sequence below 900px (arrows rotate to point down).
The lanes are also the page's spoken account of the relationships, so every card
is a real link or reference — nothing is drawn-only.

The cross-cluster **Topology** screen keeps its graph board; its plates are
summary object cards (232px) and every edge keeps a word label for its kind and,
when unhealthy, its state. The board stays `aria-hidden` with the edge list as its
spoken twin.

---

## Finding (what / why / fix)

A problem explained: request errors (the `Problem` body), failing doctor checks,
fired gates, a snapshot's failure.

```html
<article class="finding" data-health="failed">
  <span class="finding__icon" aria-hidden="true"><svg /></span>
  <div class="finding__body">
    <p class="finding__what">Repository <code>media/nas-offsite</code> cannot reach its bucket.</p>
    <p class="finding__why">kopia returned <code>403 AccessDenied</code> …</p>
    <p class="finding__fix"><span class="finding__fix-label">Fix</span>Give the keys in Secret …</p>
    <p class="finding__meta">repository-unreachable · 2026-10-07 22:41:03Z</p>
  </div>
</article>
```

Card with a 4px left edge in the state's `-fg` token, a 32px rounded icon tile on
the state's `-bg`, _what_ in semibold, _why_ in `--fg-muted`, the _fix_ on an
`--accent-soft` plate led by a "FIX" label in `--accent-ink` caps, metadata in
small mono. The text is always the server's own; never paraphrase it. A 403 uses
the degraded state and the `ShieldX` icon and offers no retry.

---

## Action → confirm → receipt

1. **Action row** — buttons in the detail hero (or a table cell's single trigger).
2. **Confirm** — opens **inline**, directly under the action row, inside the hero
   (never a modal or drawer). A panel on `--bg-canvas` with a hairline: a title
   ("Snapshot `app-data` now"), prose saying exactly what will be created and
   where, any **choice** fieldsets, then primary + quiet Cancel. Opening it moves
   focus in; Escape or Cancel returns focus to the trigger. Only one confirmation
   is open at a time. Inside a table, the cell holds only the trigger and the
   confirmation renders below the table.
3. **Choice** — for a field whose wrong value costs data (`pin`, `overwrite`): a
   fieldset of radio rows, each row the consequence in prose naming the wire field
   (`spec.pin: true`). **Neither starts selected**; the confirm button is blocked
   with the reason until one is chosen. The destructive answer gets a 2px
   `--health-failed-fg` left rule.
4. **Receipt** — replaces the confirm panel: a `--health-healthy-bg` card with a
   healthy-tinted border, a "Requested" pill, the server's `note` (never dropped),
   created objects as references, and the request instant in small mono. Actions
   that answer 202 say **requested**, never **done**. A live region.

---

## Page states

Left-aligned and compact, inside the region they replace: a 32px rounded icon
tile, a semibold title, a muted explanation, an optional action.

| State         | Icon                     | Title says                                                          | Action                                |
| ------------- | ------------------------ | ------------------------------------------------------------------- | ------------------------------------- |
| Empty         | `Inbox` on unknown-bg    | what would appear here                                              | how to make one (or none)             |
| Loading       | —                        | skeleton rows of the content to come + visually-hidden "Loading X…" | —                                     |
| Error         | `OctagonX` on failed-bg  | "Could not load X" + the Problem's what/why/fix                     | Retry                                 |
| Not permitted | `ShieldX` on degraded-bg | "You can't list X in ns" + the missing verb/resource                | **none** — retrying won't change RBAC |

Loading is always a skeleton (`.skeleton-ledger`, 1.4s sheen, off under reduced
motion), never a spinner. Two sections fed by one refused read say it once.

---

## Charts

One series per chart, small multiples for several. Card with a caption row (title
left, the latest value and anything excluded on the right — "1 run failed, not
plotted"), the SVG, then a `<details>` holding the same numbers as a table.

- Line in `--accent` at 2.2px with 3px dots (surface fill, accent stroke) over
  invisible 9px hit targets; area beneath at `--accent` 16%.
- Three recessive gridlines in `--line`, zero-based y, only first and last x labels.
- The point under the pointer is the only emphasis; the readout sits in the caption
  row, never floating over the plot.
- A value the operator never recorded is excluded and counted, never plotted at
  zero. A failed run is not a point.
- The SVG is `aria-hidden`; the table is its spoken twin.

---

## Derived screens (no separate mockup)

| Screen / part  | Built from                                                                                                                                                                                                   |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Retention plan | verdict line + one table per bucket; the subject row gets `--accent-soft` and the words "this snapshot"; pruned rows use the unknown pill (pruning is the policy working); the holding rule is a text column |
| Doctor         | verdict line + a table of checks (outcome pill, check, scope, finding)                                                                                                                                       |
| Conditions     | a table: type, status, reason, message, since                                                                                                                                                                |
| Gates          | findings (one per fired gate); the registry page is a table                                                                                                                                                  |
| File browser   | breadcrumbs + table (entry, kind, size, modified, mode, download) + session bar as a finding-style panel                                                                                                     |
| Log tail       | mono block on `--bg-inset`, `--radius-md`, max 22rem, scrolls, never wraps; empty is a sentence                                                                                                              |
| Lineage        | the trail pattern, one hop up and one down                                                                                                                                                                   |
| Topology       | graph board with summary-card plates (see Relationships)                                                                                                                                                     |
