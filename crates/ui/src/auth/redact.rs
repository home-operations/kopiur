//! Keeping credentials out of what the UI hands back or logs.
//!
//! Inbound `Cookie`, `Authorization` and `X-Forwarded-Access-Token` are stripped
//! before any handler sees them — a handler that cannot read a credential cannot
//! leak one — and secret-shaped tokens are masked out of the pod log tails quoted
//! into problem responses.

use http::header::{HeaderMap, HeaderName};

/// Headers removed from every inbound request before any handler runs.
///
/// The UI authenticates nobody itself: the proxy in front of it does, and
/// whatever bearer token or session cookie the browser sent is the proxy's
/// business, not this backend's. Removing them here means no handler, no
/// `TraceLayer`, and no error path *can* record one, rather than merely
/// promising not to. `x-forwarded-access-token` and `x-forwarded-authorization`
/// are what oauth2-proxy forwards when it is configured to pass the upstream
/// token through — a live credential for the identity provider.
pub const STRIPPED_HEADERS: &[&str] = &[
    "cookie",
    "authorization",
    "x-forwarded-access-token",
    "x-forwarded-authorization",
];

/// What a masked value is replaced with.
pub const REDACTED: &str = "***";

/// Substrings that make a token look like it names a credential. Matched
/// case-insensitively, anywhere in the token, so `AWS_SECRET_ACCESS_KEY`,
/// `--password` and `KOPIA_TOKEN:` all hit.
const SECRET_MARKERS: &[&str] = &["aws_", "key", "token", "password", "secret"];

/// Remove every credential-bearing header from a request.
///
/// Called by [`super::identity_middleware`] before anything else, including
/// before the proxy-secret check — the identity headers and the proxy token are
/// deliberately *not* in [`STRIPPED_HEADERS`], because they are this backend's
/// own inputs.
pub fn strip_sensitive(headers: &mut HeaderMap) {
    for name in STRIPPED_HEADERS {
        // `HeaderName::from_static` would panic on a malformed constant; these
        // are compile-time literals, so the parse cannot fail, but a `remove`
        // that silently did nothing would be a security hole rather than a bug,
        // so the name is built once and reused.
        let name = HeaderName::from_static(name);
        headers.remove(&name);
    }
}

/// Mask secret-shaped values in free text (a mover pod's log tail, a kopia
/// stderr) before it is quoted into a problem response.
///
/// The rule is deliberately blunt: a whitespace-delimited token that *looks* like
/// it names a credential — it contains `AWS_`, `KEY`, `TOKEN`, `PASSWORD` or
/// `SECRET`, in any case — has its value masked. "Its value" is whatever follows
/// the first `=` or `:` inside the token (`AWS_SECRET_ACCESS_KEY=wJal…` →
/// `AWS_SECRET_ACCESS_KEY=***`), or, when the token has no value attached, the
/// whole of the next token (`--password hunter2` → `--password ***`).
///
/// It over-masks rather than under-masks — `no secrets here` loses `here` — and
/// that is the right direction for a function whose failure mode is printing a
/// repository password into a browser. It is a safety net, not a guarantee: the
/// real defence is that the UI never reads Secrets at all.
pub fn redact_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut mask_next = false;
    let mut rest = s;

    while !rest.is_empty() {
        let gap = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        out.push_str(&rest[..gap]);
        rest = &rest[gap..];
        if rest.is_empty() {
            break;
        }

        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (token, tail) = rest.split_at(end);
        rest = tail;
        mask_next = push_token(&mut out, token, mask_next);
    }

    out
}

/// Append one token to the output, and report whether the *next* token is the
/// value of a credential flag and must therefore be masked whole.
fn push_token(out: &mut String, token: &str, mask_this: bool) -> bool {
    if mask_this {
        out.push_str(REDACTED);
        return false;
    }
    if !looks_secret(token) {
        out.push_str(token);
        return false;
    }
    match token.find(['=', ':']) {
        // `KEY=value` — keep the name and the separator, mask the value.
        Some(at) if at + 1 < token.len() => {
            out.push_str(&token[..=at]);
            out.push_str(REDACTED);
            false
        }
        // `--password` or `PASSWORD:` — the value is the next token.
        _ => {
            out.push_str(token);
            true
        }
    }
}

/// Whether a token names something that is likely to have a credential attached.
fn looks_secret(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    SECRET_MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;

    #[test]
    fn every_credential_header_is_removed_and_the_identity_headers_are_not() {
        let mut headers = HeaderMap::new();
        for name in STRIPPED_HEADERS {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static("secret"),
            );
        }
        headers.insert(
            HeaderName::from_static("x-forwarded-user"),
            HeaderValue::from_static("alice"),
        );
        headers.insert(
            HeaderName::from_static("x-kopiur-proxy-token"),
            HeaderValue::from_static("s3cr3t"),
        );

        strip_sensitive(&mut headers);

        for name in STRIPPED_HEADERS {
            assert!(headers.get(*name).is_none(), "{name} must be stripped");
        }
        assert!(
            headers.get("x-forwarded-user").is_some(),
            "the identity headers are this backend's input, not a credential to drop"
        );
        assert!(headers.get("x-kopiur-proxy-token").is_some());
    }

    #[test]
    fn stripping_removes_every_value_of_a_repeated_header() {
        let mut headers = HeaderMap::new();
        headers.append(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("a=1"),
        );
        headers.append(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("b=2"),
        );

        strip_sensitive(&mut headers);

        assert_eq!(headers.get_all("cookie").iter().count(), 0);
    }

    #[test]
    fn stripping_an_already_clean_request_is_a_no_op() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-forwarded-user"),
            HeaderValue::from_static("alice"),
        );
        strip_sensitive(&mut headers);
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn redaction_masks_attached_and_following_values() {
        let cases = [
            (
                "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG",
                "AWS_SECRET_ACCESS_KEY=***",
            ),
            ("KOPIA_PASSWORD=hunter2", "KOPIA_PASSWORD=***"),
            ("--password hunter2", "--password ***"),
            ("KOPIA_PASSWORD: hunter2", "KOPIA_PASSWORD: ***"),
            ("token=abc123", "token=***"),
            ("api-key abc123", "api-key ***"),
            ("Bearer eyJhbGciOi", "Bearer eyJhbGciOi"),
            ("nothing to see here", "nothing to see here"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(redact_text(input), want, "redacting {input:?}");
        }
    }

    #[test]
    fn redaction_preserves_the_surrounding_layout() {
        let log = "  time=12:00 msg=\"connect failed\"\n  AWS_ACCESS_KEY_ID=AKIA1234 \
                   --password hunter2\n";
        let redacted = redact_text(log);

        assert!(redacted.starts_with("  time=12:00"), "{redacted:?}");
        assert!(redacted.ends_with('\n'), "{redacted:?}");
        assert!(redacted.contains("AWS_ACCESS_KEY_ID=***"), "{redacted:?}");
        assert!(redacted.contains("--password ***"), "{redacted:?}");
        assert!(!redacted.contains("AKIA1234"), "{redacted:?}");
        assert!(!redacted.contains("hunter2"), "{redacted:?}");
        // The line structure survives, so the tail is still readable.
        assert_eq!(redacted.lines().count(), log.lines().count());
    }

    #[test]
    fn the_marker_match_is_case_insensitive() {
        assert_eq!(redact_text("Secret=x"), "Secret=***");
        assert_eq!(redact_text("MyToken=x"), "MyToken=***");
        assert_eq!(redact_text("aws_thing=x"), "aws_thing=***");
    }

    #[test]
    fn a_masked_value_is_never_itself_scanned_for_a_further_mask() {
        // `--password` masks the next token whole; that token must not then be
        // treated as a new flag and eat the one after it.
        assert_eq!(redact_text("--password key=1 keep"), "--password *** keep");
    }
}
