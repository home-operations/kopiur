---
name: documentation
description: How Kopiur writes and maintains user-facing docs — the Astro Starlight site under docs/ and the example manifests under deploy/examples/. Use when adding or editing any doc page, writing or changing an example manifest, or whenever you change operator behavior that affects how someone deploys, uses, or configures Kopiur (CRD fields, defaults, Helm values, RBAC, reconciler UX, kopia/mover invocation). Encodes the painfully-clear-for-three-audiences rule, the examples-live-in-deploy/examples-and-are-included convention, the simple→complex example ladder, and the docs-change-with-the-code discipline.
---

# Kopiur documentation norms

Kopiur's docs are an **Astro Starlight** site in `docs/`. Pages live under
`docs/src/content/docs/`, and a page's path is its URL (`backends/s3.mdx` →
`/backends/s3/`). Page order and grouping live in the `sidebar` of
`docs/astro.config.mjs`; a page left out of it still builds but is unlinked, and
nothing catches that. A sidebar label that differs from the page title goes in
the page's front matter (`sidebar: { label: … }`), not in the config. Example
manifests live in `deploy/examples/` and are pulled into the prose at build time
(see below); they are never copy-pasted into pages. `mise run docs` builds the
site, and starlight-links-validator fails the build on a broken cross-link or
heading anchor, so it doubles as our doc lint.

The Astro + Starlight toolchain is pinned in `docs/package.json` /
`docs/pnpm-lock.yaml` and installed with the node and pnpm mise pins. Mermaid
fences render through the `astro-mermaid` integration.

## The bar: painfully clear for three audiences

Every user-facing page must let a reader who has never seen Kopiur succeed at
**all three** of:

1. **Deploy** — how to install / apply this (Helm value, `kubectl apply`, scope,
   prerequisites). No "obviously you'd…" steps left implicit.
2. **Use** — the mental model first. Kopiur separates the **recipe**
   (`SnapshotPolicy`) from the **invocation** (`Snapshot`) from the **schedule**
   (`SnapshotSchedule`); say which one does what before showing fields.
3. **Modify common values** — call out the handful of fields a real user
   actually changes (bucket/prefix/endpoint, secret names, cron + jitter,
   retention, identity, deletionPolicy) and what each does. Don't make readers
   reverse-engineer the CRD to find the knobs.

When in doubt, over-explain the "why", and prefer a worked example over a prose
description. Use Starlight **asides** for the gotchas — alpha API,
webhook-enforced constraints, version prerequisites. The title goes in brackets
and the block closes with `:::`:

```markdown
:::caution[Lose the password, lose the backups]

The `KOPIA_PASSWORD` encrypts the repository. If you lose it, the backups are
unrecoverable — kopia cannot decrypt without it.

:::
```

The types are `note`, `tip`, `caution` and `danger`; use `caution` for a
webhook-enforced constraint. For a collapsible block use plain HTML
`<details>`/`<summary>`, and for tabs the `<Tabs>`/`<TabItem>` components in an
`.mdx` page.

## Examples are the backbone (simple → complex)

Reach for an example for any non-trivial capability. The example set must form a
ladder: the lowest-numbered examples are the canonical first run with the fewest
moving parts; later ones layer in complexity (cluster scope, selectors, GitOps
deploy-or-restore, discovered snapshots, fine-grained maintenance). Add new
examples at the complexity tier they belong to, not just at the end.

### The hard rule: example YAML lives in `deploy/examples/`, never inline

Pages include manifests with a `?raw` import and Starlight's `<Code>` component,
so prose and apply-ready files cannot drift. The page must be `.mdx`, and the
`@repo/` import alias is the repo root:

```mdx
import { Code } from "@astrojs/starlight/components";
import examples09KebabName from "@repo/deploy/examples/09-<kebab-name>.yaml?raw";

## Example 09 — <what it demonstrates>

<1–3 sentences: what this shows and when you'd reach for it.>

<Code code={examples09KebabName} lang="yaml" title="deploy/examples/09-<kebab-name>.yaml" />
```

To show one part of a file, wrap it in `# region <name>` / `# endregion <name>`
comments in the manifest and pass `region(file, "<name>")` from `@utils/region`
as the `code`. A region that no longer exists fails the build.

**Never** paste a literal multi-line manifest into a page. If you're about
to, stop and make it a file under `deploy/examples/` instead. (Short inline
`console`/`kubectl` snippets that aren't manifests are fine.)

### Example manifest conventions (match the existing files)

- Filename `NN-kebab-name.yaml`, `NN` zero-padded and ordered by complexity.
- A top-of-file comment block: what it demonstrates, the CRD type/field it maps
  to (verified against `crates/api`), and any caveats on field shapes.
- `REPLACE_ME` for secrets the user must supply; a real-looking but obviously-
  placeholder value otherwise.
- Inline comments on every non-obvious field — especially the "common values"
  above and any default being made explicit for teaching.
- Backends are **externally tagged** (`backend.s3`, not `backend: { kind: S3 }`)
  — see [[kopiur-design]]. Apply-ready and self-contained (Secret + CRs in one
  file) so `kubectl apply -f` just works after filling in `REPLACE_ME`.

### Adding an example: which of the two homes it belongs to

There are **two** conventions, and the number tells you which:

**01–20 — the `docs/src/content/docs/examples.mdx` ladder.** A three-touch change:

1. Create `deploy/examples/NN-name.yaml`.
2. Add a `## Example NN — …` section in `docs/src/content/docs/examples.mdx` with
   the `<Code>` include of `deploy/examples/NN-name.yaml`.
3. Add a row to the table at the top of `docs/src/content/docs/examples.mdx`.

**21+ — capability-page inclusion.** The ladder is a _tutorial_ sequence and
stops at 20 deliberately; everything after it is a per-capability example that
belongs beside the prose explaining that capability, not at the end of a
tutorial. So a two-touch change:

1. Create `deploy/examples/NN-name.yaml`, usually with
   `# region …` / `# endregion …` markers so the page can pull just the CR that
   teaches the point.
2. Include it from the capability page that documents the field
   (`docs/src/content/docs/replication.mdx` for 19's replication CR, `docs/src/content/docs/repositories.mdx` for
   the `spec.seed` examples, `docs/src/content/docs/backends/s3.mdx` for 38, …).

   **Do not** add a `docs/src/content/docs/examples.mdx` row/section for a 21+ example — that is
   how the same manifest ends up documented twice and the two copies drift.

Either way the file must be reachable from _some_ page: an example nothing
includes is an orphan, and nothing in the build catches it.

**Scenario bundles** are a third home: `deploy/examples/scenarios/NN-name.yaml`,
one per page under `docs/src/content/docs/scenarios/`, pulled by that page (whole-file or by
section) and listed in the scenarios index table. They are typed-checked by
`crates/api/tests/examples_match_crd_shapes.rs` like everything else.

A new **page** (not an example) also needs an entry in the `sidebar` of
`docs/astro.config.mjs`, or it builds unlinked. A new **scenario**
page additionally needs its row in `docs/src/content/docs/scenarios/index.md`.

## Docs change in the same PR as the behavior

**Whenever you change logic that affects how someone deploys, uses, or configures
Kopiur, update the docs in the same change.** This is not a follow-up task.

Triggers that REQUIRE a docs/examples update:

| You changed…                                           | Update…                                                                                            |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| A CRD field / shape / default in `crates/api`          | the relevant page under `docs/src/content/docs/` **and** any example manifest that uses it         |
| A Helm value or install scope (`deploy/helm`)          | `docs/src/content/docs/install.mdx`                                                                |
| RBAC / SA / mover behavior                             | `docs/src/content/docs/movers.mdx`                                                                 |
| Maintenance defaults or projection                     | `docs/src/content/docs/maintenance.mdx`                                                            |
| A webhook-enforced constraint                          | the affected page (state it as a `:::caution` aside) + the example that would otherwise violate it |
| A reconciler UX change (status, print columns, phases) | wherever that surface is described                                                                 |

Field shapes shown in docs/examples must be the real ones — the manifests are
apply-ready, so a wrong field is a user-facing bug, not a typo.

## Verify before claiming done

```bash
mise run docs   # rustdoc + astro build (broken link or anchor = build fail)
```

The build also resolves every include — a renamed or missing example file or
region fails here. The link validator checks heading anchors too, so a
deep-link to a `#section` that no longer exists fails. Don't claim a docs change
is done without a green `mise run docs`. For a fast inner loop, use
`mise run docs-serve` (live-reload `astro dev`). If you
added/changed a manifest, the field shapes should be the same ones the tests in
`crates/api` accept (see [[kopiur-design]] for the `from_yaml` parse path); a
manifest that wouldn't survive admission is wrong even if the site builds.

## Common mistakes

- Pasting YAML inline in a page instead of including a file → drift.
- Adding a **01–20** ladder example but forgetting the `docs/src/content/docs/examples.mdx` table
  row or the section (or vice-versa) → orphaned file or dead reference.
- Adding a **21+** example to the `docs/src/content/docs/examples.mdx` ladder as well as its
  capability page → the same manifest documented twice, and the copies drift.
- New page not added to the `sidebar` in `docs/astro.config.mjs` → builds unlinked.
- Writing an MkDocs admonition (`/// note | Title` or `!!! note`) instead of a
  `:::note[Title]` aside → it renders as literal text and does not error.
- A bare `{` or `<` in the prose of an `.mdx` page → MDX parses it as an
  expression or a tag and the build fails; put it in a code span.
- Documenting the field list without the mental model or the "which values do I
  actually change" guidance → technically complete, practically useless.
- Shipping a behavior change with "docs later" → the docs now lie. Same PR.
- Internally-tagged backend in an example → won't admit; contradicts the CRDs.
