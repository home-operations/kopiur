//! Shared source-scanning primitives for the xtask ratchets.
//!
//! Both ratchets — [`crate::wiring`] (is this CRD field read by anyone?) and
//! [`crate::phases`] (is this phase branch exhaustive?) — answer their question
//! by reading the workspace's own `.rs` files as *text*. Neither parses Rust:
//! a parser is a dependency, a build-time cost, and a new way for the check to
//! fail on valid source. Text scanning can only ever be conservative, which is
//! the property both checks need.
//!
//! What lives here is the part they share:
//!
//! * [`scrub`] — remove comments and string/char-adjacent literals, so a name
//!   that appears only in a doc comment or an error message is not mistaken for
//!   code;
//! * [`strip_cfg_test`] — remove `#[cfg(test)]` items, so fixtures do not count;
//! * [`strip_use_stmts`] — remove imports, so a `use` is not mistaken for a use;
//! * [`sources`] — the crate-scoped `.rs` walker, excluding test files.
//!
//! # Line-preserving vs collapsing
//!
//! [`wiring`](crate::wiring) concatenates every consumer file into one corpus
//! and asks only "does this identifier appear anywhere", so it does not care
//! where the text came from and collapses each removed span to a single space.
//! [`phases`](crate::phases) reports a *file and line*, so it needs the removed
//! spans to keep their newlines. That is the only difference, and it is spelled
//! [`Lines`] rather than duplicated: [`scrub`]/[`strip_cfg_test`] are the
//! collapsing forms kept byte-identical to what `wiring` used before the
//! extraction, and [`scrub_lines`]/[`strip_cfg_test_lines`] are the
//! line-preserving forms.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::workspace_root;

/// What a scrubber leaves behind where it removed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lines {
    /// Replace the removed span with a single space. Shortest output; 1-based
    /// line numbers computed over it are meaningless.
    Collapse,
    /// Replace the removed span with a space plus the newlines it contained, so
    /// every surviving character keeps its original **line** number. Columns
    /// are still not preserved.
    Preserve,
}

impl Lines {
    /// The replacement text for a removed span, given the span's own text: a
    /// single space, plus (in [`Lines::Preserve`]) the newlines it contained.
    fn filler(self, removed: &str) -> String {
        let mut s = String::with_capacity(1 + removed.len());
        s.push(' ');
        s.push_str(&self.newlines_only(removed));
        s
    }

    /// The newlines a removed span contained, and nothing else. Used where the
    /// collapsing form emits *no* filler at all (see [`strip_cfg_test_mode`]),
    /// so that mode stays byte-identical to what it replaced.
    fn newlines_only(self, removed: &str) -> String {
        match self {
            Lines::Collapse => String::new(),
            Lines::Preserve => removed.chars().filter(|c| *c == '\n').collect(),
        }
    }
}

// --- scrubbing -------------------------------------------------------------

/// Remove comments and string literals from Rust source, so a name that appears
/// only in a doc comment or an error message is not mistaken for code.
///
/// This is deliberately a character scanner rather than a parser: it must never
/// fail on valid source, and over-removal only makes the callers more
/// conservative.
///
/// ```
/// use xtask::scan::scrub;
/// assert_eq!(scrub("let a = 1; // pvc_selector"), "let a = 1; ");
/// assert!(!scrub(r#"err("pvc_selector missing")"#).contains("pvc_selector"));
/// ```
pub fn scrub(src: &str) -> String {
    scrub_mode(src, Lines::Collapse)
}

/// [`scrub`], but every removed span keeps the newlines it contained, so 1-based
/// line numbers over the result still name the right source line.
///
/// ```
/// use xtask::scan::scrub_lines;
/// let out = scrub_lines("a();\n/* two\nline */\nb();\n");
/// assert_eq!(out.lines().count(), 4);
/// assert!(out.lines().nth(3).unwrap().contains("b()"));
/// ```
pub fn scrub_lines(src: &str) -> String {
    scrub_mode(src, Lines::Preserve)
}

/// The scrubber both [`scrub`] and [`scrub_lines`] are named modes of.
///
/// Handles line comments (`//`, `///`, `//!`), nesting-aware block comments,
/// raw strings (`r"…"`, `r#"…"#`, …) and normal strings with escapes.
pub fn scrub_mode(src: &str, lines: Lines) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // Line comment (covers `//`, `///` and `//!`). The terminating newline
        // is left in place, so a line comment never costs a line either way.
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Block comment, nesting-aware (Rust allows nesting).
        if c == '/' && b.get(i + 1) == Some(&'*') {
            let start = i;
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
            out.push_str(&lines.filler(&b[start..i].iter().collect::<String>()));
            continue;
        }
        // Raw string: r"..." / r#"..."# / r##"..."##
        if c == 'r' {
            let start = i;
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
                out.push_str(&lines.filler(&b[start..j].iter().collect::<String>()));
                i = j;
                continue;
            }
        }
        // Normal string literal.
        if c == '"' {
            let start = i;
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
            out.push_str(&lines.filler(&b[start..i.min(b.len())].iter().collect::<String>()));
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Remove `#[cfg(test)]`-annotated items by brace matching, so fixtures do not
/// count as real code.
///
/// ```
/// use xtask::scan::strip_cfg_test;
/// let s = "fn real() { a(); }\n#[cfg(test)]\nmod tests { fn f() { pvc_selector(); } }\n";
/// assert!(!strip_cfg_test(s).contains("pvc_selector"));
/// assert!(strip_cfg_test(s).contains("real"));
/// ```
pub fn strip_cfg_test(src: &str) -> String {
    strip_cfg_test_mode(src, Lines::Collapse)
}

/// [`strip_cfg_test`], but the removed item's newlines are kept, so 1-based line
/// numbers over the result still name the right source line.
///
/// ```
/// use xtask::scan::strip_cfg_test_lines;
/// let s = "fn real() {}\n#[cfg(test)]\nmod tests {\n  fn f() {}\n}\nfn tail() {}\n";
/// let out = strip_cfg_test_lines(s);
/// assert!(!out.contains("mod tests"));
/// assert_eq!(out.lines().nth(5).unwrap().trim(), "fn tail() {}");
/// ```
pub fn strip_cfg_test_lines(src: &str) -> String {
    strip_cfg_test_mode(src, Lines::Preserve)
}

/// The stripper both [`strip_cfg_test`] and [`strip_cfg_test_lines`] are named
/// modes of.
///
/// A `#[cfg(test)]` item ends at the close of its brace body **or** at the `;`
/// that says it has no body — see [`cfg_test_item_end`]. Only a genuinely
/// unterminated marker drops the rest of the file, which can never do more than
/// make a caller more conservative.
pub fn strip_cfg_test_mode(src: &str, lines: Lines) -> String {
    const MARK: &str = "#[cfg(test)]";
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.find(MARK) {
        out.push_str(&rest[..at]);
        let dropped_from = &rest[at..];
        let after = &rest[at + MARK.len()..];
        match cfg_test_item_end(after) {
            Some(e) => {
                out.push_str(&lines.newlines_only(&after[..e]));
                rest = &after[e..];
            }
            // Neither a body nor a `;` — not valid Rust. Drop the tail rather
            // than risk a runaway.
            None => {
                out.push_str(&lines.newlines_only(dropped_from));
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Where the `#[cfg(test)]` item starting at `after` (the text just past the
/// attribute) ends: one past its closing `}`, or one past the `;` of a
/// **body-less** item.
///
/// The `;` case is the bug this function exists for. The repo's dominant
/// convention is `#[cfg(test)]\nmod tests;` — a declaration with no body, whose
/// contents live in a separate `tests.rs` the walker already skips. Scanning to
/// "the next `{`" for one of those lands on *the next item's* body and deletes
/// everything in between: measured on `crates/controller/src/snapshot/mod.rs`
/// that silently removed lines 72–109, including all of `pub async fn reconcile`,
/// from the scan corpus. So the first `;` and the first `{` race, and whichever
/// comes first wins.
///
/// Depth-tracked over `(`/`[`, and blind to nothing: comments, string literals
/// and char literals are stepped over, so none of them can supply a `;`, a `{`
/// or a `}` that decides the item's extent. That matters in **both**
/// directions and this function runs before [`scrub`], so it cannot lean on it:
///
/// * `#[cfg(test)] fn f() -> [u8; 4] { … }` — a `;` inside a type must not win
///   the race, or the body leaks back into the corpus;
/// * `#[cfg(test)]\n// fixtures; see docs\nmod tests { … }` — a `;` inside a
///   *comment* must not win it either. That failure is silent
///   **over-inclusion**: the whole test module stays in the corpus and starts
///   counting as production code;
/// * `mod tests { let s = "}"; }` — a brace inside a literal must not close the
///   body early.
///
/// ```
/// use xtask::scan::cfg_test_item_end;
/// // Body-less declaration: ends at the `;`.
/// assert_eq!(cfg_test_item_end("\nmod tests;\nfn real() {}"), Some(11));
/// // With a body: ends at the matching close brace.
/// assert_eq!(cfg_test_item_end(" mod t { fn f() {} }"), Some(20));
/// // A `;` in a comment does not make a braced module look body-less.
/// assert_eq!(cfg_test_item_end(" // fixtures; see docs\nmod t { }"), Some(32));
/// ```
pub fn cfg_test_item_end(after: &str) -> Option<usize> {
    let b = after.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        if let Some(next) = trivia_end(b, i) {
            i = next;
            continue;
        }
        match b[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b';' if depth == 0 => return Some(i + 1),
            b'{' if depth == 0 => return brace_group_end(b, i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// One past the `}` matching the `{` at `open`, skipping trivia.
fn brace_group_end(b: &[u8], open: usize) -> Option<usize> {
    let mut d = 0usize;
    let mut i = open;
    while i < b.len() {
        if let Some(next) = trivia_end(b, i) {
            i = next;
            continue;
        }
        match b[i] {
            b'{' => d += 1,
            b'}' => {
                d -= 1;
                if d == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// If `b[i]` opens a comment or a literal, the index just past it.
///
/// Returns `None` for ordinary code, including a lifetime (`'a`), which must not
/// be mistaken for an unterminated char literal.
fn trivia_end(b: &[u8], i: usize) -> Option<usize> {
    match b[i] {
        // Line comment, to the newline (which is left in place).
        b'/' if b.get(i + 1) == Some(&b'/') => Some(
            b[i..]
                .iter()
                .position(|c| *c == b'\n')
                .map_or(b.len(), |n| i + n),
        ),
        // Block comment, nesting-aware like the language.
        b'/' if b.get(i + 1) == Some(&b'*') => {
            let mut depth = 1usize;
            let mut j = i + 2;
            while j < b.len() && depth > 0 {
                if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            Some(j)
        }
        // Raw string: r"…" / r#"…"# / r##"…"##
        b'r' => {
            let hashes = b[i + 1..].iter().take_while(|c| **c == b'#').count();
            if b.get(i + 1 + hashes) != Some(&b'"') {
                return None;
            }
            let mut j = i + 2 + hashes;
            while j < b.len() {
                if b[j] == b'"' && b[j + 1..].iter().take(hashes).all(|h| *h == b'#') {
                    return Some(j + 1 + hashes);
                }
                j += 1;
            }
            Some(b.len())
        }
        // Normal string, with escapes.
        b'"' => {
            let mut j = i + 1;
            while j < b.len() {
                match b[j] {
                    b'\\' => j += 2,
                    b'"' => return Some(j + 1),
                    _ => j += 1,
                }
            }
            Some(b.len())
        }
        // Char literal — but `'a` is a lifetime, so require the closing quote.
        b'\'' => {
            if b.get(i + 1) == Some(&b'\\') {
                return (i + 2..=(i + 12).min(b.len().saturating_sub(1)))
                    .find(|&j| b.get(j) == Some(&b'\''))
                    .map(|j| j + 1);
            }
            (b.get(i + 2) == Some(&b'\'')).then_some(i + 3)
        }
        _ => None,
    }
}

/// Remove `use ...;` statements (including multi-line brace groups).
///
/// An import is not a use: `use kopiur_api::GroupBy;` contains `::GroupBy` and
/// would otherwise read as dispatch on the type without anyone ever matching on
/// it. Stripping them keeps "wired" meaning "something acts on it".
///
/// ```
/// use xtask::scan::strip_use_stmts;
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

// --- the source walker -----------------------------------------------------

/// Every `.rs` file under `crates/<crate>/src` for each named crate, paired with
/// its **raw** text — no scrubbing, so each caller applies the pipeline (and the
/// [`Lines`] mode) it needs.
///
/// Test code is excluded at the file level: a `tests/` directory and the repo's
/// `foo/tests.rs` convention (`#[cfg(test)] mod tests;`) are both skipped.
/// In-file `#[cfg(test)]` modules are the caller's job via [`strip_cfg_test`].
///
/// Paths are returned workspace-relative with `/` separators, so they are the
/// same strings a human types and an allowlist can key on. The order is the
/// sorted absolute-path order, which is stable across platforms and crates.
pub fn sources(crates: &[&str]) -> Result<Vec<(PathBuf, String)>> {
    let root = workspace_root();
    let mut paths = Vec::new();
    for c in crates {
        let dir = root.join("crates").join(c).join("src");
        if dir.is_dir() {
            collect_rs(&dir, &mut paths)?;
        }
    }
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let raw =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let rel = p.strip_prefix(&root).unwrap_or(&p).to_path_buf();
        out.push((rel, raw));
    }
    Ok(out)
}

/// Recursively collect `.rs` files, skipping the repo's test-code conventions.
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

/// A workspace-relative path as the slash-separated string an allowlist keys on.
///
/// ```
/// use std::path::PathBuf;
/// use xtask::scan::rel_display;
/// assert_eq!(rel_display(&PathBuf::from("crates/cli/src/main.rs")), "crates/cli/src/main.rs");
/// ```
pub fn rel_display(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests;
