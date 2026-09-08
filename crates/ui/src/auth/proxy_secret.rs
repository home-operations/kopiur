//! Verifying that a request really arrived through the configured proxy.
//!
//! Identity headers are only trustworthy if nothing else can set them, and in a
//! cluster anything that can reach the Service can. The proxy therefore proves it
//! knows a shared secret, compared in constant time — a byte-by-byte `==` returns
//! early on the first mismatch and leaks the matched prefix length, which is
//! enough to recover the secret one byte at a time.

use constant_time_eq::constant_time_eq;
use http::header::HeaderMap;

use super::identity::AuthError;

/// Header the proxy presents the shared secret in.
///
/// Lowercase because [`HeaderMap`] lookups are case-insensitive over a lowercase
/// name and this constant doubles as the documented chart contract.
pub const PROXY_TOKEN_HEADER: &str = "x-kopiur-proxy-token";

/// Check the request's proxy shared secret against the configured one.
///
/// `expected` is `None` when the deployment has no shared secret — anonymous-only
/// mode, where there is no identity header to forge, or header mode with
/// `--acknowledge-no-proxy-secret`, where the operator took responsibility for
/// closing the hole some other way. Both are decided in
/// [`crate::config::UiArgs::resolve`]; this function only enforces what it was
/// given, and `None` is therefore `Ok`.
///
/// The comparison is constant-time in the *content* of the two values. It is not
/// constant-time in their *length*, which [`constant_time_eq`] short-circuits on:
/// a secret's length is not a secret worth defending, and the alternative (hashing
/// both sides first) buys nothing here.
pub fn verify_proxy_secret(headers: &HeaderMap, expected: Option<&[u8]>) -> Result<(), AuthError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let Some(presented) = headers.get(PROXY_TOKEN_HEADER) else {
        return Err(AuthError::ProxySecretMissing);
    };
    if constant_time_eq(presented.as_bytes(), expected) {
        Ok(())
    } else {
        Err(AuthError::ProxySecretMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{HeaderName, HeaderValue};

    fn with_token(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(PROXY_TOKEN_HEADER),
            HeaderValue::from_str(value).expect("test header value"),
        );
        headers
    }

    #[test]
    fn no_configured_secret_accepts_every_request() {
        assert!(verify_proxy_secret(&HeaderMap::new(), None).is_ok());
        assert!(verify_proxy_secret(&with_token("anything"), None).is_ok());
    }

    #[test]
    fn the_matching_secret_is_accepted() {
        assert!(verify_proxy_secret(&with_token("s3cr3t"), Some(b"s3cr3t")).is_ok());
    }

    #[test]
    fn a_missing_token_is_distinguishable_from_a_wrong_one() {
        assert_eq!(
            verify_proxy_secret(&HeaderMap::new(), Some(b"s3cr3t")).unwrap_err(),
            AuthError::ProxySecretMissing
        );
        assert_eq!(
            verify_proxy_secret(&with_token("nope"), Some(b"s3cr3t")).unwrap_err(),
            AuthError::ProxySecretMismatch
        );
    }

    #[test]
    fn a_matching_prefix_is_still_a_mismatch() {
        // The case a naive `starts_with`/truncating compare would let through.
        for wrong in ["s3cr3", "s3cr3t ", "s3cr3T", "", "s3cr3tt"] {
            assert_eq!(
                verify_proxy_secret(&with_token(wrong), Some(b"s3cr3t")).unwrap_err(),
                AuthError::ProxySecretMismatch,
                "{wrong:?} must not authenticate as s3cr3t"
            );
        }
    }

    #[test]
    fn the_error_messages_say_what_to_do() {
        for err in [
            AuthError::ProxySecretMissing,
            AuthError::ProxySecretMismatch,
        ] {
            let text = err.to_string();
            assert!(text.contains("X-Kopiur-Proxy-Token"), "{text}");
            assert!(text.contains("Fix:"), "{text}");
        }
    }
}
