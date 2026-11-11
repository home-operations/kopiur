//! `gen-kopia-flags` — snapshot the flags kopia's help lists for every command
//! Kopiur invokes, into `crates/api/tests/fixtures/kopia-flags/`.
//!
//! The snapshots are the input to the hermetic ratchet in
//! `crates/api/tests/kopia_flag_coverage.rs`, which fails when a flag in them has
//! no reviewed entry in `kopiur_api::kopia_flags` (or an entry names a flag kopia
//! dropped). This subcommand is the only thing that needs a kopia binary: it runs
//! the mise-pinned `kopia help <command>` (override with `KOPIA_BIN`), so it is
//! part of `mise run gen` / `gen-check` but NOT of [`crate::collect`], whose
//! callers include the hermetic test suite.
//!
//! Each file starts with the kopia version, so bumping kopia in
//! `.mise/config.toml` without regenerating fails `gen-check` even when no flag
//! changed; regenerating then surfaces every new flag to the ratchet.
//!
//! Normalized line format, one flag per line, sorted: `--name=V` (valued),
//! `--[no-]name` (boolean), optionally prefixed `-x, ` for a single-letter alias.
//! Per-command files list only the command's OWN flags (globals live in
//! `global.txt`); `storage_<backend>.txt` lists only the backend's flags beyond
//! `repository connect`'s, which this generator verifies are identical under
//! `repository create` and `repository sync-to`.

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Context, Result, bail};
use kopiur_api::kopia_flags::{KopiaCommand, StorageBackend};

use crate::artifact::{Artifact, check_all, write_all};

/// Where the snapshots live, relative to the workspace root.
pub const FIXTURE_DIR: &str = "crates/api/tests/fixtures/kopia-flags";

/// One parsed flag line: long name → its normalized rendering.
type FlagSet = BTreeMap<String, String>;

/// Generate (or `--check`) the snapshots. Returns the exit code.
pub fn run(check: bool) -> Result<i32> {
    let artifacts = artifacts()?;
    if check {
        if check_all(&artifacts)? {
            println!(
                "gen-kopia-flags --check: OK (no drift, {} files)",
                artifacts.len()
            );
            Ok(0)
        } else {
            eprintln!(
                "gen-kopia-flags --check: DRIFT detected — run `mise run gen` (with the \
                 pinned kopia on PATH), then classify any new flag the \
                 kopia_flag_coverage test names"
            );
            Ok(1)
        }
    } else {
        write_all(&artifacts)?;
        println!("gen-kopia-flags: wrote {} files", artifacts.len());
        Ok(0)
    }
}

/// Build every snapshot by running kopia.
pub fn artifacts() -> Result<Vec<Artifact>> {
    let kopia = Kopia::locate()?;
    let version = kopia.version()?;
    let global = parse_flags(&kopia.help(&[])?);
    let mut out = vec![render("global.txt", &version, "kopia", &global)];

    for cmd in KopiaCommand::ALL {
        let words = cmd.argv();
        let own = minus(parse_flags(&kopia.help(words)?), &global);
        out.push(render(
            &cmd.fixture_name(),
            &version,
            &format!("kopia {}", words.join(" ")),
            &own,
        ));
    }

    let verb_flags =
        |verb: &str| -> Result<FlagSet> { Ok(parse_flags(&kopia.help(&["repository", verb])?)) };
    let connect = verb_flags("connect")?;
    let create = verb_flags("create")?;
    let sync_to = verb_flags("sync-to")?;
    for backend in StorageBackend::ALL {
        let name = backend.argv_name();
        let storage = minus(
            parse_flags(&kopia.help(&["repository", "connect", name])?),
            &connect,
        );
        if !backend.connect_only() {
            for (verb, base) in [("create", &create), ("sync-to", &sync_to)] {
                let other = minus(parse_flags(&kopia.help(&["repository", verb, name])?), base);
                if other != storage {
                    bail!(
                        "kopia's `repository {verb} {name}` storage flags differ from \
                         `repository connect {name}`'s, so one table per backend no longer \
                         covers all three verbs. Fix: split StorageBackend's table per verb \
                         in crates/api/src/kopia_flags"
                    );
                }
            }
        }
        out.push(render(
            &backend.fixture_name(),
            &version,
            &format!("kopia repository connect {name}"),
            &storage,
        ));
    }
    Ok(out)
}

/// The kopia binary to run.
struct Kopia(String);

impl Kopia {
    fn locate() -> Result<Self> {
        let bin = std::env::var("KOPIA_BIN")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "kopia".to_string());
        Ok(Self(bin))
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.0).args(args).output().with_context(|| {
            format!(
                "running `{} {}` — gen-kopia-flags needs the mise-pinned kopia on PATH. \
                 Fix: run it as `mise run gen` (or set KOPIA_BIN)",
                self.0,
                args.join(" ")
            )
        })?;
        if !out.status.success() {
            bail!(
                "`{} {}` exited {}: {}",
                self.0,
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // kingpin prints help on stdout; fall back to stderr if a build differs.
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        Ok(if stdout.trim().is_empty() {
            String::from_utf8_lossy(&out.stderr).into_owned()
        } else {
            stdout
        })
    }

    fn version(&self) -> Result<String> {
        let v = self.run(&["--version"])?;
        v.split_whitespace()
            .next()
            .map(str::to_string)
            .context("`kopia --version` printed nothing")
    }

    fn help(&self, words: &[&str]) -> Result<String> {
        let mut args = vec!["help"];
        args.extend_from_slice(words);
        self.run(&args)
    }
}

/// Parse the `Flags:` section of kingpin help output.
///
/// Only lines inside `Flags:` count (a command's description can quote flags,
/// e.g. `snapshot restore` mentions `--no-overwrite-files`), and only lines
/// indented like a flag column: kingpin wraps descriptions onto lines indented
/// far deeper, and those can start with `--` too (`--help-long and --help-man).`).
pub fn parse_flags(help: &str) -> FlagSet {
    let mut set = FlagSet::new();
    let mut in_flags = false;
    for line in help.lines() {
        let trimmed = line.trim_start();
        if line.starts_with("Flags:") {
            in_flags = true;
            continue;
        }
        if line.starts_with("Args:") || line.starts_with("Commands:") {
            in_flags = false;
            continue;
        }
        let indent = line.len() - trimmed.len();
        if !in_flags || indent > 8 || !trimmed.starts_with('-') {
            continue;
        }
        if let Some((name, rendered)) = parse_flag_line(trimmed) {
            set.insert(name, rendered);
        }
    }
    set
}

/// `-p, --password=PASSWORD  Repository password.` → (`password`, `-p, --password=V`).
fn parse_flag_line(line: &str) -> Option<(String, String)> {
    let (short, rest) = match line.strip_prefix('-') {
        Some(after) if !after.starts_with('-') => {
            let c = after.chars().next()?;
            let rest = after.get(c.len_utf8()..)?.strip_prefix(", ")?;
            (Some(c), rest)
        }
        _ => (None, line),
    };
    let rest = rest.strip_prefix("--")?;
    let (bool_flag, rest) = match rest.strip_prefix("[no-]") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if name.is_empty() {
        return None;
    }
    let valued = rest[name.len()..].starts_with('=');
    let mut rendered = String::new();
    if let Some(c) = short {
        rendered.push_str(&format!("-{c}, "));
    }
    match (bool_flag, valued) {
        (true, _) => rendered.push_str(&format!("--[no-]{name}")),
        (false, true) => rendered.push_str(&format!("--{name}=V")),
        // A bare `--name` with neither marker: kingpin prints that for a
        // non-negatable boolean. Record it as a boolean.
        (false, false) => rendered.push_str(&format!("--[no-]{name}")),
    }
    Some((name, rendered))
}

fn minus(mut set: FlagSet, base: &FlagSet) -> FlagSet {
    set.retain(|k, _| !base.contains_key(k));
    set
}

fn render(file: &str, version: &str, source: &str, flags: &FlagSet) -> Artifact {
    let mut content = format!(
        "# kopia {version}: flags of `{source}`{scope}\n\
         # GENERATED by `cargo xtask gen-kopia-flags` (`mise run gen`) — DO NOT EDIT.\n",
        scope = if file == "global.txt" {
            " (accepted by every command)"
        } else {
            " beyond the shared ones"
        }
    );
    for line in flags.values() {
        content.push_str(line);
        content.push('\n');
    }
    Artifact::repo(format!("{FIXTURE_DIR}/{file}"), content)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELP: &str = "\
usage: kopia snapshot restore [<flags>] <sources>...

Restore a directory.

--no-overwrite-files --no-overwrite-directories

Flags:
      --[no-]help                Show context-sensitive help (also try
                                 --help-long and --help-man).
  -p, --password=PASSWORD        Repository password.
      --description=DESCRIPTIONFree-form snapshot description.
      --parallel=8               Restore parallelism
  -a, --[no-]all                 All

Args:
  <sources>  --not-a-flag
";

    #[test]
    fn parses_only_the_flag_column_of_the_flags_section() {
        let set = parse_flags(HELP);
        let lines: Vec<&str> = set.values().map(String::as_str).collect();
        assert_eq!(
            lines,
            vec![
                "-a, --[no-]all",
                "--description=V",
                "--[no-]help",
                "--parallel=V",
                "-p, --password=V",
            ]
        );
    }
}
