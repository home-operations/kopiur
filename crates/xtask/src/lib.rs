#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

pub mod artifact;
pub mod crds;
pub mod dashboards;
pub mod docs;
pub mod paths;
pub mod phases;
pub mod rbac;
pub mod scan;
pub mod ui_types;
pub mod wiring;

use anyhow::Result;

use artifact::{Artifact, check_all, write_all};

/// Collect the artifacts a subcommand is responsible for.
pub fn collect(cmd: &str) -> Result<Vec<Artifact>> {
    Ok(match cmd {
        "gen-crds" => crds::artifacts()?,
        "gen-rbac" => rbac::artifacts()?,
        "gen-docs" => docs::artifacts()?,
        "gen-all" => {
            let mut v = crds::artifacts()?;
            v.extend(rbac::artifacts()?);
            v.extend(dashboards::artifacts()?);
            v.extend(docs::artifacts()?);
            v
        }
        other => anyhow::bail!("unknown command {other}"),
    })
}

/// Run an artifact-generating subcommand. Returns the process exit code to use.
///
/// `check-wiring` and `check-phases` are deliberately NOT routed here: they
/// produce no artifact and have no write mode, so they would have no meaning as
/// [`collect`] arms. Each has its own entry point, [`wiring::run`] and
/// [`phases::run`].
///
/// `gen-ui-types` IS routed here — it is a generated artifact and belongs to the
/// same drift gate — but it cannot be a [`collect`] arm either: `ts-rs` writes
/// its files itself instead of handing back content, and the tree it owns needs
/// a stale-file sweep the content-keyed [`Artifact`] machinery has no notion of.
/// It therefore runs beside the artifacts, and `gen-all` takes the worse of the
/// two exit codes so a TypeScript drift can never be masked by clean YAML.
pub fn run(cmd: &str, check: bool) -> Result<i32> {
    if cmd == "gen-ui-types" {
        return ui_types::run(check);
    }
    let code = run_artifacts(cmd, check)?;
    if cmd != "gen-all" {
        return Ok(code);
    }
    Ok(code.max(ui_types::run(check)?))
}

/// The [`Artifact`]-keyed half of [`run`]: generate in memory, then write or
/// compare against the checked-in files.
fn run_artifacts(cmd: &str, check: bool) -> Result<i32> {
    let artifacts = collect(cmd)?;
    if check {
        if check_all(&artifacts)? {
            println!("{cmd} --check: OK (no drift, {} files)", artifacts.len());
            Ok(0)
        } else {
            eprintln!("{cmd} --check: DRIFT detected — run `cargo xtask {cmd}` and commit");
            Ok(1)
        }
    } else {
        write_all(&artifacts)?;
        println!("{cmd}: wrote {} files", artifacts.len());
        Ok(0)
    }
}
