# kopiur-ui — the single-page app

The web console for Kopiur: ten screens over the read-only and action endpoints
`crates/ui` serves at `/api/v1`. It is built into `dist/` and embedded into the
`kopiur-ui` binary by `crates/ui/build.rs`, so **the SPA is not deployed
separately** — there is one binary, and it carries this bundle.

Everything here runs through `mise` from the workspace root. `pnpm` is pinned by
`.mise/config.toml` and installed by mise itself; you do not need a global node.

```bash
mise run ui-check     # tsc + eslint + oxfmt + vitest, then the generated-types drift gate
mise run ui-build     # type-check, bundle into dist/, enforce the bundle budget
mise run ui-dev       # the dev server, proxying /api to a local backend
```

## Running the dev server against a local backend

`ui-dev` starts Vite on its own port and proxies `/api` to
`http://127.0.0.1:8090` (`vite.config.ts`), which is `kopiur-ui`'s default
listen address. So you need the backend running beside it, pointed at a cluster:

```bash
# Terminal 1 — the backend, against whatever kubeconfig is current.
# --anonymous-user gives you an identity to impersonate without a proxy in
# front; in a real deployment the identity comes from trusted headers.
cargo run -p kopiur-ui -- --anonymous-user "$(kubectl config view --minify -o jsonpath='{.contexts[0].context.user}')"

# Terminal 2 — the SPA, with hot reload.
mise run ui-dev
```

Open the URL Vite prints, not `:8090`. The backend on `:8090` serves the
_embedded_ bundle — the one from the last `ui-build` — so hitting it directly
shows you stale code and none of your edits.

Two things about the proxy that will bite if you change them:

- **`changeOrigin` stays `false`.** The backend's CSRF gate compares each
  mutating request's `Origin` against its `Host`. Rewriting `Host` to
  `127.0.0.1:8090` would make every action from the dev server a cross-origin
  refusal.
- **Everything is impersonated.** The backend performs every read and write as
  the signed-in user, so what you can see in the dev server is exactly what your
  own RBAC allows. A screen that looks empty or disabled is usually a binding
  you do not have, not a bug — the identity strip in the header lists every
  capability with a yes or a no.

Against a cluster with nothing in it, most screens are empty states. `crates/e2e`
is what stands up a populated one.

## The API types are generated, and that is the rule

Everything under `src/api/types/` is written by
[`ts-rs`](https://github.com/Aleph-Alpha/ts-rs) from the Rust types in
`crates/ui-model`, by `cargo xtask gen-ui-types` (which `mise run ui-types` and
`mise run ui-check` both invoke). Those files are checked in, and `ui-check`
ends by failing if regenerating them changes anything.

**No API shape is ever hand-written.** Not a field, not a union, not an
"obviously equivalent" interface in a component. The reason is not tidiness: a
hand-written type is a _claim_ about what the server sends, and nothing checks
it. When the Rust type changes — a field becomes optional, an enum gains a
variant, a name is renamed by serde — the generated types change with it and
`tsc` points at every line that has to be reconsidered. A hand-written type
keeps compiling and starts lying, and this console's job is to tell an operator
whether their backups exist.

So, in order:

1. Need a new field or a new shape? **Change the Rust type** in
   `crates/ui-model`, then `mise run ui-types`. If the generated TypeScript
   looks wrong, the Rust type is wrong — fix it there, never in the `.ts`.
2. Import from `src/api/types.ts`, the hand-maintained barrel. It sits
   _outside_ the generated directory on purpose: `gen-ui-types` sweeps stale
   files inside `src/api/types/`, and a barrel in there would be deleted on the
   next run. Add the re-export when you add a type.
3. `src/api/types/**` is excluded from eslint and from the formatter — ts-rs
   emits trailing whitespace, and a formatter that "fixed" it would break the
   drift gate on the next run.

There is exactly **one** sanctioned exception, and it is not an exception to the
rule above. `StatusOverview.report` is `unknown` on the wire by design: it is
the CLI's own report, which the backend passes through without a schema. The
SPA never declares a type for it either — `src/api/statusReport.ts` _narrows_
it, field by field, with every read guarded, dropping rows this bundle cannot
understand rather than patching them, and reporting that the read was partial.
If you need another field from the report, add a guarded read there.

## Layout

| Path                      | What lives there                                                                                                                                                                                                                      |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src/routes/`             | One file per route; the tree is generated into `src/routeTree.gen.ts`. A detail route's filename needs the escaping `_` (`snapshots_.$namespace.$name.tsx`) or the list route silently becomes its layout and the page renders blank. |
| `src/components/`         | Presentation. A component takes data and callbacks; the route owns the hooks, so a component test needs no query client.                                                                                                              |
| `src/components/actions/` | The shared confirmation panel and the dialogs built on it.                                                                                                                                                                            |
| `src/charts/`             | The charts, each with a table twin — the SVG is always `aria-hidden`, because an operator using a screen reader still needs the number.                                                                                               |
| `src/api/`                | The one `fetch` wrapper, the query hooks, the problem types, and the generated types. eslint forbids `fetch` anywhere else.                                                                                                           |
| `src/styles.css`          | One stylesheet, design tokens at the top. `DESIGN.md` describes the patterns; `PRODUCT.md` describes what the screens are for.                                                                                                        |

`src/styles.focus.test.ts` reads the stylesheet as text and guards two things a
browser reports silently and jsdom cannot see at all: a later `box-shadow` that
eats the global focus ring, and `light-dark()` handed anything but a colour.
Both had already shipped once.
