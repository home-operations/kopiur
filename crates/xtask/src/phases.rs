//! Phase-handling exhaustiveness ratchet — the guard that catches a *silent*
//! phase branch.
//!
//! # Why this exists
//!
//! The repo's load-bearing idea is that an invalid state is unrepresentable and
//! reconcilers `match` exhaustively, so a new enum variant cannot compile until
//! every handler accounts for it. Four constructs opt out of that guarantee
//! *without the compiler ever saying so*, and they have shipped bugs:
//!
//! * **`matches!`** is definitionally non-exhaustive — it carries an implicit
//!   `_ => false`. `kubectl kopiur doctor`'s `check_stuck` classified stuck work
//!   with `matches!` over `SnapshotPhase`/`RestorePhase`, so `Deleting` read as
//!   terminal and a wedged finalizer was invisible (**#359**, defect 2).
//! * **A `_ =>` arm** in an otherwise exhaustive `match` does the same thing
//!   with more ceremony. When **#351** added `SnapshotPhase::Unchanged` (a
//!   deduped backup is a healthy terminal outcome), two wildcard arms swallowed
//!   it: `metrics.rs::policy_backup_health`'s `_ => continue` left the last
//!   *failed* run as the winning observation forever, pinning the policy
//!   unhealthy so `KopiurSnapshotFailed`'s recovery gate could never clear; and
//!   `snapshot_policy.rs::consecutive_failures`' `_ => {}` dropped `Unchanged`
//!   runs out of the streak entirely. Neither was a compile error.
//! * **`==` / `!=` against one variant** answers a set-shaped question with a
//!   single-variant test. Adding a variant never breaks an equality, so this is
//!   the one class the `Unknown(String)` decode fallback added in this same
//!   change *cannot* catch: the new variant simply inherits the `else`.
//! * **`if let <Enum>::X = …`** is the same single-variant probe again, spelled
//!   so that none of the three scans above can see it — there is no `matches!`,
//!   no `match` block and no `==` anywhere in it.
//!
//! And one drift class with no construct at all:
//!
//! * **A gate condition born controller-side.** #359's actual symptom was that
//!   the reconciler grew a gate (`MoverPermitted=False` /
//!   `PrivilegedMoverNotPermitted`) that the CLI never learned about, so doctor
//!   reported all-green over a permanently parked Snapshot. Shared-by-
//!   construction is now the design (the typed registry in `kopiur_api::gates`),
//!   and this ratchet is what keeps a new condition from quietly skipping it.
//!
//! # What it checks
//!
//! Five rules over the *scrubbed* source (see [`crate::scan`]) of
//! [`SCAN_CRATES`]. Every hit must be covered by an entry in
//! `crates/xtask/phase-allowlist.yaml` carrying a written reason:
//!
//! * **Rule A** — a `matches!(…)` whose arguments name a [`phase_enums`] enum.
//!   All three delimiters (`(`, `[`, `{`) count.
//! * **Rule B** — a `match` block that names a phase enum *at its own level* and
//!   has an arm-initial wildcard: `_ =>`, `_ if … =>`, or a `_` that is the sole
//!   payload of a wrapper pattern (`Some(_) =>`, `Ok(_) =>`, `&_ =>`). The
//!   wrapper case is not a nicety — every phase in this repo is read as
//!   `Option<&Phase>`, so `Some(_)` is the most natural spelling of the next
//!   #351. A named variant with a binding hole (`SnapshotPhase::Unknown(_)`) is
//!   not a wildcard and is deliberately not charged.
//! * **Rule C** — a `pub const …_CONDITION` still defined in
//!   [`CONTROLLER_CONSTS_REL`], i.e. not hoisted into `kopiur_api` where both
//!   the controller and the CLI can share it.
//! * **Rule D** — an `==` / `!=` comparison against a phase-enum variant.
//! * **Rule E** — a `let` pattern (`if let` / `while let` / `let … else`) naming
//!   a phase-enum variant. Rule A's question in a spelling A, B and D all miss.
//!
//! Both directions fail, which is what makes it a ratchet rather than a
//! snapshot: an **uncovered** hit fails (write the reason or rewrite the code),
//! and a **stale** entry — one that matches no hit any more — fails too, so the
//! list drains as the code is paid down instead of rotting. A **duplicate**
//! entry fails as well, because only the first of two identical keys is ever
//! consulted and the second is a reason nobody reads.
//!
//! # Deliberate limits
//!
//! * **`crates/e2e` is not scanned.** The harness asserts on phases as *strings*
//!   (`wait_phase("Succeeded")`), which carry no `Enum::` token for a text
//!   scanner to see. Rules A/B/D/E are structurally blind there. That is
//!   acceptable — e2e is a test tier, and a test asserting a concrete expected
//!   outcome is not a production classification.
//! * **Test code is not scanned**, at the file level (`tests/`, `foo/tests.rs`)
//!   and in-file (`#[cfg(test)]`). Verified zero loss at introduction: no
//!   production-relevant phase classification lives in test code today.
//! * **Not every equality is visible to Rule D.** A compare has to name a
//!   variant *path* for the scanner to see it. Three shapes in the reviewed
//!   Tier-3 inventory do not, and Rule D does not pretend to cover them:
//!   `io/repo.rs::repository_ready` and `request_repository_reverify` compare
//!   against a `ready` **binding** (`== ready` / `!= ready`), and
//!   `snapshot/plan.rs::needs_terminal_pin` is generic over its caller's target
//!   (`observed != Some(target)`). A rewrite of one of those into the same defect
//!   would not be caught here; the `Unknown`-variant fallback and the reviewed
//!   inventory in the PR are what cover them. The same blindness applies to Rule
//!   E: `if let Some(p) = phase.filter(|p| p.is_unknown())` names no variant path
//!   and is invisible, which is a good reason to prefer a named predicate like
//!   [`SnapshotPhase::is_unknown`](kopiur_api::SnapshotPhase::is_unknown) — it
//!   moves the exhaustive `match` into `crates/api` where the compiler guards it.
//! * **Enum discovery is `crates/api`-scoped**, plus [`MOVER_PHASE`] named
//!   explicitly. It is not "every `*Phase` type in the workspace" because
//!   `crates/controller/src/hooks.rs` defines an unrelated `HookPhase` (a
//!   `Pre`/`Post` hook slot, not a CR status phase); folding it in would flag
//!   every hook dispatch as a phase-handling defect. The api half is
//!   self-ratcheting — see [`discover_api_phase_enums`] — so a *sixth* CR phase
//!   enum cannot be added without this list being updated.
//! * **Rule B's wrapper-wildcard charge can over-report.** It asks "does this
//!   block mention a phase enum at its own level, and does it have a wildcard
//!   alternative" — it does not know what the scrutinee's *type* is. So a match
//!   whose wildcard is over something else entirely, but whose arm bodies
//!   happen to name a phase, is charged:
//!
//!   ```text
//!   match res {
//!       Ok(_)  => SnapshotPhase::Succeeded,
//!       Err(_) => SnapshotPhase::Failed,
//!   }
//!   ```
//!
//!   That is exhaustive and correct, and it yields two Rule B findings. Zero
//!   occurrences today. The remedy is an allowlist entry with a reason, which is
//!   the right cost for a shape this rare — the alternative is type inference,
//!   and a scanner that guesses types would start *under*-reporting the case
//!   Rule B exists for. Pinned by
//!   `rule_b_over_reports_a_wildcard_over_a_non_phase_scrutinee` so the limit
//!   stays visible rather than becoming folklore.
//! * **It is a text scanner, not a parser.** It cannot see through a macro that
//!   generates a `match`, and it reports the *file and line* of the construct,
//!   not a call graph. Under-reporting is the intended direction: the check must
//!   never fail on valid source.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths::workspace_root;
use crate::scan;

/// The `crates/api` CR status-phase enums. Self-ratcheted against the real
/// source by [`discover_api_phase_enums`].
pub const API_PHASE_ENUMS: &[&str] = &[
    "ManualRunPhase",
    "RepositoryPhase",
    "RepositoryReplicationPhase",
    "RestorePhase",
    "SnapshotPhase",
    "SnapshotReplicationPhase",
];

/// The mover's own job-outcome phase, which lives in `crates/mover` rather than
/// `crates/api` and so has to be named explicitly.
///
/// It is in scope for the same reason as the CR phases: the controller reads a
/// mover result and turns it into a `Snapshot`/`Restore` phase, so a wildcard
/// there loses the same information one arm earlier.
pub const MOVER_PHASE: &str = "MoverPhase";

/// Every phase enum the ratchet reasons about — [`API_PHASE_ENUMS`] plus
/// [`MOVER_PHASE`].
pub fn phase_enums() -> Vec<&'static str> {
    let mut v = API_PHASE_ENUMS.to_vec();
    v.push(MOVER_PHASE);
    v.sort_unstable();
    v
}

/// Crates whose source is scanned.
///
/// `api` is included even though it is the *definition* site: its own
/// classifications (`is_terminal`, the `phase_serde!` round-trips, the gate
/// registry) are exactly where a `_ =>` would do the most damage, because every
/// consumer inherits the answer.
pub const SCAN_CRATES: &[&str] = &["api", "cli", "controller", "kopia", "mover", "webhook"];

/// The one file Rule C reads: condition-type constants still defined
/// controller-side instead of in `kopiur_api::consts`.
pub const CONTROLLER_CONSTS_REL: &str = "crates/controller/src/consts.rs";

/// Where the reviewed exemptions live, relative to the workspace root.
const ALLOWLIST_REL: &str = "crates/xtask/phase-allowlist.yaml";

// --- rules -----------------------------------------------------------------

/// Which construct a [`Finding`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// `matches!(…)` over a phase enum — implicit `_ => false`.
    NonExhaustiveMatches,
    /// A `match` over a phase enum with an arm-initial `_ =>` / `_ if … =>`.
    WildcardArm,
    /// A `…_CONDITION` const defined controller-side, invisible to the CLI.
    ControllerCondition,
    /// `==` / `!=` against a single phase variant.
    PhaseCompare,
    /// `if let <Enum>::X = …` — Rule A's question, in a spelling no other rule
    /// can see.
    IfLetProbe,
}

impl Rule {
    /// The rule's short label, as the brief and the error output name it.
    pub fn id(self) -> &'static str {
        match self {
            Rule::NonExhaustiveMatches => "A",
            Rule::WildcardArm => "B",
            Rule::ControllerCondition => "C",
            Rule::PhaseCompare => "D",
            Rule::IfLetProbe => "E",
        }
    }

    /// One line saying what the rule flags and what to do about it.
    pub fn title(self) -> &'static str {
        match self {
            Rule::NonExhaustiveMatches => {
                "`matches!` over a phase enum — non-exhaustive by construction \
                 (implicit `_ => false`). Rewrite as an exhaustive `match`."
            }
            Rule::WildcardArm => {
                "a `match` over a phase enum with a `_ =>` arm — a new variant \
                 inherits an answer nobody chose. Name every arm."
            }
            Rule::ControllerCondition => {
                "a gate condition defined controller-side — the CLI cannot see it \
                 (#359). Hoist it to `kopiur_api::consts` and register it in \
                 `kopiur_api::gates`, or declare it CLI-irrelevant."
            }
            Rule::PhaseCompare => {
                "`==`/`!=` against one phase variant — adding a variant never \
                 breaks an equality, so the new variant silently takes the \
                 `else`. Rewrite as an exhaustive classification, or record why \
                 the question really is one bit."
            }
            Rule::IfLetProbe => {
                "`if let` naming one phase variant — the same non-exhaustive \
                 single-variant probe Rule A flags, in a spelling `matches!`, \
                 `match` and `==` scanning all miss. Rewrite as an exhaustive \
                 `match`, or record why the question really is one bit."
            }
        }
    }
}

/// One flagged construct.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// Which rule flagged it.
    pub rule: Rule,
    /// Workspace-relative path, `/`-separated.
    pub file: String,
    /// 1-based line of the construct in the original file.
    pub line: usize,
    /// The construct, whitespace-normalized. This is the allowlist key, and it
    /// is deliberately reflow-proof: rustfmt moving a line break must not
    /// invalidate a reviewed exemption.
    pub snippet: String,
}

impl Finding {
    /// The `(file, canonical snippet)` pair an allowlist entry has to match.
    pub fn key(&self) -> (String, String) {
        (self.file.clone(), normalize_code(&self.snippet))
    }
}

// --- the allowlist file ----------------------------------------------------

/// One reviewed exemption.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    /// Workspace-relative path, `/`-separated — exactly as [`Finding::file`]
    /// prints it. No globs: an exemption names the file it was reviewed in.
    pub file: String,
    /// The construct this exempts, matched **whitespace-normalized** so a
    /// rustfmt reflow does not invalidate it.
    pub snippet: String,
    /// Why this is exempt. Required — an exemption without a reason is how the
    /// list rots into a rubber stamp.
    pub reason: String,
}

impl Entry {
    /// The `(file, canonical snippet)` pair this entry covers.
    pub fn key(&self) -> (String, String) {
        (self.file.clone(), normalize_code(&self.snippet))
    }
}

/// The parsed `phase-allowlist.yaml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Allowlist {
    /// Constructs knowingly left as they are.
    #[serde(default)]
    pub allow: Vec<Entry>,
}

impl Allowlist {
    /// Read and parse the checked-in allowlist.
    pub fn load() -> Result<Self> {
        let p = workspace_root().join(ALLOWLIST_REL);
        let raw =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", p.display()))
    }
}

/// Collapse every run of whitespace to a single space and trim.
///
/// This is what makes a snippet key survive rustfmt: `a\n    == b` and `a == b`
/// are the same key.
///
/// ```
/// use xtask::phases::normalize_ws;
/// assert_eq!(normalize_ws("  a\n   ==\tb "), "a == b");
/// ```
pub fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The canonical form a snippet is *matched* by: [`normalize_ws`], then drop
/// every remaining space that is not between two identifier characters.
///
/// [`normalize_ws`] alone is not reflow-proof, because rustfmt breaking a method
/// chain leaves a space no one-line form has (`repo.status .as_ref()`). In Rust
/// a space only carries meaning between two word tokens (`as u8`, `_ if x`), so
/// dropping the rest is lossless for this purpose and makes the key identical
/// whichever way the formatter laid the expression out. Snippets are stored and
/// printed in the readable [`normalize_ws`] form; only the *key* is canonical.
///
/// ```
/// use xtask::phases::normalize_code;
/// assert_eq!(normalize_code("repo.status\n    .as_ref()"), "repo.status.as_ref()");
/// assert_eq!(normalize_code("repo.status.as_ref()"), "repo.status.as_ref()");
/// assert_eq!(normalize_code("phase != Some(&P::Failed)"), "phase!=Some(&P::Failed)");
/// assert_eq!(normalize_code("_ if n > 3 =>"), "_ if n>3=>");
/// ```
pub fn normalize_code(s: &str) -> String {
    let c: Vec<char> = normalize_ws(s).chars().collect();
    let word = |ch: Option<&char>| ch.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    let mut out = String::with_capacity(c.len());
    for (i, ch) in c.iter().enumerate() {
        if *ch == ' ' && !(word(c.get(i.wrapping_sub(1))) && word(c.get(i + 1))) {
            continue;
        }
        out.push(*ch);
    }
    out
}

// --- text primitives -------------------------------------------------------

/// Whether `text` names any of `enums` as a *path* (`SnapshotPhase::…`).
///
/// The trailing `::` is load-bearing: it is what makes this a use of a variant
/// rather than a mention of the type, so `phase: Option<SnapshotPhase>` does not
/// count. The leading boundary rejects `MySnapshotPhase::` while accepting a
/// qualified `kopiur_api::SnapshotPhase::`.
///
/// ```
/// use xtask::phases::mentions_enum;
/// let e = ["SnapshotPhase"];
/// assert!(mentions_enum("kopiur_api::SnapshotPhase::Failed", &e));
/// assert!(!mentions_enum("p: Option<SnapshotPhase>", &e));
/// assert!(!mentions_enum("MySnapshotPhase::Failed", &e));
/// ```
pub fn mentions_enum(text: &str, enums: &[&str]) -> bool {
    enums.iter().any(|e| {
        let needle = format!("{e}::");
        let mut from = 0;
        while let Some(rel) = text[from..].find(&needle) {
            let start = from + rel;
            let leading_ok = start == 0
                || !text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if leading_ok {
                return true;
            }
            from = start + 1;
        }
        false
    })
}

/// The 1-based line of char index `at` within `chars`.
fn line_of(chars: &[char], at: usize) -> usize {
    1 + chars[..at.min(chars.len())]
        .iter()
        .filter(|c| **c == '\n')
        .count()
}

/// If a `'` at `i` opens a **char literal**, the index of its closing `'`.
///
/// A lifetime (`'a`, `'static`) returns `None`, which is the case that matters:
/// treating `'a` as a literal would swallow the rest of the line. Bounded so a
/// stray quote cannot run away.
fn char_lit_end(c: &[char], i: usize) -> Option<usize> {
    if c.get(i) != Some(&'\'') {
        return None;
    }
    if c.get(i + 1) == Some(&'\\') {
        // `'\n'`, `'\''`, `'\u{1F600}'` — bounded scan for the close.
        //
        // The scan starts at `i + 3`, not `i + 2`, and that is what makes the
        // escaped quote `'\''` work: an escape body is at least one char and
        // begins at `i + 2`, so the terminator can never be there — but for
        // `'\''` the body IS a quote, and a scan from `i + 2` would stop on it
        // and report the literal one char short, leaving the real terminator to
        // be re-read as the start of a fresh literal.
        return (i + 3..=(i + 12).min(c.len().saturating_sub(1)))
            .find(|&j| c.get(j) == Some(&'\''));
    }
    (c.get(i + 2) == Some(&'\'')).then_some(i + 2)
}

/// Index just past the balanced group opened by `c[open] == o`.
///
/// Char literals are stepped over, so `matches!(x, Sep(')'))` still balances.
/// Comments and strings are assumed already removed by [`scan::scrub`].
///
/// ```
/// use xtask::phases::balanced;
/// let c: Vec<char> = "f(a, g(b))tail".chars().collect();
/// assert_eq!(balanced(&c, 1, '(', ')'), Some(10));
/// ```
pub fn balanced(c: &[char], open: usize, o: char, cl: char) -> Option<usize> {
    if c.get(open) != Some(&o) {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open;
    while i < c.len() {
        if let Some(end) = char_lit_end(c, i) {
            i = end + 1;
            continue;
        }
        if c[i] == o {
            depth += 1;
        } else if c[i] == cl {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

/// Whether the char before `at` (if any) can end an identifier.
fn word_before(c: &[char], at: usize) -> bool {
    at > 0 && (c[at - 1].is_alphanumeric() || c[at - 1] == '_')
}

/// Whether `c[at..]` starts with `word` as a whole word.
fn word_at(c: &[char], at: usize, word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    if at + w.len() > c.len() || c[at..at + w.len()] != w[..] {
        return false;
    }
    let after_ok = c
        .get(at + w.len())
        .is_none_or(|n| !(n.is_alphanumeric() || *n == '_'));
    after_ok && !word_before(c, at)
}

// --- Rule A: `matches!` ----------------------------------------------------

/// Every `matches!` invocation in `text`, as `(char index, full text)`.
///
/// The argument list is extracted by balanced delimiters rather than by scanning
/// to the next closer, which is what makes a nested call — or, before scrubbing,
/// the `sweep.rs` case of a `// …)` comment sitting inside the arguments — parse
/// the same as a flat one.
///
/// All three macro-call delimiters are recognized. `matches![…]` and
/// `matches!{…}` are rarer but identical to the compiler, so a rule that only
/// knew `(` could be sidestepped by a formatting choice.
///
/// ```
/// use xtask::phases::matches_calls;
/// let hits = matches_calls("if matches!(p, Some(A::B(_))) { }");
/// assert_eq!(hits.len(), 1);
/// assert_eq!(hits[0].1, "matches!(p, Some(A::B(_)))");
/// assert_eq!(matches_calls("matches![p, A::B]")[0].1, "matches![p, A::B]");
/// assert_eq!(matches_calls("matches!{p, A::B}")[0].1, "matches!{p, A::B}");
/// ```
pub fn matches_calls(text: &str) -> Vec<(usize, String)> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if word_at(&c, i, "matches") && c.get(i + 7) == Some(&'!') {
            // A qualified `std::matches!` is still the same macro; only a
            // preceding identifier char (`x.matches!`) would not be.
            let mut j = i + 8;
            while c.get(j).is_some_and(|ch| ch.is_whitespace()) {
                j += 1;
            }
            let delims = [('(', ')'), ('[', ']'), ('{', '}')];
            if let Some(end) = delims.iter().find_map(|(o, cl)| balanced(&c, j, *o, *cl)) {
                out.push((i, c[i..end].iter().collect::<String>()));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

// --- Rule B: wildcard arms -------------------------------------------------

/// One `match` block located in scrubbed source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchBlock {
    /// Char index of the `match` keyword.
    pub at: usize,
    /// Char index of the first char of the body (just past the block's `{`).
    pub body_at: usize,
    /// The scrutinee text between `match` and the block's `{`, normalized.
    pub scrutinee: String,
    /// The block body, `{`/`}` exclusive.
    pub body: String,
}

/// Every `match … { … }` block in `text`, innermost first.
///
/// "Innermost first" is by body length ascending, which is the same order for
/// any actual nesting and is deterministic for siblings. It matters because
/// [`mask_nested`] then hides each block's inner brace groups, so an outer match
/// is judged only on its own arms.
pub fn match_blocks(text: &str) -> Vec<MatchBlock> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if !word_at(&c, i, "match") {
            i += 1;
            continue;
        }
        // Scan to the block's `{`: the first one not inside a `(`/`[` group, so
        // a closure or an index expression in the scrutinee does not fool it.
        let mut j = i + 5;
        let mut depth = 0i32;
        let mut open = None;
        while j < c.len() {
            if let Some(end) = char_lit_end(&c, j) {
                j = end + 1;
                continue;
            }
            match c[j] {
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                '{' if depth == 0 => {
                    open = Some(j);
                    break;
                }
                ';' if depth == 0 => break,
                _ => {}
            }
            j += 1;
        }
        let Some(open) = open else {
            i += 1;
            continue;
        };
        let Some(end) = balanced(&c, open, '{', '}') else {
            i += 1;
            continue;
        };
        out.push(MatchBlock {
            at: i,
            body_at: open + 1,
            scrutinee: normalize_ws(&c[i + 5..open].iter().collect::<String>()),
            body: c[open + 1..end - 1].iter().collect::<String>(),
        });
        i += 5;
    }
    out.sort_by_key(|b| (b.body.len(), b.at));
    out
}

/// The marker a masked nested brace group is replaced with. Cannot occur in
/// source, so it is unambiguous as an arm boundary.
pub const MASK: char = '\u{1}';

/// Replace every brace group nested inside `body` with [`MASK`], leaving only
/// the match's own arm patterns, guards and brace-less arm bodies.
///
/// This is what makes nesting work without any ordering logic: an inner
/// `match`'s arms live inside *its* `{…}`, which is a brace group at this
/// block's own level, so it is masked out here and judged on its own pass.
///
/// ```
/// use xtask::phases::{mask_nested, MASK};
/// let masked = mask_nested("A::X => { match y { _ => 1 } }, A::Y => 2,");
/// assert!(!masked.contains("_ =>"));
/// assert!(masked.contains(MASK));
/// assert!(masked.contains("A::Y => 2"));
/// ```
pub fn mask_nested(body: &str) -> String {
    let c: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < c.len() {
        if let Some(end) = char_lit_end(&c, i) {
            out.extend(&c[i..=end]);
            i = end + 1;
            continue;
        }
        if c[i] == '{' {
            match balanced(&c, i, '{', '}') {
                Some(end) => {
                    out.push(MASK);
                    // Keep the group's newlines so line numbers still hold.
                    out.extend(c[i..end].iter().filter(|ch| **ch == '\n'));
                    i = end;
                    continue;
                }
                None => break,
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// Every **wildcard arm head** in a masked block body, as `(char index within
/// the masked body, head text)`.
///
/// An arm alternative begins at the start of the body, after a top-level `,`,
/// after a masked brace-bodied arm ([`MASK`]), or after an or-pattern `|`.
/// Requiring that position is what separates a wildcard *arm* from a `_` used as
/// a binding, a tuple hole (`(a, _)`) or a numeric separator.
///
/// An alternative counts as a wildcard when
/// [`pattern_is_wildcard_shape`] accepts it **and** it names no phase enum —
/// see that function for why both halves are needed. The `enums` list is
/// therefore load-bearing here, not just at the block level.
///
/// ```
/// use xtask::phases::wildcard_arm_heads;
/// let e = ["SnapshotPhase"];
/// assert_eq!(wildcard_arm_heads("A::X => 1, _ => 2,", &e).len(), 1);
/// // The `Option<&Phase>` shape every phase in this repo is matched through.
/// assert_eq!(wildcard_arm_heads("Some(SnapshotPhase::Failed) => 1, Some(_) => 2,", &e).len(), 1);
/// // A binding hole inside a NAMED variant is not a wildcard.
/// assert_eq!(wildcard_arm_heads("SnapshotPhase::Unknown(_) => 1, A::Y => 2,", &e).len(), 0);
/// assert_eq!(wildcard_arm_heads("_ if n > 3 => 1, A::Y => 2,", &e).len(), 1);
/// ```
pub fn wildcard_arm_heads(masked_body: &str, enums: &[&str]) -> Vec<(usize, String)> {
    let c: Vec<char> = masked_body.chars().collect();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut arm_start = true;
    let mut i = 0;
    while i < c.len() {
        if let Some(end) = char_lit_end(&c, i) {
            arm_start = false;
            i = end + 1;
            continue;
        }
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if arm_start
            && depth == 0
            && !word_before(&c, i)
            && let Some((pat_end, arrow_end)) = alternative_span(&c, i)
            && is_wildcard_alternative(&c[i..pat_end], enums)
        {
            out.push((i, normalize_ws(&c[i..arrow_end].iter().collect::<String>())));
            // Skip past the arrow: nothing inside this head can start another
            // alternative.
            arm_start = false;
            i = arrow_end;
            continue;
        }
        match ch {
            '(' | '[' => {
                depth += 1;
                arm_start = false;
            }
            ')' | ']' => {
                depth -= 1;
                arm_start = false;
            }
            ',' if depth == 0 => arm_start = true,
            MASK => arm_start = true,
            '|' if depth == 0 => arm_start = true,
            _ => arm_start = false,
        }
        i += 1;
    }
    out
}

/// Whether one arm alternative is a catch-all: [`pattern_is_wildcard_shape`]
/// accepts it AND it names no phase enum. Both halves are needed — see
/// `pattern_is_wildcard_shape` for which direction each one guards.
fn is_wildcard_alternative(pat: &[char], enums: &[&str]) -> bool {
    let pat: String = pat.iter().collect();
    pattern_is_wildcard_shape(&pat) && !mentions_enum(&pat, enums)
}

/// For an arm alternative starting at `i`, `(end of its pattern, end of the
/// arm's `=>`)`.
///
/// The pattern ends at the alternative's own boundary — a depth-0 `|`, the start
/// of an `if` guard, or the `=>` itself — while the arrow is the *arm's*, which
/// may be several alternatives further on. Returns `None` if this is not an arm
/// head at all (no `=>` before the next depth-0 `,`).
fn alternative_span(c: &[char], i: usize) -> Option<(usize, usize)> {
    let mut depth = 0i32;
    let mut j = i;
    let mut pat_end = None;
    while j < c.len() {
        if let Some(end) = char_lit_end(c, j) {
            j = end + 1;
            continue;
        }
        match c[j] {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            MASK => return None,
            ',' if depth == 0 => return None,
            '|' if depth == 0 => pat_end = pat_end.or(Some(j)),
            '=' if depth == 0 && c.get(j + 1) == Some(&'>') => {
                return Some((pat_end.unwrap_or(j), j + 2));
            }
            _ if depth == 0 && word_at(c, j, "if") && pat_end.is_none() => pat_end = Some(j),
            _ => {}
        }
        j += 1;
    }
    None
}

/// Whether a match-arm pattern is *shaped* like a wildcard: a bare `_`, or a `_`
/// that is the sole content of single-depth wrappers — `Some(_)`, `Ok(_)`,
/// `&_`, `Some(&_)`, `ref _`.
///
/// Shape alone is not enough, and the caller must also check the pattern names
/// no phase enum. Both halves are load-bearing in opposite directions:
///
/// * without the shape rule, `Some(_)` slips through — and since **every** phase
///   in this repo is read as `Option<&Phase>`, `Some(_)` is the most natural way
///   to write the next #351, not `_`;
/// * without the enum check, `SnapshotPhase::Unknown(_)` would be charged, and
///   that is a named variant with a binding hole, not a catch-all — flagging it
///   would make the rule fire on correct exhaustive code.
///
/// ```
/// use xtask::phases::pattern_is_wildcard_shape;
/// assert!(pattern_is_wildcard_shape("_"));
/// assert!(pattern_is_wildcard_shape("Some(_)"));
/// assert!(pattern_is_wildcard_shape("Some( & _ )"));
/// assert!(pattern_is_wildcard_shape("&_"));
/// // A tuple has no single sole payload…
/// assert!(!pattern_is_wildcard_shape("(_, _)"));
/// // …and a named payload is not a hole.
/// assert!(!pattern_is_wildcard_shape("Some(A::B)"));
/// assert!(!pattern_is_wildcard_shape("None"));
/// ```
pub fn pattern_is_wildcard_shape(pat: &str) -> bool {
    let p = pat.trim();
    let p = p
        .strip_prefix("ref ")
        .or_else(|| p.strip_prefix("&mut "))
        .or_else(|| p.strip_prefix('&'))
        .unwrap_or(p)
        .trim();
    if p == "_" {
        return true;
    }
    // `PATH ( INNER )`, where the parens span the whole remainder. A tuple
    // pattern has an empty PATH and is rejected here, which is what keeps
    // `(_, _)` and `(SnapshotPhase::Failed, _)` from being charged.
    let Some(open) = p.find('(') else {
        return false;
    };
    if !p.ends_with(')') || open == 0 {
        return false;
    }
    let path = &p[..open];
    if !path
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
    {
        return false;
    }
    let inner = &p[open + 1..p.len() - 1];
    // Exactly one payload: a comma at depth 0 means a tuple variant, whose holes
    // are positional and not a catch-all.
    let mut depth = 0i32;
    for ch in inner.chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => return false,
            _ => {}
        }
    }
    pattern_is_wildcard_shape(inner)
}

// --- Rule E: `if let` / `while let` single-variant probes -------------------

/// Every `let` binding whose **pattern** names something, as
/// `(char index of the `let`, pattern text, full head text through the `=`)`.
///
/// This is the `if let` sibling of Rule A. `if let SnapshotPhase::Unknown(raw) =
/// p` asks exactly the question `matches!` asks and is exactly as
/// non-exhaustive, but it contains no `matches!`, no `match` block and no `==`,
/// so Rules A/B/D are all blind to it.
///
/// Plain `let` bindings are scanned too rather than only `if let`/`while let`,
/// because `let … else` is the third spelling of the same probe and an
/// irrefutable `let` cannot name an enum variant in its pattern anyway — so the
/// wider net costs nothing.
///
/// ```
/// use xtask::phases::let_patterns;
/// let hits = let_patterns("if let Some(P::Unknown(r)) = phase { }");
/// assert_eq!(hits.len(), 1);
/// assert_eq!(hits[0].1, "Some(P::Unknown(r))");
/// assert_eq!(hits[0].2, "if let Some(P::Unknown(r)) =");
/// // A line break between `if` and `let` must not lose the `if `.
/// assert_eq!(let_patterns("if\n    let Some(P::X) = p {}")[0].2, "if let Some(P::X) =");
/// // An ordinary binding names nothing in its pattern.
/// assert_eq!(let_patterns("let x = P::Failed;")[0].1, "x");
/// ```
pub fn let_patterns(text: &str) -> Vec<(usize, String, String)> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if !word_at(&c, i, "let") {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        let mut depth = 0i32;
        let mut eq = None;
        while j < c.len() {
            if let Some(end) = char_lit_end(&c, j) {
                j = end + 1;
                continue;
            }
            match c[j] {
                // `{` is tracked because a struct pattern has one.
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ';' if depth == 0 => break,
                '=' if depth == 0
                    && c.get(j + 1) != Some(&'=')
                    && c.get(j + 1) != Some(&'>')
                    && !matches!(c.get(j.wrapping_sub(1)), Some('=' | '!' | '<' | '>')) =>
                {
                    eq = Some(j);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let Some(eq) = eq else {
            i += 3;
            continue;
        };
        // `if let` / `while let` prefix, so the head reads the way it was
        // written. The keyword is found by skipping back over whitespace rather
        // than by a fixed offset, so a rustfmt line break between `if` and `let`
        // does not silently drop the `if ` from the snippet — and with it the
        // allowlist key.
        let mut k = i;
        while k > 0 && c[k - 1].is_whitespace() {
            k -= 1;
        }
        let prefix = ["if", "while"]
            .iter()
            .find(|kw| k >= kw.len() && word_at(&c, k - kw.len(), kw))
            .map(|kw| format!("{kw} "))
            .unwrap_or_default();
        let pattern = normalize_ws(&c[i + 3..eq].iter().collect::<String>());
        out.push((
            i,
            pattern.clone(),
            normalize_ws(&format!("{prefix}let {pattern} =")),
        ));
        i = eq;
    }
    out
}

// --- Rule D: `==` / `!=` compares ------------------------------------------

/// Every `==` / `!=` comparison in `text`, as `(char index of the operator,
/// left operand, operator, right operand)`.
///
/// Operands are extracted by *balanced* backward/forward scans rather than by
/// taking the line, which is what makes the snippet reflow-proof: rustfmt
/// breaking `repo.status.as_ref()\n  .and_then(…)\n  != Some(&P::Degraded)`
/// across three lines yields the same key as the one-line form.
///
/// ```
/// use xtask::phases::compares;
/// let hits = compares("if phase != Some(&P::Failed) { }");
/// assert_eq!(hits.len(), 1);
/// assert_eq!(hits[0].1, "phase");
/// assert_eq!(hits[0].2, "!=");
/// assert_eq!(hits[0].3, "Some(&P::Failed)");
/// ```
pub fn compares(text: &str) -> Vec<(usize, String, &'static str, String)> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < c.len() {
        let op = match (c[i], c[i + 1]) {
            // `a == b`, but not `<=`/`>=`/`!=`/`=>`/`===`.
            ('=', '=')
                if !matches!(c.get(i.wrapping_sub(1)), Some('<' | '>' | '!' | '='))
                    && c.get(i + 2) != Some(&'=') =>
            {
                "=="
            }
            ('!', '=') if c.get(i + 2) != Some(&'=') => "!=",
            _ => {
                i += 1;
                continue;
            }
        };
        let left = operand_left(&c, i);
        let right = operand_right(&c, i + 2);
        out.push((i, left, op, right));
        i += 2;
    }
    out
}

/// The expression immediately left of the operator at `at`, balanced.
fn operand_left(c: &[char], at: usize) -> String {
    let mut depth = 0i32;
    let mut j = at;
    // Skip the whitespace between the operand and the operator.
    while j > 0 && c[j - 1].is_whitespace() {
        j -= 1;
    }
    let end = j;
    while j > 0 {
        let ch = c[j - 1];
        match ch {
            ')' | ']' => depth += 1,
            '(' | '[' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            '{' | '}' | ';' => break,
            ',' if depth == 0 => break,
            '&' | '|' if depth == 0 && j >= 2 && c[j - 2] == ch => break,
            c0 if c0.is_whitespace() && depth == 0 => {
                // A rustfmt line break inside a method chain is not a boundary;
                // anything else is.
                let mut k = j;
                while k > 0 && c[k - 1].is_whitespace() {
                    k -= 1;
                }
                if c.get(j) == Some(&'.') || c.get(j) == Some(&'?') {
                    j = k;
                    continue;
                }
                break;
            }
            _ => {}
        }
        j -= 1;
    }
    normalize_ws(&c[j..end].iter().collect::<String>())
}

/// The expression immediately right of the operator ending at `at`, balanced.
fn operand_right(c: &[char], at: usize) -> String {
    let mut j = at;
    while c.get(j).is_some_and(|ch| ch.is_whitespace()) {
        j += 1;
    }
    let start = j;
    let mut depth = 0i32;
    while j < c.len() {
        if let Some(end) = char_lit_end(c, j) {
            j = end + 1;
            continue;
        }
        match c[j] {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            '{' | '}' | ';' => break,
            ',' if depth == 0 => break,
            '&' | '|' if depth == 0 && c.get(j + 1) == Some(&c[j]) => break,
            ch if ch.is_whitespace() && depth == 0 => {
                let mut k = j;
                while c.get(k).is_some_and(|w| w.is_whitespace()) {
                    k += 1;
                }
                if matches!(c.get(k), Some('.') | Some('?')) {
                    j = k;
                    continue;
                }
                break;
            }
            _ => {}
        }
        j += 1;
    }
    normalize_ws(&c[start..j].iter().collect::<String>())
}

// --- Rule C: controller-side condition consts ------------------------------

/// Every `pub const <NAME>_CONDITION` declared in `text`, as
/// `(char index, name)`.
///
/// ```
/// use xtask::phases::condition_consts;
/// let hits = condition_consts("pub const PINNED_CONDITION: &str = ;\nconst X: u8 = 1;");
/// assert_eq!(hits[0].1, "PINNED_CONDITION");
/// assert_eq!(hits.len(), 1);
/// ```
pub fn condition_consts(text: &str) -> Vec<(usize, String)> {
    const HEAD: &str = "pub const ";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(HEAD) {
        let byte_at = from + rel;
        from = byte_at + HEAD.len();
        let name: String = text[from..]
            .chars()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
            .collect();
        if name.ends_with("_CONDITION") {
            out.push((text[..byte_at].chars().count(), name));
        }
    }
    out
}

// --- scanning --------------------------------------------------------------

/// Scan one already-scrubbed, line-preserving source for Rules A, B and D.
pub fn scan_source(file: &str, scrubbed: &str, enums: &[&str]) -> Vec<Finding> {
    let chars: Vec<char> = scrubbed.chars().collect();
    let mut out = Vec::new();

    for (at, call) in matches_calls(scrubbed) {
        if mentions_enum(&call, enums) {
            out.push(Finding {
                rule: Rule::NonExhaustiveMatches,
                file: file.to_string(),
                line: line_of(&chars, at),
                snippet: normalize_ws(&call),
            });
        }
    }

    for block in match_blocks(scrubbed) {
        let masked = mask_nested(&block.body);
        if !mentions_enum(&masked, enums) {
            continue;
        }
        for (rel, head) in wildcard_arm_heads(&masked, enums) {
            // `rel` indexes the masked body, which is line-aligned with the
            // original because `mask_nested` keeps the newlines it removes.
            let line = line_of(&chars, block.body_at)
                + masked.chars().take(rel).filter(|c| *c == '\n').count();
            out.push(Finding {
                rule: Rule::WildcardArm,
                file: file.to_string(),
                line,
                snippet: format!("match {} … {}", block.scrutinee, head),
            });
        }
    }

    for (at, pattern, head) in let_patterns(scrubbed) {
        if mentions_enum(&pattern, enums) {
            out.push(Finding {
                rule: Rule::IfLetProbe,
                file: file.to_string(),
                line: line_of(&chars, at),
                snippet: head,
            });
        }
    }

    for (at, left, op, right) in compares(scrubbed) {
        if mentions_enum(&right, enums) || mentions_enum(&left, enums) {
            out.push(Finding {
                rule: Rule::PhaseCompare,
                file: file.to_string(),
                line: line_of(&chars, at),
                snippet: normalize_ws(&format!("{left} {op} {right}")),
            });
        }
    }
    out
}

/// The `*Phase` enums actually declared in `crates/api/src`.
///
/// The self-ratchet behind [`API_PHASE_ENUMS`]: a sixth CR phase enum cannot be
/// added without this check noticing, so the rules cannot quietly stop covering
/// a new one.
pub fn discover_api_phase_enums() -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for (_, raw) in scan::sources(&["api"])? {
        let scrubbed = scan::scrub(&scan::strip_cfg_test(&raw));
        out.extend(declared_phase_enums(&scrubbed));
    }
    Ok(out)
}

/// Every `pub enum <Name>Phase` declared in `text`.
///
/// ```
/// use xtask::phases::declared_phase_enums;
/// let found = declared_phase_enums("pub enum SnapshotPhase { A }\npub enum Backend { B }");
/// assert_eq!(found, vec!["SnapshotPhase".to_string()]);
/// ```
pub fn declared_phase_enums(text: &str) -> Vec<String> {
    const HEAD: &str = "pub enum ";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(HEAD) {
        from += rel + HEAD.len();
        let name: String = text[from..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.ends_with("Phase") {
            out.push(name);
        }
    }
    out
}

/// Every finding in the working tree, sorted.
pub fn collect() -> Result<Vec<Finding>> {
    let enums = phase_enums();
    let mut out = Vec::new();
    for (path, raw) in scan::sources(SCAN_CRATES)? {
        let file = scan::rel_display(&path);
        let scrubbed = scan::scrub_lines(&scan::strip_cfg_test_lines(&raw));
        out.extend(scan_source(&file, &scrubbed, &enums));
        if file == CONTROLLER_CONSTS_REL {
            let chars: Vec<char> = scrubbed.chars().collect();
            for (at, name) in condition_consts(&scrubbed) {
                out.push(Finding {
                    rule: Rule::ControllerCondition,
                    file: file.clone(),
                    line: line_of(&chars, at),
                    snippet: name,
                });
            }
        }
    }
    out.sort();
    Ok(out)
}

// --- the check itself ------------------------------------------------------

/// The outcome of one phase pass.
#[derive(Debug, Default)]
pub struct Report {
    /// Findings no allowlist entry covers.
    pub uncovered: Vec<Finding>,
    /// Allowlist entries that match no finding — the code was paid down (or the
    /// snippet was mistyped), so the exemption has to go.
    pub stale: Vec<Entry>,
    /// Entries sharing a `(file, snippet)` key with an earlier one. Only the
    /// first would ever be consulted, so the rest are invisible reasons: a
    /// reviewer reads two justifications where the ratchet honors one, and
    /// deleting the *live* one silently promotes a duplicate instead of failing.
    pub duplicates: Vec<Entry>,
    /// How many constructs were flagged in total (covered or not).
    pub examined: usize,
}

impl Report {
    /// Whether the ratchet passes.
    pub fn ok(&self) -> bool {
        self.uncovered.is_empty() && self.stale.is_empty() && self.duplicates.is_empty()
    }
}

/// **Pure.** Decide the report from an already-collected finding set and
/// allowlist, so the whole ratchet is unit-testable without touching disk.
pub fn evaluate(findings: &[Finding], allow: &Allowlist) -> Report {
    let mut covered: BTreeMap<(String, String), bool> = BTreeMap::new();
    let mut report = Report {
        examined: findings.len(),
        ..Default::default()
    };
    for e in &allow.allow {
        if covered.insert(e.key(), false).is_some() {
            report.duplicates.push(e.clone());
        }
    }
    for f in findings {
        let key = f.key();
        match covered.get_mut(&key) {
            Some(used) => *used = true,
            None => report.uncovered.push(f.clone()),
        }
    }
    for e in &allow.allow {
        if covered.get(&e.key()) == Some(&false) {
            report.stale.push(e.clone());
        }
    }
    report.uncovered.sort();
    report
}

/// Run the ratchet against the working tree. Returns the process exit code.
pub fn run() -> Result<i32> {
    let allow = Allowlist::load()?;
    let findings = collect()?;
    let report = evaluate(&findings, &allow);

    // Self-ratchet: the hard-coded enum list must still be the real one. Reported
    // alongside the rule findings rather than instead of them — short-circuiting
    // here would hide every uncovered construct behind one unrelated failure, and
    // the run that adds a phase enum is exactly the run with the most to say.
    let discovered = discover_api_phase_enums()?;
    let expected: BTreeSet<String> = API_PHASE_ENUMS.iter().map(|s| (*s).to_string()).collect();
    let enums_drifted = discovered != expected;
    if enums_drifted {
        eprintln!(
            "check-phases: the `*Phase` enums declared in crates/api/src no longer match\n\
             `xtask::phases::API_PHASE_ENUMS`. A phase enum this list does not name is a\n\
             phase enum none of the rules cover.\n\
             \n  declared: {discovered:?}\n  expected: {expected:?}\n"
        );
    }

    if report.ok() && !enums_drifted {
        println!(
            "check-phases: OK ({} construct(s) flagged, all {} allowlisted; \
             {} phase enums, {} crates scanned)",
            report.examined,
            allow.allow.len(),
            phase_enums().len(),
            SCAN_CRATES.len(),
        );
        return Ok(0);
    }
    if !report.uncovered.is_empty() {
        eprintln!(
            "check-phases: {} phase-handling construct(s) are not covered by a reviewed\n\
             exemption. Each one lets a NEW phase variant take an answer nobody chose —\n\
             the defect class behind #351 (`Unchanged` swallowed by `_ =>`) and #359\n\
             (doctor's `matches!` calling a wedged `Deleting` terminal).\n\
             Rewrite it, or add it to {ALLOWLIST_REL} with a reason.\n",
            report.uncovered.len()
        );
        let mut last = None;
        for f in &report.uncovered {
            if last != Some(f.rule) {
                eprintln!("  Rule {} — {}", f.rule.id(), f.rule.title());
                last = Some(f.rule);
            }
            eprintln!("    {}:{}  {}", f.file, f.line, f.snippet);
        }
        eprintln!();
    }
    if !report.stale.is_empty() {
        eprintln!(
            "check-phases: {} allowlist entr(ies) match nothing any more — the construct was\n\
             rewritten (good) or the snippet is mistyped. Delete them from {ALLOWLIST_REL}:\n",
            report.stale.len()
        );
        for e in &report.stale {
            eprintln!("    {}  {}", e.file, normalize_ws(&e.snippet));
        }
        eprintln!();
    }
    if !report.duplicates.is_empty() {
        eprintln!(
            "check-phases: {} allowlist entr(ies) repeat a (file, snippet) already listed.\n\
             Only the first is ever consulted, so the others are reasons nobody reads.\n\
             Merge them into one entry in {ALLOWLIST_REL}:\n",
            report.duplicates.len()
        );
        for e in &report.duplicates {
            eprintln!("    {}  {}", e.file, normalize_ws(&e.snippet));
        }
    }
    Ok(1)
}

#[cfg(test)]
mod tests;
