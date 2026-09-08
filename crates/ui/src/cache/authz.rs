//! Per-identity authorization for cache-backed reads.
//!
//! Objects in the shared stores were fetched by the UI's own ServiceAccount, so
//! the apiserver did not filter them for the caller — which means the cache path
//! has to re-establish what the impersonated path would have got for free. Every
//! cache read is gated by a `SubjectAccessReview` for that identity (cluster-wide
//! first, then per-namespace over the namespaces the stores actually hold), with
//! a short TTL so a revoked RoleBinding stops mattering in seconds.
