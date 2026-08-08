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
pub fn run(cmd: &str, check: bool) -> Result<i32> {
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
