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
3. **Two cards side by side** in a split pane (`primitives.md`), an 8px gutter
   between them holding a three-dot grip; drag it or use the arrow keys, and the
   share is remembered. When the pane is too narrow for both (each keeps 440px),
   they stack, **Needs attention first**, and the grip goes.
   - **Needs attention · N** (start) — one list of attention rows
     (`primitives.md`), worst first: every object that needs someone exactly
     once, whether the doctor named it, the status report lists it as stalled, or
     its kind's list shows it unhealthy — the doctor's account wins, then the
     stalled message, then what the list row says. Failing doctor checks about no
     object follow as rows of the same shape. Never a second or third style beside
     it (no stalled table, no finding cards). Empty state: "Nothing needs you"
     with the doctor link.
   - **Recent activity** (end) — every run, newest first, of every kind: each
     snapshot and restore, each replication's last pass, each maintenance
     object's last quick and full run (`components/activity.ts`). A compact row
     per run: chip, kind, name (the whole row opens the drawer), the outcome pill
     in the run's own word, when, and what ran ("photos → nas · 2.0 KiB · took
     30s"). Something that never ran is left out. A refused read is named in
     place.

Both lists run the length of the page; nothing is held back behind a count and
nothing scrolls on its own.

The fleet's counts by kind are not on this page: each sidebar section carries
its own (`shell.md`).

---

## Resource drawer (a resource's details)

There are no detail pages. Every resource — whatever its kind — is looked at in
the side panel (`primitives.md` → Side panel), over the page it was opened from,
in three bands:

1. **Main information** (`.drawer__head`): the verdict (a lettered lamp and one
   sentence; never healthy while anything it reads is loading or refused), then
   the **chain** — what feeds it → _this_ → what it feeds, each step a list of
   references with their own pills (`kinds.md` → Relationships) — then three or
   four headline stats, then any gate holding it or the failure that ended it.
2. **Tabs** for everything else, one tab per question (Storage, Catalog,
   Maintenance, Sessions, Conditions for a repository; Backs up, Retention,
   Verification, Snapshots, Conditions for a policy; Run, Storage, Retention,
   Lineage, Log, Conditions for a snapshot; Source, Target, Log, Conditions for a
   restore). The panel is the scroller; a tab that reads something (a snapshot's
   retention plan) reads it when opened. A kind with one tab draws no tab list.
3. **Actions** in the foot: the kind's controls in a row, an open inline
   confirmation and any receipt stacked above them. The buttons never scroll out
   of view; a tall confirmation scrolls inside itself. A kind with nothing to act
   on (a Restore) has no foot.

Old detail addresses (`/policies/<ns>/<name>` and the like) redirect to the
kind's list with the drawer open on the same object. The snapshot file browser
stays a page of its own (a workspace, not a detail); the snapshot drawer's
"Browse files" opens it, and its "Back to <name>" returns to the drawer.

The cross-cluster **Topology** screen keeps its graph board; its plates are
summary object cards (232px) and every edge keeps a word label for its kind and,
when unhealthy, its state. The board has no drawer of its own: a resource plate
is a link that opens the resource drawer over the board, and a backend,
namespace or selector plate is inert (it is not a resource; it appears in the
Relationships tab of whatever points at it). The board stays `aria-hidden`
behind its links, with the edge list as its spoken twin.

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
