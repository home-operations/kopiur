//! CRD-field wiring ratchet — the guard that catches an *inert* CRD field.
//!
//! # Why this exists
//!
//! `gen-crds` proves the checked-in YAML matches the Rust types. It says
//! nothing about whether anything ever *reads* a field. Because every
//! `crates/api` type is `pub`, `dead_code` can never fire on one either. So a
//! field could be defined, schema-generated, documented in
//! `docs/field-reference.md`, accepted by admission — and do absolutely
//! nothing. Two shipped bugs came from exactly that:
//!
//! * **#346** — `SnapshotPolicy.spec.sources[].pvcSelector` had no
//!   implementation anywhere, so a policy using it failed with
//!   `invariant violated: ... This is likely a bug in kopiur`.
//! * **#351** — `SnapshotPolicy.spec.files.ignoreIdenticalSnapshots` was never
//!   mapped to a kopia flag, so the knob silently did nothing.
//!
//! An audit at the time found 7 inert spec fields and 9 never-written status
//! fields. This module makes that population an explicit, reviewed set.
//!
//! # What it checks
//!
//! For every property in every CRD's `spec` and `status` subtree, the Rust
//! identifier (the camelCase schema name, snake_cased) must be **reachable in a
//! consumer crate** — `controller`, `mover`, `kopia`, `webhook`, `cli` — or be
//! listed in `crates/xtask/wiring-allowlist.yaml` with a written reason.
//!
//! Both directions fail, which is what makes it a ratchet rather than a
//! snapshot:
//!
//! * an **offender** (unreachable, not allowlisted) fails — you wired a field
//!   into the API and nothing consumes it;
//! * a **stale** allowlist entry (allowlisted, now reachable) fails — you wired
//!   it up, so delete the exemption.
//!
//! # Deliberate limits
//!
//! Reachability is a whole-word identifier search over *scrubbed* source (see
//! [`scrub`]): comments and string literals are removed, so a field named only
//! in a doc comment or an error message does not count as wired. `#[cfg(test)]`
//! modules and test files are excluded, so a field used only by fixtures does
//! not count either.
//!
//! It is still a name search, not a call graph, so it **under-reports**: a
//! generic name (`name`, `path`, `enabled`) matches something somewhere and is
//! assumed wired. That direction is intentional — the check must not produce
//! false failures. It catches distinctive names, which is where this defect
//! class actually lives (`pvc_selector`, `ignore_identical_snapshots`,
//! `source_path_strategy`, `group_by` were all caught).
//!
//! `crates/api` is not searched (that is the definition site) and neither is
//! `crates/migrate`, which only *emits* CRs — a field only the migrator writes
//! is still inert at runtime.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::JSONSchemaProps;
use kube::core::CustomResourceExt;
use serde::Deserialize;

use crate::paths::workspace_root;

/// Crates whose source counts as "something consumes this field".
///
/// `migrate` only *writes* CRs, so a field only it mentions is still inert at
/// runtime. `xtask`/`telemetry`/`e2e` are tooling.
const CONSUMER_CRATES: &[&str] = &["controller", "mover", "kopia", "webhook", "cli"];

// `crates/api` is deliberately NOT a consumer, even though it owns real
// resolver helpers (`api::identity`, `CatalogBounds`, `effective_*`). Including
// it was tried and reverted: it makes the check miss the very bug it exists for.
// `pvcSelector` is named by `source_mutates_live_volume`
// (`snapshot_policy.rs:414`) for an unrelated read-only guard, which would have
// read as "wired" for the whole time #346 was reachable. So a field whose only
// reader lives in `crates/api` goes in `allow` with the helper named — that is
// documentation a reviewer can check, not noise.

/// Where the reviewed exemptions live, relative to the workspace root.
const ALLOWLIST_REL: &str = "crates/xtask/wiring-allowlist.yaml";

// --- the allowlist file ----------------------------------------------------

/// One reviewed exemption or pruned subtree.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    /// Dotted CRD path, e.g. `SnapshotPolicy.spec.files.ignoreIdenticalSnapshots`.
    ///
    /// A path may start with `*.`, matching any path with that *suffix* —
    /// `*.securityContext` covers the dozen places kopiur embeds one, and
    /// `*.gdrive.credentialsSecretRef` covers the same field on `Repository`,
    /// `ClusterRepository` and `RepositoryReplication`.
    ///
    /// A glob in `allow` stays honest because the stale check is per-field: if
    /// *any* field the glob covers becomes wired, the entry fails and has to be
    /// narrowed. It cannot silently absorb a new inert sibling.
    pub path: String,
    /// Why this is exempt. Required — an exemption without a reason is how the
    /// list rots into a rubber stamp.
    pub reason: String,
}

impl Entry {
    /// Whether this entry's (possibly `*.`-suffixed) path matches `path`.
    fn matches(&self, path: &str) -> bool {
        match self.path.strip_prefix("*.") {
            Some(suffix) => path == suffix || path.ends_with(&format!(".{suffix}")),
            None => self.path == path,
        }
    }
}

/// A field whose Rust identifier is not derivable from the schema name,
/// because `#[serde(rename = "...")]` broke the camelCase correspondence.
#[derive(Debug, Clone, Deserialize)]
pub struct Rename {
    /// Dotted CRD path, exact.
    pub path: String,
    /// The real Rust identifier to search for.
    pub ident: String,
    /// Why the names differ.
    pub reason: String,
}

/// The parsed `wiring-allowlist.yaml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Allowlist {
    /// Fields whose Rust identifier is not the camel→snake of the schema name.
    #[serde(default)]
    pub rename: Vec<Rename>,
    /// Subtrees not to descend into: upstream `k8s-openapi` types that kopiur
    /// reuses as whole objects and never names field-by-field.
    ///
    /// The kopiur-owned field that *holds* the upstream type is still checked —
    /// pruning stops the walk at it, it does not exempt it. So a `mover.
    /// securityContext` that nothing applied would still be caught, while its
    /// twenty upstream leaves (`seccompProfile.localhostProfile`, …) are not
    /// reported as kopiur's problem.
    #[serde(default)]
    pub prune: Vec<Entry>,
    /// Individual fields that are knowingly not read by any consumer.
    #[serde(default)]
    pub allow: Vec<Entry>,
}

impl Allowlist {
    /// Read and parse the checked-in allowlist.
    pub fn load() -> Result<Self> {
        let p = workspace_root().join(ALLOWLIST_REL);
        let raw =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", p.display()))
    }

    /// Whether `path` carries a reviewed exemption.
    pub fn is_allowed(&self, path: &str) -> bool {
        self.allow.iter().any(|e| e.matches(path))
    }

    /// Whether the walk should stop descending at `path`.
    pub fn is_pruned(&self, path: &str) -> bool {
        self.prune.iter().any(|e| e.matches(path))
    }
}

// --- schema walk -----------------------------------------------------------

/// One field discovered in a CRD schema.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Field {
    /// Dotted path from the CRD kind, e.g. `Snapshot.spec.source.target`.
    pub path: String,
    /// The struct-field identifier the schema name maps to (snake_case).
    pub ident: String,
    /// The enum-variant *path suffix* it maps to, e.g. `::S3`.
    ///
    /// A schema property is one or the other: `backend.s3` is the externally
    /// tagged `Backend::S3`, not a field named `s3`. Accepting either keeps the
    /// repo's discriminated-union convention from reading as inert.
    ///
    /// The leading `::` is load-bearing — it is what makes this a *use* rather
    /// than a mention of the type. Matching a bare `GroupBy` would be satisfied
    /// by `pub enum GroupBy` or a `pub use` re-export, i.e. by the type
    /// existing at all, which is exactly the non-answer this check exists to
    /// reject.
    pub variant: String,
}

/// Every `spec`/`status` field of every CRD. The walk records each field but
/// stops descending at a pruned one (see [`Allowlist::prune`]).
pub fn schema_fields(prune: &Allowlist) -> Vec<Field> {
    let crds = [
        kopiur_api::Repository::crd(),
        kopiur_api::ClusterRepository::crd(),
        kopiur_api::SnapshotPolicy::crd(),
        kopiur_api::Snapshot::crd(),
        kopiur_api::SnapshotSchedule::crd(),
        kopiur_api::Restore::crd(),
        kopiur_api::Maintenance::crd(),
        kopiur_api::RepositoryReplication::crd(),
    ];
    let mut out = Vec::new();
    for crd in &crds {
        let kind = &crd.spec.names.kind;
        let Some(schema) = crd.spec.versions[0]
            .schema
            .as_ref()
            .and_then(|s| s.open_api_v3_schema.as_ref())
        else {
            continue;
        };
        let Some(props) = schema.properties.as_ref() else {
            continue;
        };
        for top in ["spec", "status"] {
            if let Some(node) = props.get(top) {
                walk(&format!("{kind}.{top}"), node, prune, &mut out);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Recursively collect properties, recording each but not descending past a
/// pruned one.
fn walk(path: &str, node: &JSONSchemaProps, prune: &Allowlist, out: &mut Vec<Field>) {
    // An array of objects: descend through `items` without adding a path
    // segment, so `sources[].pvcSelector` reads as `...sources.pvcSelector`.
    if let Some(items) = node.items.as_ref() {
        if let k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::JSONSchemaPropsOrArray::Schema(inner) = items {
            walk(path, inner, prune, out);
        }
        return;
    }
    let Some(props) = node.properties.as_ref() else {
        return;
    };
    for (name, prop) in props {
        let child = format!("{path}.{name}");
        // Record the field either way: pruning stops the DESCENT (the upstream
        // type's own leaves are not kopiur's to consume), it does not exempt
        // the kopiur-owned field that holds it.
        out.push(Field {
            path: child.clone(),
            ident: camel_to_snake(name),
            variant: camel_to_pascal(name),
        });
        if !prune.is_pruned(&child) {
            walk(&child, prop, prune, out);
        }
    }
}

/// Convert a camelCase schema name to the Rust identifier `rename_all =
/// "camelCase"` derives it from.
///
/// A run of consecutive capitals is one segment, so an acronym survives:
/// `kopiaSnapshotID` → `kopia_snapshot_id`, not `kopia_snapshot_i_d`.
///
/// ```
/// use xtask::wiring::camel_to_snake;
/// assert_eq!(camel_to_snake("pvcSelector"), "pvc_selector");
/// assert_eq!(camel_to_snake("kopiaSnapshotID"), "kopia_snapshot_id");
/// assert_eq!(camel_to_snake("path"), "path");
/// ```
pub fn camel_to_snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if prev_lower || (prev_upper && next_lower) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

/// Convert a camelCase schema name to the PascalCase enum variant an
/// externally-tagged union derives it from: `webDav` → `WebDav`, `s3` → `S3`.
///
/// ```
/// use xtask::wiring::camel_to_pascal;
/// assert_eq!(camel_to_pascal("httpRequest"), "HttpRequest");
/// assert_eq!(camel_to_pascal("s3"), "S3");
/// ```
pub fn camel_to_pascal(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Whether `variant` is *used* as an enum variant — i.e. `Something::Variant`
/// occurs, not merely the bare type name.
///
/// ```
/// use xtask::wiring::mentions_variant;
/// assert!(mentions_variant("match b { Backend::S3(c) => () }", "S3"));
/// assert!(!mentions_variant("pub enum GroupBy { VolumeGroupSnapshot }", "GroupBy"));
/// assert!(!mentions_variant("Backend::S3Extra", "S3"));
/// ```
pub fn mentions_variant(haystack: &str, variant: &str) -> bool {
    if variant.is_empty() {
        return false;
    }
    let needle = format!("::{variant}");
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(&needle) {
        let start = from + rel;
        let end = start + needle.len();
        // Only the trailing boundary matters: `::` is itself a boundary, and
        // whatever precedes it is the type/module path.
        if end >= haystack.len() || !haystack[end..].chars().next().is_some_and(word) {
            return true;
        }
        from = start + 1;
    }
    false
}

// --- source scanning -------------------------------------------------------

/// Remove comments and string/char literals from Rust source, so a field named
/// only in a doc comment or an error message is not mistaken for a consumer.
///
/// This is deliberately a character scanner rather than a parser: it must never
/// fail on valid source, and over-removal only makes the check more
/// conservative.
///
/// ```
/// use xtask::wiring::scrub;
/// assert_eq!(scrub("let a = 1; // pvc_selector"), "let a = 1; ");
/// assert!(!scrub(r#"err("pvc_selector missing")"#).contains("pvc_selector"));
/// ```
pub fn scrub(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // Line comment (covers `//`, `///` and `//!`).
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Block comment, nesting-aware (Rust allows nesting).
        if c == '/' && b.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
            continue;
        }
        // Raw string: r"..." / r#"..."# / r##"..."##
        if c == 'r' {
            let mut hashes = 0;
            let mut j = i + 1;
            while b.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) == Some(&'"') {
                j += 1;
                loop {
                    if j >= b.len() {
                        break;
                    }
                    if b[j] == '"' && b[j + 1..].iter().take(hashes).all(|h| *h == '#') {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                out.push(' ');
                i = j;
                continue;
            }
        }
        // Normal string literal.
        if c == '"' {
            i += 1;
            while i < b.len() {
                if b[i] == '\\' {
                    i += 2;
                    continue;
                }
                if b[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(' ');
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Remove `#[cfg(test)]`-annotated items by brace matching, so fixtures do not
/// count as consumers.
///
/// ```
/// use xtask::wiring::strip_cfg_test;
/// let s = "fn real() { a(); }\n#[cfg(test)]\nmod tests { fn f() { pvc_selector(); } }\n";
/// assert!(!strip_cfg_test(s).contains("pvc_selector"));
/// assert!(strip_cfg_test(s).contains("real"));
/// ```
pub fn strip_cfg_test(src: &str) -> String {
    const MARK: &str = "#[cfg(test)]";
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.find(MARK) {
        out.push_str(&rest[..at]);
        let after = &rest[at + MARK.len()..];
        // Skip to the item's opening brace, then match to its close.
        match after.find('{') {
            Some(open) => {
                let bytes = after.as_bytes();
                let mut depth = 0usize;
                let mut end = None;
                for (k, ch) in bytes.iter().enumerate().skip(open) {
                    match ch {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(k + 1);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                match end {
                    Some(e) => rest = &after[e..],
                    // Unbalanced (shouldn't happen in valid source) — drop the tail.
                    None => return out,
                }
            }
            // e.g. `#[cfg(test)] use ...;` — drop to end of statement.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Whether `ident` occurs in `haystack` as a whole word.
///
/// ```
/// use xtask::wiring::mentions;
/// assert!(mentions("let pvc_selector = 1;", "pvc_selector"));
/// assert!(!mentions("let pvc_selector_x = 1;", "pvc_selector"));
/// assert!(!mentions("let my_pvc_selector = 1;", "pvc_selector"));
/// ```
pub fn mentions(haystack: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(ident) {
        let start = from + rel;
        let end = start + ident.len();
        let before_ok = start == 0 || !haystack[..start].chars().next_back().is_some_and(word);
        let after_ok = end >= haystack.len() || !haystack[end..].chars().next().is_some_and(word);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Remove `use ...;` statements (including multi-line brace groups).
///
/// An import is not a use: `use kopiur_api::GroupBy;` contains `::GroupBy` and
/// would otherwise satisfy [`mentions_variant`] without anyone ever dispatching
/// on the type. Stripping them keeps "wired" meaning "something acts on it".
///
/// ```
/// use xtask::wiring::strip_use_stmts;
/// assert!(!strip_use_stmts("use kopiur_api::GroupBy;\nlet a = 1;").contains("GroupBy"));
/// assert!(strip_use_stmts("use a::B;\nmatch x { GroupBy::None => () }").contains("GroupBy::None"));
/// ```
pub fn strip_use_stmts(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    // Walk line by line; a `use` statement always starts one (after indentation
    // and an optional `pub `), and runs to the next `;` — which may be several
    // lines later for a brace group.
    let mut rest = src;
    while !rest.is_empty() {
        let line_end = rest.find('\n').map_or(rest.len(), |i| i + 1);
        let line = &rest[..line_end];
        let head = line.trim_start();
        let head = head.strip_prefix("pub ").unwrap_or(head);
        if head.starts_with("use ") {
            match rest.find(';') {
                Some(semi) => {
                    out.push('\n');
                    rest = &rest[semi + 1..];
                }
                // Unterminated (not valid Rust) — drop the tail rather than loop.
                None => break,
            }
        } else {
            out.push_str(line);
            rest = &rest[line_end..];
        }
    }
    out
}

/// Every `.rs` file whose text counts as a consumer, excluding test files.
fn consumer_sources() -> Result<Vec<PathBuf>> {
    let root = workspace_root().join("crates");
    let mut out = Vec::new();
    for c in CONSUMER_CRATES {
        let dir = root.join(c).join("src");
        if dir.is_dir() {
            collect_rs(&dir, &mut out)?;
        }
    }
    out.sort();
    Ok(out)
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        if p.is_dir() {
            // `src/**/tests/` holds fixtures only.
            if p.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            collect_rs(&p, out)?;
        } else if p.extension().is_some_and(|x| x == "rs") {
            // `foo/tests.rs` is the repo's `#[cfg(test)] mod tests;` file.
            if p.file_stem().is_some_and(|n| n == "tests") {
                continue;
            }
            out.push(p);
        }
    }
    Ok(())
}

/// The scrubbed, test-stripped text of every consumer source, concatenated.
pub fn consumer_corpus() -> Result<String> {
    let mut corpus = String::new();
    for p in consumer_sources()? {
        let raw =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        corpus.push_str(&strip_use_stmts(&scrub(&strip_cfg_test(&raw))));
        corpus.push('\n');
    }
    Ok(corpus)
}

// --- the check itself ------------------------------------------------------

/// The outcome of one wiring pass.
#[derive(Debug, Default)]
pub struct Report {
    /// Fields no consumer reads and the allowlist does not cover.
    pub offenders: Vec<Field>,
    /// Allowlisted paths that ARE now reachable — delete the exemption.
    pub stale: Vec<String>,
    /// Allowlist/prune paths that match no schema field at all — the field was
    /// renamed or removed and the entry was left behind.
    pub unknown: Vec<String>,
    /// How many fields were examined.
    pub examined: usize,
}

impl Report {
    /// Whether the ratchet passes.
    pub fn ok(&self) -> bool {
        self.offenders.is_empty() && self.stale.is_empty() && self.unknown.is_empty()
    }
}

/// **Pure.** Decide the report from an already-collected field set, corpus and
/// allowlist, so the whole ratchet is unit-testable without touching disk.
pub fn evaluate(fields: &[Field], corpus: &str, allow: &Allowlist) -> Report {
    let known: BTreeSet<&str> = fields.iter().map(|f| f.path.as_str()).collect();

    let mut report = Report {
        examined: fields.len(),
        ..Default::default()
    };
    for f in fields {
        // A `#[serde(rename)]` override wins outright; otherwise a field is
        // wired if EITHER shape of its name is mentioned — struct field or
        // externally-tagged enum variant.
        let wired = match allow.rename.iter().find(|r| r.path == f.path) {
            Some(r) => mentions(corpus, &r.ident),
            None => mentions(corpus, &f.ident) || mentions_variant(corpus, &f.variant),
        };
        let exempt = allow.is_allowed(&f.path);
        match (wired, exempt) {
            (false, false) => report.offenders.push(f.clone()),
            (true, true) => report.stale.push(f.path.clone()),
            _ => {}
        }
    }
    // An allow/prune entry matching nothing is dead weight — the field moved or
    // was renamed and the exemption was left behind.
    for e in allow.allow.iter().chain(allow.prune.iter()) {
        if !fields.iter().any(|f| e.matches(&f.path)) {
            report.unknown.push(e.path.clone());
        }
    }
    for r in &allow.rename {
        if !known.contains(r.path.as_str()) {
            report.unknown.push(r.path.clone());
        }
    }
    report.offenders.sort();
    report.stale.sort();
    report.unknown.sort();
    report
}

/// Run the ratchet against the working tree. Returns the process exit code.
pub fn run() -> Result<i32> {
    let allow = Allowlist::load()?;
    let fields = schema_fields(&allow);
    let corpus = consumer_corpus()?;
    let report = evaluate(&fields, &corpus, &allow);

    if report.ok() {
        println!(
            "check-wiring: OK ({} CRD fields examined, {} allowlisted)",
            report.examined,
            allow.allow.len()
        );
        return Ok(0);
    }
    if !report.offenders.is_empty() {
        eprintln!(
            "check-wiring: {} CRD field(s) are defined and schema-generated but read by NO consumer crate.\n\
             Each is inert: users can set it and nothing happens (see #346 / #351).\n\
             Wire it up, delete it from `crates/api`, or add it to {ALLOWLIST_REL} with a reason.\n",
            report.offenders.len()
        );
        for f in &report.offenders {
            eprintln!("  {}  (looked for `{}`)", f.path, f.ident);
        }
        eprintln!();
    }
    if !report.stale.is_empty() {
        eprintln!(
            "check-wiring: {} allowlist entr(ies) are now WIRED — delete them from {ALLOWLIST_REL}:\n",
            report.stale.len()
        );
        for p in &report.stale {
            eprintln!("  {p}");
        }
        eprintln!();
    }
    if !report.unknown.is_empty() {
        eprintln!(
            "check-wiring: {} allowlist entr(ies) match no CRD field — renamed or removed:\n",
            report.unknown.len()
        );
        for p in &report.unknown {
            eprintln!("  {p}");
        }
    }
    Ok(1)
}

#[cfg(test)]
mod tests;
