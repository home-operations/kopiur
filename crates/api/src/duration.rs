//! Go-style duration strings used across the CRDs (`30m`, `1h`, `90s`).
//!
//! Lives in `kopiur-api` (not the controller) so the admission validators and
//! the reconcilers parse the exact same grammar — a value the webhook admits
//! must never fail to parse at reconcile time.

use std::time::Duration;

/// Parse a Go-style duration string used in the CRDs (`30m`, `1h`, `90s`, or a
/// bare number of seconds). Returns `None` for unparseable input.
pub fn parse_go_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Support a single unit suffix (s/m/h) or a bare number of seconds.
    let (num, mult) = if let Some(stripped) = s.strip_suffix('h') {
        (stripped, 3600u64)
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, 60)
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, 1)
    } else {
        (s, 1)
    };
    // `checked_mul` so an absurd value (`9999999999999999h`) returns `None` — the
    // webhook then rejects it instead of panicking (debug) or wrapping to a garbage
    // duration (release) on the unchecked multiply.
    num.trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .map(Duration::from_secs)
}

/// Resolve an optional policy timeout string to an effective deadline duration,
/// with the semantics every `*.timeout` field shares (`spec.preflight.timeout`,
/// `spec.staging.timeout`): absent ⇒ `default`; parsed-zero (`0`/`0s`) ⇒ `None`
/// (indefinite — never expires); unparseable ⇒ `default` (defensive only — the
/// webhook rejects unparseable values at admission).
pub fn resolve_timeout(spec: Option<&str>, default: Duration) -> Option<Duration> {
    match spec {
        None => Some(default),
        Some(s) => match parse_go_duration(s) {
            Some(d) if d.is_zero() => None,
            Some(d) => Some(d),
            None => Some(default),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_timeout_shared_semantics() {
        let default = Duration::from_secs(600);
        assert_eq!(resolve_timeout(None, default), Some(default));
        assert_eq!(
            resolve_timeout(Some("30m"), default),
            Some(Duration::from_secs(1800))
        );
        // Zero means indefinite (never expires), not "expire immediately".
        assert_eq!(resolve_timeout(Some("0"), default), None);
        assert_eq!(resolve_timeout(Some("0s"), default), None);
        // Unparseable falls back to the default (webhook rejects it anyway).
        assert_eq!(resolve_timeout(Some("every-hour"), default), Some(default));
    }

    #[test]
    fn parse_go_duration_handles_units() {
        assert_eq!(parse_go_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_go_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_go_duration("45s"), Some(Duration::from_secs(45)));
        assert_eq!(parse_go_duration("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_go_duration(" 5m "), Some(Duration::from_secs(300)));
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("bogus"), None);
        assert_eq!(parse_go_duration("-5m"), None);
        // Overflow on the unit multiply must be rejected, not panic/wrap.
        assert_eq!(parse_go_duration("9999999999999999h"), None);
        assert_eq!(parse_go_duration(&format!("{}m", u64::MAX)), None);
        // A bare (unmultiplied) large second count still parses.
        assert_eq!(
            parse_go_duration(&u64::MAX.to_string()),
            Some(Duration::from_secs(u64::MAX))
        );
    }
}
