//! The single error shape every kopiur UI endpoint returns.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// An RFC 7807 `application/problem+json` body, extended with kopiur's
/// what/why/fix triple so the SPA can render an actionable error instead of a
/// status code.
///
/// The three extension members mirror the message discipline the operator's own
/// `OpsError`/`thiserror` types follow: say what happened, why it happened, and
/// what the operator should do next.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Problem {
    /// Stable URI reference identifying the problem class, e.g.
    /// `https://kopiur.dev/problems/forbidden`. Machine-readable; the SPA
    /// switches on this, never on `title`.
    pub r#type: String,
    /// Short, human-readable summary of the problem class.
    pub title: String,
    /// HTTP status code repeated in the body, per RFC 7807.
    pub status: u16,
    /// Human-readable explanation specific to this occurrence.
    pub detail: String,
    /// What happened, in the user's terms.
    pub what: String,
    /// Why it happened — the underlying cause.
    pub why: String,
    /// What to do about it — the remediation step.
    pub fix: String,
    /// URI reference identifying this specific occurrence, typically the
    /// request path that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// The Kubernetes `Status.reason` (e.g. `Forbidden`, `NotFound`,
    /// `Conflict`) when the failure came from the apiserver, so the SPA can
    /// distinguish an RBAC denial from a missing object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kube_reason: Option<String>,
}
