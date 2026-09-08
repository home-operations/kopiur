//! Who the browser is talking as, and what that identity may do.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// How the backend established the caller's identity.
///
/// `Eq` + `Hash` because this is a field of the backend's `Identity`, which keys
/// the impersonating-client and `SubjectAccessReview` caches: two callers that
/// differ only in how they were authenticated must not share a cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum IdentitySource {
    /// An authenticating proxy in front of the UI supplied the user and groups
    /// via trusted headers.
    TrustedHeaders,
    /// No proxy identity was presented; the request runs as the UI's own
    /// service account.
    Anonymous,
}

/// The coarse per-verb permissions the SPA uses to enable or hide controls.
///
/// Each flag is the result of a `SelfSubjectAccessReview`-style check performed
/// server-side for the caller — it is a UI affordance, never the authorization
/// itself, which the apiserver always re-enforces on the real request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Capabilities {
    /// May create `Snapshot` resources (the "snapshot now" action).
    pub create_snapshots: bool,
    /// May delete `Snapshot` resources.
    pub delete_snapshots: bool,
    /// May create `Restore` resources.
    pub create_restores: bool,
    /// May patch `SnapshotPolicy` resources (e.g. suspend/resume).
    pub patch_policies: bool,
    /// May open browse sessions, which exec into a mover pod.
    pub exec_sessions: bool,
}

/// The answer to "who am I" — what the UI shows in the account menu and uses to
/// decide which actions to offer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Me {
    /// Username the backend will impersonate for this caller's requests.
    pub user: String,
    /// Groups the backend will impersonate alongside `user`.
    pub groups: Vec<String>,
    /// Email address, when the authenticating proxy supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// How `user`/`groups` were established.
    pub source: IdentitySource,
    /// Namespace the UI is scoped to, when it runs namespace-scoped rather than
    /// cluster-wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// What this identity is allowed to do.
    pub can: Capabilities,
}
