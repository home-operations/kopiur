---
version: 1
slug: "src-routes-index-tsx"
primary_target: "src/routes/index.tsx"
related_targets:
  [
    "src/routes/doctor.tsx",
    "src/routes/gates.tsx",
    "src/components/StatusCards.tsx",
    "src/components/WorkTable.tsx",
    "src/components/GateList.tsx",
  ]
---

# kopiur-ui overview, doctor and gates — surface brief

Scope: the landing overview (`src/routes/index.tsx`), the doctor report (`src/routes/doctor.tsx`), the structural-gate registry (`src/routes/gates.tsx`) and the shared pieces they introduce (`StatusCards`, `WorkTable`, `GateList`, `Finding`). Visitor mode: **Operate**. These surfaces extend the shell's committed world ("the vault ledger", surface brief `src-routes-root-tsx`); no new direction was rolled.

Audience and job: the operator opens the overview first when they suspect something is wrong. It must answer "is my data safe?" in one glance and, when the answer is no, name the object, the cause and the fix. Doctor is the same question asked of the installation, with the two windows (`stuckThreshold`, `failureLookback`) and a namespace scope that moves only the checks that can move. Gates is the registry the operator uses to explain a parked object.

Decision path: unattended (no question channel); the incumbent direction is extended, not re-rolled.

## Direction contract

THESIS: The overview is a verdict line and a ledger, not a wall of stat tiles. One sentence says whether the vault is safe; everything under it is rows. It refuses the hero-metric dashboard of same-size traffic-light cards.

OWN-WORLD: The shell's slate chassis, hairlines and 6px chamfers. Health counts are lettered lamps in a strip (icon + word + tabular count), worst first; work is ledger rows with label strips; a finding is what / why / fix with the fix on its own plate, the same plate the problem banner uses. The accent stays with links, the current marker and the one primary control.

STORY: The operator reads the verdict, then the lamps, then — only when something is lit — the findings with their fix. Doctor teaches which checks a namespace scope moves and which stay installation-wide. Gates teaches what each parked condition means.

FIRST VIEWPORT: Verdict heading (lamp + one sentence) over a health strip of six lamp links; below, two ledgers side by side at width — work in flight and stalled objects — and the findings list with fix plates. Doctor: a one-row control bar (namespace, stuck threshold, failure lookback, run) over a ten-row ledger with outcome lamps and a scope column. Gates: one ledger, severity lamp first.

FORM: extension of the shell's committed form (tape vault media ledger; seed 6b8ff260); no re-roll.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
