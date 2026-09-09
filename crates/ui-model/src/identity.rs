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
///
/// # One flag per (verb, resource) the API actually writes
///
/// The flags are named after the review that produced them, not after a button,
/// because several buttons share one review and two buttons can share one name
/// while asking different questions. Every mutating endpoint maps onto one:
///
/// | endpoint | flag |
/// |---|---|
/// | `POST /actions/snapshot-now` | [`Self::create_snapshots`] |
/// | `POST /actions/restore` | [`Self::create_restores`] |
/// | `DELETE /snapshots/{ns}/{name}` | [`Self::delete_snapshots`] |
/// | `POST /actions/suspend` `kind: policy` | [`Self::patch_policies`] |
/// | `POST /actions/suspend` `kind: schedule` | [`Self::patch_schedules`] |
/// | `POST /actions/suspend` `kind: repository` | [`Self::patch_repositories`] |
/// | `POST /actions/suspend` `kind: cluster-repository` | [`Self::patch_cluster_repositories`] |
/// | `POST /actions/suspend` `kind: replication` | [`Self::patch_repository_replications`] |
/// | `POST /actions/suspend` `kind: snapshot-replication` | [`Self::patch_snapshot_replications`] |
/// | `POST /actions/maintenance-run` | [`Self::patch_maintenances`] |
/// | `POST /actions/replication-run` | [`Self::patch_repository_replications`] or [`Self::patch_snapshot_replications`], by the kind the request names or detection finds |
/// | `POST /actions/scan-catalog` | [`Self::patch_repositories`] or [`Self::patch_cluster_repositories`], by `kind` |
/// | `POST`/`DELETE …/session`, `GET …/tree`, `GET …/file` | [`Self::exec_sessions`] |
///
/// # These answers are namespace-scoped
///
/// Every flag except [`Self::patch_cluster_repositories`] is the answer for the
/// namespace `/me` was asked about (`GET /api/v1/me?namespace=`). With no
/// `?namespace=` the review is **cluster-scoped**, so a user holding only a
/// namespaced `RoleBinding` gets all-`false` and a UI where every control is
/// disabled with a reason that is not true. **The SPA must re-fetch `/me` for
/// each namespace it scopes to**, and must not cache one answer across a
/// namespace switch.
///
/// [`Self::patch_cluster_repositories`] is asked cluster-scoped whatever
/// `?namespace=` says, because `ClusterRepository` is a cluster-scoped resource:
/// a namespaced review of it would report a `RoleBinding` grant that cannot
/// actually authorize the write.
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
    /// May patch `SnapshotPolicy` resources (suspend/resume).
    pub patch_policies: bool,
    /// May patch `SnapshotSchedule` resources (suspend/resume).
    pub patch_schedules: bool,
    /// May patch `Repository` resources (suspend/resume, catalog scan).
    pub patch_repositories: bool,
    /// May patch `ClusterRepository` resources (suspend/resume, catalog scan).
    ///
    /// Always the cluster-scoped answer — see the type docs.
    pub patch_cluster_repositories: bool,
    /// May patch `Maintenance` resources (request a manual run).
    pub patch_maintenances: bool,
    /// May patch `RepositoryReplication` resources (suspend/resume, run now).
    pub patch_repository_replications: bool,
    /// May patch `SnapshotReplication` resources (suspend/resume, run now).
    pub patch_snapshot_replications: bool,
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
