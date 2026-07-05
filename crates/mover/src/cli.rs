//! The mover's command-line surface: `ready` / `serve [path]` subcommands plus
//! the run-once default mode, with `KOPIUR_*` env fallback for the flags.
//!
//! The argv shapes are a deployed-cluster contract — pod specs already stamped
//! into running clusters invoke this binary as `kopiur-mover` (run-once Job,
//! spec path via `KOPIUR_WORK_SPEC_PATH`), `kopiur-mover serve` (server
//! Deployment, spec path via `KOPIUR_SERVER_SPEC_PATH`) and `kopiur-mover
//! ready` (browse-session readinessProbe). Every one of those must keep
//! parsing byte-identically; the tests below are the regression guard.
//!
//! The two spec paths deliberately do NOT use `#[arg(env)]`: their positional
//! → env fallback stays manual (in `main.rs`'s `work_spec_path` /
//! `server_spec_path` helpers), which preserves the exact argv > env
//! precedence and keeps env-filled positionals out of clap's
//! positional-vs-subcommand resolution.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::env::{KOPIA_BINARY, RESULT_CONFIGMAP};

/// The mover binary's command line. No subcommand = a run-once operation
/// (snapshot/restore/delete/bootstrap/maintenance/verify/replicate/pin): the
/// *operation* is selected by the work-spec JSON, never by argv.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "kopiur-mover",
    version,
    about = "Kopiur mover: runs kopia work (backup/restore/bootstrap/…) inside a Job pod"
)]
pub struct MoverCli {
    /// `ready` / `serve`; absent = run-once mode.
    #[command(subcommand)]
    pub command: Option<MoverCommand>,

    /// Path to the work-spec JSON for a run-once invocation
    /// (falls back to $KOPIUR_WORK_SPEC_PATH).
    #[arg(value_name = "WORK_SPEC_PATH")]
    pub work_spec: Option<PathBuf>,

    /// Override for the kopia binary path (default: `kopia` on PATH).
    // `global` so `kopiur-mover serve --kopia-binary …` also works; the env
    // var applies to every mode today.
    #[arg(long, env = KOPIA_BINARY, global = true)]
    pub kopia_binary: Option<String>,

    /// ConfigMap the bootstrap result is written into (set by the controller
    /// for BootstrapRepository runs only).
    // `global` (not run-once-scoped) so a set KOPIUR_RESULT_CONFIGMAP env can
    // never conflict with a subcommand invocation.
    #[arg(long, env = RESULT_CONFIGMAP, global = true)]
    pub result_configmap: Option<String>,
}

impl MoverCli {
    /// The kopia-binary override, with an empty value meaning "unset" (an
    /// empty `KOPIUR_KOPIA_BINARY` must not become an empty program path).
    pub fn kopia_binary(&self) -> Option<&str> {
        self.kopia_binary.as_deref().filter(|s| !s.is_empty())
    }

    /// The bootstrap-result ConfigMap name, with an empty value meaning
    /// "unset" (preserves the pre-clap `!n.is_empty()` filter).
    pub fn result_configmap(&self) -> Option<&str> {
        self.result_configmap.as_deref().filter(|s| !s.is_empty())
    }
}

/// The mover's non-run-once entrypoints. Derived subcommand names (`ready`,
/// `serve`) are byte-identical to the argv already baked into deployed pod
/// specs — renaming a variant breaks running clusters.
#[derive(Debug, Clone, Subcommand)]
pub enum MoverCommand {
    /// Browse-session readiness probe: exit 0 iff the session marker exists.
    Ready,
    /// Long-lived kopia web-UI server (connect, then exec `kopia server start`).
    Serve {
        /// Path to the server work-spec JSON
        /// (falls back to $KOPIUR_SERVER_SPEC_PATH).
        #[arg(value_name = "SERVER_SPEC_PATH")]
        spec: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Every parsing test is `#[serial]`: clap consults the process env for
    // every `env = ...` field on every parse, so a test that mutates the env
    // could poison a concurrently-running parse (the repo's env-test idiom).

    fn parse(args: &[&str]) -> MoverCli {
        MoverCli::try_parse_from(std::iter::once("kopiur-mover").chain(args.iter().copied()))
            .expect("args must parse")
    }

    // --- the deployed-cluster argv contract (jobs.rs / server/mod.rs /
    // browse session probe): each shape must keep parsing exactly as the
    // hand-rolled dispatch did. ---

    #[test]
    #[serial]
    fn no_args_is_run_once_with_env_spec_path() {
        let cli = parse(&[]);
        assert!(cli.command.is_none());
        assert_eq!(cli.work_spec, None);
    }

    #[test]
    #[serial]
    fn positional_path_is_run_once() {
        let cli = parse(&["/spec/work.json"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.work_spec, Some(PathBuf::from("/spec/work.json")));
    }

    #[test]
    #[serial]
    fn ready_is_the_probe_mode() {
        assert!(matches!(
            parse(&["ready"]).command,
            Some(MoverCommand::Ready)
        ));
    }

    #[test]
    #[serial]
    fn serve_without_path_uses_env_spec_path() {
        match parse(&["serve"]).command {
            Some(MoverCommand::Serve { spec: None }) => {}
            other => panic!("expected bare serve, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn serve_with_path() {
        match parse(&["serve", "/spec/server.json"]).command {
            Some(MoverCommand::Serve { spec: Some(p) }) => {
                assert_eq!(p, PathBuf::from("/spec/server.json"));
            }
            other => panic!("expected serve with path, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn kopia_binary_flag_works_in_both_modes() {
        assert_eq!(
            parse(&["--kopia-binary", "/opt/kopia"]).kopia_binary(),
            Some("/opt/kopia")
        );
        // `global = true`: valid after the subcommand too.
        assert_eq!(
            parse(&["serve", "--kopia-binary", "/opt/kopia"]).kopia_binary(),
            Some("/opt/kopia")
        );
    }

    #[test]
    #[serial]
    fn empty_overrides_mean_unset() {
        let cli = parse(&["--kopia-binary=", "--result-configmap="]);
        assert_eq!(cli.kopia_binary(), None);
        assert_eq!(cli.result_configmap(), None);
    }

    #[test]
    #[serial]
    fn result_configmap_env_does_not_conflict_with_subcommands() {
        // A set KOPIUR_RESULT_CONFIGMAP must never break a `serve`/`ready`
        // invocation (the flags are global precisely so env-provided values
        // can't collide with subcommand parsing).
        // SAFETY: serialized by #[serial] against every other test in this module.
        unsafe { std::env::set_var(RESULT_CONFIGMAP, "bootstrap-result") };
        let cli = MoverCli::try_parse_from(["kopiur-mover", "serve"]);
        unsafe { std::env::remove_var(RESULT_CONFIGMAP) };
        let cli = cli.expect("serve must parse with the env set");
        assert_eq!(cli.result_configmap(), Some("bootstrap-result"));
    }

    #[test]
    #[serial]
    fn kopia_binary_env_fallback_and_flag_precedence() {
        // SAFETY: serialized by #[serial] against every other test in this module.
        unsafe { std::env::set_var(KOPIA_BINARY, "/env/kopia") };
        let from_env = parse(&[]);
        let from_flag = parse(&["--kopia-binary", "/flag/kopia"]);
        unsafe { std::env::remove_var(KOPIA_BINARY) };
        assert_eq!(from_env.kopia_binary(), Some("/env/kopia"));
        assert_eq!(from_flag.kopia_binary(), Some("/flag/kopia"));
    }

    // clap derive self-check: catches attribute mistakes (conflicting ids,
    // bad defaults, positional/subcommand clashes) that only surface at
    // runtime otherwise.
    #[test]
    fn clap_debug_assert() {
        use clap::CommandFactory as _;
        MoverCli::command().debug_assert();
    }
}
