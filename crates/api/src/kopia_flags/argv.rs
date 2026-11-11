//! Classify an escape-hatch arg list against the flag tables.

use super::{Arity, FlagDisposition, FlagSpec, KopiaCommand, display_spec_path};

/// One refused escape-hatch arg, with a what/why/fix message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraArgViolation {
    /// The arg as written (`--force-hash=100`, or `--force-hash 100` for the
    /// two-token form).
    pub arg: String,
    /// The flag's long name, when the arg is a flag (`force-hash`).
    pub flag: Option<String>,
    /// Ready to show a user: what is wrong, why, and what to do instead.
    pub message: String,
}

/// How one token parsed.
enum Token<'a> {
    /// `--name`, `--name=value`, `--no-name`, `-x`, `-xVALUE`.
    Flag {
        /// The long name the token resolves to (after `no-` stripping, short lookup).
        name: String,
        /// The table entry, when Kopiur knows the flag.
        spec: Option<&'static FlagSpec>,
        /// Whether the value is attached (`=value` / `-xVALUE`).
        inline_value: bool,
        raw: &'a str,
    },
    /// `--`: everything after it is positional.
    EndOfFlags,
    /// Anything else.
    Positional(&'a str),
}

fn parse_token<'a>(cmd: KopiaCommand, raw: &'a str) -> Token<'a> {
    if raw == "--" {
        return Token::EndOfFlags;
    }
    if let Some(long) = raw.strip_prefix("--") {
        let (name, inline_value) = match long.split_once('=') {
            Some((n, _)) => (n, true),
            None => (long, false),
        };
        // kingpin booleans also answer to `--no-<name>`; only strip the prefix
        // when the un-prefixed name is a known boolean, so a flag that genuinely
        // starts with `no-` still resolves to itself.
        let spec = cmd.lookup(name).or_else(|| {
            name.strip_prefix("no-")
                .and_then(|n| cmd.lookup(n))
                .filter(|s| s.arity == Arity::Bool)
        });
        let name = spec.map_or(name, |s| s.name).to_string();
        return Token::Flag {
            name,
            spec,
            inline_value,
            raw,
        };
    }
    let mut chars = raw.chars();
    if chars.next() == Some('-')
        && let Some(c) = chars.next()
    {
        let spec = cmd.lookup_short(c);
        return Token::Flag {
            name: spec.map_or_else(|| c.to_string(), |s| s.name.to_string()),
            spec,
            inline_value: raw.len() > 2,
            raw,
        };
    }
    Token::Positional(raw)
}

/// Check escape-hatch args destined for `cmd` against its flag table.
///
/// Refused: a flag the table marks [`FlagDisposition::Modeled`] (set the field
/// instead), [`FlagDisposition::OwnedByKopiur`] or [`FlagDisposition::Rejected`],
/// and any bare positional word (it would become another target of the command,
/// e.g. a second `policy set` path). Accepted: [`FlagDisposition::Global`] and
/// [`FlagDisposition::Unmodeled`] flags, and flags the table does not know
/// (kopia itself then refuses a misspelling at run time). Handles `--flag=value`,
/// `--flag value` (the value token is consumed, never mistaken for a positional),
/// `--no-flag` for booleans, and `-x` / `-xVALUE` aliases.
pub fn check_extra_args(cmd: KopiaCommand, args: &[String]) -> Vec<ExtraArgViolation> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let raw = args[i].as_str();
        i += 1;
        match parse_token(cmd, raw) {
            Token::EndOfFlags => {
                // Refused even as the LAST token: Kopiur appends its own common
                // flags (`--no-auto-maintenance`) after the extra args, and `--`
                // would turn them into positionals.
                out.push(end_of_flags_violation(cmd));
                out.extend(
                    args[i..]
                        .iter()
                        .map(|a| positional_violation(cmd, a.as_str())),
                );
                break;
            }
            Token::Positional(word) => out.push(positional_violation(cmd, word)),
            Token::Flag {
                name,
                spec,
                inline_value,
                raw,
            } => {
                // Does this flag swallow the next token as its value? Known valued
                // flags always do (kingpin's behaviour); an unknown flag is assumed to
                // when the next token does not look like a flag, so `--unknown 5`
                // is not reported as a stray positional `5`.
                let takes_next = !inline_value
                    && i < args.len()
                    && match spec {
                        Some(s) => s.arity == Arity::Value,
                        None => !args[i].starts_with('-'),
                    };
                let shown = if takes_next {
                    i += 1;
                    format!("{raw} {}", args[i - 1])
                } else {
                    raw.to_string()
                };
                out.extend(known_flag_violation(
                    cmd,
                    shown,
                    &name,
                    spec,
                    inline_value || takes_next,
                ));
            }
        }
    }
    out
}

/// The violation, if any, for a flag the table knows: its disposition refuses
/// it, or it is valued and ends the list without its value (it would then take
/// Kopiur's own appended `--no-auto-maintenance` as the value).
fn known_flag_violation(
    cmd: KopiaCommand,
    shown: String,
    name: &str,
    spec: Option<&'static FlagSpec>,
    has_value: bool,
) -> Option<ExtraArgViolation> {
    let s = spec?;
    if !s.disposition.allowed_in_extra_args() {
        return Some(flag_violation(cmd, shown, name, &s.disposition));
    }
    (s.arity == Arity::Value && !has_value).then(|| ExtraArgViolation {
        message: format!(
            "`{shown}`: `--{name}` needs a value, and with none after it kopia would \
             take the next flag Kopiur appends as its value. Fix: write `--{name}=<value>`"
        ),
        arg: shown,
        flag: Some(name.to_string()),
    })
}

fn end_of_flags_violation(cmd: KopiaCommand) -> ExtraArgViolation {
    ExtraArgViolation {
        arg: "--".to_string(),
        flag: None,
        message: format!(
            "`--` ends kopia's flag parsing: everything after it, including the flags \
             Kopiur itself appends to `{}`, would be read as positional arguments and the \
             run would fail. Fix: remove it",
            cmd.display()
        ),
    }
}

fn positional_violation(cmd: KopiaCommand, word: &str) -> ExtraArgViolation {
    ExtraArgViolation {
        arg: word.to_string(),
        flag: None,
        message: format!(
            "`{word}` is not a flag: extra args may only carry kopia flags (`--name`, \
             `--name=value` or `--name value`), because a bare word becomes another \
             argument of `{}` (for `policy set`, a second policy target). Fix: remove \
             it, or attach it to its flag as `--name={word}`",
            cmd.display()
        ),
    }
}

fn flag_violation(
    cmd: KopiaCommand,
    shown: String,
    name: &str,
    disposition: &FlagDisposition,
) -> ExtraArgViolation {
    let command = cmd.display();
    let message = match disposition {
        FlagDisposition::Modeled { spec_path } => format!(
            "`{shown}`: Kopiur already sets `--{name}` on `{command}` from \
             {field}, and two sources for one setting would disagree. Fix: remove it and \
             set {field} instead",
            field = display_spec_path(spec_path)
        ),
        FlagDisposition::OwnedByKopiur { why } => format!(
            "`{shown}`: Kopiur controls `--{name}` on `{command}` itself: {why}. Fix: \
             remove it"
        ),
        FlagDisposition::Rejected { why, use_instead } => match use_instead {
            Some(field) => format!(
                "`{shown}`: `--{name}` is not supported on `{command}`: {why}. Fix: remove \
                 it and use {} instead",
                display_spec_path(field)
            ),
            None => format!(
                "`{shown}`: `--{name}` is not supported on `{command}`: {why}. Fix: remove it"
            ),
        },
        // Only reached through a bug in `allowed_in_extra_args`; say so plainly
        // rather than refusing with a misleading reason.
        FlagDisposition::Global { why } | FlagDisposition::Unmodeled { why } => {
            format!("`{shown}`: `--{name}` is allowed ({why})")
        }
    };
    ExtraArgViolation {
        arg: shown,
        flag: Some(name.to_string()),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(cmd: KopiaCommand, args: &[&str]) -> Vec<ExtraArgViolation> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        check_extra_args(cmd, &args)
    }

    #[test]
    fn modeled_flag_is_refused_with_the_field_to_use_in_every_spelling() {
        for form in [&["--force-hash=100"][..], &["--force-hash", "100"]] {
            let v = check(KopiaCommand::SnapshotCreate, form);
            assert_eq!(v.len(), 1, "{form:?}: {v:?}");
            assert_eq!(v[0].flag.as_deref(), Some("force-hash"));
            assert_eq!(v[0].arg, form.join(" "));
            assert!(
                v[0].message
                    .contains("SnapshotPolicy spec.upload.forceHashPercent"),
                "{}",
                v[0].message
            );
        }
    }

    #[test]
    fn negated_boolean_resolves_to_the_flag() {
        let v = check(KopiaCommand::SnapshotCreate, &["--no-fail-fast"]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].flag.as_deref(), Some("fail-fast"));
        assert!(v[0].message.contains("spec.errorHandling.failFast"));
    }

    #[test]
    fn owned_and_rejected_flags_explain_why() {
        let v = check(KopiaCommand::SnapshotCreate, &["--json"]);
        assert!(
            v[0].message.contains("controls `--json`"),
            "{}",
            v[0].message
        );
        let v = check(KopiaCommand::PolicySet, &["--keep-latest", "3"]);
        assert_eq!(v[0].arg, "--keep-latest 3");
        assert!(v[0].message.contains("SnapshotPolicy spec.retention"));
        let v = check(KopiaCommand::SnapshotCreate, &["--start-time=2020-01-01"]);
        assert!(v[0].message.ends_with("Fix: remove it"), "{}", v[0].message);
    }

    #[test]
    fn short_alias_resolves() {
        let v = check(KopiaCommand::PolicySet, &["-p", "hunter2"]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].flag.as_deref(), Some("password"));
        let v = check(KopiaCommand::PolicySet, &["-phunter2"]);
        assert_eq!(v[0].arg, "-phunter2");
    }

    #[test]
    fn unmodeled_global_and_unknown_flags_pass() {
        assert!(
            check(
                KopiaCommand::PolicySet,
                &[
                    "--one-file-system=true",
                    "--max-file-size",
                    "10000",
                    "--log-level=debug",
                    "--some-future-flag",
                    "7",
                ]
            )
            .is_empty()
        );
        assert!(check(KopiaCommand::SnapshotCreate, &["--send-snapshot-report"]).is_empty());
    }

    #[test]
    fn stray_positionals_are_refused() {
        let v = check(KopiaCommand::PolicySet, &["/other/path"]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].flag, None);
        assert!(v[0].message.contains("second policy target"));
        // Everything after `--` is positional (and `--` itself is refused).
        let v = check(KopiaCommand::PolicySet, &["--", "--add-ignore=x"]);
        assert_eq!(v.len(), 2, "{v:?}");
        assert_eq!(v[1].arg, "--add-ignore=x");
    }

    /// Greptile on #717: Kopiur appends its own common flags
    /// (`--no-auto-maintenance`) AFTER the extra args, so a `--` anywhere — even
    /// as the last token — turns them into positionals and the run fails.
    #[test]
    fn end_of_flags_marker_is_refused_even_when_last() {
        for args in [&["--"][..], &["--log-level=debug", "--"], &["--", "x"]] {
            let v = check(KopiaCommand::SnapshotCreate, args);
            assert!(
                v.iter()
                    .any(|x| x.arg == "--" && x.message.contains("`--`")),
                "{args:?}: {v:?}"
            );
        }
    }

    /// Same hazard: a valued flag ending the list would take Kopiur's
    /// appended `--no-auto-maintenance` as its value.
    #[test]
    fn a_trailing_valued_flag_without_its_value_is_refused() {
        let v = check(KopiaCommand::PolicySet, &["--max-file-size"]);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].message.contains("needs a value"), "{}", v[0].message);
        // With its value it is fine, in both spellings.
        assert!(check(KopiaCommand::PolicySet, &["--max-file-size", "10"]).is_empty());
        assert!(check(KopiaCommand::PolicySet, &["--max-file-size=10"]).is_empty());
        // A short valued alias at the end is refused the same way.
        assert!(!check(KopiaCommand::PolicySet, &["-p"]).is_empty());
    }

    #[test]
    fn a_boolean_never_swallows_the_next_token() {
        // `--fail-fast` is boolean, so `/x` after it is a positional, not its value.
        let v = check(
            KopiaCommand::SnapshotCreate,
            &["--send-snapshot-report", "/x"],
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].arg, "/x");
    }
}
