# Product

<!-- impeccable:product-schema 1 -->

> Provenance: written without an interview. The implementing agent had no
> question channel, so every fact below is either **[repo]** (read from the
> code, CRDs, ADRs and task briefs in this repository) or **[inferred]** (a
> labelled assumption to be confirmed or corrected by a human). Nothing here
> is a commercial claim.

## Platform

web

## Stack

Vite 7 + React 19 + TypeScript 5 (strict), TanStack Router (file-based) and
TanStack Query, vitest + testing-library, eslint flat config, pnpm pinned by
mise. Served by the Rust `kopiur-ui` binary (axum) which embeds the built
bundle with `rust-embed`. **[repo]** — decided in the M3 plan
(`docs/superpowers/plans/2026-09-08-web-ui-pr3-spa.md`) before this file.

## Users

Platform / SRE engineers who run Kopiur, a Kopia-native Kubernetes backup
operator, on their own clusters. **[repo]**

Situation: they open this console for one of two reasons — a routine check
that backups are landing and retention is doing what the policy says, or an
incident (a failed snapshot, a stuck restore, a repository that went
degraded, a mass-deletion breaker that tripped) where something is already
wrong and they need to know what, why, and what to do next. **[inferred]**
from the operator's own what/why/fix error discipline and the doctor/gates
surfaces the backend exposes.

They are fluent in `kubectl` tables, Kubernetes phases and conditions, and
Kopia's snapshot model. They may be on call at night in a dim room, or at a
desk in daylight. **[inferred]**

## Product Purpose

A read-mostly operator's console over the Kopiur CRDs: repositories and
their replication topology, snapshots and the files inside them, policies,
schedules, restores, maintenance, doctor checks and structural gates — with a
bounded set of operations (snapshot now, restore, delete snapshot,
suspend/resume, run maintenance or replication, scan catalog) and a
read-only file browser with single-file download. **[repo]**

Success: an operator can answer "is my data safe?" within seconds of opening
the page, and when it is not, the console names the failing object, the
cause, and the remediation without a trip to `kubectl`. **[inferred]**

## Positioning

The console impersonates the signed-in user on every apiserver call; it has
no authority of its own. What a user can do here is exactly what their RBAC
lets them do, and the UI shows disabled controls with the reason rather than
hiding them, so the interface teaches the permission model. **[repo]** —
`crates/ui/src/auth`, `Capabilities` wire type.

Every error is an RFC 9457 problem carrying what / why / fix, split from the
same messages the CLI prints, so the browser and `kubectl kopiur` never
disagree about a failure. **[repo]** — `crates/ui/src/api/problem.rs`.

## Operating Context

- Runs in-cluster behind the operator's own SSO / auth proxy (trusted
  headers); no login screen of its own. **[repo]**
- Served from a distroless image; the SPA is one embedded bundle. **[repo]**
- Sits alongside `kubectl kopiur` (same `crates/ops` logic); operators move
  between the two. **[repo]**
- Nine CRDs: Repository, ClusterRepository, SnapshotPolicy, Snapshot,
  SnapshotSchedule, Restore, Maintenance, RepositoryReplication,
  SnapshotReplication. Terminology follows the CRDs and Kopia exactly
  (snapshot, policy, schedule, retention, GFS, pin, maintenance, catalog,
  replication, seed, gate, breaker). **[repo]**

## Capabilities and Constraints

- Wire types are generated from Rust (`ts-rs`) into
  `src/api/types/`; the SPA never hand-declares a server shape. **[repo]**
- Enum unions are heterogeneous (unit variants are strings, struct variants
  are single-key objects) and fallbacks differ per enum; a newer server may
  send values this bundle has never seen and the UI must render them, never
  throw. **[repo]** — `crates/ui-model/src/lib.rs` module doc.
- Mutations carry `X-Kopiur-Request: 1` and JSON content type; some answer
  `204` with no body. **[repo]**
- `/me` capabilities are namespace-scoped; the UI re-asks per namespace.
  **[repo]**
- Downloads are top-level navigations the SPA cannot observe; size limits are
  published up front (`SessionInfo.downloadMaxBytes`). **[repo]**
- The topology route and its layout engine (`elkjs`) load as a lazy chunk.
  **[repo]**
- Undecided: whether a namespace switcher is global or per-route (the
  backend supports both). **[open]**

## Brand Commitments

Name: **Kopiur** (product), **kopiur-ui** (this console). No logo, wordmark
or palette exists in the repository. **[repo]**

Binding visual constraints from the task brief **[repo]**: dense, calm,
legible at a glance; dark and light both first-class, following
`prefers-color-scheme` with a user override (the server sends no theme);
health colours colour-blind safe and never carrying meaning by hue alone —
every health colour pairs with an icon or a word; loading is a skeleton, not
a spinner; empty states say what would appear and how to create it; error
states render what / why / fix with fix visually distinct; a not-permitted
state derives from a 403 problem.

## Evidence on Hand

- Real wire types and their doc comments: `crates/ui/web/src/api/types/`.
- Real problem vocabulary: `urn:kopiur:problem:<kind>` (forbidden,
  not-found, session-required, download-too-large, …) in
  `crates/ui/src/api/problem.rs` and `crates/ui/src/browse/mod.rs`.
- Real CLI output discipline (what/why/fix) in `crates/ops`.
- No screenshots, no customer names, no benchmarks. Do not fabricate any.

## Product Principles

1. Quiet when healthy, loud and specific when not — an all-green cluster
   should read as calm, and a failure should name the object, the cause and
   the fix in that order.
2. Never lie about what is not known: unknown phases, missing fields and
   unrecognised problem types render as "unknown", not as "OK".
3. Teach the permission model: show every action, disable what the user
   cannot do, and say why.
4. Density over decoration: this is a console read many times a day, not a
   page visited once.
5. Same words as the CLI and the CRDs; no invented vocabulary.

## Accessibility & Inclusion

Colour-blind-safe health vocabulary (icon + word on every health colour),
WCAG AA contrast in both themes, full keyboard operation, visible focus,
`prefers-reduced-motion` honoured, tables and charts with text alternatives.
**[repo]** — from the task brief and the M3 plan's Task 9.
