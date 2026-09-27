//! Request bodies for the UI's mutating endpoints.
//!
//! Every body is `#[serde(deny_unknown_fields)]`: a typo in a field name is a
//! 400 the user can see and fix, not a silently ignored option that makes the
//! action do something other than what was asked. That matters more here than
//! anywhere else in the contract, because these bodies create and destroy
//! backups.
//!
//! Every body also carries `#[ts(optional_fields = nullable)]`, so an `Option<T>`
//! field exports as `field?: T | null` rather than `field: T | null`. That is what
//! `#[serde(default)]` actually means on the wire — the key may be omitted — and
//! without it the SPA would be forced to spell out `null` for every option it does
//! not set.
//!
//! Note what this deliberately does *not* cover: `SnapshotNowBody::tags` and
//! `SnapshotNowBody::pin` are `Vec` and `bool`, not `Option`, so they stay required
//! in TypeScript even though `#[serde(default)]` would accept them missing. That is
//! on purpose — `pin` decides whether a snapshot is exempt from GFS pruning, so the
//! caller should have to say `pin: false` rather than get it by omission.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Take a snapshot right now under an existing `SnapshotPolicy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub struct SnapshotNowBody {
    /// Namespace of the policy, and of the `Snapshot` to create.
    pub namespace: String,
    /// Name of the `SnapshotPolicy` to snapshot under.
    pub policy: String,
    /// Name for the new `Snapshot`; the server generates one when omitted.
    #[serde(default)]
    pub name: Option<String>,
    /// Extra kopia tags to attach, as `(key, value)` pairs.
    #[serde(default)]
    pub tags: Vec<(String, String)>,
    /// Pin the resulting kopia manifest, exempting it from GFS pruning.
    #[serde(default)]
    pub pin: bool,
    /// Free-text description recorded on the `Snapshot`.
    #[serde(default)]
    pub description: Option<String>,
    /// Restrict a multi-repository policy to this one repository; omit to fan
    /// out to all of them.
    #[serde(default)]
    pub repository: Option<String>,
}

/// Create a `Restore`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub struct RestoreBody {
    /// Namespace to create the `Restore` in.
    pub namespace: String,
    /// Where the data comes from.
    pub source: RestoreSourceBody,
    /// Where the data goes.
    pub target: RestoreTargetBody,
    /// Repository to read from; inferred from the source when omitted.
    #[serde(default)]
    pub repository: Option<RepositoryRefBody>,
    /// Name for the new `Restore`; the server generates one when omitted.
    #[serde(default)]
    pub name: Option<String>,
    /// Overwrite files that already exist on the target.
    #[serde(default)]
    pub overwrite: Option<bool>,
    /// Which kopia source path to read FROM, overriding the one kopiur would
    /// derive.
    ///
    /// A source *selector*, not a subtree filter: it picks which of a
    /// repository's kopia sources the snapshot is pulled from, and does not
    /// change what is written or restore only part of a snapshot. Mirrors the
    /// CRD's own `fromPolicy.sourcePath` and `identity.sourcePath`, which is
    /// where it lands.
    ///
    /// Needed when the derivation is ambiguous or wrong — most often a
    /// multi-PVC `pvcSelector` policy, where each member wrote its own
    /// `/pvc/<name>` source and the restore has to say which one it wants.
    /// Meaningless with a `snapshotRef` source, which already pins the path its
    /// `Snapshot` was written from.
    #[serde(default)]
    pub source_path: Option<String>,
}

/// Which snapshot to restore, as an externally-tagged union — exactly one
/// variant, matching the `Restore` CRD's own "exactly one of" source surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub enum RestoreSourceBody {
    /// An existing `Snapshot` resource, named directly.
    #[serde(rename_all = "camelCase")]
    SnapshotRef {
        /// `metadata.name` of the `Snapshot`.
        name: String,
        /// `metadata.namespace` of the `Snapshot`; defaults to the restore's
        /// own namespace.
        #[serde(default)]
        namespace: Option<String>,
    },
    /// The latest snapshot a policy produced, optionally stepped back in time.
    #[serde(rename_all = "camelCase")]
    FromPolicy {
        /// `metadata.name` of the `SnapshotPolicy`.
        name: String,
        /// `metadata.namespace` of the policy; defaults to the restore's own
        /// namespace.
        #[serde(default)]
        namespace: Option<String>,
        /// RFC3339 instant to pick the newest snapshot at or before.
        #[serde(default)]
        as_of: Option<String>,
        /// Step back this many snapshots from the newest — `0` is the latest.
        #[serde(default)]
        offset: Option<u32>,
    },
    /// A kopia identity, addressing snapshots the operator may not own.
    #[serde(rename_all = "camelCase")]
    Identity {
        /// Kopia username component of the identity.
        username: String,
        /// Kopia hostname component of the identity.
        hostname: String,
        /// Source path component; required when the identity has more than one.
        #[serde(default)]
        source_path: Option<String>,
        /// A specific kopia manifest ID; the newest is used when omitted.
        #[serde(default)]
        snapshot_id: Option<String>,
    },
}

/// Where a restore writes, as an externally-tagged union — exactly one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub enum RestoreTargetBody {
    /// An existing `PersistentVolumeClaim`.
    #[serde(rename_all = "camelCase")]
    PvcRef {
        /// `metadata.name` of the claim.
        name: String,
    },
    /// A `PersistentVolumeClaim` the operator creates for this restore.
    #[serde(rename_all = "camelCase")]
    Pvc {
        /// `metadata.name` for the new claim.
        name: String,
        /// Storage class for the new claim; the cluster default when omitted.
        #[serde(default)]
        storage_class_name: Option<String>,
        /// Requested size, as a Kubernetes quantity (e.g. `10Gi`).
        size: String,
    },
}

/// A reference to a `Repository` or `ClusterRepository`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub struct RepositoryRefBody {
    /// `Repository` or `ClusterRepository`.
    pub kind: String,
    /// `metadata.name`.
    pub name: String,
    /// `metadata.namespace`; omitted for `ClusterRepository`.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Suspend or resume a resource by flipping its `spec.suspend`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub struct SuspendBody {
    /// CRD kind of the resource to flip.
    pub kind: String,
    /// `metadata.namespace`; omitted for cluster-scoped kinds.
    #[serde(default)]
    pub namespace: Option<String>,
    /// `metadata.name`.
    pub name: String,
    /// `true` to suspend, `false` to resume. Explicit rather than a toggle so
    /// two concurrent clients cannot flip each other's intent.
    pub suspend: bool,
}

/// Request a maintenance run right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub struct MaintenanceRunBody {
    /// Namespace of the `Maintenance` resource.
    pub namespace: String,
    /// Name of the `Maintenance` resource; resolved from `repository` when
    /// omitted.
    #[serde(default)]
    pub name: Option<String>,
    /// The repository to maintain, when addressing it by repository rather than
    /// by `Maintenance` name.
    #[serde(default)]
    pub repository: Option<RepositoryRefBody>,
    /// `quick` or `full`.
    pub mode: String,
}

/// Trigger a replication run right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub struct ReplicationRunBody {
    /// Namespace of the replication resource.
    pub namespace: String,
    /// Name of the replication resource.
    pub name: String,
    /// `RepositoryReplication` or `SnapshotReplication`, or the kebab tokens;
    /// omit to detect by name.
    ///
    /// Detection reads both kinds and fails when one name exists in both — this
    /// is how the caller says which one they meant, so the remediation on that
    /// ambiguity is something the API can actually satisfy.
    #[serde(default)]
    pub kind: Option<String>,
}

/// Trigger a catalog scan of a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub struct ScanCatalogBody {
    /// `Repository` or `ClusterRepository`.
    pub kind: String,
    /// `metadata.namespace`; omitted for `ClusterRepository`.
    #[serde(default)]
    pub namespace: Option<String>,
    /// `metadata.name`.
    pub name: String,
}

/// Open a browse session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
#[ts(export)]
pub struct SessionCreateBody {
    /// How long the session may sit idle before it is reaped; the server's
    /// default applies when omitted.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}
