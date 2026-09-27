---
version: 1
slug: "src-routes-topology-tsx"
primary_target: "src/routes/topology.tsx"
related_targets:
  [
    "src/components/topology/Graph.tsx",
    "src/components/topology/NodeCard.tsx",
    "src/components/topology/EdgeLegend.tsx",
    "src/components/topology/Drawer.tsx",
  ]
---

# kopiur-ui topology — surface brief

Scope: the replication topology (`src/routes/topology.tsx`) and its pieces under `src/components/topology/` (board, node plate, line legend, drawer, layout worker). Visitor mode: **Operate**. Extends the shell's committed world ("the vault ledger", brief `src-routes-root-tsx`); no new direction was rolled.

Audience and job: an operator answering "where does my data actually live, and where does it go from there?" — and noticing that a repository they believed replicated is not, or that a policy points at a repository that does not exist. Content is the server's `RepositoryGraph`: six node kinds, five edge kinds, a health per node and edge, `missing` ghosts for dangling references. Constraints pinned by the task: health never by hue alone; a missing node unmistakable; the legend always visible; layout in a web worker; the route, the worker and elkjs a lazy chunk.

Decision path: unattended (no question channel). Structures ordered by resonance: 1 free layered graph with legend, 2 ledger-first with a graph inset, 3 per-repository fan rows, 4 flow board, 5 radial ring, 6 adjacency matrix, 7 namespace-grouped compounds. The roll (seed 59503601) dealt 4, 3, 2 with 4 leading; the lead is built. Challengers (ASCII live scene, split-flap concourse) declined: an Operate console read at 3am is not a spectacle.

Unresolved: the detail routes (`/repositories/{kindPath}/{name}`, `/policies/{ns}/{name}`) do not exist in this bundle yet; the drawer links to the scoped list until they land.

## Direction contract

THESIS: A floor plan of the vault read left to right — sources, the repositories, their copies — so where data lives and where it goes is seen, not read. It refuses the force-directed cloud of coloured dots.

OWN-WORLD: Slate chassis, hairlines, 6px chamfers. Each node is a label strip (KIND caps, mono name) on a surface plate with its lettered lamp; lines are faint hairlines when healthy and lamp-coloured with a lettered chip when not; a dangling reference is a dashed ghost plate reading "referenced but not found". Five line styles, legend fixed under the board. Accent marks the selected node only.

STORY: Read the verdict, spot a ghost or a lit line, select the node; the drawer names its relationships and where to go.

FIRST VIEWPORT: Verdict line; a board filling the viewport, policies pinned to the first column, bare backends to the last; legend strip beneath; drawer at the right on selection.

FORM: flow board, candidate 4 of 7, the roll's lead; seed 59503601.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
