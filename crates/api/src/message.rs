//! A tiny, dependency-free builder for **operator-facing** diagnostic messages.
//!
//! Kopiur's house style is that every message a human reads — an admission
//! denial, a Warning Event `note`, a `status.conditions[].message` — says
//! *what* failed, *why*, and *how to fix it*. That rule was enforced only by
//! discipline and code review. [`Diagnostic`] makes the **shape** mechanical:
//! it renders in one canonical order — **lead → why → fix** — so the specific
//! problem is always first.
//!
//! Leading with the specific problem is not cosmetic. `kubectl get` truncates a
//! condition message to its column width, and a `kubectl describe` reader scans
//! the first clause of a wall of Events. If the lead is a generic
//! `"reconcile failed: …"` or the raw first line of a stack of kopia stderr, the
//! useful part is exactly what gets cut. A tight lead survives truncation; the
//! *why* and *fix* trail behind it where there is room.
//!
//! This module is pure `core::fmt` — no `serde`, no `kube`, no `tokio` — so both
//! `kopiur-api` (validators) and the controller/mover can build messages the same
//! way without pulling controller-runtime into the API crate.
//!
//! ```
//! use kopiur_api::message::Diagnostic;
//!
//! let msg = Diagnostic::new("a repository lock is held by another writer")
//!     .fix("it usually clears on its own; retry")
//!     .to_string();
//! assert_eq!(msg, "a repository lock is held by another writer. Fix: it usually clears on its own; retry");
//!
//! // Lead alone is a valid message; why + fix are optional.
//! assert_eq!(Diagnostic::new("nothing to do").to_string(), "nothing to do");
//! ```

use std::borrow::Cow;
use std::fmt;

/// A structured operator-facing message rendered as **lead → why → fix**.
///
/// Build it with [`Diagnostic::new`] (the lead — the specific problem, stated
/// tightly), then optionally chain [`because`](Self::because) (why it happened)
/// and [`fix`](Self::fix) (the concrete remedy: a field to set, a command to
/// run, an expected value). `Display` renders the canonical string.
///
/// Content sources are `&'static str` (fixed prose) or `String` (an interpolated
/// `format!`); a borrowed non-`'static` `&str` intentionally does not fit, which
/// keeps callers from smuggling a short-lived borrow into a persisted status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    summary: Cow<'static, str>,
    because: Option<Cow<'static, str>>,
    fix: Option<Cow<'static, str>>,
}

impl Diagnostic {
    /// Start a diagnostic from its **lead**: the specific problem, stated as a
    /// short clause with no trailing punctuation (e.g. `"source PVC data/db does
    /// not exist"`). This is the part that must survive `kubectl get` truncation.
    pub fn new(summary: impl Into<Cow<'static, str>>) -> Self {
        Self {
            summary: summary.into(),
            because: None,
            fix: None,
        }
    }

    /// Add the **why**: the cause or consequence, as a clause (no leading/trailing
    /// period). Rendered after the lead, joined with ` — `.
    #[must_use]
    pub fn because(mut self, why: impl Into<Cow<'static, str>>) -> Self {
        self.because = Some(why.into());
        self
    }

    /// Add the **fix**: the concrete remedy, imperative (e.g. `"set
    /// spec.create.enabled: true"`). Rendered last, introduced by `. Fix: `.
    #[must_use]
    pub fn fix(mut self, how: impl Into<Cow<'static, str>>) -> Self {
        self.fix = Some(how.into());
        self
    }

    /// The canonical rendered string (identical to `Display`/`to_string`).
    pub fn render(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(trim_clause(&self.summary))?;
        if let Some(because) = &self.because {
            let because = trim_clause(because);
            if !because.is_empty() {
                write!(f, " — {because}")?;
            }
        }
        if let Some(fix) = &self.fix {
            let fix = trim_clause(fix);
            if !fix.is_empty() {
                write!(f, ". Fix: {fix}")?;
            }
        }
        Ok(())
    }
}

/// Trim surrounding whitespace and trailing sentence punctuation from a clause so
/// the renderer can add exactly the connectors it wants without doubling them
/// (`"foo." + " — bar"` would read `"foo. — bar"`).
fn trim_clause(s: &str) -> &str {
    s.trim()
        .trim_end_matches(|c: char| c == '.' || c == ';' || c == ',' || c.is_whitespace())
}

/// Report why `msg` violates the operator-message shape rules, or `None` if it is
/// well-formed. Pure and always-compiled so tests in **any** crate (validators in
/// `kopiur-api`, event/condition builders in the controller and mover) can assert
/// their user-facing strings against one checker.
///
/// The rules are deliberately conservative — they flag the failure modes this
/// overhaul removes, not stylistic taste, so they can run over the existing
/// (already-good) messages without false positives:
///
/// * empty / whitespace-only,
/// * a doubled space or a `". ."` gap (copy/format slips),
/// * a leaked volatile kopia temp fragment (`.shards` / `.tmp.<hex>`) — these must
///   only ever live in `status.failure.stderrTail`, never in a built message,
/// * a generic filler lead (`"error:"`, `"failed:"`, …) that buries the specific
///   problem behind a word truncation would waste,
/// * absurd length (> 900 chars) — the anti-ramble backstop.
pub fn message_shape_issue(msg: &str) -> Option<String> {
    let trimmed = msg.trim();
    if trimmed.is_empty() {
        return Some("message is empty or whitespace-only".to_string());
    }
    if msg.contains("  ") {
        return Some("message contains a doubled space".to_string());
    }
    if msg.contains(". .") {
        return Some("message contains a `. .` gap".to_string());
    }
    if msg.contains(".shards") || contains_temp_hex_fragment(msg) {
        return Some(
            "message leaks a volatile kopia temp-path fragment (belongs only in \
             status.failure.stderrTail)"
                .to_string(),
        );
    }
    let lead = trimmed.to_ascii_lowercase();
    const FILLER_LEADS: &[&str] = &[
        "error:",
        "error ",
        "failed:",
        "failed ",
        "failure:",
        "an error occurred",
        "unknown error",
        "invalid input",
    ];
    for filler in FILLER_LEADS {
        if lead.starts_with(filler) {
            return Some(format!(
                "lead starts with the generic filler {filler:?}; lead with the specific problem"
            ));
        }
    }
    if trimmed.chars().count() > 900 {
        return Some(format!(
            "message is {} chars (> 900); likely a ramble — trim to what/why/fix",
            trimmed.chars().count()
        ));
    }
    None
}

/// Detect a kopia per-attempt temp suffix like `.tmp.9f3ac1` (a `.tmp.` followed
/// by hex). These are random per run and are the classic volatility that must
/// never reach a condition/event message.
fn contains_temp_hex_fragment(msg: &str) -> bool {
    let bytes = msg.as_bytes();
    if let Some(pos) = msg.find(".tmp.") {
        let after = &bytes[pos + ".tmp.".len()..];
        // At least two hex digits immediately after `.tmp.` marks the random suffix.
        let hex = after.iter().take_while(|b| b.is_ascii_hexdigit()).count();
        return hex >= 2;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_lead_only() {
        assert_eq!(
            Diagnostic::new("source PVC data/db does not exist").to_string(),
            "source PVC data/db does not exist"
        );
    }

    #[test]
    fn renders_lead_why_fix_in_order() {
        let msg = Diagnostic::new("the storage backend denied access")
            .because("the credentials Secret may lack permission, or the bucket does not exist")
            .fix("verify the credentials Secret and that the bucket exists")
            .to_string();
        assert_eq!(
            msg,
            "the storage backend denied access — the credentials Secret may lack permission, \
             or the bucket does not exist. Fix: verify the credentials Secret and that the \
             bucket exists"
        );
        // The lead — the specific problem — is first, so it survives truncation.
        assert!(msg.starts_with("the storage backend denied access"));
    }

    #[test]
    fn fix_without_because() {
        assert_eq!(
            Diagnostic::new("a repository lock is held by another writer")
                .fix("it usually clears on its own; retry")
                .to_string(),
            "a repository lock is held by another writer. Fix: it usually clears on its own; retry"
        );
    }

    #[test]
    fn trims_trailing_punctuation_so_connectors_do_not_double() {
        // Callers may pass clauses with their own trailing period; the renderer
        // must not produce `foo. — bar` or `bar.. Fix:`.
        let msg = Diagnostic::new("the mover pod is stuck.")
            .because("the securityContext is invalid for the namespace's Pod Security policy.")
            .fix("fix mover.securityContext, then re-run.")
            .to_string();
        assert_eq!(
            msg,
            "the mover pod is stuck — the securityContext is invalid for the namespace's Pod \
             Security policy. Fix: fix mover.securityContext, then re-run"
        );
        assert!(!msg.contains(". —"));
        assert!(!msg.contains(".."));
    }

    #[test]
    fn empty_optional_clauses_are_dropped() {
        assert_eq!(
            Diagnostic::new("nothing to do")
                .because("   ")
                .fix("")
                .to_string(),
            "nothing to do"
        );
    }

    #[test]
    fn accepts_static_and_owned() {
        let name = "db";
        let _owned = Diagnostic::new(format!("source PVC {name} missing"));
        let _static = Diagnostic::new("source PVC missing");
    }

    #[test]
    fn diagnostic_output_is_well_formed() {
        let msg = Diagnostic::new("the repository backend is unreachable")
            .because("the endpoint did not answer within the connect deadline")
            .fix("check the endpoint/network and retry")
            .to_string();
        assert_eq!(message_shape_issue(&msg), None, "{msg}");
    }

    #[test]
    fn shape_checker_flags_empty() {
        assert!(message_shape_issue("").is_some());
        assert!(message_shape_issue("   ").is_some());
    }

    #[test]
    fn shape_checker_flags_doubled_space_and_gap() {
        assert!(message_shape_issue("two  spaces").is_some());
        assert!(message_shape_issue("a gap . . here").is_some());
    }

    #[test]
    fn shape_checker_flags_generic_filler_lead() {
        assert!(message_shape_issue("error: something went wrong").is_some());
        assert!(message_shape_issue("failed: could not connect").is_some());
        assert!(message_shape_issue("an error occurred while reconciling").is_some());
        // A specific lead that merely contains those words later is fine.
        assert_eq!(
            message_shape_issue("spec.retention keeps nothing; every keep* bucket is 0"),
            None
        );
        // The InvalidFieldValue prefix names the field, so it is specific enough.
        assert_eq!(
            message_shape_issue("invalid value for spec.sources[0].nfs.path: must be absolute"),
            None
        );
    }

    #[test]
    fn shape_checker_flags_volatile_temp_fragments() {
        assert!(
            message_shape_issue("unable to create /repo/.shards.tmp.9f3ac1: permission denied")
                .is_some()
        );
        assert!(message_shape_issue("wrote /cache/.tmp.a1b2 then failed").is_some());
        // A plain path with no hex temp suffix is fine.
        assert_eq!(
            message_shape_issue("repository path /repo is not writable by the operator's UID"),
            None
        );
    }

    #[test]
    fn shape_checker_flags_rambles() {
        let ramble = "x".repeat(950);
        assert!(message_shape_issue(&ramble).is_some());
    }

    #[test]
    fn shape_checker_passes_representative_real_messages() {
        // A sample of the existing house-style strings must pass unchanged — the
        // checker enforces the failure modes we remove, not stylistic taste.
        let samples = [
            "repository.namespace must not be set when repository.kind is ClusterRepository \
             (a ClusterRepository is referenced by name only; got namespace \"prod\")",
            "the storage backend denied access; check the credentials Secret and that the \
             bucket/container/path exists and is reachable",
            "spec.server.auth.insecure requires acknowledgeInsecure: true — a no-auth kopia \
             server exposes full read/write/delete of every backup with no login",
            "invalid value for spec.sync.parallel: must be >= 1 (got 0)",
        ];
        for s in samples {
            assert_eq!(message_shape_issue(s), None, "unexpected issue for: {s}");
        }
    }
}
