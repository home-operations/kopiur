//! Who a request runs as.
//!
//! [`Identity`] is the one value the whole backend keys on: it decides which
//! `Impersonate-*` headers go to the apiserver, which entry of the impersonating
//! client cache is used, which `SubjectAccessReview` answers apply, and which
//! per-identity exec semaphore a browse request takes. It is derived from the
//! trusted proxy headers (or the configured anonymous identity) and is never
//! influenced by an inbound `Impersonate-*` header — those are stripped.
//!
//! Task 3 adds the extraction and validation functions; the type itself lives
//! here because everything downstream is written against it.

use std::collections::BTreeMap;

use kopiur_ui_model::identity::IdentitySource;

/// The authenticated caller, as kopiur-ui will assert them to the apiserver.
///
/// `Eq` + `Hash` are load-bearing, not conveniences: the impersonating client
/// cache and the `SubjectAccessReview` cache are both keyed by the whole
/// identity, so two callers who differ in *any* field — one extra group, a
/// different `userextras` value — must never share a cached client or a cached
/// authorization decision. `BTreeMap` (not `HashMap`) for `extra` so that key
/// order is deterministic wherever the identity is rendered or hashed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    /// Username to impersonate (`Impersonate-User`).
    pub user: String,
    /// Groups to impersonate (`Impersonate-Group`, repeated).
    pub groups: Vec<String>,
    /// Email address the proxy supplied, when it did. Display only: never
    /// impersonated, never used for authorization.
    pub email: Option<String>,
    /// `userextras` entries to impersonate (`Impersonate-Extra-<key>`),
    /// restricted to the keys named by `KOPIUR_UI_IMPERSONATE_EXTRA_KEYS` — the
    /// same keys the UI's ClusterRole enumerates.
    pub extra: BTreeMap<String, Vec<String>>,
    /// Whether the proxy asserted this identity or it is the configured
    /// anonymous one. Reported on `kopiur_ui_requests_total{identity_source}` so
    /// a fallback to anonymous is visible rather than silent.
    pub source: IdentitySource,
}
