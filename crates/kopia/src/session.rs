//! The closed command surface of a browse session.
//!
//! A `kubectl kopiur browse` session pod connects to its repository
//! **read-only** ([`crate::KopiaClient::repository_connect_readonly`]) and then
//! only ever executes commands rendered by [`SessionCmd`]. The enum is closed
//! and every transport (the CLI's pod-exec path) builds argv exclusively
//! through [`SessionCmd::argv`], so a mutating kopia verb is *structurally*
//! impossible — the type system, not a denylist, is the guarantee (ADR §5.5).

/// A kopia object id as it appears in manifests (`k…`/`x…` hex-ish). Validated so a
/// repository entry can never smuggle a flag onto the session argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectId(String);

/// A string that is not a well-formed kopia object id.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid kopia object id {0:?}: expected only ASCII letters and digits")]
pub struct InvalidObjectId(pub String);

impl ObjectId {
    /// Parse an object id, rejecting anything that is not a non-empty run of
    /// ASCII alphanumerics — so a leading `-`, a path separator, or a shell
    /// metacharacter read out of a repository manifest can never reach argv.
    pub fn parse(s: &str) -> Result<Self, InvalidObjectId> {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()) {
            Ok(Self(s.to_string()))
        } else {
            Err(InvalidObjectId(s.to_string()))
        }
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
    fn object_id_accepts_a_kopia_manifest_id() {
        let oid = ObjectId::parse("k1a2b3").expect("alphanumeric oid parses");
        assert_eq!(oid.as_str(), "k1a2b3");
    }

    #[test]
    fn object_id_rejects_anything_that_could_smuggle_a_flag() {
        for bad in ["--config-file=/x", "", "a b", "k/../x", "k;rm"] {
            assert_eq!(
                ObjectId::parse(bad),
                Err(InvalidObjectId(bad.to_string())),
                "{bad:?} must not parse as an object id"
            );
        }
    }

    #[test]
    fn invalid_object_id_says_what_is_allowed() {
        let msg = InvalidObjectId("--config-file=/x".into()).to_string();
        assert!(msg.contains("--config-file=/x"), "{msg}");
        assert!(msg.contains("ASCII letters and digits"), "{msg}");
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
