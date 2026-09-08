//! Snapshot listing and inspection: the join behind `kubectl kopiur snapshots`.
//!
//! Everything here is the *data* half: which Snapshots a filter selects, in
//! what order, and the kube IO that fetches them. Rendering (tables, `-o name`,
//! humanized cells) belongs to the caller — `kubectl kopiur` draws a table, the
//! web UI draws something else, and both agree on these rows.

use chrono::{DateTime, Utc};
use kopiur_api::common::RepositoryKind;
use kopiur_api::consts::{CONFIG_LABEL, ORIGIN_LABEL, REPOSITORY_UID_LABEL};
use kopiur_api::{ClusterRepository, Origin, Repository, Snapshot};
use kube::api::{Api, ListParams};

use crate::ctx::{OpsCtx, Scope};
use crate::error::{OpsError, classify_kube};

/// A resolved `--repository` filter: the repo's identity plus its UID (which
/// discovered Snapshots carry as a dedup label).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFilter {
    /// The repository's `metadata.uid`.
    pub uid: String,
    /// Its name, matched against `status.resolved.repository`.
    pub name: String,
    /// Repository vs ClusterRepository.
    pub kind: RepositoryKind,
    /// The namespace the Repository lives in; `None` for ClusterRepository.
    pub namespace: Option<String>,
}

/// The label-backed half of a snapshot-list query, free of any CLI parser
/// type so a web request can build the same filter a `--policy`/`--origin`
/// flag pair does. The `--repository` half is not here: it cannot be a label
/// selector (see [`label_selector`]) and resolves to a [`RepoFilter`] instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotListFilter {
    /// Only snapshots produced from this SnapshotPolicy.
    pub policy: Option<String>,
    /// Only snapshots with this origin.
    pub origin: Option<Origin>,
}

/// Server-side label selector for the list call. `policy` and `origin`
/// map 1:1 onto the labels the operator stamps; a repository filter cannot be
/// a selector (produced Snapshots record their repository in status, not a
/// label) so it filters client-side via [`matches_repository`].
pub fn label_selector(filter: &SnapshotListFilter) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(policy) = &filter.policy {
        parts.push(format!("{CONFIG_LABEL}={policy}"));
    }
    if let Some(origin) = filter.origin {
        parts.push(format!("{ORIGIN_LABEL}={}", origin.label_value()));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

/// Does this Snapshot belong to the filtered repository? Two paths, matching
/// how the operator records the relationship:
/// - discovered Snapshots carry the repository UID as a dedup label;
/// - produced Snapshots pin the `RepositoryRef` in `status.resolved.repository`
///   (namespace absent = the Snapshot's own namespace).
pub fn matches_repository(snap: &Snapshot, filter: &RepoFilter) -> bool {
    if let Some(labels) = &snap.metadata.labels
        && labels.get(REPOSITORY_UID_LABEL) == Some(&filter.uid)
    {
        return true;
    }
    let Some(rref) = snap
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.repository.as_ref())
    else {
        return false;
    };
    if rref.kind != filter.kind || rref.name != filter.name {
        return false;
    }
    match filter.kind {
        // Cluster-scoped: name+kind is the whole identity.
        RepositoryKind::ClusterRepository => true,
        // Namespaced: an absent ref namespace means "same as the Snapshot".
        RepositoryKind::Repository => {
            let effective = rref
                .namespace
                .as_deref()
                .or(snap.metadata.namespace.as_deref());
            effective == filter.namespace.as_deref()
        }
    }
}

/// Convert a k8s-openapi `Time` (a `jiff::Timestamp` since k8s-openapi 0.27)
/// to the chrono type the humanizers use.
pub fn meta_time(
    t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time,
) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(t.0.as_second(), t.0.subsec_nanosecond().max(0) as u32)
}

/// Sort key: most recent first by run start time, falling back to CR creation
/// time for Snapshots that never started (Pending/Discovered).
pub fn sort_key(snap: &Snapshot) -> DateTime<Utc> {
    snap.status
        .as_ref()
        .and_then(|s| s.timing.as_ref())
        .and_then(|t| t.start_time.as_deref())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
        .or(snap
            .metadata
            .creation_timestamp
            .as_ref()
            .and_then(meta_time))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Resolve a `--repository NAME` into a [`RepoFilter`] by looking the repo up
/// (its UID backs the discovered-Snapshot label path). Shared with `status`.
pub async fn resolve_repo_filter_for(
    ctx: &OpsCtx,
    name: &str,
    kind: RepositoryKind,
    repository_namespace: Option<&str>,
) -> Result<RepoFilter, OpsError> {
    match kind {
        RepositoryKind::Repository => {
            let ns = repository_namespace
                .map(str::to_string)
                .unwrap_or_else(|| ctx.namespace.clone());
            let api: Api<Repository> = Api::namespaced(ctx.client.clone(), &ns);
            let repo = get_repo(api, "Repository", "repositories", name, Some(&ns)).await?;
            Ok(RepoFilter {
                uid: repo.metadata.uid.unwrap_or_default(),
                name: name.to_string(),
                kind,
                namespace: Some(ns),
            })
        }
        RepositoryKind::ClusterRepository => {
            let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
            let repo =
                get_repo(api, "ClusterRepository", "clusterrepositories", name, None).await?;
            Ok(RepoFilter {
                uid: repo.metadata.uid.unwrap_or_default(),
                name: name.to_string(),
                kind,
                namespace: None,
            })
        }
    }
}

async fn get_repo<K>(
    api: Api<K>,
    kind: &'static str,
    plural: &'static str,
    name: &str,
    namespace: Option<&str>,
) -> Result<K, OpsError>
where
    K: kube::Resource + Clone + std::fmt::Debug + serde::de::DeserializeOwned,
{
    api.get(name)
        .await
        .map_err(|e| classify_kube("get", kind, plural, namespace, Some(name), e))
}

/// List Snapshots in the context's scope, narrowed server-side by `selector`
/// (build one with [`label_selector`]), newest run first per [`sort_key`].
///
/// A repository filter is applied by the caller with [`matches_repository`]:
/// it is client-side by necessity, and filtering a stably-sorted list keeps
/// this ordering intact.
pub async fn list_snapshots(
    ctx: &OpsCtx,
    selector: Option<&str>,
) -> Result<Vec<Snapshot>, OpsError> {
    let api: Api<Snapshot> = match &ctx.scope {
        Scope::All => Api::all(ctx.client.clone()),
        Scope::Namespace(ns) => Api::namespaced(ctx.client.clone(), ns),
    };
    let mut params = ListParams::default();
    if let Some(selector) = selector {
        params = params.labels(selector);
    }
    let list_ns = match &ctx.scope {
        Scope::All => None,
        Scope::Namespace(ns) => Some(ns.as_str()),
    };
    let listed = api
        .list(&params)
        .await
        .map_err(|e| classify_kube("list", "Snapshot", "snapshots", list_ns, None, e))?;

    let mut snaps = listed.items;
    snaps.sort_by_key(|s| std::cmp::Reverse(sort_key(s)));
    Ok(snaps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use kopiur_api::testutil::from_yaml;

    const SUCCEEDED_SNAPSHOT: &str = r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: nightly-20260611
  namespace: media
  creationTimestamp: "2026-06-11T03:00:00Z"
  labels:
    kopiur.home-operations.com/origin: scheduled
    kopiur.home-operations.com/config: nightly
spec:
  policyRef:
    name: nightly
  deletionPolicy: Delete
status:
  phase: Succeeded
  origin: scheduled
  snapshot:
    kopiaSnapshotID: a1b2c3d4e5f6
    identity:
      username: nightly
      hostname: media
      sourcePath: /pvc/data
  timing:
    startTime: "2026-06-11T03:00:12Z"
    endTime: "2026-06-11T03:05:12Z"
    durationSeconds: 300
  stats:
    sizeBytes: 5368709120
    filesNew: 10
    filesModified: 5
    filesUnchanged: 985
  resolved:
    repository:
      kind: Repository
      name: nas
"#;

    #[test]
    fn selector_combines_policy_and_origin_labels() {
        let mut filter = SnapshotListFilter::default();
        assert_eq!(label_selector(&filter), None);
        filter.policy = Some("nightly".into());
        filter.origin = Some(Origin::Discovered);
        assert_eq!(
            label_selector(&filter).unwrap(),
            "kopiur.home-operations.com/config=nightly,kopiur.home-operations.com/origin=discovered"
        );
    }

    #[test]
    fn repository_filter_matches_via_uid_label_or_resolved_ref() {
        let produced: Snapshot = from_yaml(SUCCEEDED_SNAPSHOT);
        let filter = RepoFilter {
            uid: "repo-uid-1".into(),
            name: "nas".into(),
            kind: RepositoryKind::Repository,
            namespace: Some("media".into()),
        };
        // Produced snapshot: matches through status.resolved.repository
        // (ref namespace absent = the snapshot's own namespace).
        assert!(matches_repository(&produced, &filter));

        // Same repo name in a different namespace must NOT match.
        let other_ns = RepoFilter {
            namespace: Some("other".into()),
            ..filter.clone()
        };
        assert!(!matches_repository(&produced, &other_ns));

        // A ClusterRepository filter of the same name must NOT match either.
        let cluster = RepoFilter {
            kind: RepositoryKind::ClusterRepository,
            namespace: None,
            ..filter.clone()
        };
        assert!(!matches_repository(&produced, &cluster));

        // Discovered snapshot: matches through the repository-uid label.
        let discovered: Snapshot = from_yaml(
            r#"
metadata:
  name: discovered-1
  namespace: media
  labels:
    kopiur.home-operations.com/repository-uid: repo-uid-1
spec: {}
"#,
        );
        assert!(matches_repository(&discovered, &filter));
        let wrong_uid = RepoFilter {
            uid: "other-uid".into(),
            ..filter
        };
        assert!(!matches_repository(&discovered, &wrong_uid));
    }

    #[test]
    fn sort_key_prefers_start_time_and_falls_back_to_creation() {
        let with_start: Snapshot = from_yaml(SUCCEEDED_SNAPSHOT);
        assert_eq!(
            sort_key(&with_start),
            Utc.with_ymd_and_hms(2026, 6, 11, 3, 0, 12).unwrap()
        );
        let pending: Snapshot = from_yaml(
            r#"
metadata:
  name: pending-1
  creationTimestamp: "2026-06-10T00:00:00Z"
spec: {}
"#,
        );
        assert_eq!(
            sort_key(&pending),
            Utc.with_ymd_and_hms(2026, 6, 10, 0, 0, 0).unwrap()
        );
    }
}
