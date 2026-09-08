# API reference (rustdoc)

The Rust API documentation for every crate in the Kopiur workspace is generated with `cargo doc` and published alongside this site. The crates are `kopiur-api`, `kopiur-kopia`, `kopiur-telemetry`, `kopiur-controller`, `kopiur-webhook`, `kopiur-mover` and `xtask`.

/// tip

Start with `kopiur-api`. It holds the strongly-typed CRD definitions and the shared validation, identity and retention logic, and it has no controller-runtime dependencies.

///

<a href="/rustdoc/index.html">Open the rustdoc API reference →</a>

/// note

The API reference is built from the same commit as this site. `scripts/build-docs.sh` nests it under `/rustdoc/` after `mkdocs build`.

The link above starts at the site root, because the site is served at the root of the custom domain, and it lands on a redirect into the `kopiur_api` crate. The rustdoc tree does not exist during `mkdocs serve`, so the link only resolves in the assembled `site/` directory, meaning after `mise run docs`.

///
