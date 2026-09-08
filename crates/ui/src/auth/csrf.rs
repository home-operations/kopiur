//! The mutation gate.
//!
//! Every non-GET request must carry proof it came from the SPA rather than from
//! another origin's page: a custom `X-Kopiur-Request` header (which a
//! cross-origin form cannot set without triggering a preflight the UI does not
//! answer), a JSON `Content-Type` on bodies, and a same-origin
//! `Sec-Fetch-Site`/`Origin` whenever the browser sends one.
