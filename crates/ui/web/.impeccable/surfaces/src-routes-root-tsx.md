---
version: 1
slug: "src-routes-root-tsx"
primary_target: "src/routes/__root.tsx"
related_targets: ["src/styles.css", "src/components/AppShell.tsx"]
---

# kopiur-ui app shell — surface brief

Scope: the application shell (`src/routes/__root.tsx`, `src/components/AppShell.tsx`), the token sheet (`src/styles.css`) and the four state components. Visitor mode: **Operate**.

Audience and job: a platform/SRE engineer opening the console to confirm backups are safe, or mid-incident to find what failed, why, and the fix. Frequency: many times a day; sometimes at 3am in a dark room, sometimes at a daylit desk. Constraints pinned by the task brief: dense, calm, legible at a glance; dark and light both first-class via `prefers-color-scheme` with a user override; health never by hue alone (icon or word on every health colour); skeleton loading, teaching empty states, what/why/fix errors with fix set apart, a not-permitted state from a 403.

Decision path: unattended (no question channel in this harness). The roll's assigned direction was built; no decision page was served, no pick card presented. Challenger verdicts: design annual — declined (raise: one family, hairlines only); airline timetable — declined (raise: rank by weight/case/rule, state as a mark in a fixed cell); CRT arcade — declined (raise: accent reserved by law); drum-machine step row — competitive on identification (raise: lit = state, every other surface silkscreen-flat); orizuru — declined (raise: one current marker); cyclorama — declined (raise: tabular numerals, labelled phases).

Unresolved: whether the namespace scope lives in the shell or per route (backend supports both; the shell reads `?namespace=` and re-asks `/me` with it).

## Direction contract

THESIS: A backup console read like a tape vault's media ledger — slate chassis, each object a label strip (small-caps KIND + mono name), each state a lettered lamp. It refuses the stat-tile dashboard of traffic-light cards.

OWN-WORLD: Anodised slate neutrals in two lights (daylit vault, 3am vault); one indigo accent, by law only for selection and primary action; six health lamps (Okabe–Ito hues) each icon + word. One system sans; mono only for identifiers and paths; tabular numerals; 1px hairlines; 6px chamfers; no card inside a card. Raises: hairlines-only (annual), rank by weight not size (timetable), lit = state / chrome flat (drum machine), one current marker (crane), accent by law (arcade).

STORY: The operator sees who they are, what they may do, and whether anything is lit; every failure reads what, why, fix.

FIRST VIEWPORT: 232px rail: wordmark, ten nav rows (icon + label), current row marked by one accent bar. 48px header: page title; identity strip (user, source lamp, namespace), capabilities disclosure, theme switch. Ledger content below; a problem is a full-width banner under the header, fix set apart.

FORM: candidate 6 of 7 (tape vault media ledger); seed 6b8ff260.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
