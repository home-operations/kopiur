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

/// Render a [`Duration`] back to a Go-style duration string, using the largest unit that
/// divides it exactly (`21600s` → `"6h"`, `1200s` → `"20m"`, else `"{n}s"`).
///
/// Round-trips through [`parse_go_duration`] by construction — it emits only the
/// single-unit grammar that function accepts. Two callers need it:
///
/// - **kopia argv.** Durations that reach a kopia CLI flag must never be the user's raw
///   text. kopia's `time.ParseDuration` REJECTS a bare number (`--epoch-min-duration=3600`
///   → `time: missing unit in duration "3600"`) while `parse_go_duration` happily accepts
///   one — so passing the string through would admit at the webhook and crash in the mover,
///   breaking this module's stated contract. Parse, then render, and the mismatch is gone.
/// - **Status mirrors.** kopia reports durations as `time.Duration` nanoseconds; rendering
///   them through here is what makes `status` comparable to `spec` and stable across
///   reconciles (no `"24h"` vs `"24h0m0s"` ambiguity).
pub fn render_go_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs != 0 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs != 0 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
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

    #[test]
    fn render_go_duration_picks_the_largest_exact_unit() {
        assert_eq!(render_go_duration(Duration::from_secs(21600)), "6h");
        assert_eq!(render_go_duration(Duration::from_secs(86400)), "24h");
        assert_eq!(render_go_duration(Duration::from_secs(1200)), "20m");
        assert_eq!(render_go_duration(Duration::from_secs(45)), "45s");
        // Not evenly divisible → fall back to seconds rather than lose precision.
        assert_eq!(render_go_duration(Duration::from_secs(5400)), "90m");
        assert_eq!(render_go_duration(Duration::from_secs(3661)), "3661s");
        // Zero must not divide into "0h".
        assert_eq!(render_go_duration(Duration::ZERO), "0s");
        // Sub-second precision is not representable in this grammar; truncation is
        // acceptable because every CRD field using it is coarse (minutes and up).
        assert_eq!(render_go_duration(Duration::from_millis(1500)), "1s");
    }

    #[test]
    fn render_go_duration_round_trips_through_parse() {
        for secs in [0u64, 1, 45, 59, 60, 90, 1200, 3600, 5400, 21600, 86400] {
            let d = Duration::from_secs(secs);
            assert_eq!(
                parse_go_duration(&render_go_duration(d)),
                Some(d),
                "render must emit only what parse accepts ({secs}s)"
            );
        }
    }

    #[test]
    fn rendering_is_what_makes_a_bare_number_safe_for_kopias_cli() {
        // kopia's time.ParseDuration REJECTS a bare number: `--epoch-min-duration=3600`
        // fails with `time: missing unit in duration "3600"`. parse_go_duration accepts
        // it, so passing user text straight to kopia would admit at the webhook and die
        // in the mover. Rendering always emits a unit — that is the whole point.
        let d = parse_go_duration("3600").expect("kopiur accepts a bare second count");
        let rendered = render_go_duration(d);
        assert_eq!(rendered, "1h");
        assert!(
            rendered.ends_with(['h', 'm', 's']),
            "a rendered duration must always carry a unit for kopia: {rendered}"
        );
    }
}
