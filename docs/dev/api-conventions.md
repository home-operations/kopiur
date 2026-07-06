# kopiur-api conventions (READ BEFORE EDITING `crates/api`)

These conventions are load-bearing — they were derived empirically against `kube 3.1` + `k8s-openapi 0.27` + `schemars 1.2` on Rust 1.95. Violating them breaks either CRD schema generation or compilation. ADR-0003 is the source of truth for _what_ the fields are; this file is _how_ to encode them in Rust.

## 1. CRD top-level types

```rust
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "SnapshotPolicy",
    namespaced,                       // OMIT this line for ClusterRepository (cluster-scoped)
    status = "SnapshotPolicyStatus",
    shortname = "kopiasp",
    category = "kopiur",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPolicySpec { ... }
```

- The `kind` derive generates the root struct named by `kind` (e.g. `SnapshotPolicy`), with your `*Spec` as `.spec` and `*Status` as `.status`. Re-export both from `lib.rs`.
- Every spec/sub-object/status struct: `#[serde(rename_all = "camelCase")]`.

## 2. Discriminated unions = **externally-tagged** Rust enums

Do **NOT** use `#[serde(tag = "...")]` (internally tagged). kube's structural-schema rewriter hoists `oneOf` branch properties to the root and panics if a shared property (the tag) differs across branches. Use serde's default external tagging:

```rust
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Backend { S3(S3Backend), Filesystem(FilesystemBackend), ... }
```

Wire shape: `backend: { s3: {...} }` (this matches ADR-0001 §3.1's YAML). The enum still gives compile-time "exactly one variant" + exhaustive `match` — the ADR §5.5 thesis is fully preserved. Provide a `kind_str(&self) -> &'static str` helper for status/metrics/printcolumns. Webhook validates per-variant _content_.

This applies to: `Backend`, `AllowedNamespaces`, `RestoreSource`, `RestoreTarget`, `Hook`, and any other "exactly one of" surface.

Simple closed string enums (no payload) are fine as plain unit enums and serialize as strings: `DeletionPolicy{Delete,Retain,Orphan}`, `Origin`, `*Phase`, `RepositoryKind`, `ConcurrencyPolicy`, etc. Give them `#[derive(... Copy, Eq, Default ...)]` and mark the default variant `#[default]`.

## 3. `Eq` and k8s-openapi types

`k8s-openapi` types (`LabelSelector`, `ResourceRequirements`, `SecurityContext`, `PodSpec`, `JobSpec`, `Condition`, …) implement `PartialEq` but **not** `Eq`. Any struct embedding one (directly or transitively) must derive `PartialEq` only — never `Eq`. Reuse these types from k8s-openapi; do not re-invent them. The `schemars` feature is enabled on k8s-openapi workspace-wide so they derive `JsonSchema`.

Use `k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, Condition}` and `k8s_openapi::api::core::v1::{ResourceRequirements, SecurityContext, ...}`.

## 4. Optional blocks & forward-compat (ADR §4.11)

Every credential/policy/identity/schedule surface is a **sub-object**, not a leaf field. Optionals: `#[serde(default, skip_serializing_if = "Option::is_none")] pub x: Option<T>`. Bools that default false: `#[serde(default, skip_serializing_if = "std::ops::Not::not")]`. Vecs: `#[serde(default, skip_serializing_if = "Vec::is_empty")]`.

## 4a. Schema defaults (`#[schemars(default = …)]`)

A schemars `default` becomes a CRD `default:` that the **API server materializes
server-side** (absent → present at admission), and it is what `kubectl explain`,
the YAML language server, and the generated [field reference](../field-reference.md)
show. schemars 1 does **not** pick up a `#[serde(default = "fn")]` on its own — pair
it with `#[schemars(default = "fn")]`, and for an `Option<T>` field the fn must
return the field's `Option` type (a bare `T` fails the `JsonSchema` derive):

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
#[schemars(default = "default_failed_jobs_history_limit")]
pub failed_jobs_history_limit: Option<u32>,
// ...
fn default_failed_jobs_history_limit() -> Option<u32> { Some(DEFAULT_FAILED_JOBS_HISTORY_LIMIT) }
```

**Only emit a schema default where it is context-free** — where the controller
resolves an absent field to *exactly* that constant (an `unwrap_or(CONST)` shape),
and the field's meaning does not depend on where it sits. Materializing the value
must be behavior-preserving. Do **NOT** emit one for a field whose effective
default is context-dependent (origin/source/kind/webhook-resolved, or the lower
link of an inheritance chain) — those stay `—` in the reference and state their
resolution rule in the doc comment instead. Guard each added default with a test
that walks `T::crd()` and asserts the schema `default` equals the constant (see
`repository::tests::repository_schema_emits_context_free_defaults`); duration-string
defaults additionally assert `parse("30m") == DEFAULT_…`.

## 4b. Docs are generated — edit the doc comment, then `mise run gen`

`docs/field-reference.md` is generated from the CRD schemas by `cargo xtask
gen-docs` (folded into `mise run gen`) and drift-gated by `mise run gen-check`.
The **field descriptions come from the Rust doc comments** in `crates/api`, so any
`///` edit that changes a field's meaning, type, or default must be followed by
`mise run gen` — otherwise `gen-check` fails in CI. Never hand-edit
`field-reference.md`. Doc comments render as Markdown (rustdoc intra-doc links like
`` [`X`] `` are stripped to `` `X` `` by the generator), so write them to read
well in both `cargo doc` and the reference page.

## 5. Status

Always carries `resolved.*` pinned values (ADR §4.2: resolved identity pinned at admission, never re-rendered). `conditions: Vec<Condition>` using the k8s-openapi type. Phase is a closed enum with a `#[default]` of `Pending`.

## 6. Tests (in each CRD module or `tests/`)

Use the YAML→JSON→typed bridge (the API-server path), NOT `serde_yaml` directly (serde_yaml 0.9 encodes externally-tagged enums as non-standard `!Variant` tags):

```rust
fn from_yaml<T: serde::de::DeserializeOwned>(yaml: &str) -> T {
    let v: serde_json::Value = serde_yaml::from_str(yaml).unwrap();
    serde_json::from_value(v).unwrap()
}
```

Per CRD, test: (a) `T::crd()` group/kind/scope/version; (b) round-trip the exact ADR YAML and assert key fields + structural `spec == reparse(serialize(spec))`; (c) each union variant (de)serializes under its expected key; (d) unknown variant is rejected.

Run: `cargo test -p kopiur-api`. Schema generation is exercised by any `T::crd()` call — if an enum is mis-encoded, that call panics, so the `crd()` test catches it.
