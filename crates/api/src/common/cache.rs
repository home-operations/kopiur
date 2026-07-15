use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a mover's kopia cache volume is provisioned.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum CacheVolumeMode {
    /// Cache lives only for the run (ephemeral volume or `emptyDir`), fresh each run; the default.
    #[default]
    Ephemeral,
    /// Cache persists across runs in a controller-owned `ReadWriteOnce` PVC (a warm kopia cache).
    Persistent,
}

/// kopia cache defaults inherited by every mover unless overridden per-recipe.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheDefaults {
    /// Size of the PVC backing the mover's kopia cache (e.g. `10Gi`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<String>,
    /// StorageClass for the cache PVC; absent uses the cluster default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// kopia metadata cache budget in MiB (`--metadata-cache-size-mb`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_cache_size_mb: Option<i64>,
    /// kopia content cache budget in MiB (`--content-cache-size-mb`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_cache_size_mb: Option<i64>,
    /// How the cache volume is provisioned (`Ephemeral` default, or `Persistent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CacheVolumeMode>,
}

impl CacheDefaults {
    /// Overlay `over` onto `base` field-by-field — a value set in `over` wins,
    /// otherwise `base`'s is kept. Resolves a mover's effective cache config from the
    /// repository's `cacheDefaults` (base) and the run's `mover.cache` (override).
    /// Returns `None` only when both are absent.
    pub fn merge(
        base: Option<&CacheDefaults>,
        over: Option<&CacheDefaults>,
    ) -> Option<CacheDefaults> {
        match (base, over) {
            (None, None) => None,
            (Some(b), None) => Some(b.clone()),
            (None, Some(o)) => Some(o.clone()),
            (Some(b), Some(o)) => Some(CacheDefaults {
                capacity: o.capacity.clone().or_else(|| b.capacity.clone()),
                storage_class_name: o
                    .storage_class_name
                    .clone()
                    .or_else(|| b.storage_class_name.clone()),
                metadata_cache_size_mb: o.metadata_cache_size_mb.or(b.metadata_cache_size_mb),
                content_cache_size_mb: o.content_cache_size_mb.or(b.content_cache_size_mb),
                mode: o.mode.or(b.mode),
            }),
        }
    }

    /// The provisioning mode, defaulting to `Ephemeral` when unset.
    pub fn effective_mode(&self) -> CacheVolumeMode {
        self.mode.unwrap_or_default()
    }
}

/// Defaults for the deep-verification **scratch** volume — the throwaway restore-test target.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScratchDefaults {
    /// StorageClass for the ephemeral scratch PVC; only applies when `capacity` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// Size of the ephemeral scratch PVC (e.g. `100Gi`); absent falls back to an `emptyDir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<String>,
}

impl ScratchDefaults {
    /// Overlay `over` onto `base` field-by-field — a value set in `over` wins,
    /// otherwise `base`'s is kept. Resolves a deep-verify's effective scratch config
    /// from the repository's `moverDefaults.scratch` (base) and the recipe's
    /// `verification.deep.{storageClassName,capacity}` (override). Returns `None` only
    /// when both are absent. Mirrors [`CacheDefaults::merge`].
    pub fn merge(
        base: Option<&ScratchDefaults>,
        over: Option<&ScratchDefaults>,
    ) -> Option<ScratchDefaults> {
        match (base, over) {
            (None, None) => None,
            (Some(b), None) => Some(b.clone()),
            (None, Some(o)) => Some(o.clone()),
            (Some(b), Some(o)) => Some(ScratchDefaults {
                storage_class_name: o
                    .storage_class_name
                    .clone()
                    .or_else(|| b.storage_class_name.clone()),
                capacity: o.capacity.clone().or_else(|| b.capacity.clone()),
            }),
        }
    }
}

/// How catalog discovery treats snapshots written by ANOTHER cluster.
/// `status.catalog.foreignSnapshotCount` counts an identity hostname carrying
/// a different `.<cluster>` suffix ALWAYS, under either value below, plus —
/// under `Ignore` only — a bare hostname naming no allowed namespace here.
/// Under `Fallback`, that same disallowed bare host is NOT counted foreign:
/// it materializes into `catalog.fallbackNamespace` exactly like a disallowed
/// `OwnCluster` host would, so it is placed rather than dropped-and-counted
/// (see `classify_hostname`/`decide_cluster_placement`).
/// Meaningful only when `identityDefaults.cluster` is set: without a cluster
/// identity there is no notion of "foreign" and the legacy
/// hostname-names-a-namespace placement applies (validators enforce this).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum ForeignSnapshots {
    /// Skip foreign snapshots: no discovered `Snapshot` CR is materialized;
    /// they are counted in `status.catalog.foreignSnapshotCount` and the
    /// `kopiur_repo_foreign_snapshots` gauge, never silently invisible.
    #[default]
    Ignore,
    /// Materialize foreign snapshots into `catalog.fallbackNamespace`
    /// (ClusterRepository only; requires `fallbackNamespace`).
    Fallback,
}

/// Bounds on materialization of `origin: discovered` `Snapshot` CRs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBounds {
    /// How many discovered `Snapshot` CRs to keep materialized (bounds etcd footprint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain: Option<CatalogRetain>,
    /// Opt-in: periodically re-scan the repository to keep discovered `Snapshot` CRs
    /// current (re-list snapshots; for object-store / volume-backed repos this recycles
    /// the bootstrap Job every `refreshInterval`). **Off by default** — the repository
    /// still bootstraps once, re-bootstraps on a spec change, and re-probes on a backup
    /// failure, but does not re-run on a timer. Enable it if you rely on discovered
    /// snapshots reflecting changes made outside this operator.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub periodic_refresh: bool,
    /// How often to re-scan when `periodicRefresh: true` (Go-style duration; minimum
    /// `30s`, default `1h`). Inert unless `periodicRefresh` is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_catalog_refresh_interval")]
    pub refresh_interval: Option<String>,
    /// Where to materialize discovered `Snapshot`s with no allowed-namespace mapping (ClusterRepository only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_namespace: Option<String>,
    /// How to treat discovered snapshots written by another cluster. Requires
    /// `identityDefaults.cluster`. Absent resolves to `Ignore`. When BOTH
    /// `cluster` and `fallbackNamespace` are set this field is REQUIRED
    /// (explicit), so adopting a cluster identity can never silently switch a
    /// configured fallback collector off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreign_snapshots: Option<ForeignSnapshots>,
}

impl CatalogBounds {
    /// Whether periodic re-scan is opted in (`catalog.periodicRefresh`). Off by
    /// default, so a repository does not re-run its bootstrap Job on a timer.
    pub fn periodic_refresh_enabled(catalog: Option<&Self>) -> bool {
        catalog.is_some_and(|c| c.periodic_refresh)
    }

    /// The effective catalog re-scan cadence used **when `periodicRefresh` is on**:
    /// `refreshInterval` when set and parseable, else
    /// [`crate::consts::DEFAULT_CATALOG_REFRESH_INTERVAL`]. (The webhook rejects an
    /// unparseable value, so the fallback only covers objects admitted before the
    /// validator existed.)
    pub fn effective_refresh_interval(catalog: Option<&Self>) -> std::time::Duration {
        catalog
            .and_then(|c| c.refresh_interval.as_deref())
            .and_then(crate::duration::parse_go_duration)
            .unwrap_or(crate::consts::DEFAULT_CATALOG_REFRESH_INTERVAL)
    }

    /// The effective foreign-snapshot policy: `catalog.foreignSnapshots` when
    /// set, else [`ForeignSnapshots::Ignore`] (absent `catalog`, or the field
    /// unset). Only meaningful alongside a cluster identity
    /// (`identityDefaults.cluster`) — the validators enforce that coupling, so
    /// this resolver never has to guess at one.
    pub fn effective_foreign_snapshots(catalog: Option<&Self>) -> ForeignSnapshots {
        catalog
            .and_then(|c| c.foreign_snapshots)
            .unwrap_or_default()
    }
}

/// schemars default for [`CatalogBounds::refresh_interval`] — the string form of
/// [`DEFAULT_CATALOG_REFRESH_INTERVAL`](crate::consts::DEFAULT_CATALOG_REFRESH_INTERVAL)
/// (`1h`). `effective_refresh_interval` resolves an absent value to that same
/// duration and the field is inert unless `periodicRefresh`, so materializing
/// `1h` is behavior-preserving. A unit test pins `"1h"` to the constant.
fn default_catalog_refresh_interval() -> Option<String> {
    Some("1h".to_string())
}

/// Bounds on the *number* of discovered `Snapshot` CRs kept materialized.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRetain {
    /// Keep the most-recent N discovered `Snapshot` CRs per identity; `0` disables materialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_identity: Option<i64>,
    /// Expire discovered `Snapshot` CRs older than this many days (minimum 1); kopia snapshots untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_days: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_refresh_interval_schema_default_matches_the_duration_constant() {
        // The schema default is the STRING "1h"; the controller resolves an
        // absent value to the Duration constant. Keep them in lockstep so
        // server-side defaulting materializes exactly the resolver's fallback.
        let s = default_catalog_refresh_interval().expect("some");
        assert_eq!(
            crate::duration::parse_go_duration(&s),
            Some(crate::consts::DEFAULT_CATALOG_REFRESH_INTERVAL),
            "default_catalog_refresh_interval() string must parse to DEFAULT_CATALOG_REFRESH_INTERVAL"
        );
        // And the resolver agrees when the field is absent.
        assert_eq!(
            CatalogBounds::effective_refresh_interval(None),
            crate::consts::DEFAULT_CATALOG_REFRESH_INTERVAL
        );
    }

    #[test]
    fn foreign_snapshots_serializes_to_bare_pascal_case_strings() {
        // Same wire encoding as TakeoverPolicy/NamespaceDeletePolicy — bare
        // PascalCase unit-variant strings, not a `{ "ignore": {} }` externally
        // tagged shape (this is a closed enum with no payload).
        assert_eq!(
            serde_json::to_value(ForeignSnapshots::Ignore).unwrap(),
            "Ignore"
        );
        assert_eq!(
            serde_json::to_value(ForeignSnapshots::Fallback).unwrap(),
            "Fallback"
        );
        assert_eq!(ForeignSnapshots::default(), ForeignSnapshots::Ignore);
        assert_eq!(
            serde_json::from_value::<ForeignSnapshots>(serde_json::json!("Ignore")).unwrap(),
            ForeignSnapshots::Ignore
        );
        assert_eq!(
            serde_json::from_value::<ForeignSnapshots>(serde_json::json!("Fallback")).unwrap(),
            ForeignSnapshots::Fallback
        );
        // Unknown variant rejected.
        assert!(serde_json::from_value::<ForeignSnapshots>(serde_json::json!("Delete")).is_err());
    }

    #[test]
    fn effective_foreign_snapshots_defaults_to_ignore() {
        // Absent catalog.
        assert_eq!(
            CatalogBounds::effective_foreign_snapshots(None),
            ForeignSnapshots::Ignore
        );
        // Catalog present but the field unset.
        let bare = CatalogBounds::default();
        assert_eq!(
            CatalogBounds::effective_foreign_snapshots(Some(&bare)),
            ForeignSnapshots::Ignore
        );
        // Explicit value resolves verbatim.
        let explicit = CatalogBounds {
            foreign_snapshots: Some(ForeignSnapshots::Fallback),
            ..Default::default()
        };
        assert_eq!(
            CatalogBounds::effective_foreign_snapshots(Some(&explicit)),
            ForeignSnapshots::Fallback
        );
    }
}
