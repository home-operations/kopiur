//! Where reads come from.
//!
//! Two shapes, chosen once at startup by `KOPIUR_UI_CACHE`: watch-fed reflector
//! stores under the UI's own ServiceAccount, gated per identity by
//! `SubjectAccessReview` ([`authz`]), or one impersonated LIST per request. Every
//! `load_*` matches [`Source`] exhaustively, so a third backing store cannot be
//! added until every reader has accounted for it.

pub mod authz;
pub mod stores;

/// Which backing store a read goes to.
///
/// Task 4 adds the `Cache(stores::Stores)` variant; until then every read is a
/// live impersonated request.
#[derive(Debug)]
pub enum Source {
    /// One impersonated LIST/GET per request. No shared cache, so the apiserver's
    /// own RBAC is the only filter the read needs.
    Impersonated,
}
