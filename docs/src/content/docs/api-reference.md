---
title: API reference (rustdoc)
---

The Rust API documentation for every crate in the Kopiur workspace is generated with `cargo doc` and published alongside this site. The crates are `kopiur-api`, `kopiur-kopia`, `kopiur-telemetry`, `kopiur-controller`, `kopiur-webhook`, `kopiur-mover` and `xtask`.

:::tip

Start with `kopiur-api`. It holds the strongly-typed CRD definitions and the shared validation, identity and retention logic, and it has no controller-runtime dependencies.

:::

[Open the rustdoc API reference →](/rustdoc/)

:::note

The API reference is built from the same commit as this site. `mise run docs` runs `mise run rustdoc`, which copies the `cargo doc` output into `docs/public/rustdoc/`, and Astro publishes that directory unchanged under `/rustdoc/`. The link lands on a redirect into the `kopiur_api` crate.

`mise run docs-serve` serves the API reference only after `mise run rustdoc` has run at least once.

:::
