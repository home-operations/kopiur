//! Suspend/resume decisions over the suspendable kopiur kinds.
//!
//! Every kind that exposes a declarative suspend field (ADR-0005 §14(e)) is
//! toggled through one entry point: the kind picks the API and the JSON path,
//! and the read-before-patch makes the operation idempotent — asking for the
//! value it already has issues no write at all.

use kopiur_api::{
    ClusterRepository, Repository, RepositoryReplication, SnapshotPolicy, SnapshotReplication,
    SnapshotSchedule,
};
use kube::api::{Api, Patch, PatchParams};
use serde::de::DeserializeOwned;

use crate::ctx::OpsCtx;
use crate::error::{OpsError, classify_kube};

/// The `managedFields` owner recorded on the suspend PATCH, so the change is
/// attributed to kopiur's own tooling rather than an anonymous client.
const FIELD_MANAGER: &str = "kubectl-kopiur";

/// Every kind that exposes a declarative suspend field (ADR-0005 §14(e)).
/// Closed enum: the patch path and API routing `match` it exhaustively, so a
/// future suspendable kind cannot compile until both are extended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendableKind {
    /// SnapshotPolicy — `spec.suspend`.
    Policy,
    /// SnapshotSchedule — `spec.schedule.suspend`.
    Schedule,
    /// Repository — `spec.suspend`.
    Repository,
    /// ClusterRepository (cluster-scoped) — `spec.suspend`.
    ClusterRepository,
    /// RepositoryReplication — `spec.suspend`.
    Replication,
    /// SnapshotReplication — `spec.suspend`.
    SnapshotReplication,
}

/// Identity strings for one suspendable kind, used in messages and `-o name`.
#[derive(Debug, Clone, Copy)]
pub struct KindMeta {
    /// CamelCase kind, for messages.
    pub kind: &'static str,
    /// Lowercase singular, for `-o name` (`<singular>.<group>/<name>`).
    pub singular: &'static str,
    /// Lowercase plural, for RBAC hints and `kubectl get` remediation.
    pub plural: &'static str,
}

/// Resolve the naming for each suspendable kind. Exhaustive.
pub fn kind_meta(kind: SuspendableKind) -> KindMeta {
    match kind {
        SuspendableKind::Policy => KindMeta {
            kind: "SnapshotPolicy",
            singular: "snapshotpolicy",
            plural: "snapshotpolicies",
        },
        SuspendableKind::Schedule => KindMeta {
            kind: "SnapshotSchedule",
            singular: "snapshotschedule",
            plural: "snapshotschedules",
        },
        SuspendableKind::Repository => KindMeta {
            kind: "Repository",
            singular: "repository",
            plural: "repositories",
        },
        SuspendableKind::ClusterRepository => KindMeta {
            kind: "ClusterRepository",
            singular: "clusterrepository",
            plural: "clusterrepositories",
        },
        SuspendableKind::Replication => KindMeta {
            kind: "RepositoryReplication",
            singular: "repositoryreplication",
            plural: "repositoryreplications",
        },
        SuspendableKind::SnapshotReplication => KindMeta {
            kind: "SnapshotReplication",
            singular: "snapshotreplication",
            plural: "snapshotreplications",
        },
    }
}

/// The merge patch that sets the suspend field for this kind. The path is the
/// only thing that varies: `SnapshotSchedule` nests it under `spec.schedule`
/// (it is a schedule property there), every other kind has `spec.suspend`.
pub fn patch_for(kind: SuspendableKind, desired: bool) -> serde_json::Value {
    match kind {
        SuspendableKind::Schedule => {
            serde_json::json!({ "spec": { "schedule": { "suspend": desired } } })
        }
        SuspendableKind::Policy
        | SuspendableKind::Repository
        | SuspendableKind::ClusterRepository
        | SuspendableKind::Replication
        | SuspendableKind::SnapshotReplication => {
            serde_json::json!({ "spec": { "suspend": desired } })
        }
    }
}

/// Outcome of a suspend/resume, with everything a renderer needs.
#[derive(Debug, serde::Serialize)]
pub struct SuspendReport {
    /// Kind naming (not serialized as-is; flattened into the fields below).
    #[serde(skip)]
    pub meta: KindMeta,
    /// CamelCase kind.
    pub kind: &'static str,
    /// Object name.
    pub name: String,
    /// Namespace, absent for ClusterRepository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Suspend value before this command ran.
    pub previous: bool,
    /// Suspend value requested (and now in effect).
    pub desired: bool,
    /// The full object after patching (verbatim CR for `-o yaml|json`).
    pub object: serde_json::Value,
}

/// Toggle one object's suspend field: get (for the previous value and a real
/// not-found message), merge-patch only when it would change, return the
/// resulting object. Idempotent by construction.
async fn toggle<K>(
    api: Api<K>,
    meta: KindMeta,
    namespace: Option<&str>,
    name: &str,
    kind: SuspendableKind,
    desired: bool,
    current: impl Fn(&K) -> bool,
) -> Result<SuspendReport, OpsError>
where
    K: kube::Resource + Clone + std::fmt::Debug + DeserializeOwned + serde::Serialize,
{
    let obj = api
        .get(name)
        .await
        .map_err(|e| classify_kube("get", meta.kind, meta.plural, namespace, Some(name), e))?;
    let previous = current(&obj);
    let patched = if previous == desired {
        obj
    } else {
        let params = PatchParams {
            field_manager: Some(FIELD_MANAGER.to_string()),
            ..Default::default()
        };
        api.patch(name, &params, &Patch::Merge(patch_for(kind, desired)))
            .await
            .map_err(|e| classify_kube("patch", meta.kind, meta.plural, namespace, Some(name), e))?
    };
    let object = serde_json::to_value(&patched).map_err(|e| OpsError::Serialization {
        what: "patched object",
        source: e.into(),
    })?;
    Ok(SuspendReport {
        meta,
        kind: meta.kind,
        name: name.to_string(),
        namespace: namespace.map(str::to_string),
        previous,
        desired,
        object,
    })
}

/// Set (or clear) one object's suspend field, whatever its kind. `desired` is
/// the value to end up with — `true` suspends, `false` resumes — so both verbs
/// are the same call, and asking for the value already in place is a no-op that
/// still reports the object.
///
/// `namespace` overrides the context's namespace for the namespaced kinds;
/// `None` uses [`OpsCtx::namespace`]. It is ignored for `ClusterRepository`,
/// which is cluster-scoped — that arm always reports no namespace.
pub async fn set_suspended(
    ctx: &OpsCtx,
    kind: SuspendableKind,
    namespace: Option<&str>,
    name: &str,
    desired: bool,
) -> Result<SuspendReport, OpsError> {
    let meta = kind_meta(kind);
    let ns = namespace.unwrap_or(ctx.namespace.as_str());
    let client = ctx.client.clone();
    match kind {
        SuspendableKind::Policy => {
            let api: Api<SnapshotPolicy> = Api::namespaced(client, ns);
            toggle(api, meta, Some(ns), name, kind, desired, |o| o.spec.suspend).await
        }
        SuspendableKind::Schedule => {
            let api: Api<SnapshotSchedule> = Api::namespaced(client, ns);
            toggle(api, meta, Some(ns), name, kind, desired, |o| {
                o.spec.schedule.suspend
            })
            .await
        }
        SuspendableKind::Repository => {
            let api: Api<Repository> = Api::namespaced(client, ns);
            toggle(api, meta, Some(ns), name, kind, desired, |o| o.spec.suspend).await
        }
        SuspendableKind::ClusterRepository => {
            let api: Api<ClusterRepository> = Api::all(client);
            toggle(api, meta, None, name, kind, desired, |o| o.spec.suspend).await
        }
        SuspendableKind::Replication => {
            let api: Api<RepositoryReplication> = Api::namespaced(client, ns);
            toggle(api, meta, Some(ns), name, kind, desired, |o| o.spec.suspend).await
        }
        SuspendableKind::SnapshotReplication => {
            let api: Api<SnapshotReplication> = Api::namespaced(client, ns);
            toggle(api, meta, Some(ns), name, kind, desired, |o| o.spec.suspend).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_patches_the_nested_schedule_suspend_path() {
        let p = patch_for(SuspendableKind::Schedule, true);
        assert_eq!(p["spec"]["schedule"]["suspend"], true);
        assert!(p["spec"].get("suspend").is_none());
    }

    #[test]
    fn flat_kinds_patch_spec_suspend() {
        for kind in [
            SuspendableKind::Policy,
            SuspendableKind::Repository,
            SuspendableKind::ClusterRepository,
            SuspendableKind::Replication,
            SuspendableKind::SnapshotReplication,
        ] {
            let p = patch_for(kind, false);
            assert_eq!(p["spec"]["suspend"], false, "{kind:?}");
            assert!(p["spec"].get("schedule").is_none(), "{kind:?}");
        }
    }

    #[test]
    fn every_kind_names_a_distinct_resource() {
        // The naming feeds `-o name`, the RBAC hint and the not-found message:
        // two kinds sharing a plural would send a user to the wrong resource.
        let kinds = [
            SuspendableKind::Policy,
            SuspendableKind::Schedule,
            SuspendableKind::Repository,
            SuspendableKind::ClusterRepository,
            SuspendableKind::Replication,
            SuspendableKind::SnapshotReplication,
        ];
        let mut plurals: Vec<&str> = kinds.iter().map(|k| kind_meta(*k).plural).collect();
        plurals.sort_unstable();
        let unique = plurals.len();
        plurals.dedup();
        assert_eq!(plurals.len(), unique, "{plurals:?}");
        assert_eq!(
            kind_meta(SuspendableKind::Replication).kind,
            "RepositoryReplication"
        );
        assert_eq!(
            kind_meta(SuspendableKind::SnapshotReplication).singular,
            "snapshotreplication"
        );
    }
}
