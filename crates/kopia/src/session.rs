//! The closed command surface of a browse session.
//!
//! A `kubectl kopiur browse` session pod connects to its repository
//! **read-only** ([`crate::KopiaClient::repository_connect_readonly`]) and then
//! only ever executes commands rendered by [`SessionCmd`]. The enum is closed
//! and every transport (the CLI's pod-exec path) builds argv exclusively
//! through [`SessionCmd::argv`], so a mutating kopia verb is *structurally*
//! impossible — the type system, not a denylist, is the guarantee (ADR §5.5).

/// A kopia object id as it appears in manifests. Validated so a repository
/// entry can never smuggle a flag onto the session argv.
///
/// kopia's id space is wider than plain hex: content ids (`k…`/`x…`), indirect
/// ids (`Ix…`), and *section* ids (`S<start>,<length>,<base>`) all appear in
/// real manifests, and entry names carry through into ids that hold `-`, `_`
/// or `.`. The gate is therefore a **deny-list**, not an allow-list: the only
/// threat is argv flag injection (the session never goes through a shell —
/// [`SessionCmd::argv`] is exec'd directly), so a legitimate-but-unforeseen id
/// form must not be rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectId(String);

/// A string that is not a well-formed kopia object id, naming the value and the
/// rule it broke.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid kopia object id {value:?}: {reason}")]
pub struct InvalidObjectId {
    /// The rejected value, verbatim.
    pub value: String,
    /// Which rule it broke (e.g. `must not start with '-'`).
    pub reason: &'static str,
}

impl ObjectId {
    /// Parse an object id read out of a repository manifest, rejecting only
    /// what could subvert the session's argv or its logs: an empty string, a
    /// leading `-` (kopia would read the id as a flag), a `/` (a path, never an
    /// id), whitespace (argv word-splitting in anything that re-renders the
    /// command), and control/non-ASCII bytes (terminal escapes in error text).
    /// Everything else — commas, dashes, dots, underscores — is a shape kopia
    /// itself emits and passes through.
    pub fn parse(s: &str) -> Result<Self, InvalidObjectId> {
        let reject = |reason: &'static str| {
            Err(InvalidObjectId {
                value: s.to_string(),
                reason,
            })
        };
        if s.is_empty() {
            return reject("must not be empty");
        }
        if s.starts_with('-') {
            return reject("must not start with '-', which kopia would read as a flag");
        }
        if s.contains('/') {
            return reject("must not contain '/'");
        }
        if s.bytes().any(|b| b.is_ascii_whitespace()) {
            return reject("must not contain whitespace");
        }
        if s.bytes().any(|b| !b.is_ascii() || b.is_ascii_control()) {
            return reject("must be printable ASCII");
        }
        Ok(Self(s.to_string()))
    }

    /// The validated id, for rendering onto argv.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The ONLY kopia invocations a browse session may issue. Closed enum — the
/// CLI/exec transport renders argv exclusively through this, so a mutating
/// verb is structurally impossible. A new variant cannot compile until
/// [`SessionCmd::argv`] (and its read-only test) accounts for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCmd {
    /// `kopia snapshot list --json --all`: the repository's full snapshot
    /// catalog (every user@host identity), parsed as
    /// [`Vec<SnapshotListEntry>`](crate::SnapshotListEntry).
    SnapshotListJson,
    /// `kopia show <oid>`: a directory object's manifest
    /// ([`DirManifest`](crate::DirManifest) JSON) or a file object's raw bytes.
    ShowObject {
        /// The kopia object id to show (a directory's manifest or a file's
        /// content stream).
        oid: ObjectId,
    },
}

impl SessionCmd {
    /// Render the full argv (binary first) for this command. Exhaustive
    /// `match`: a new session command must decide its argv here to compile.
    pub fn argv(&self, kopia_bin: &str) -> Vec<String> {
        match self {
            SessionCmd::SnapshotListJson => vec![
                kopia_bin.to_string(),
                "snapshot".to_string(),
                "list".to_string(),
                "--json".to_string(),
                "--all".to_string(),
            ],
            SessionCmd::ShowObject { oid } => {
                vec![
                    kopia_bin.to_string(),
                    "show".to_string(),
                    oid.as_str().to_string(),
                ]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One representative of every variant, so the read-only sweep below cannot
    /// silently skip a new command.
    fn all_commands() -> Vec<SessionCmd> {
        vec![
            SessionCmd::SnapshotListJson,
            SessionCmd::ShowObject {
                oid: ObjectId::parse("kdeadbeef").expect("valid oid"),
            },
        ]
    }

    #[test]
    fn snapshot_list_renders_the_exact_argv() {
        assert_eq!(
            SessionCmd::SnapshotListJson.argv("/usr/local/bin/kopia"),
            vec![
                "/usr/local/bin/kopia",
                "snapshot",
                "list",
                "--json",
                "--all"
            ]
        );
    }

    #[test]
    fn show_object_renders_the_exact_argv() {
        let cmd = SessionCmd::ShowObject {
            oid: ObjectId::parse("k9c0ffee").expect("valid oid"),
        };
        assert_eq!(
            cmd.argv("/usr/local/bin/kopia"),
            vec!["/usr/local/bin/kopia", "show", "k9c0ffee"]
        );
    }

    #[test]
    fn object_id_accepts_every_shape_kopia_actually_emits() {
        // Plain content id, a *section* id (commas), a manifest entry whose id
        // carries a dash, and an indirect id — all real kopia forms. An
        // allow-list of alphanumerics would false-reject the last three and
        // break browsing on real repositories.
        // `k;rm` is accepted deliberately: a shell metacharacter is not a
        // threat here — argv is exec'd directly as a list (no shell anywhere on
        // the session path), so the leading-flag prefix is the only injection
        // vector, and an allow-list that rejected `;` would also have to guess
        // which of kopia's id shapes are legal.
        for good in ["k1a2b3", "S12,34,kabc", "kfile-a", "Ixdeadbeef", "k;rm"] {
            let oid = ObjectId::parse(good).unwrap_or_else(|e| panic!("{good:?} must parse: {e}"));
            assert_eq!(oid.as_str(), good);
        }
    }

    #[test]
    fn object_id_rejects_anything_that_could_smuggle_a_flag() {
        // Every rejection names the value and the rule it broke.
        for (bad, needle) in [
            ("", "must not be empty"),
            ("-h", "must not start with '-'"),
            ("--config-file=/x", "must not start with '-'"),
            ("a b", "must not contain whitespace"),
            ("a\nb", "must not contain whitespace"),
            ("a/b", "must not contain '/'"),
            ("k/../x", "must not contain '/'"),
            ("k\u{7f}rm", "must be printable ASCII"),
        ] {
            let err =
                ObjectId::parse(bad).expect_err(&format!("{bad:?} must not parse as an object id"));
            assert_eq!(err.value, bad);
            let msg = err.to_string();
            assert!(msg.contains(needle), "{bad:?}: {msg}");
            assert!(msg.contains(&format!("{bad:?}")), "{bad:?}: {msg}");
        }
    }

    #[test]
    fn no_variant_renders_a_mutating_verb() {
        // The structural guarantee, asserted: no session command's argv may
        // ever contain a kopia verb that can change the repository.
        const MUTATING: &[&str] = &[
            "delete",
            "create",
            "set",
            "policy",
            "maintenance",
            "restore",
        ];
        for cmd in all_commands() {
            let argv = cmd.argv("kopia");
            for verb in MUTATING {
                assert!(
                    !argv.iter().any(|a| a == verb),
                    "{cmd:?} renders mutating verb {verb}: {argv:?}"
                );
            }
        }
    }
}
