//! The mutation gate.
//!
//! Every non-GET request must carry proof it came from the SPA rather than from
//! another origin's page: a custom `X-Kopiur-Request` header (which a
//! cross-origin form cannot set without triggering a preflight the UI does not
//! answer), a JSON `Content-Type` on bodies, and a same-origin
//! `Sec-Fetch-Site`/`Origin` whenever the browser sends one.
//!
//! The layering matters. `X-Kopiur-Request` is the load-bearing check, because it
//! is the one an attacker's page cannot satisfy at all: the three request shapes
//! a browser will send cross-origin without a preflight (a form post, an image,
//! a navigation) can set neither a custom header nor `application/json`. The
//! Fetch-Metadata and `Origin` checks are defence in depth — modern browsers send
//! them, older ones do not, and a check that fails open when a header is absent is
//! worth having only *alongside* one that never can.

use http::header::{CONTENT_TYPE, HOST, HeaderMap, HeaderValue, ORIGIN};

/// Header the SPA sets on every mutating request.
pub const REQUEST_HEADER: &str = "x-kopiur-request";

/// The only value [`REQUEST_HEADER`] may carry. A fixed value rather than "any
/// value" so the header is unmistakably deliberate.
pub const REQUEST_HEADER_VALUE: &str = "1";

/// The Fetch Metadata header browsers use to describe the request's origin
/// relationship.
pub const SEC_FETCH_SITE: &str = "sec-fetch-site";

/// The `Content-Type` a JSON body must declare, ignoring parameters.
const JSON_MEDIA_TYPE: &str = "application/json";

/// Why a mutating (or navigating) request was refused.
///
/// Separate from [`super::identity::AuthError`] because the two say different
/// things to a user: an `AuthError` means "we do not know who you are", a
/// `CsrfError` means "we know who you are, and this request did not come from the
/// kopiur UI".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CsrfError {
    /// The SPA's marker header was absent or carried the wrong value.
    #[error(
        "the request did not carry the {REQUEST_HEADER}: {REQUEST_HEADER_VALUE} header the \
         kopiur UI sets on every change it makes. A page on another site can submit a form to \
         this endpoint, but it cannot set a custom header, so this header is what separates \
         the two. \
         Fix: make the change from the kopiur UI; if you are scripting against the API, send \
         {REQUEST_HEADER}: {REQUEST_HEADER_VALUE} (or use kubectl kopiur, which writes the \
         same CRs)"
    )]
    MissingRequestHeader,

    /// A request with a body declared something other than JSON.
    #[error(
        "the request body is declared as {got:?}, not {JSON_MEDIA_TYPE}. The form content \
         types are exactly the ones a cross-origin page can send without a preflight, so the \
         API accepts none of them. \
         Fix: send the body as {JSON_MEDIA_TYPE}"
    )]
    WrongContentType {
        /// The `Content-Type` that was sent.
        got: String,
    },

    /// The browser said the request came from another site.
    #[error(
        "the browser reported this request as {sec_fetch_site:?}, meaning it was made from a \
         page that is not the kopiur UI. \
         Fix: make the change from the kopiur UI itself rather than from another site or an \
         embedded frame"
    )]
    CrossSite {
        /// The `Sec-Fetch-Site` value the browser sent.
        sec_fetch_site: String,
    },

    /// The `Origin` header names a different host than the request was sent to.
    #[error(
        "the request's Origin ({origin}) is not the host it was sent to ({host}), so it did \
         not come from the kopiur UI served at that address. \
         Fix: use the UI at {host}; if kopiur-ui sits behind a proxy that rewrites Host, make \
         the proxy forward the browser-visible host"
    )]
    OriginMismatch {
        /// The `Origin` the browser sent.
        origin: String,
        /// The `Host` the request was addressed to.
        host: String,
    },
}

/// Gate a mutating request.
///
/// `has_body` says whether the request carries a payload; the caller decides that
/// (see [`super::mutation_guard`]), because it is a property of the request
/// framing rather than of the headers alone.
pub fn require_mutation_headers(headers: &HeaderMap, has_body: bool) -> Result<(), CsrfError> {
    if headers.get(REQUEST_HEADER).map(HeaderValue::as_bytes)
        != Some(REQUEST_HEADER_VALUE.as_bytes())
    {
        return Err(CsrfError::MissingRequestHeader);
    }
    if has_body {
        require_json_body(headers)?;
    }
    require_same_site(headers)?;
    require_matching_origin(headers)
}

/// Gate a same-site navigation — the `GET …/file` download, which the browser
/// performs as a top-level navigation rather than as a `fetch`.
///
/// A download is a GET, so it carries neither a body nor the SPA's marker header;
/// `Sec-Fetch-Site` is all there is. Absent counts as allowed: a browser too old
/// to send Fetch Metadata must still be able to download a file, and the request
/// discloses nothing the caller could not read through the API anyway.
pub fn require_same_site_navigation(headers: &HeaderMap) -> Result<(), CsrfError> {
    require_same_site(headers)
}

/// `Sec-Fetch-Site`, when present, must say the request came from this origin
/// (`same-origin`) or from no page at all (`none` — a typed URL or a bookmark).
/// `same-site` is deliberately refused: a sibling subdomain is not the kopiur UI.
fn require_same_site(headers: &HeaderMap) -> Result<(), CsrfError> {
    let Some(value) = headers.get(SEC_FETCH_SITE) else {
        return Ok(());
    };
    let site = value.to_str().unwrap_or("<non-ascii>");
    if site == "same-origin" || site == "none" {
        Ok(())
    } else {
        Err(CsrfError::CrossSite {
            sec_fetch_site: site.to_string(),
        })
    }
}

/// `Origin`, when present, must name the host the request was addressed to.
///
/// Compared as scheme-stripped authority against `Host`, which is what the two
/// headers actually contain: `Origin: https://kopiur.example` versus `Host:
/// kopiur.example`. A request with an `Origin` but no `Host` is refused rather
/// than waved through — HTTP/1.1 requires `Host` and HTTP/2 synthesises it, so its
/// absence is not a shape the UI needs to serve.
fn require_matching_origin(headers: &HeaderMap) -> Result<(), CsrfError> {
    let Some(origin) = headers.get(ORIGIN) else {
        return Ok(());
    };
    let origin = origin.to_str().unwrap_or("<non-ascii>");
    // `Origin: null` is what a sandboxed frame or a redirected cross-origin
    // request sends. It is not this host.
    let authority = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or(origin);
    let host = headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();

    if !host.is_empty() && authority == host {
        Ok(())
    } else {
        Err(CsrfError::OriginMismatch {
            origin: origin.to_string(),
            host: host.to_string(),
        })
    }
}

/// A body must be JSON. Parameters (`; charset=utf-8`) are ignored, case is not
/// significant, and a missing `Content-Type` on a request that has a body is a
/// mismatch rather than a pass.
fn require_json_body(headers: &HeaderMap) -> Result<(), CsrfError> {
    let got = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let media_type = got.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case(JSON_MEDIA_TYPE) {
        Ok(())
    } else {
        Err(CsrfError::WrongContentType {
            got: got.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderName;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::try_from(*name).expect("test header name"),
                HeaderValue::from_str(value).expect("test header value"),
            );
        }
        map
    }

    /// The headers a well-behaved SPA request carries.
    fn good() -> Vec<(&'static str, &'static str)> {
        vec![
            (REQUEST_HEADER, REQUEST_HEADER_VALUE),
            ("content-type", "application/json"),
            (SEC_FETCH_SITE, "same-origin"),
            ("origin", "https://kopiur.example"),
            ("host", "kopiur.example"),
        ]
    }

    fn without(name: &str) -> HeaderMap {
        headers(
            &good()
                .into_iter()
                .filter(|(n, _)| *n != name)
                .collect::<Vec<_>>(),
        )
    }

    fn with<'a>(name: &'a str, value: &'a str) -> HeaderMap {
        let mut pairs: Vec<(&'a str, &'a str)> =
            good().into_iter().filter(|(n, _)| *n != name).collect();
        pairs.push((name, value));
        headers(&pairs)
    }

    #[test]
    fn the_spas_own_request_passes() {
        assert_eq!(require_mutation_headers(&headers(&good()), true), Ok(()));
    }

    #[test]
    fn the_marker_header_is_required_and_must_carry_its_value() {
        assert_eq!(
            require_mutation_headers(&without(REQUEST_HEADER), true),
            Err(CsrfError::MissingRequestHeader)
        );
        assert_eq!(
            require_mutation_headers(&with(REQUEST_HEADER, "0"), true),
            Err(CsrfError::MissingRequestHeader)
        );
        assert_eq!(
            require_mutation_headers(&with(REQUEST_HEADER, "true"), true),
            Err(CsrfError::MissingRequestHeader)
        );
    }

    #[test]
    fn a_body_must_be_json() {
        for got in [
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "text/plain",
        ] {
            assert_eq!(
                require_mutation_headers(&with("content-type", got), true),
                Err(CsrfError::WrongContentType {
                    got: got.to_string()
                }),
                "{got} is a form content type a cross-origin page can send"
            );
        }
        assert_eq!(
            require_mutation_headers(&without("content-type"), true),
            Err(CsrfError::WrongContentType { got: String::new() })
        );
    }

    #[test]
    fn json_parameters_and_casing_are_accepted() {
        for got in [
            "application/json; charset=utf-8",
            "Application/JSON",
            "application/json ",
        ] {
            assert_eq!(
                require_mutation_headers(&with("content-type", got), true),
                Ok(()),
                "{got} is JSON"
            );
        }
    }

    #[test]
    fn a_bodyless_request_needs_no_content_type() {
        // DELETE /snapshots/{ns}/{name} sends no body.
        assert_eq!(
            require_mutation_headers(&without("content-type"), false),
            Ok(())
        );
    }

    #[test]
    fn a_cross_site_fetch_is_refused_and_absent_metadata_is_allowed() {
        for site in ["cross-site", "same-site"] {
            assert_eq!(
                require_mutation_headers(&with(SEC_FETCH_SITE, site), true),
                Err(CsrfError::CrossSite {
                    sec_fetch_site: site.to_string()
                })
            );
        }
        assert_eq!(
            require_mutation_headers(&with(SEC_FETCH_SITE, "none"), true),
            Ok(())
        );
        assert_eq!(
            require_mutation_headers(&without(SEC_FETCH_SITE), true),
            Ok(()),
            "a browser that sends no Fetch Metadata still has to pass the marker header"
        );
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        assert_eq!(
            require_mutation_headers(&with("origin", "https://evil.example"), true),
            Err(CsrfError::OriginMismatch {
                origin: "https://evil.example".to_string(),
                host: "kopiur.example".to_string(),
            })
        );
        assert_eq!(
            require_mutation_headers(&with("origin", "null"), true),
            Err(CsrfError::OriginMismatch {
                origin: "null".to_string(),
                host: "kopiur.example".to_string(),
            })
        );
    }

    #[test]
    fn an_origin_matching_the_host_passes_over_either_scheme_and_port() {
        assert_eq!(
            require_mutation_headers(&with("origin", "http://kopiur.example"), true),
            Ok(())
        );
        let mut map = with("origin", "https://kopiur.example:8443");
        map.insert(HOST, HeaderValue::from_static("kopiur.example:8443"));
        assert_eq!(require_mutation_headers(&map, true), Ok(()));
    }

    #[test]
    fn an_origin_with_no_host_is_refused() {
        let mut map = without("host");
        map.insert(ORIGIN, HeaderValue::from_static("https://kopiur.example"));
        assert!(matches!(
            require_mutation_headers(&map, true),
            Err(CsrfError::OriginMismatch { .. })
        ));
    }

    #[test]
    fn no_origin_header_at_all_passes() {
        assert_eq!(require_mutation_headers(&without("origin"), true), Ok(()));
    }

    #[test]
    fn a_navigation_download_only_checks_fetch_metadata() {
        // No marker header, no content type — a top-level navigation has neither.
        for site in ["same-origin", "none"] {
            assert_eq!(
                require_same_site_navigation(&headers(&[(SEC_FETCH_SITE, site)])),
                Ok(())
            );
        }
        assert_eq!(require_same_site_navigation(&HeaderMap::new()), Ok(()));
        assert_eq!(
            require_same_site_navigation(&headers(&[(SEC_FETCH_SITE, "cross-site")])),
            Err(CsrfError::CrossSite {
                sec_fetch_site: "cross-site".to_string()
            })
        );
    }

    #[test]
    fn every_refusal_says_what_to_do() {
        for err in [
            CsrfError::MissingRequestHeader,
            CsrfError::WrongContentType {
                got: "text/plain".to_string(),
            },
            CsrfError::CrossSite {
                sec_fetch_site: "cross-site".to_string(),
            },
            CsrfError::OriginMismatch {
                origin: "https://evil.example".to_string(),
                host: "kopiur.example".to_string(),
            },
        ] {
            assert!(err.to_string().contains("Fix:"), "{err}");
        }
    }
}
