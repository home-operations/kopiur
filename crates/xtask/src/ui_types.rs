//! `gen-ui-types` — export the `kopiur-ui-model` wire types to TypeScript.
//!
//! `ts-rs` writes the files itself rather than returning their content, so the
//! [`crate::artifact::Artifact`] write/check machinery every other subcommand
//! shares does not apply here. This subcommand exports into a scratch directory
//! under `target/` and then either syncs it into the tree (`gen`) or reports the
//! difference (`--check`). One source of truth: the Rust types.
//!
//! # Two rules that are easy to get wrong
//!
//! **A stale file is drift too.** `run(false)` deletes any `*.ts` in
//! [`UI_TYPES_DIR`] the export no longer produces, so deleting a wire type in
//! Rust removes its TypeScript instead of leaving a file the SPA can still
//! import. The sweep is deliberately confined to that one directory and to
//! `*.ts` files directly inside it: `ts-rs` emits no barrel, so the SPA's
//! `src/api/types.ts` re-export file is hand-maintained and lives *outside*
//! `src/api/types/` precisely so this sweep can never reach it.
//!
//! **Generated output is normalized before it is written.** `ts-rs` emits lines
//! with trailing whitespace, and the repo's `.editorconfig` sets
//! `trim_trailing_whitespace = true` — so a byte-exact gate over raw `ts-rs`
//! output would go red the first time any editor saved one of these files.
//! Rather than carve an `.editorconfig` exemption for the directory (which only
//! binds editorconfig-aware tools, and holes the invariant the rest of the repo
//! keeps), [`normalize`] strips trailing whitespace at generation time. The
//! checked-in tree therefore already satisfies the repo's own whitespace rule,
//! trimming it is a no-op, and the drift comparison stays a plain byte compare.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::workspace_root;

/// Directory the SPA's generated types live in, relative to the repo root.
pub const UI_TYPES_DIR: &str = "crates/ui/web/src/api/types";

/// The SPA's hand-maintained re-export barrel, relative to the repo root.
///
/// Deliberately a *sibling* of [`UI_TYPES_DIR`] rather than an `index.ts` inside
/// it: the stale-file sweep owns everything in that directory, and `ts-rs` emits
/// no barrel of its own, so one placed inside would be deleted on the next
/// regeneration.
pub const UI_TYPES_BARREL: &str = "crates/ui/web/src/api/types.ts";

/// The fix hint printed next to every reported difference.
const FIX_HINT: &str = "run `mise run gen` (or `cargo xtask gen-ui-types`) and commit the result";

/// A scratch directory that removes itself when it goes out of scope.
///
/// Hand-rolled rather than pulling in `tempfile`: a handful of lines beats a new
/// dependency, and keeping the directory under `target/` means a killed process
/// leaves its debris somewhere `cargo clean` already sweeps.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Scratch {
    /// Create (or re-create, empty) the scratch export directory.
    ///
    /// The PID suffix keeps two concurrent `cargo xtask` invocations — or a test
    /// running beside one — from exporting into each other's directory.
    fn new() -> Result<Self> {
        let dir = workspace_root()
            .join("target")
            .join("xtask")
            .join(format!("ui-types-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating scratch directory {}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

/// Strip trailing horizontal whitespace from every line and end with exactly
/// one newline. See the module docs for why this happens at generation time.
///
/// ```
/// # use xtask::ui_types::normalize;
/// assert_eq!(normalize("type A = {   \n  b: string;\t\n}"), "type A = {\n  b: string;\n}\n");
/// assert_eq!(normalize(""), "");
/// ```
pub fn normalize(src: &str) -> String {
    if src.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        out.push_str(line.trim_end_matches([' ', '\t']));
        out.push('\n');
    }
    out
}

/// The set of `*.ts` file names directly inside `dir`.
///
/// A directory that does not exist yet is an empty set, not an error: that is
/// the very first `gen-ui-types` run, before anything has been checked in.
fn list_ts(dir: &Path) -> Result<BTreeSet<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading an entry of {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".ts") && entry.path().is_file() {
            names.insert(name);
        }
    }
    Ok(names)
}

/// Describe how one file differs between the two trees, or `None` if it matches.
fn diff_one(name: &str, exported: &Path, checked_in: &Path) -> Result<Option<String>> {
    let left = std::fs::read(exported.join(name)).ok();
    let right = std::fs::read(checked_in.join(name)).ok();
    Ok(match (left, right) {
        (Some(a), Some(b)) if a == b => None,
        (Some(_), Some(_)) => Some(format!("changed: {name} differs from the generated export")),
        (Some(_), None) => Some(format!(
            "missing: {name} is exported but absent from {UI_TYPES_DIR}"
        )),
        (None, Some(_)) => Some(format!(
            "stale:   {name} is in {UI_TYPES_DIR} but no longer exported"
        )),
        (None, None) => None,
    })
}

/// Compare an exported tree against the checked-in one, returning one
/// human-readable line per difference (missing, extra, or changed file).
pub fn compare_trees(exported: &Path, checked_in: &Path) -> Result<Vec<String>> {
    let mut names = list_ts(exported)?;
    names.extend(list_ts(checked_in)?);
    let mut drift = Vec::new();
    for name in &names {
        if let Some(line) = diff_one(name, exported, checked_in)? {
            drift.push(line);
        }
    }
    Ok(drift)
}

/// Export every wire type into `dir` and normalize what `ts-rs` wrote.
pub fn export_to(dir: &Path) -> Result<()> {
    kopiur_ui_model::export_all(dir).with_context(|| {
        format!(
            "exporting kopiur-ui-model TypeScript into {}",
            dir.display()
        )
    })?;
    for name in list_ts(dir)? {
        let path = dir.join(&name);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let normalized = normalize(&raw);
        if normalized != raw {
            std::fs::write(&path, &normalized)
                .with_context(|| format!("normalizing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Mirror `exported` into `target`, deleting `*.ts` the export no longer
/// produces. Returns `(written, deleted)`.
fn sync_tree(exported: &Path, target: &Path) -> Result<(usize, usize)> {
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    let (fresh, existing) = (list_ts(exported)?, list_ts(target)?);
    let mut written = 0usize;
    for name in &fresh {
        let (src, dst) = (exported.join(name), target.join(name));
        let content = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
        if std::fs::read(&dst).ok().as_deref() == Some(content.as_slice()) {
            continue;
        }
        std::fs::write(&dst, &content).with_context(|| format!("writing {}", dst.display()))?;
        written += 1;
    }
    let mut deleted = 0usize;
    for name in existing.difference(&fresh) {
        let path = target.join(name);
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        println!("removed stale {}", path.display());
        deleted += 1;
    }
    Ok((written, deleted))
}

/// Export into a scratch directory, then write the tree or report its drift.
///
/// Returns the process exit code to use — `0` on success, `1` when `--check`
/// found any difference — matching [`crate::run`]'s contract.
pub fn run(check: bool) -> Result<i32> {
    let scratch = Scratch::new()?;
    export_to(scratch.path())?;
    let target = workspace_root().join(UI_TYPES_DIR);

    if !check {
        let (written, deleted) = sync_tree(scratch.path(), &target)?;
        println!(
            "gen-ui-types: {} up to date ({written} written, {deleted} removed)",
            target.display()
        );
        return Ok(0);
    }

    let drift = compare_trees(scratch.path(), &target)?;
    if drift.is_empty() {
        println!(
            "gen-ui-types --check: OK (no drift, {} files)",
            list_ts(&target)?.len()
        );
        return Ok(0);
    }
    eprintln!("gen-ui-types --check: DRIFT detected in {UI_TYPES_DIR}");
    for line in &drift {
        eprintln!("  {line}");
    }
    eprintln!("  fix: {FIX_HINT}");
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_reported_as_drift() {
        let exported = scratch("missing-export");
        let checked_in = scratch("missing-tree");
        std::fs::write(exported.join("Problem.ts"), "export type Problem = {};\n").unwrap();

        let drift = compare_trees(&exported, &checked_in).expect("compare");

        assert!(
            drift.iter().any(|d| d.contains("Problem.ts")),
            "a file present in the export and absent from the tree must be drift, got {drift:?}"
        );
    }

    #[test]
    fn a_changed_file_is_reported_as_drift() {
        let exported = scratch("changed-export");
        let checked_in = scratch("changed-tree");
        std::fs::write(
            exported.join("Problem.ts"),
            "export type Problem = { a: string };\n",
        )
        .unwrap();
        std::fs::write(
            checked_in.join("Problem.ts"),
            "export type Problem = { b: string };\n",
        )
        .unwrap();

        let drift = compare_trees(&exported, &checked_in).expect("compare");

        assert!(
            drift.iter().any(|d| d.contains("Problem.ts")),
            "differing bytes must be drift, got {drift:?}"
        );
    }

    #[test]
    fn identical_trees_are_not_drift() {
        let exported = scratch("identical-export");
        let checked_in = scratch("identical-tree");
        std::fs::write(exported.join("Problem.ts"), "export type Problem = {};\n").unwrap();
        std::fs::write(checked_in.join("Problem.ts"), "export type Problem = {};\n").unwrap();

        let drift = compare_trees(&exported, &checked_in).expect("compare");

        assert!(
            drift.is_empty(),
            "identical trees are not drift, got {drift:?}"
        );
    }

    /// A type deleted in Rust must take its `.ts` with it, or the SPA keeps
    /// importing a shape the server no longer serializes.
    #[test]
    fn a_stale_file_is_reported_as_drift() {
        let exported = scratch("stale-export");
        let checked_in = scratch("stale-tree");
        std::fs::write(checked_in.join("Removed.ts"), "export type Removed = {};\n").unwrap();

        let drift = compare_trees(&exported, &checked_in).expect("compare");

        assert!(
            drift.iter().any(|d| d.contains("Removed.ts")),
            "a file the export no longer produces must be drift, got {drift:?}"
        );
    }

    /// Only `*.ts` participates, so a sibling `README.md` or a subdirectory is
    /// neither drift nor a sweep target.
    #[test]
    fn non_typescript_files_are_ignored() {
        let exported = scratch("ignore-export");
        let checked_in = scratch("ignore-tree");
        std::fs::write(checked_in.join("README.md"), "# generated\n").unwrap();
        std::fs::create_dir_all(checked_in.join("nested")).unwrap();

        let drift = compare_trees(&exported, &checked_in).expect("compare");

        assert!(
            drift.is_empty(),
            "only .ts files are compared, got {drift:?}"
        );
    }

    /// The barrel is a sibling of the generated directory, not a file inside it,
    /// so no `gen-ui-types` run can sweep it. Asserted on the constants rather
    /// than on behaviour because the sweep never *sees* the barrel — which is
    /// exactly the property worth pinning.
    #[test]
    fn the_barrel_lives_outside_the_swept_directory() {
        let barrel = Path::new(UI_TYPES_BARREL);
        assert!(
            !barrel.starts_with(UI_TYPES_DIR),
            "{UI_TYPES_BARREL} must not sit inside {UI_TYPES_DIR}: the stale-file \
             sweep owns that directory and would delete the barrel"
        );
        assert_eq!(
            barrel.parent(),
            Path::new(UI_TYPES_DIR).parent(),
            "the barrel must be a sibling of the generated directory"
        );
        assert!(
            workspace_root().join(UI_TYPES_BARREL).is_file(),
            "the SPA's re-export barrel is missing; the generated types are \
             unusable without it"
        );
    }

    /// The barrel is hand-maintained, so nothing stops a new wire type from
    /// being exported to `.ts` and never re-exported. This is what stops it.
    #[test]
    fn the_barrel_reexports_every_generated_type() {
        let generated: BTreeSet<String> = list_ts(&workspace_root().join(UI_TYPES_DIR))
            .expect("list generated types")
            .iter()
            .map(|n| n.trim_end_matches(".ts").to_owned())
            .collect();
        let barrel = std::fs::read_to_string(workspace_root().join(UI_TYPES_BARREL))
            .expect("read the barrel");
        let exported: BTreeSet<String> = barrel
            .lines()
            .filter_map(|l| l.strip_prefix("export type { "))
            .filter_map(|l| l.split_once(" }"))
            .map(|(name, _)| name.to_owned())
            .collect();

        assert!(!generated.is_empty(), "no generated types were found");
        assert_eq!(
            generated,
            exported,
            "{UI_TYPES_BARREL} is out of step with {UI_TYPES_DIR}. Missing from \
             the barrel: {:?}; re-exported but not generated: {:?}",
            generated.difference(&exported).collect::<Vec<_>>(),
            exported.difference(&generated).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn normalize_strips_trailing_whitespace_and_pins_a_final_newline() {
        assert_eq!(normalize("a   \nb\t\t\n"), "a\nb\n");
        assert_eq!(normalize("no trailing newline"), "no trailing newline\n");
        assert_eq!(normalize("  keeps: indent;   \n"), "  keeps: indent;\n");
        assert_eq!(normalize(""), "");
    }

    /// The real export must survive normalization: no line may keep trailing
    /// whitespace (`.editorconfig` would trim it and the byte-exact gate would
    /// go red), and re-exporting must produce the identical bytes.
    #[test]
    fn the_real_export_is_normalized_and_idempotent() {
        let first = scratch("real-first");
        let second = scratch("real-second");
        export_to(&first).expect("export");
        export_to(&second).expect("re-export");

        let files = list_ts(&first).expect("list");
        assert!(
            files.len() > 50,
            "expected the whole wire contract, got {} files",
            files.len()
        );

        for name in &files {
            let body = std::fs::read_to_string(first.join(name)).unwrap();
            for (i, line) in body.lines().enumerate() {
                assert_eq!(
                    line.trim_end_matches([' ', '\t']),
                    line,
                    "{name}:{} keeps trailing whitespace after normalization",
                    i + 1
                );
            }
            assert!(
                body.ends_with('\n'),
                "{name} must end with exactly one newline"
            );
        }

        let drift = compare_trees(&first, &second).expect("compare");
        assert!(
            drift.is_empty(),
            "the export must be deterministic, got {drift:?}"
        );
    }

    /// `run(false)` must remove a `.ts` the export no longer produces, and
    /// `run(true)` must then be clean — the write/check round trip the CI gate
    /// depends on.
    #[test]
    fn sync_writes_the_export_and_sweeps_stale_files() {
        let exported = scratch("sync-export");
        let target = scratch("sync-target");
        std::fs::write(exported.join("Kept.ts"), "export type Kept = {};\n").unwrap();
        std::fs::write(target.join("Gone.ts"), "export type Gone = {};\n").unwrap();
        std::fs::write(target.join("keep-me.txt"), "not typescript\n").unwrap();

        let (written, deleted) = sync_tree(&exported, &target).expect("sync");

        assert_eq!((written, deleted), (1, 1), "one written, one swept");
        assert!(
            target.join("Kept.ts").is_file(),
            "the export must be written"
        );
        assert!(
            !target.join("Gone.ts").exists(),
            "a stale type must be swept"
        );
        assert!(
            target.join("keep-me.txt").is_file(),
            "the sweep must not reach non-TypeScript files"
        );
        assert!(
            compare_trees(&exported, &target)
                .expect("compare")
                .is_empty(),
            "a synced tree must not drift"
        );
    }

    /// A scratch directory for one test case, emptied on creation.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kopiur-xtask-ui-types-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
