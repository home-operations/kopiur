# xtask

Kopiur's codegen library + binary: generate the CRDs, RBAC, and dashboards that ship under `deploy/`, the field reference under `docs/`, and the web UI's TypeScript wire types under `crates/ui/web/`.

## Role in the workspace

`xtask` is Kopiur's developer-tooling crate, invoked as `cargo xtask <cmd>`. It is
the single source that turns the typed CRD definitions (from `kopiur-api`'s
`kube::CustomResource` derives) and the hand-written RBAC/dashboard sources into
the checked-in deploy artifacts:

- `gen-crds` → structural-schema CRD YAML under `deploy/crds/`
- `gen-rbac` → the controller/webhook RBAC manifests under `deploy/`
- `gen-docs` → `docs/field-reference.md`, rendered from the CRD schemas
- `gen-ui-types` → one `.ts` per `kopiur-ui-model` wire type under
  `crates/ui/web/src/api/types/`, exported with `ts-rs`
- `gen-all` → all of the above + the Grafana dashboard copy under
  `deploy/helm/kopiur/files/dashboards/`

Each subcommand also takes a `--check` mode (`mise run gen-check`) that re-renders
everything in memory and compares it against the checked-in files **without
writing**, so CI fails on drift instead of silently shipping stale YAML.

`gen-ui-types` is the one exception to "in memory": `ts-rs` writes its own files,
so it exports into a scratch directory under `target/` and then syncs or
compares. It also owns a stale-file sweep the content-keyed [`artifact::Artifact`]
machinery has no notion of — deleting a wire type in Rust deletes its
TypeScript, rather than leaving a file the SPA can still import. See
[`ui_types`] for that and for why the generated output is whitespace-normalized
before it is written.

It also hosts two non-generating gates:

- `check-wiring` (`mise run wiring-check`) → fail if a CRD field is defined and
  schema-generated but read by **no** consumer crate.
- `check-phases` (`mise run phase-check`) → fail if a phase branch opts out of
  the exhaustive-match guarantee without the compiler saying so.

That gate exists because `gen-check` answers the wrong question. It proves the
checked-in YAML matches the Rust types; it says nothing about whether anything
_reads_ a field, and since every `kopiur-api` type is `pub`, `dead_code` can
never fire on one either. Two bugs shipped through that gap — [#346] (`sources[].
pvcSelector` had no implementation anywhere, so a policy using it died with
`invariant violated … likely a bug in kopiur`) and [#351]
(`files.ignoreIdenticalSnapshots` was never mapped to a kopia flag, so the knob
silently did nothing). Exemptions live in `wiring-allowlist.yaml`, each with a
written reason; see [`wiring`] for what counts as "read" and the deliberate
limits of the search.

The phase gate exists because the compiler cannot see the four constructs that
opt out of exhaustiveness: `matches!` (an implicit `_ => false`), a `_ =>` arm
(including the wrapper form `Some(_) =>`, which is how every phase in this repo
is actually read), `==`/`!=` against a single variant, and `if let` naming one
variant — plus one drift class with no construct at all, a gate condition the
controller defines and the CLI never learns about. They have shipped bugs:
[#351]'s `SnapshotPhase::Unchanged` was swallowed by two `_ =>` arms, and [#359]
was doctor reporting all-green over a `Snapshot` parked on a condition it had
never heard of. Exemptions live in `phase-allowlist.yaml`, each with a written
reason; see [`phases`] for the five rules and the deliberate limits of the scan.

[#346]: https://github.com/home-operations/kopiur/issues/346
[#351]: https://github.com/home-operations/kopiur/issues/351
[#359]: https://github.com/home-operations/kopiur/issues/359

The generation logic deliberately lives in the **library** (`xtask::`), not in
`main.rs`: a binary crate's modules aren't importable, so keeping it in the lib
lets the integration tests under `tests/` exercise it directly.

> Note: the package name is `xtask` (not `kopiur-xtask`); the import path is
> `xtask::`.

## Key modules / types

| Item                                              | Role                                                                                                                                     |
| ------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| [`collect`]                                       | Returns the [`artifact::Artifact`]s a subcommand (`gen-crds` / `gen-rbac` / `gen-docs` / `gen-all`) is responsible for.                  |
| [`run`]                                           | Drives a subcommand end-to-end: writes the artifacts, or in `--check` mode reports drift and returns the process exit code.              |
| [`artifact::Artifact`]                            | One generated file: a `deploy/`-relative path + its full content (including the generated-file header).                                  |
| [`artifact::write_all`] / [`artifact::check_all`] | Write every artifact to disk / compare against the checked-in files (the drift guard).                                                   |
| [`paths::workspace_root`] / [`paths::deploy_dir`] | Deterministic workspace-root resolution and the `deploy/` directory under it.                                                            |
| [`crds`] / [`rbac`] / [`dashboards`] / [`docs`]   | The per-kind artifact generators.                                                                                                        |
| [`ui_types`]                                      | `gen-ui-types`: exports the `kopiur-ui-model` wire types to TypeScript, sweeps stale ones, and drift-checks the result.                  |
| [`wiring`]                                        | The inert-field ratchet: walks the CRD schemas and asserts each field is read by a consumer crate or reviewed-and-allowlisted.           |
| [`phases`]                                        | The phase-exhaustiveness ratchet: flags `matches!` / `_ =>` / `==` / `if let` over a phase enum, and condition types the CLI cannot see. |
| [`scan`]                                          | The source-scanning primitives both ratchets share: comment/string scrubbing, `#[cfg(test)]` stripping, and the `.rs` walker.            |

## Example

[`artifact::Artifact`] is a pure value type — constructing one and reading its
relative path needs no filesystem, cluster, or codegen:

```rust
use xtask::artifact::Artifact;

let a = Artifact::new("crds/repositories.yaml".into(), "# GENERATED\n".into());
assert_eq!(a.rel_path, "crds/repositories.yaml");
assert!(a.content.starts_with("# GENERATED"));
```

Running a subcommand (touches the filesystem under `deploy/`, so `no_run`):

```rust,no_run
# fn main() -> anyhow::Result<()> {
// `false` = write mode; `true` = --check drift guard (returns exit code 1 on drift).
let exit_code = xtask::run("gen-all", false)?;
std::process::exit(exit_code);
# }
```

From the command line:

```text
cargo xtask gen-all           # regenerate CRDs + RBAC + dashboard + docs + SPA types
cargo xtask gen-all --check   # CI drift guard: nonzero exit if artifacts are stale
cargo xtask gen-ui-types      # just the SPA's TypeScript (no CRD rebuild)
cargo xtask check-wiring      # inert-field gate (no --check; it never writes)
cargo xtask check-phases      # phase-exhaustiveness gate (likewise)
mise run gen                  # the same, via the pinned task runner
mise run gen-check
mise run ui-types
mise run wiring-check
mise run phase-check
```

## See also

- [ADR-0003](../../docs/adr/0003-kopiur-rust-operator.md) — the canonical CRD
  surface these artifacts are generated from.
