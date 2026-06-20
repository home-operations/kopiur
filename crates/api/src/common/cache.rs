use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a mover's kopia cache volume is provisioned. ADR §3.1.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum CacheVolumeMode {
    /// Cache lives only for the run: an inline generic ephemeral volume (when
    /// `capacity` is set) or an `emptyDir`, provisioned and garbage-collected with
    /// the mover `Job`. Fresh each run. The default.
    #[default]
    Ephemeral,
    /// Cache persists across runs in a controller-owned PVC (a warm kopia cache).
    /// `ReadWriteOnce`, so it assumes non-overlapping runs for a given owner.
    Persistent,
}

/// Cache defaults inherited by `Snapshot`/`Restore` movers unless overridden. ADR §3.1.
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
    /// How the cache volume is provisioned (`Ephemeral` default, or `Persistent`
    /// for a warm cache reused across runs). ADR §3.1.
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

/// Repository-wide defaults for the deep-verification **scratch** volume — the
/// throwaway restore target a `deep` restore-test writes into and then discards.
/// Inherited by `SnapshotPolicy.spec.verification.deep` unless overridden there,
/// the same `moverDefaults ⊂ recipe` field-wise overlay as [`CacheDefaults`].
///
/// Distinct from [`CacheDefaults`]: scratch is **always ephemeral** (auto-deleted
/// with the verify Job), so unlike the cache it has no `mode` / persistent-PVC form.
/// `storageClassName` only takes effect when `capacity` is set — an `emptyDir` has
/// no StorageClass (the capacity gate, mirrored from the cache).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScratchDefaults {
    /// StorageClass for the ephemeral scratch PVC; absent uses the cluster default.
    /// Only applies when `capacity` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// Size of the ephemeral scratch PVC (e.g. `100Gi`) — size it to comfortably hold
    /// the restored snapshot. When absent (here and on `verification.deep`), scratch
    /// falls back to a node-ephemeral `emptyDir`.
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

/// Bounds on materialization of `origin: discovered` `Snapshot` CRs. ADR §3.1 `catalog`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBounds {
    /// How many discovered `Snapshot` CRs to keep materialized; bounds etcd footprint
    /// for large repositories. Expiring a CR row never deletes the kopia snapshot
    /// behind it (discovered snapshots are always `deletionPolicy: Retain`).
    /// ADR §3.1/§4.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain: Option<CatalogRetain>,
    /// How often to re-scan the repository for snapshots to materialize as (or
    /// expire from) `origin: discovered` `Snapshot` CRs. Go-style duration
    /// (`30s`, `5m`, `1h`); minimum `30s` (webhook-enforced), default `1h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_interval: Option<String>,
    /// Where to materialize discovered `Snapshot`s whose identity hostname does not
    /// map to an allowed namespace (ClusterRepository only; rejected on a namespaced
    /// `Repository`, which always materializes into its own namespace). ADR §3.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_namespace: Option<String>,
}

impl CatalogBounds {
    /// The effective catalog re-scan cadence: `refreshInterval` when set and
    /// parseable, else [`crate::consts::DEFAULT_CATALOG_REFRESH_INTERVAL`].
    /// (The webhook rejects an unparseable value, so the fallback only covers
    /// objects admitted before the validator existed.)
    pub fn effective_refresh_interval(catalog: Option<&Self>) -> std::time::Duration {
        catalog
            .and_then(|c| c.refresh_interval.as_deref())
            .and_then(crate::duration::parse_go_duration)
            .unwrap_or(crate::consts::DEFAULT_CATALOG_REFRESH_INTERVAL)
    }
}

/// Bounds on the *number* of discovered `Snapshot` CRs kept materialized. ADR §3.1 `catalog.retain`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRetain {
    /// Keep the most-recent N discovered `Snapshot` CRs per `username@hostname:path`
    /// identity (snapshots this cluster produced don't count against N). `0` disables
    /// discovered-Snapshot materialization entirely; negative values are rejected by
    /// the webhook. Rows beyond N are expired (the CR is deleted; the kopia snapshot
    /// is untouched and stays restorable via `Restore.source.identity`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_identity: Option<i64>,
    /// Don't materialize (and expire) discovered `Snapshot` CRs for snapshots whose
    /// end time is older than this many days. Minimum 1 (webhook-enforced). The
    /// kopia snapshots themselves are untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_days: Option<i64>,
}
