//! Establishing who the caller is, and speaking to the apiserver as them.
//!
//! `kopiur-ui` authorizes nothing itself. It resolves an [`identity::Identity`]
//! from trusted proxy headers (or the configured anonymous identity), verifies
//! that the request really arrived through the proxy ([`proxy_secret`]), and then
//! issues every apiserver call *impersonating* that identity ([`impersonate`]) —
//! so RBAC is enforced exactly where it always was, in the apiserver, and a bug
//! in this crate cannot grant a permission the caller does not have.

pub mod csrf;
pub mod identity;
pub mod impersonate;
pub mod proxy_secret;
pub mod redact;

/// Everything the identity middleware needs at request time: the resolved
/// [`crate::config::AuthConfig`] and the bounded per-identity impersonating
/// client cache.
///
/// Filled in by Task 3.
#[derive(Debug, Default)]
pub struct AuthState {}
