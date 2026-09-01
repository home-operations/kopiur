//! Turn kopia's raw stderr tail into a tight, operator-readable extract.
//!
//! kopia prints a lot to stderr: a running progress meter (`… 95.0% 12s left`),
//! per-shard bookkeeping, and — buried among it — the one line that actually
//! explains the failure. The raw tail also carries **per-attempt volatile
//! fragments** like `.shards.tmp.9f3ac1`, whose randomness is exactly what once
//! caused a reconcile hot-loop when it leaked into a status condition.
//!
//! [`humanize_tail`] extracts the salient error line(s), drops the progress
//! noise, and strips the volatile temp fragments — deterministically, so the same
//! failure always renders the same string. It feeds
//! [`KopiaError`](crate::error::KopiaError)'s `Display` (which flows to Warning
//! Events, `status.failure.message`, and logs); the full raw tail is preserved
//! separately in the error's `stderr_tail` field for `status.failure.stderrTail`.
//!
//! Pure string work only — no allocation-heavy regex crate, no new dependencies.

/// Render a process exit code for an operator, without the `Some(1)` `Debug`
/// leak the old `{code:?}` interpolation produced.
///
/// ```
/// use kopiur_kopia::humanize::exit_code_desc;
/// assert_eq!(exit_code_desc(&Some(1)), "exit code 1");
/// assert_eq!(exit_code_desc(&None), "no exit code (process killed by a signal)");
/// ```
pub fn exit_code_desc(code: &Option<i32>) -> String {
    match code {
        Some(c) => format!("exit code {c}"),
        None => "no exit code (process killed by a signal)".to_string(),
    }
}

/// Extract the actionable part of a kopia stderr tail: drop progress/noise lines,
/// strip volatile temp-path fragments, and keep at most the last few salient
/// lines (newest last), joined with `; ` so the result stays a single line.
///
/// Deterministic and volatile-free: the same failure yields the same string, and
/// no per-attempt `.tmp.<hex>` / `.shards` fragment survives — safe to render into
/// any operator-facing surface.
///
/// ```
/// use kopiur_kopia::humanize::humanize_tail;
///
/// // Progress noise is dropped; the error line is kept and de-noised.
/// let raw = "| 3 hashing, 12 hashed (2.1 GB), uploaded 1.9 GB (95.0%) 12s left\n\
///            ERROR unable to create directory /repo/.shards.tmp.9f3ac1: permission denied";
/// let out = humanize_tail(raw);
/// assert_eq!(out, "unable to create directory /repo: permission denied");
/// assert!(!out.contains(".tmp"));
/// assert!(!out.contains(".shards"));
/// ```
pub fn humanize_tail(stderr: &str) -> String {
    // Split on \n, and for each line take the text after the last carriage
    // return — kopia's progress meter overwrites one line with \r, so the final
    // segment is what a terminal would actually show.
    let cleaned: Vec<String> = stderr
        .split('\n')
        .map(|raw| raw.rsplit('\r').next().unwrap_or(raw))
        .map(clean_line)
        .filter(|l| !l.is_empty())
        .collect();

    if cleaned.is_empty() {
        return "(kopia produced no stderr)".to_string();
    }

    let signal: Vec<&String> = cleaned.iter().filter(|l| !is_progress_noise(l)).collect();
    // Prefer lines that actually name an error; otherwise fall back to all
    // signal lines, and finally to the raw cleaned lines if everything looked
    // like progress.
    let error_lines: Vec<&String> = signal
        .iter()
        .copied()
        .filter(|l| has_error_keyword(l))
        .collect();
    let source: &[&String] = if !error_lines.is_empty() {
        &error_lines
    } else if !signal.is_empty() {
        &signal
    } else {
        // All noise: keep the last cleaned line so we say *something* concrete.
        return cleaned.last().cloned().unwrap_or_default();
    };

    // Keep the last up to 3 (newest = most actionable), dropping consecutive
    // duplicates, then restore chronological order.
    let mut kept: Vec<&str> = Vec::new();
    for line in source.iter().rev() {
        let s = line.as_str();
        if kept.last() == Some(&s) {
            continue;
        }
        kept.push(s);
        if kept.len() == 3 {
            break;
        }
    }
    kept.reverse();
    kept.join("; ")
}

/// Strip a leading kopia log prefix and any volatile temp-path fragments from one
/// line, and trim it.
fn clean_line(line: &str) -> String {
    let stripped = strip_volatile_paths(line);
    let trimmed = stripped.trim();
    // kopia prefixes error lines with `ERROR ` (and occasionally `error: ` /
    // `FATAL `); the prefix adds no information once the line is clearly the error.
    for prefix in ["ERROR ", "error: ", "FATAL ", "fatal: "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Remove kopia per-attempt temp-path segments (`…/.shards.tmp.<hex>`,
/// `…/.tmp.<hex>`) so they never reach an operator-facing message. The whole
/// path segment (including its leading `/`) is excised, leaving the stable prefix.
fn strip_volatile_paths(line: &str) -> String {
    let mut out = line.to_string();
    while let Some(marker) = find_volatile_marker(&out) {
        // Segment start: the last '/' before the marker (drop it too), else the
        // marker itself.
        let seg_start = out[..marker].rfind('/').unwrap_or(marker);
        // Segment end: first whitespace or ':' at/after the marker.
        let seg_end = out[marker..]
            .find(|c: char| c.is_whitespace() || c == ':')
            .map(|i| marker + i)
            .unwrap_or(out.len());
        out.replace_range(seg_start..seg_end, "");
    }
    out
}

/// Locate the start of a volatile fragment: `.shards` (kopia shard dirs) or a
/// `.tmp.` followed by at least two hex digits (a random temp suffix).
fn find_volatile_marker(s: &str) -> Option<usize> {
    let shards = s.find(".shards");
    let tmp = {
        let mut at = None;
        let mut from = 0;
        while let Some(rel) = s[from..].find(".tmp.") {
            let idx = from + rel;
            let after = &s.as_bytes()[idx + ".tmp.".len()..];
            let hex = after.iter().take_while(|b| b.is_ascii_hexdigit()).count();
            if hex >= 2 {
                at = Some(idx);
                break;
            }
            from = idx + ".tmp.".len();
        }
        at
    };
    match (shards, tmp) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// A running-progress / bookkeeping line, not an error. Kopia's meter always
/// carries a `%`, and its per-shard lines carry these hashing/upload counters.
fn is_progress_noise(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    if l.contains('%') {
        return true;
    }
    const PROGRESS_TOKENS: &[&str] = &[
        " hashing,",
        " hashed ",
        " hashed(",
        " cached ",
        "uploaded ",
        "estimating",
        "estimated ",
        " b/s",
        "kb/s",
        "mb/s",
        "gb/s",
        " eta ",
        "s left",
        "processed ",
    ];
    PROGRESS_TOKENS.iter().any(|t| l.contains(t))
}

/// Whether a line names an actual failure (worth surfacing over a bare
/// informational line like `Snapshotting app@host:/data …`).
fn has_error_keyword(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    const ERROR_TOKENS: &[&str] = &[
        "error",
        "fatal",
        "failed",
        "failure",
        "unable",
        "cannot",
        "can't",
        "denied",
        "not found",
        "not initialized",
        "no such",
        "does not exist",
        "refused",
        "unauthorized",
        "forbidden",
        "invalid",
        "timeout",
        "timed out",
        "permission",
        "no route",
        "x509",
        "tls:",
        "certificate",
    ];
    ERROR_TOKENS.iter().any(|t| l.contains(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_renders_cleanly() {
        assert_eq!(exit_code_desc(&Some(1)), "exit code 1");
        assert_eq!(exit_code_desc(&Some(137)), "exit code 137");
        assert_eq!(
            exit_code_desc(&None),
            "no exit code (process killed by a signal)"
        );
    }

    #[test]
    fn keeps_a_plain_error_line_unchanged() {
        assert_eq!(
            humanize_tail("repository is locked by another process"),
            "repository is locked by another process"
        );
    }

    #[test]
    fn drops_progress_and_strips_volatile_temp_path() {
        // The plan's named fixture: progress interleaved with the real error,
        // which carries a random `.shards.tmp.<hex>` segment.
        let raw = "| 3 hashing, 1092 hashed (2.1 GB), 0 cached, uploaded 1.9 GB (95.0%) 12s left\n\
                   ERROR unable to create directory /repo/.shards.tmp.9f3ac1: permission denied";
        let out = humanize_tail(raw);
        assert_eq!(out, "unable to create directory /repo: permission denied");
        assert!(!out.contains(".tmp"), "{out}");
        assert!(!out.contains(".shards"), "{out}");
        assert!(!out.contains("9f3ac1"), "{out}");
        assert!(out.contains("permission denied"), "{out}");
    }

    #[test]
    fn strips_bare_tmp_hex_segment() {
        let out = humanize_tail("wrote /cache/.tmp.a1b2 then failed to sync");
        assert!(!out.contains(".tmp"), "{out}");
        assert!(!out.contains("a1b2"), "{out}");
        assert!(out.contains("failed to sync"), "{out}");
    }

    #[test]
    fn prefers_the_error_line_over_informational_ones() {
        let raw = "Snapshotting app@host:/pvc/data ...\n\
                   uploaded 500 MB (100.0%)\n\
                   ERROR upload error: connection reset by peer";
        assert_eq!(humanize_tail(raw), "upload error: connection reset by peer");
    }

    #[test]
    fn collapses_carriage_return_progress_overwrites() {
        // kopia overwrites one physical line with \r; only the final segment shows.
        let raw = "hashing 10%\rhashing 50%\rhashing 100%\nERROR unable to open repository: dial tcp: connection refused";
        assert_eq!(
            humanize_tail(raw),
            "unable to open repository: dial tcp: connection refused"
        );
    }

    #[test]
    fn empty_stderr_is_reported_not_blank() {
        assert_eq!(humanize_tail(""), "(kopia produced no stderr)");
        assert_eq!(humanize_tail("   \n  \n"), "(kopia produced no stderr)");
    }

    #[test]
    fn keeps_last_few_when_no_explicit_error_keyword() {
        // No error keyword anywhere → fall back to the last signal lines.
        let raw = "step one\nstep two\nstep three\nstep four";
        assert_eq!(humanize_tail(raw), "step two; step three; step four");
    }

    #[test]
    fn real_classifier_fixtures_survive_humanization() {
        // The strings the classifier is tested on must remain readable and keep
        // the substrings that make them actionable.
        for (raw, needle) in [
            ("invalid repository password", "invalid repository password"),
            (
                "ERROR error connecting to repository: dial tcp ...",
                "connecting to repository",
            ),
            (
                "unable to open repository: lookup minio.storage.svc on 10.96.0.10:53: no such host",
                "no such host",
            ),
            ("x509: certificate signed by unknown authority", "x509"),
            (
                "repository not initialized in the provided storage",
                "not initialized",
            ),
            (
                "can't connect to storage: error retrieving storage config from bucket \"kopiur\": Access Denied",
                "access denied",
            ),
        ] {
            let out = humanize_tail(raw);
            assert!(
                out.to_ascii_lowercase().contains(needle),
                "humanized {out:?} lost {needle:?}"
            );
        }
    }
}
