# Primitives

The atoms and per-object building blocks. Every screen is assembled from these;
if a screen needs something this file does not have, add it here first (with the
same sections) rather than styling one-off markup in a route.

Markup is given as the HTML the component renders, CSS as the rules that belong
in `styles.css`. Class names follow the SPA's existing `block__element` style.
Where a test already pins markup (`.health[data-health]`, `.finding__fix`,
`[data-state="not-permitted"]`, `.skeleton-ledger__row`, `.chart__plot`), the
markup below keeps it.

---

## Status pill

**What:** the one way a state is shown. Tinted pill, icon, word.

**Markup** (rendered by `HealthBadge` for a `Health`, `LampBadge` for anything
else):

```html
<span class="health" data-health="failed">
  <svg aria-hidden="true"><!-- OctagonX --></svg>Failed
</span>
```

```css
.health {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 2px 9px 2px 6px;
  border-radius: var(--radius-pill);
  font-size: var(--text-sm);
  font-weight: var(--weight-semibold);
  white-space: nowrap;
  background: var(--health-unknown-bg);
  color: var(--health-unknown-fg);
}
.health svg {
  width: 13px;
  height: 13px;
  stroke-width: 2;
  flex: none;
}
.health[data-health="healthy"] {
  background: var(--health-healthy-bg);
  color: var(--health-healthy-fg);
}
.health[data-health="failed"] {
  background: var(--health-failed-bg);
  color: var(--health-failed-fg);
}
.health[data-health="degraded"] {
  background: var(--health-degraded-bg);
  color: var(--health-degraded-fg);
}
.health[data-health="pending"] {
  background: var(--health-pending-bg);
  color: var(--health-pending-fg);
}
.health[data-health="suspended"] {
  background: var(--health-suspended-bg);
  color: var(--health-suspended-fg);
}
```

**States:** the six `data-health` values. The word is the object's own phase
("Succeeded", "Restoring") or a fact ("2 failed runs", "never verified") — see
`kinds.md` → Status source for the mapping. An unrecognised phase renders the
**raw word** on the unknown style; it never falls through to healthy.

**A11y:** the word is real text. If a count replaces the word (a tile's "3"), keep
the state word as visually-hidden text ("3 (Failed)").

**Don't:** draw a coloured dot, colour a row, or put a status hue on anything
that is not a `.health` pill, a loud absence (below) or an icon beside one.

---

## Kind mark

**What:** how an object's kind is attached to it. Three parts, used together:

- **Stripe** — a `--stripe` (4px) left edge on the card, table row's first cell,
  detail hero, tree node, attention item.
- **Chip** — a tinted rounded square with the kind's glyph. 28px on cards and
  headers (40px in the detail hero), 22px in tables and references.
- **Name** — the CRD kind in small caps above the object's name, on cards,
  headers and references (not in table cells, where the column says it).

```html
<li class="attention-row" data-kind="snapshot-policy">
  <span class="kind-chip" aria-hidden="true"
    ><svg><!-- ScrollText --></svg></span
  >
  <span class="object-id">
    <span class="kind-name">SnapshotPolicy</span>
    <code class="object-name">app-data</code>
    <span class="object-ns">kopiur-dev</span>
  </span>
  …
</article>
```

```css
[data-kind="repository"] {
  --kc: var(--kind-repository);
  --kc-bg: var(--kind-repository-bg);
}
[data-kind="cluster-repository"] {
  --kc: var(--kind-cluster-repository);
  --kc-bg: var(--kind-cluster-repository-bg);
}
[data-kind="maintenance"] {
  --kc: var(--kind-maintenance);
  --kc-bg: var(--kind-maintenance-bg);
}
[data-kind="snapshot-policy"] {
  --kc: var(--kind-snapshot-policy);
  --kc-bg: var(--kind-snapshot-policy-bg);
}
[data-kind="snapshot-schedule"] {
  --kc: var(--kind-snapshot-schedule);
  --kc-bg: var(--kind-snapshot-schedule-bg);
}
[data-kind="snapshot"] {
  --kc: var(--kind-snapshot);
  --kc-bg: var(--kind-snapshot-bg);
}
[data-kind="restore"] {
  --kc: var(--kind-restore);
  --kc-bg: var(--kind-restore-bg);
}
[data-kind="repository-replication"] {
  --kc: var(--kind-repository-replication);
  --kc-bg: var(--kind-repository-replication-bg);
}
[data-kind="snapshot-replication"] {
  --kc: var(--kind-snapshot-replication);
  --kc-bg: var(--kind-snapshot-replication-bg);
}

.kind-chip {
  display: inline-grid;
  place-items: center;
  flex: none;
  width: 28px;
  height: 28px;
  border-radius: var(--radius-md);
  background: var(--kc-bg);
  color: var(--kc);
}
.kind-chip svg {
  width: 14px;
  height: 14px;
}
.kind-chip--sm {
  width: 22px;
  height: 22px;
  border-radius: var(--radius-sm);
}
.kind-chip--sm svg {
  width: 12px;
  height: 12px;
}
.kind-chip--lg {
  width: 40px;
  height: 40px;
  border-radius: 12px;
}
.kind-chip--lg svg {
  width: 20px;
  height: 20px;
}
.kind-name {
  font-size: 0.625rem;
  font-weight: var(--weight-bold);
  letter-spacing: var(--tracking-caps);
  text-transform: uppercase;
  color: var(--kc);
}
.has-stripe {
  border-left: var(--stripe) solid var(--kc);
}
td.has-stripe {
  border-left: 0;
  box-shadow: inset var(--stripe) 0 0 var(--kc);
}
```

**Rules:** the chip is square, the status pill is round — the shape tells you
which you are looking at before the colour does. A kind colour never marks a
state, and a status colour never marks a kind. The chip is `aria-hidden`; the
kind is said by the kind name (or the column header in a table).

---

## Object reference

**What:** one object naming another ("writes into", "fired by", a table cell's
Policy column). A mini card: chip, kind name over the name, the target's live
status pill.

```html
<a class="ref" data-kind="repository" href="/repositories/repository/dev-repo?namespace=kopiur-dev">
  <span class="kind-chip kind-chip--sm" aria-hidden="true"><svg /></span>
  <span class="ref__id"
    ><span class="kind-name">Repository</span><code class="ref__name">dev-repo</code></span
  >
  <span class="health" data-health="healthy"><svg aria-hidden="true" />Healthy</span>
</a>
```

```css
.ref {
  display: inline-grid;
  grid-template-columns: auto auto auto;
  align-items: center;
  gap: 8px;
  padding: 6px 10px 6px 6px;
  border-radius: 12px;
  vertical-align: middle;
  border: 1px solid var(--line);
  background: var(--bg-surface);
  color: inherit;
  text-decoration: none;
}
.ref:hover {
  background: var(--bg-hover);
}
.ref__id {
  display: grid;
  line-height: 1.2;
}
.ref__name {
  font-family: var(--font-mono);
  font-weight: var(--weight-semibold);
  font-size: var(--text-sm);
}
.ref .health {
  font-size: var(--text-xs);
}
```

**Variants:** show `namespace/` before the name only when it differs from the
surrounding context. Every reference is a link that opens its target in the
resource drawer, whatever its kind. A
dangling reference (graph `missing`, a policy naming a repository that does not
exist) gets a dashed border and the unknown pill reading "not found".

**Wire:** the target's name/namespace come from the referring row; its status
needs the target's row. If the target's status is not loaded, omit the pill —
never guess one.

---

## Button

```html
<button class="button button--primary"><svg aria-hidden="true" />Snapshot now</button>
<button class="button">Run maintenance</button>
<!-- soft -->
<button class="button button--danger"><svg />Suspend</button>
<span class="button-row">
  <button class="button button--danger" aria-disabled="true" aria-describedby="r1">Delete</button>
  <span class="button__reason" id="r1"
    ><svg aria-hidden="true" />Not permitted: your account can't <code>delete</code> snapshots in
    <code>kopiur-dev</code>.</span
  >
</span>
```

```css
.button {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  height: 34px;
  padding: 0 14px;
  border-radius: var(--radius-md);
  border: 1px solid transparent;
  background: var(--accent-soft);
  color: var(--accent-ink);
  font: var(--weight-semibold) var(--text-sm) / 1 var(--font-sans);
  cursor: pointer;
  transition: background var(--dur-fast) var(--ease-out);
}
.button svg {
  width: 14px;
  height: 14px;
}
.button--primary {
  background: var(--accent);
  color: var(--fg-on-accent);
}
.button--danger {
  background: var(--health-failed-bg);
  color: var(--health-failed-fg);
}
.button--quiet {
  background: transparent;
  color: var(--accent-ink);
}
.button[aria-disabled="true"] {
  background: transparent;
  color: var(--fg-muted);
  border: 1px dashed var(--line-strong);
  cursor: not-allowed;
}
.button__reason {
  display: inline-flex;
  gap: 5px;
  align-items: center;
  color: var(--fg-muted);
  font-size: var(--text-sm);
}
```

**Two refusals:** `disabledReason` = "you may not" (lock icon, "Not permitted: …",
from `/me` via `capabilityReason`). `blockedReason` = "answer something first"
(no icon, e.g. "Choose whether to pin it first."). Both are always visible beside
the button and on `aria-describedby`. Inside a table cell, show the short form
("not permitted") with the full sentence on `title` and `aria-describedby`.
Never hide a button the user may not use. Disabled is `aria-disabled`, still
focusable, never `disabled`.

One primary per region. Danger is for actions that stop backups or delete data.

---

## Stat strip (facts)

**What:** a handful of named values about one object. One card, cells divided by
hairlines, label above value. Four per row at the top of the drawer, three in a
compact card.

```html
<dl class="stats">
  <div class="stats__item">
    <dt>Size</dt>
    <dd>2.0 MiB</dd>
  </div>
  <div class="stats__item">
    <dt>Started</dt>
    <dd>58m ago<time class="stats__abs">2026-10-07 15:32:38</time></dd>
  </div>
  <div class="stats__item">
    <dt>Last verified</dt>
    <dd class="absent absent--loud"><svg aria-hidden="true" />never verified</dd>
  </div>
  <div class="stats__item">
    <dt>New bytes</dt>
    <dd><abbr class="absent absent--unreported" title="…">not reported</abbr></dd>
  </div>
  <div class="stats__item">
    <dt>Files failed</dt>
    <dd class="absent">—</dd>
  </div>
</dl>
```

```css
.stats {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  margin: 0;
  background: var(--bg-surface);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-1);
  border: 1px solid var(--card-border);
  overflow: hidden;
}
.stats__item {
  display: grid;
  gap: 5px;
  padding: 12px 16px;
  border-right: 1px solid var(--line);
  border-bottom: 1px solid var(--line);
}
.stats dt {
  font-size: var(--text-xs);
  font-weight: var(--weight-semibold);
  letter-spacing: var(--tracking-caps);
  text-transform: uppercase;
  color: var(--fg-muted);
}
.stats dd {
  margin: 0;
  font-size: var(--text-lg);
  font-weight: var(--weight-semibold);
  font-variant-numeric: tabular-nums;
}
.stats__abs {
  display: block;
  font: var(--text-xs) var(--font-mono);
  color: var(--fg-muted);
  font-weight: var(--weight-regular);
}
```

**The three absences** (never a blank, never a bare `0`):

| Absence              | Means                                                                  | Style                                                                           |
| -------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `absent--loud`       | a real problem: "never verified", "never succeeded", "never run"       | `--health-failed-fg`, semibold, `OctagonX` icon                                 |
| `absent--unreported` | the CRD declares it, no controller writes it (`components/unwired.ts`) | italic `--fg-muted`, dotted underline, reason on `title` + visually-hidden text |
| `absent`             | does not apply to this object                                          | `—` in `--fg-muted`                                                             |

An identifier never goes in a `<dt>` (it is set in caps, which changes the name).
A fact with no value that is none of the three is **dropped**, not rendered.

---

## Time

Relative first ("58m ago", "in 39m"), in tabular figures. In the drawer and in
stat strips the absolute instant sits beneath in small mono (`.stats__abs`). In
tables, the absolute instant is the cell's `title` and a `<time datetime>`.

---

## Attention row

**What:** one object that needs someone, wherever objects of mixed kinds are
listed by what is wrong with them (the overview's "Needs attention"). One row
per object, however many reads noticed it (`components/attention.ts`).

Anatomy: **first line** — chip · kind name / mono name (opens the resource
drawer) / namespace, and on the right the status pill whose word says what is
wrong ("Stuck", "Failed", "Never succeeded" — never the object's own phase or
suspend state) with the relative time it went wrong when known. **Body**, under
the name — the problem in the operator's own words (`.attention-row__what`) and
the fix on the finding's FIX plate. The operator's `Fix:` clause is lifted out
of its message onto the plate. Stripe on the left edge. A failing doctor check
that is about no object takes the same row with a neutral icon tile, "Doctor
check" for the kind and the check's title linking to the doctor page.

```html
<li class="attention-row has-stripe" data-kind="snapshot" data-health="failed">
  <span class="kind-chip" aria-hidden="true"><svg /></span>
  <span class="object-id">
    <span class="kind-name">Snapshot</span>
    <a class="object-name" href="?inspect=snapshot/billing/ledger-manual">ledger-manual</a>
    <span class="object-ns">billing</span>
  </span>
  <span class="attention-row__state">
    <span class="health" data-health="failed"><svg aria-hidden="true" />Stuck</span>
  </span>
  <div class="attention-row__body">
    <p class="attention-row__what">blocked on MoverPermitted=False …</p>
    <p class="finding__fix"><span class="finding__fix-label">Fix</span>…</p>
  </div>
</li>
```

Grid `auto minmax(0, 1fr) auto` with areas `"chip id state" ". body body"`;
below 560px the pill drops under the name. At most `ATTENTION_MAX` rows, then
"And N more" pointing at the doctor report and the lists.
---

## Table (list pages)

**What:** every list page. Table on a card; the kind stripe and small chip on the
first cell; related objects as references; numbers right-aligned.

```html
<div class="table-wrap">
  <table class="table">
    <thead>
      <tr>
        <th scope="col">Snapshot</th>
        <th scope="col">Phase</th>
        <th scope="col">Policy</th>
        <th scope="col" class="num">Size</th>
        <th scope="col" class="num">Age</th>
      </tr>
    </thead>
    <tbody>
      <tr data-kind="snapshot">
        <td class="has-stripe">
          <span class="table__object"
            ><span class="kind-chip kind-chip--sm" aria-hidden="true"><svg /></span>
            <span
              ><a href="…"><code>app-data-manual</code></a
              ><span class="object-ns">kopiur-dev</span></span
            ></span
          >
        </td>
        <td>
          <span class="health" data-health="healthy"><svg aria-hidden="true" />Succeeded</span>
        </td>
        <td><a class="ref" data-kind="snapshot-policy">…</a></td>
        <td class="num">2.0 MiB</td>
        <td class="num"><time datetime="…" title="2026-10-07 15:32:38">58m</time></td>
      </tr>
    </tbody>
  </table>
</div>
```

```css
.table-wrap {
  overflow-x: auto;
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-1);
  border: 1px solid var(--card-border);
  background: var(--bg-surface);
}
.table {
  width: 100%;
  border-collapse: separate;
  border-spacing: 0;
  font-size: var(--text-sm);
}
.table th {
  text-align: left;
  padding: 9px 14px;
  border-bottom: 1px solid var(--line);
  font-size: var(--text-xs);
  font-weight: var(--weight-semibold);
  letter-spacing: var(--tracking-caps);
  text-transform: uppercase;
  color: var(--fg-muted);
}
.table td {
  padding: 11px 14px;
  border-bottom: 1px solid var(--line);
  vertical-align: middle;
}
.table tbody tr:last-child td {
  border-bottom: 0;
}
.table tbody tr:hover td {
  background: var(--bg-hover);
}
.table .num {
  text-align: right;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
.table__object {
  display: flex;
  align-items: center;
  gap: 10px;
}
```

Rows stay in server order unless the page offers sorting. A wide table scrolls
inside `.table-wrap` rather than starving a column. Long names wrap at `/` and `-`
(`overflow-wrap: anywhere` on `code`), never ellipsised in a table.

---

## Side panel (the resource drawer)

**What:** a resource's details, floating over the right edge of the page. Every
list row, reference, attention row and search result opens one; it is how a
resource is looked at without leaving the page you are on.

```html
<dialog class="side-panel has-stripe" data-kind="snapshot-policy" aria-labelledby="t">
  <div class="side-panel__frame">
    <header class="side-panel__head">
      <span class="kind-chip kind-chip--md" data-kind="snapshot-policy" aria-hidden="true"
        ><svg
      /></span>
      <div class="side-panel__heading">
        <span class="kind-name">SnapshotPolicy</span>
        <h2 class="side-panel__title" id="t">app-data</h2>
        <div class="side-panel__status"><span class="health" data-health="healthy">…</span></div>
      </div>
      <button class="button button--quiet" aria-label="Close details">×</button>
    </header>
    <div class="side-panel__body">…stat strip, facts, related references…</div>
    <footer class="side-panel__foot">
      <div class="drawer-actions"><div class="action-bar">…</div></div>
    </footer>
  </div>
</dialog>
```

- A native modal `<dialog>` (`showModal()`): top layer, `::backdrop` on `--scrim`,
  the page behind is inert. No z-index is ever needed to stack it.
- Inset `--space-3` from the top, right and bottom; `--radius-xl`, `--shadow-2`,
  kind stripe on the left edge. Full-screen below 560px.
- **Resizable.** A `role="separator"` handle (`.side-panel__grip`) hangs just outside
  the left edge: a full-height grab strip carrying a raised rectangular tab with
  three vertical dots, always shown, standing clear of the panel
  (`aria-orientation="vertical"`, `aria-valuenow` in px). Drag it, or focus it and
  use ←/→ (24px), Home/End. It opens at 50% of the window (`--drawer-width`),
  never wider than 85% (`--drawer-max`) nor narrower than `--drawer-min`, and the
  chosen width is remembered in `localStorage` (`kopiur-ui.drawer-width`) for every
  later panel, clamped to the window it reopens in. Hidden on phones.
- Slides in from the right over `--dur-base`, and back out when it closes: the
  caller keeps it mounted while `leaving` (`useExiting`) and unmounts on
  `onExited`. Reduced motion collapses both, and it goes at once.
- The head (chip, kind name, mono name, pill, close) and the foot stay put.
- **Resource drawer anatomy** (`layout="fill"`): under the panel head, the
  resource's **main information** (`.drawer__head`: the verdict as a lettered
  lamp and one sentence, then the **chain** — what feeds it → this → what it
  feeds — then three or four headline stats, then any gate or failure finding);
  then **tabs** holding everything else, whose panel is the scroller; then the
  **actions** in the foot — a row of buttons with an open inline confirmation and
  any receipt stacked above it. A kind with one tab draws no tab list. A long name wraps in the title, never widens the panel.
- **The open resource lives in the URL** — `?inspect=<kind-slug>/<namespace>/<name>`
  (`cluster-repository/<name>` for the cluster-scoped kind). Opening pushes one
  history entry, so Back closes it; opening another resource from inside replaces
  that entry; a deep link opens it.
- Focus moves into the panel on open and returns to the row or reference that
  opened it. Escape, the close button and a backdrop click close it. An inline
  confirmation inside the panel owns Escape first.
- For the kinds on the topology board (Repository, ClusterRepository,
  SnapshotPolicy) the drawer also carries what the board knows: a
  **Relationships** tab (every link both ways, with the link's own pill and
  label; backends, admitted namespaces and selectors named as what they are),
  loud findings for "nothing copies X anywhere" and for references to things
  that do not exist, and the kind's one-line meaning. Opened on a ghost (a
  referenced resource that does not exist), it explains the ghost and names
  what refers to it.

---

## Tabs

**What:** a row of names over one panel — the resource drawer's way of holding
everything that is not the resource's main information.

```html
<div class="tabs">
  <div class="tabs__list" role="tablist" aria-label="About this repository">
    <button role="tab" id="t-storage" aria-selected="true" aria-controls="p-storage" tabindex="0">
      Storage
    </button>
    <button role="tab" id="t-catalog" aria-selected="false" aria-controls="p-catalog" tabindex="-1">
      Catalog <span class="tabs__count">3</span>
    </button>
  </div>
  <div class="tabs__panel" role="tabpanel" id="p-storage" aria-labelledby="t-storage" tabindex="0">
    …
  </div>
</div>
```

- One tab stop: the selected tab is in the tab order; ←/→ move and select
  (wrapping), Home/End jump. Only the selected panel is mounted, so a tab that
  reads something reads it when opened, not before.
- 40px tabs, `--text-sm` semibold in `--fg-muted`; the selected one is `--fg`
  with a 2.5px `--accent` underline sitting on the list's hairline (an inset
  shadow, not a border). The list scrolls sideways rather than wrapping.
- A count sits beside a tab's name as a small inset pill, and is read with it.
- A tab id the list does not have selects the first tab.
