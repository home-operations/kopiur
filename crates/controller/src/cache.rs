//! Mover kopia-cache resolution: combine a repository's `cacheDefaults` with a
//! run's `mover.cache`, and project the result onto the two consumers — the
//! connect-time cache budgets ([`kopiur_kopia::CacheTuning`]) and (in the Job
//! builder) the cache volume. One place so backup/restore/maintenance resolve the
//! cache identically (ADR §3.1).

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kopiur_api::common::{
    CacheDefaults, CacheVolumeMode, LegacyBudgets, ResolvedCache, ScratchDefaults,
};
use kopiur_api::snapshot_policy::DeepVerification;
use kopiur_kopia::CacheTuning;

use crate::error::Result;
use crate::io::ResolvedRepository;
use crate::jobs::CacheVolume;

/// The mover's **effective** cache config: the repository's `moverDefaults.cache`
/// (inherited base) overlaid field-by-field by the run's `mover.cache` (override).
/// `None` when neither sets anything. Equivalent to `resolve_mover(...).cache`
/// (ADR-0004 §1) — kept as the single cache resolver for backup/restore/maintenance.
pub fn effective_cache(
    repo: &ResolvedRepository,
    mover_cache: Option<&CacheDefaults>,
) -> Option<CacheDefaults> {
    CacheDefaults::merge(
        repo.mover_defaults.as_ref().and_then(|m| m.cache.as_ref()),
        mover_cache,
    )
}

/// The deep-verify **scratch** volume's **effective** config: the repository's
/// `moverDefaults.scratch` (inherited base) overlaid field-by-field by the recipe's
/// `verification.deep.{storageClassName,capacity}` (override). `None` when neither
/// sets anything. The scratch sibling of [`effective_cache`] — pure and synchronous
/// (scratch is always ephemeral, so there is never a persistent PVC to provision).
pub fn effective_scratch(
    repo: &ResolvedRepository,
    deep: &DeepVerification,
) -> Option<ScratchDefaults> {
    // Only treat the recipe as an override when it actually sets a field, so a bare
    // `verification.deep` (schedule only) leaves the repo default untouched and
    // "nothing anywhere" collapses to `None`.
    let recipe =
        (deep.storage_class_name.is_some() || deep.capacity.is_some()).then(|| ScratchDefaults {
            storage_class_name: deep.storage_class_name.clone(),
            capacity: deep.capacity.clone(),
        });
    ScratchDefaults::merge(
        repo.mover_defaults
            .as_ref()
            .and_then(|m| m.scratch.as_ref()),
        recipe.as_ref(),
    )
}

/// The kopia cache budgets (`--content/metadata-cache-size-mb`) for an effective
/// cache config, resolved against `capacity` — the size of the cache volume the
/// mover actually mounts (`None` for an `emptyDir`; see [`ResolvedCacheVolume`]).
///
/// Thin wrapper over [`kopiur_api::common::resolve_cache_budgets`], the one place the
/// math lives (#613): percent/size/MiB budgets, the capacity defaults (content 60%,
/// metadata 15%), and the 90%-of-capacity bound. Legacy `*CacheSizeMb` values pass
/// through unchanged so an object admitted before the bound keeps running. An
/// unresolvable merged config (e.g. a `{ percent }` with no capacity on any layer)
/// is a typed `Validation` error naming the field and the fix.
pub fn cache_budgets(
    effective: Option<&CacheDefaults>,
    capacity: Option<&str>,
) -> Result<ResolvedCache> {
    let Some(c) = effective else {
        return Ok(ResolvedCache::default());
    };
    kopiur_api::common::resolve_cache_budgets(
        c,
        capacity,
        LegacyBudgets::PassThrough,
        "mover.cache",
    )
    .map_err(|e| {
        crate::error::Error::Validation(format!(
            "the effective mover cache (moverDefaults.cache merged with mover.cache) cannot be \
             resolved: {e}"
        ))
    })
}

/// [`cache_budgets`] projected onto the mover's connect-time [`CacheTuning`].
pub fn cache_tuning(
    effective: Option<&CacheDefaults>,
    capacity: Option<&str>,
) -> Result<CacheTuning> {
    cache_budgets(effective, capacity).map(|r| tuning_of(&r))
}

/// The connect-time flags for resolved budgets.
pub fn tuning_of(r: &ResolvedCache) -> CacheTuning {
    CacheTuning {
        content_cache_size_mb: r.content_cache_size_mb,
        metadata_cache_size_mb: r.metadata_cache_size_mb,
    }
}

/// The capacity cache budgets resolve against for a per-run volume decided purely
/// from config: a sized ephemeral volume's `capacity`, nothing for an `emptyDir`.
/// (A persistent PVC's capacity comes from [`resolve_cache_volume`], which reads
/// the live claim.)
pub fn configured_capacity(volume: &CacheVolume) -> Option<&str> {
    match volume {
        CacheVolume::Ephemeral { capacity, .. } => Some(capacity),
        CacheVolume::EmptyDir | CacheVolume::Pvc { .. } => None,
    }
}

/// A resolved cache volume plus the capacity kopia's budgets must fit in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCacheVolume {
    /// The volume the mover mounts.
    pub volume: CacheVolume,
    /// The capacity budgets resolve against: `None` for an `emptyDir`, the sized
    /// ephemeral volume's `capacity`, or a persistent claim's EFFECTIVE capacity
    /// (see [`crate::io::EnsuredCachePvc::capacity`]).
    pub capacity: Option<String>,
    /// The capacity the config asks for — what admission validated the budgets
    /// against. Equal to `capacity` for per-run volumes; a persistent claim can be
    /// smaller while a resize is pending or after a refused expansion.
    pub configured: Option<String>,
}

impl ResolvedCacheVolume {
    fn per_run(volume: CacheVolume) -> Self {
        let capacity = configured_capacity(&volume).map(str::to_string);
        Self {
            volume,
            configured: capacity.clone(),
            capacity,
        }
    }
}

/// The budgets for a resolved cache volume (#707 review): resolved against the
/// CONFIGURED capacity — the one admission validated, so an admitted edit can
/// never fail here just because the claim has not grown yet — then scaled down
/// to the capacity actually mounted ([`kopiur_api::common::fit_cache_budgets`]).
/// Failing here instead would block the very mover pod whose mount finishes a
/// `FileSystemResizePending` expansion, forever.
pub fn cache_budgets_for_volume(
    effective: Option<&CacheDefaults>,
    volume: &ResolvedCacheVolume,
) -> Result<ResolvedCache> {
    let r = cache_budgets(effective, volume.configured.as_deref())?;
    Ok(match volume.capacity.as_deref() {
        Some(mounted) => kopiur_api::common::fit_cache_budgets(&r, mounted),
        None => r,
    })
}

/// The cache volume and kopia budgets for a replication mover (#707 review).
///
/// Like verification ([`verify_cache_volume`]), a replication run always gets a
/// per-run volume — `mode: Persistent` is coerced to a fresh ephemeral one,
/// because a replication Job must not attach a `ReadWriteOnce` cache claim that
/// another run may hold. `kopia_caches` is how many repository connections keep a
/// kopia cache on that one volume: 1 for a `RepositoryReplication` (connect the
/// source, `sync-to` the destination's blob storage), 2 for a
/// `SnapshotReplication` (source AND destination connected), so the budgets are
/// split and every cache together still fits.
pub fn replication_cache(
    effective: Option<&CacheDefaults>,
    kopia_caches: std::num::NonZeroU32,
) -> Result<(CacheVolume, CacheTuning)> {
    let volume = verify_cache_volume(effective);
    let configured = configured_capacity(&volume);
    let r = cache_budgets(effective, configured)?;
    let ways = i64::from(kopia_caches.get());
    let share_mib = configured
        .and_then(kopiur_api::common::quantity_to_mib)
        .map(|mib| mib / ways);
    let r = match share_mib {
        Some(share) if ways > 1 => {
            // Each connection needs room for 1MiB + 1MiB under the 90% ceiling (3Mi),
            // or one of its caches would run at kopia's unbounded default (#714 review).
            if share < 3 {
                return Err(crate::error::Error::Validation(format!(
                    "mover.cache.capacity `{}` is too small: {ways} kopia caches share this \
                     volume and need at least {}Mi. Fix: raise `capacity`, or remove it for an \
                     unsized emptyDir cache",
                    configured.unwrap_or_default(),
                    3 * ways
                )));
            }
            kopiur_api::common::fit_cache_budgets(&r, &format!("{share}Mi"))
        }
        _ => r,
    };
    Ok((volume, tuning_of(&r)))
}

/// Surface what [`crate::io::ensure_cache_pvc`] did about a cache PVC's size (#613)
/// as an Event on the claim's owner: `Normal` for a grow, `Warning` (what / why /
/// fix) when a capacity change could not be applied. Best-effort, never fatal.
pub async fn report_cache_pvc_resize(
    ctx: &crate::context::Context,
    ns: &str,
    owner: &OwnerReference,
    ensured: &crate::io::EnsuredCachePvc,
) {
    let regarding = k8s_openapi::api::core::v1::ObjectReference {
        api_version: Some(owner.api_version.clone()),
        kind: Some(owner.kind.clone()),
        name: Some(owner.name.clone()),
        uid: Some(owner.uid.clone()),
        // A ClusterRepository is cluster-scoped; every other owner lives in `ns`.
        namespace: (owner.kind != "ClusterRepository").then(|| ns.to_string()),
        ..Default::default()
    };
    let Some(ev) = cache_pvc_resize_event(&ensured.claim_name, &ensured.resize) else {
        return;
    };
    match ev.severity {
        ResizeEventSeverity::Normal => {
            crate::io::publish_normal_event_on_ref(ctx, &regarding, ev.reason, "Resize", &ev.note)
                .await;
        }
        ResizeEventSeverity::Warning => {
            crate::io::publish_warning_event_on_ref(ctx, &regarding, ev.reason, "Resize", &ev.note)
                .await;
        }
    }
}

/// Event severity for a cache-PVC resize outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEventSeverity {
    /// The resize happened.
    Normal,
    /// A capacity change could not be applied.
    Warning,
}

/// The Event a cache-PVC resize outcome produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeEvent {
    /// `Normal` or `Warning`.
    pub severity: ResizeEventSeverity,
    /// Machine-readable reason.
    pub reason: &'static str,
    /// What happened, why, and the fix.
    pub note: String,
}

/// The Event for a cache-PVC resize outcome; `None` when nothing changed. Pure, so
/// the what/why/fix wording is unit-tested.
pub fn cache_pvc_resize_event(
    claim: &str,
    resize: &crate::io::CachePvcResize,
) -> Option<ResizeEvent> {
    use crate::io::CachePvcResize;
    // Exhaustive (no `_ =>`): a new outcome must decide whether it is surfaced.
    match resize {
        CachePvcResize::Unchanged => None,
        CachePvcResize::Expanded { from, to } => Some(ResizeEvent {
            severity: ResizeEventSeverity::Normal,
            reason: "CachePvcExpanded",
            note: format!(
                "Grew kopia cache PVC `{claim}` from {from} to {to} to match mover.cache.capacity. \
                 The volume resizes in the background; cache budgets use the smaller size until \
                 it has."
            ),
        }),
        CachePvcResize::ExpansionNotAllowed {
            current,
            requested,
            reason,
        } => Some(ResizeEvent {
            severity: ResizeEventSeverity::Warning,
            reason: "CachePvcExpansionNotAllowed",
            note: format!(
                "kopia cache PVC `{claim}` is {current} but mover.cache.capacity asks for \
                 {requested}; it was not grown because {reason}. Backups continue with cache \
                 budgets sized to {current}. Fix: use a StorageClass with \
                 `allowVolumeExpansion: true`, or delete the PVC so the next run recreates it at \
                 {requested} (the cache is rebuilt; no backup data is lost)."
            ),
        }),
        CachePvcResize::ShrinkRefused { current, requested } => Some(ResizeEvent {
            severity: ResizeEventSeverity::Warning,
            reason: "CachePvcShrinkRefused",
            note: format!(
                "mover.cache.capacity {requested} is smaller than kopia cache PVC `{claim}` \
                 ({current}), and Kubernetes cannot shrink a PVC, so it keeps {current}. Cache \
                 budgets follow the smaller {requested}. Fix: delete the PVC so the next run \
                 recreates it at {requested} (the cache is rebuilt; no backup data is lost), or \
                 set capacity back to {current}."
            ),
        }),
    }
}

/// Resolve how the mover's kopia cache **volume** is provisioned from an effective
/// cache config (ADR §3.1):
/// - no config, or no `capacity` → an `emptyDir` ([`CacheVolume::EmptyDir`]);
/// - `mode: Ephemeral` (default) with a `capacity` → a sized generic ephemeral
///   volume ([`CacheVolume::Ephemeral`]);
/// - `mode: Persistent` with a `capacity` → a controller-owned PVC reused across the
///   owner's runs ([`CacheVolume::Pvc`]), provisioned here (owner-referenced for GC).
///
/// `cache_owner` owns a persistent cache PVC (e.g. the `SnapshotPolicy` for backups, so
/// the warm cache survives individual `Snapshot` CRs); `claim_name` is its stable name.
///
/// An existing persistent PVC is grown (never shrunk) toward `capacity`, and any
/// resize outcome is reported on `cache_owner` ([`report_cache_pvc_resize`]).
pub async fn resolve_cache_volume(
    ctx: &crate::context::Context,
    ns: &str,
    cache_owner: OwnerReference,
    claim_name: &str,
    effective: Option<&CacheDefaults>,
) -> Result<ResolvedCacheVolume> {
    let Some(c) = effective else {
        return Ok(ResolvedCacheVolume::per_run(CacheVolume::EmptyDir));
    };
    // A sized volume needs a capacity; without one, fall back to an emptyDir.
    let Some(capacity) = c.capacity.clone() else {
        return Ok(ResolvedCacheVolume::per_run(CacheVolume::EmptyDir));
    };
    match c.effective_mode() {
        CacheVolumeMode::Persistent => {
            let ensured = crate::io::ensure_cache_pvc(
                &ctx.client,
                ns,
                claim_name,
                cache_owner.clone(),
                &capacity,
                c.storage_class_name.as_deref(),
            )
            .await?;
            report_cache_pvc_resize(ctx, ns, &cache_owner, &ensured).await;
            Ok(ResolvedCacheVolume {
                volume: CacheVolume::Pvc {
                    claim_name: ensured.claim_name,
                },
                capacity: Some(ensured.capacity),
                configured: Some(capacity),
            })
        }
        CacheVolumeMode::Ephemeral => Ok(ResolvedCacheVolume::per_run(CacheVolume::Ephemeral {
            capacity,
            storage_class: c.storage_class_name.clone(),
        })),
    }
}

/// Resolve the **verification** mover's kopia cache volume from an effective cache
/// config. Verify inherits `moverDefaults.cache` like every other mover, but — being
/// a separate, infrequent lifecycle from backups — it must **never attach the
/// `SnapshotPolicy`'s persistent (warm) cache PVC**: that PVC is `ReadWriteOnce` and
/// owned by the backup path, so sharing it would risk a Multi-Attach race and entangle
/// lifecycles. So `mode` is ignored here and the result is always per-run ephemeral:
/// - a `capacity` (under either `Ephemeral` *or* `Persistent` mode) → a fresh sized
///   generic ephemeral volume ([`CacheVolume::Ephemeral`], honoring `storageClassName`);
/// - no `capacity` → an `emptyDir`.
///
/// Pure (no IO): unlike [`resolve_cache_volume`] there is never an owned PVC to
/// provision, so this needs no client. The cache **budgets** are applied separately via
/// [`cache_tuning`] in the verify work-spec, so volume + budgets share one
/// [`effective_cache`] source.
pub fn verify_cache_volume(effective: Option<&CacheDefaults>) -> CacheVolume {
    match effective.and_then(|c| c.capacity.clone()) {
        Some(capacity) => CacheVolume::Ephemeral {
            capacity,
            storage_class: effective.and_then(|c| c.storage_class_name.clone()),
        },
        None => CacheVolume::EmptyDir,
    }
}

/// How a repository's `SnapshotDeleteBatch` (`snapdel-*`) Jobs get their kopia cache
/// (#582): the cache volume to mount, plus, only for a persistent claim, the PVC the
/// controller must ensure first. A typed plan rather than a bare [`CacheVolume`] so
/// the "ensure this PVC" step can never be reached without the capacity it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapdelCachePlan {
    /// Mount this per-run volume (`emptyDir` or a sized generic ephemeral volume);
    /// nothing to provision.
    Volume(CacheVolume),
    /// Ensure the repository's dedicated persistent cache PVC, then mount it.
    Persistent {
        /// The dedicated claim (see [`crate::naming::snapdel_cache_pvc_name`]).
        claim_name: String,
        /// PVC size (e.g. `10Gi`).
        capacity: String,
        /// StorageClass; `None` uses the cluster default.
        storage_class: Option<String>,
    },
}

impl SnapdelCachePlan {
    /// The volume the Job mounts under this plan.
    pub fn volume(&self) -> CacheVolume {
        match self {
            Self::Volume(v) => v.clone(),
            Self::Persistent { claim_name, .. } => CacheVolume::Pvc {
                claim_name: claim_name.clone(),
            },
        }
    }
}

/// Decide the kopia cache for a repository's batch snapshot-delete Jobs from the
/// repository's **effective** cache (`moverDefaults.cache`; a delete has no recipe
/// layer) and its effective `maxConcurrentDeleteJobs` (#582).
///
/// A delete Job connects to the repository and loads its index exactly like a
/// backup does, so an `emptyDir` cache re-downloads the whole index on every run
/// (gigabytes per Job on a large repository). The repository's cache settings are
/// therefore honored here too:
/// - no config, or no `capacity` → `emptyDir` (same as every other mover);
/// - `mode: Ephemeral` + `capacity` → a sized generic ephemeral volume, fresh each run;
/// - `mode: Persistent` + `capacity`, with at most ONE delete Job per repository at a
///   time (`maxConcurrentDeleteJobs: 1`, the default) → a dedicated warm PVC for this
///   repository's delete Jobs (`claim_name`). Never the backup's policy-owned PVC or
///   the `Maintenance` PVC: those belong to lifecycles that run concurrently with
///   deletes, and the mover keeps `repository.config` on the cache volume, so two
///   pods sharing one claim would clobber each other;
/// - `mode: Persistent` + `capacity` with `maxConcurrentDeleteJobs > 1` → coerced to
///   a sized ephemeral volume (as verification does): several delete Jobs may run at
///   once, and they cannot share one `ReadWriteOnce` claim.
///
/// Pure; [`resolve_snapdel_cache_volume`] does the IO.
pub fn snapdel_cache_plan(
    effective: Option<&CacheDefaults>,
    max_concurrent_delete_jobs: std::num::NonZeroUsize,
    claim_name: &str,
) -> SnapdelCachePlan {
    let Some(c) = effective else {
        return SnapdelCachePlan::Volume(CacheVolume::EmptyDir);
    };
    let Some(capacity) = c.capacity.clone() else {
        return SnapdelCachePlan::Volume(CacheVolume::EmptyDir);
    };
    let ephemeral = |capacity: String| {
        SnapdelCachePlan::Volume(CacheVolume::Ephemeral {
            capacity,
            storage_class: c.storage_class_name.clone(),
        })
    };
    match c.effective_mode() {
        CacheVolumeMode::Persistent if max_concurrent_delete_jobs.get() == 1 => {
            SnapdelCachePlan::Persistent {
                claim_name: claim_name.to_string(),
                capacity,
                storage_class: c.storage_class_name.clone(),
            }
        }
        CacheVolumeMode::Persistent => ephemeral(capacity),
        CacheVolumeMode::Ephemeral => ephemeral(capacity),
    }
}

/// Resolve (and, for a persistent plan, provision) the batch-delete Job's cache
/// volume. The dedicated PVC is owned by the repository CR (`owner`): a `Repository`
/// in its own namespace, or a `ClusterRepository` (a cluster-scoped owner of a
/// namespaced dependent, which Kubernetes garbage collection supports), so it is
/// deleted with the repository. Like the backup's policy-owned cache PVC, it is
/// left in place if the repository later stops using `mode: Persistent`, and is
/// garbage-collected with the repository.
pub async fn resolve_snapdel_cache_volume(
    ctx: &crate::context::Context,
    ns: &str,
    owner: OwnerReference,
    plan: &SnapdelCachePlan,
) -> Result<ResolvedCacheVolume> {
    match plan {
        SnapdelCachePlan::Volume(v) => Ok(ResolvedCacheVolume::per_run(v.clone())),
        SnapdelCachePlan::Persistent {
            claim_name,
            capacity,
            storage_class,
        } => {
            let ensured = crate::io::ensure_cache_pvc(
                &ctx.client,
                ns,
                claim_name,
                owner.clone(),
                capacity,
                storage_class.as_deref(),
            )
            .await?;
            report_cache_pvc_resize(ctx, ns, &owner, &ensured).await;
            Ok(ResolvedCacheVolume {
                volume: CacheVolume::Pvc {
                    claim_name: ensured.claim_name,
                },
                capacity: Some(ensured.capacity),
                configured: Some(capacity.clone()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::backend::{Backend, FilesystemBackend};
    use kopiur_api::common::{Encryption, SecretKeyRef};

    fn repo_with(cache: Option<CacheDefaults>) -> ResolvedRepository {
        ResolvedRepository {
            backend: Backend::Filesystem(FilesystemBackend {
                path: "/repo".into(),
                volume: None,
            }),
            mover_defaults: cache.map(|c| kopiur_api::common::MoverDefaults {
                cache: Some(c),
                ..Default::default()
            }),
            encryption: Encryption {
                password_secret_ref: SecretKeyRef {
                    name: "creds".into(),
                    namespace: None,
                    key: None,
                },
            },
            kind: kopiur_api::common::RepositoryKind::Repository,
            repo_namespace: Some("ns".into()),
            identity_defaults: None,
            schedule_defaults: None,
            on_namespace_delete: Default::default(),
            mode: Default::default(),
            credential_projection_allowed: false,
            owner_ref: Default::default(),
            deletion_protection: None,
            concurrency: None,
            mass_deletion_ack: None,
            catalog: None,
            ca_bundle_pem: None,
        }
    }

    #[test]
    fn effective_cache_overlays_mover_over_repo_defaults() {
        let repo = repo_with(Some(CacheDefaults {
            metadata_cache_size_mb: Some(1024),
            content_cache_size_mb: Some(4096),
            ..Default::default()
        }));
        // No mover override → repo defaults flow through as the connect budgets.
        let eff = effective_cache(&repo, None);
        let tuning = cache_tuning(eff.as_ref(), None).unwrap();
        assert_eq!(tuning.metadata_cache_size_mb, Some(1024));
        assert_eq!(tuning.content_cache_size_mb, Some(4096));

        // Mover overrides content only → metadata still inherited from the repo.
        let mover = CacheDefaults {
            content_cache_size_mb: Some(16384),
            ..Default::default()
        };
        let eff = effective_cache(&repo, Some(&mover));
        let tuning = cache_tuning(eff.as_ref(), None).unwrap();
        assert_eq!(tuning.content_cache_size_mb, Some(16384));
        assert_eq!(tuning.metadata_cache_size_mb, Some(1024));
    }

    #[test]
    fn no_cache_anywhere_is_kopia_defaults() {
        let repo = repo_with(None);
        assert!(
            cache_tuning(effective_cache(&repo, None).as_ref(), None)
                .unwrap()
                .is_unset()
        );
    }

    fn repo_with_scratch(scratch: Option<ScratchDefaults>) -> ResolvedRepository {
        let mut repo = repo_with(None);
        repo.mover_defaults = scratch.map(|s| kopiur_api::common::MoverDefaults {
            scratch: Some(s),
            ..Default::default()
        });
        repo
    }

    fn deep(storage_class: Option<&str>, capacity: Option<&str>) -> DeepVerification {
        DeepVerification {
            schedule: kopiur_api::common::CronSpec {
                cron: "0 5 * * 0".into(),
                jitter: None,
                timezone: None,
            },
            storage_class_name: storage_class.map(str::to_string),
            capacity: capacity.map(str::to_string),
            parallel: None,
        }
    }

    #[test]
    fn effective_scratch_overlays_recipe_over_repo_defaults() {
        let repo = repo_with_scratch(Some(ScratchDefaults {
            storage_class_name: Some("fast-ssd".into()),
            capacity: Some("100Gi".into()),
        }));

        // Bare verification.deep → repo defaults flow through.
        let eff = effective_scratch(&repo, &deep(None, None)).unwrap();
        assert_eq!(eff.storage_class_name.as_deref(), Some("fast-ssd"));
        assert_eq!(eff.capacity.as_deref(), Some("100Gi"));

        // Recipe overrides capacity only → storageClass still inherited.
        let eff = effective_scratch(&repo, &deep(None, Some("200Gi"))).unwrap();
        assert_eq!(eff.storage_class_name.as_deref(), Some("fast-ssd")); // repo
        assert_eq!(eff.capacity.as_deref(), Some("200Gi")); // recipe

        // Recipe overrides storageClass only → capacity still inherited.
        let eff = effective_scratch(&repo, &deep(Some("slow-hdd"), None)).unwrap();
        assert_eq!(eff.storage_class_name.as_deref(), Some("slow-hdd")); // recipe
        assert_eq!(eff.capacity.as_deref(), Some("100Gi")); // repo
    }

    #[test]
    fn effective_scratch_is_none_when_nothing_set() {
        let repo = repo_with_scratch(None);
        assert_eq!(effective_scratch(&repo, &deep(None, None)), None);
    }

    #[test]
    fn verify_cache_volume_is_emptydir_without_capacity() {
        // No effective cache, or budgets-only (no capacity) → emptyDir.
        assert_eq!(verify_cache_volume(None), CacheVolume::EmptyDir);
        assert_eq!(
            verify_cache_volume(Some(&CacheDefaults {
                metadata_cache_size_mb: Some(512),
                ..Default::default()
            })),
            CacheVolume::EmptyDir
        );
    }

    #[test]
    fn verify_cache_volume_is_sized_ephemeral_with_capacity() {
        assert_eq!(
            verify_cache_volume(Some(&CacheDefaults {
                capacity: Some("8Gi".into()),
                storage_class_name: Some("fast-ssd".into()),
                ..Default::default()
            })),
            CacheVolume::Ephemeral {
                capacity: "8Gi".into(),
                storage_class: Some("fast-ssd".into()),
            }
        );
    }

    #[test]
    fn verify_cache_volume_coerces_persistent_to_ephemeral() {
        // Verify must NEVER attach the backup's warm persistent PVC (RWO multi-attach):
        // a Persistent cache is coerced to a fresh per-run sized ephemeral volume.
        assert_eq!(
            verify_cache_volume(Some(&CacheDefaults {
                capacity: Some("16Gi".into()),
                storage_class_name: Some("block".into()),
                mode: Some(CacheVolumeMode::Persistent),
                ..Default::default()
            })),
            CacheVolume::Ephemeral {
                capacity: "16Gi".into(),
                storage_class: Some("block".into()),
            }
        );
    }

    // --- #582: SnapshotDeleteBatch Jobs honor the repository's cache ---------

    const CLAIM: &str = "kopiur-cache-snapdel-garage-1a2b3c4d";

    fn conc(n: usize) -> std::num::NonZeroUsize {
        std::num::NonZeroUsize::new(n).unwrap()
    }

    fn cache(mode: Option<CacheVolumeMode>, capacity: Option<&str>) -> CacheDefaults {
        CacheDefaults {
            capacity: capacity.map(str::to_string),
            storage_class_name: Some("local-hostpath".into()),
            mode,
            ..Default::default()
        }
    }

    #[test]
    fn snapdel_cache_is_emptydir_without_config_or_capacity() {
        for n in [1, 4] {
            assert_eq!(
                snapdel_cache_plan(None, conc(n), CLAIM),
                SnapdelCachePlan::Volume(CacheVolume::EmptyDir)
            );
            for mode in [
                None,
                Some(CacheVolumeMode::Ephemeral),
                Some(CacheVolumeMode::Persistent),
            ] {
                assert_eq!(
                    snapdel_cache_plan(Some(&cache(mode, None)), conc(n), CLAIM),
                    SnapdelCachePlan::Volume(CacheVolume::EmptyDir),
                    "mode {mode:?} without capacity, concurrency {n}"
                );
            }
        }
    }

    #[test]
    fn snapdel_cache_ephemeral_is_sized_ephemeral_at_any_concurrency() {
        for n in [1, 2, 8] {
            for mode in [None, Some(CacheVolumeMode::Ephemeral)] {
                assert_eq!(
                    snapdel_cache_plan(Some(&cache(mode, Some("10Gi"))), conc(n), CLAIM),
                    SnapdelCachePlan::Volume(CacheVolume::Ephemeral {
                        capacity: "10Gi".into(),
                        storage_class: Some("local-hostpath".into()),
                    })
                );
            }
        }
    }

    #[test]
    fn snapdel_cache_persistent_single_flight_gets_the_dedicated_pvc() {
        // The #582 reporter's shape: Persistent, 10Gi, local-hostpath, default
        // maxConcurrentDeleteJobs (1).
        let plan = snapdel_cache_plan(
            Some(&cache(Some(CacheVolumeMode::Persistent), Some("10Gi"))),
            kopiur_api::consts::DEFAULT_MAX_CONCURRENT_DELETE_JOBS,
            CLAIM,
        );
        assert_eq!(
            plan,
            SnapdelCachePlan::Persistent {
                claim_name: CLAIM.into(),
                capacity: "10Gi".into(),
                storage_class: Some("local-hostpath".into()),
            }
        );
        assert_eq!(
            plan.volume(),
            CacheVolume::Pvc {
                claim_name: CLAIM.into()
            }
        );
    }

    #[test]
    fn snapdel_cache_persistent_with_parallel_deletes_is_coerced_to_ephemeral() {
        // Two delete Jobs may run at once; they must never share a RWO claim.
        for n in [2, 3, 16] {
            assert_eq!(
                snapdel_cache_plan(
                    Some(&cache(Some(CacheVolumeMode::Persistent), Some("10Gi"))),
                    conc(n),
                    CLAIM,
                ),
                SnapdelCachePlan::Volume(CacheVolume::Ephemeral {
                    capacity: "10Gi".into(),
                    storage_class: Some("local-hostpath".into()),
                }),
                "concurrency {n}"
            );
        }
    }

    #[test]
    fn snapdel_cache_plan_volume_passes_per_run_volumes_through() {
        let v = CacheVolume::Ephemeral {
            capacity: "1Gi".into(),
            storage_class: None,
        };
        assert_eq!(SnapdelCachePlan::Volume(v.clone()).volume(), v);
    }

    // --- #613: budgets against the mounted volume; resize Events ---------------

    #[test]
    fn cache_tuning_defaults_against_the_volume_capacity() {
        let eff = CacheDefaults {
            capacity: Some("10Gi".into()),
            ..Default::default()
        };
        // The capacity handed in (the volume actually mounted) wins over the config's.
        let t = cache_tuning(Some(&eff), Some("4Gi")).unwrap();
        assert_eq!(t.content_cache_size_mb, Some(2457));
        assert_eq!(t.metadata_cache_size_mb, Some(614));
        // An emptyDir has no capacity: nothing to default against.
        assert!(cache_tuning(Some(&eff), None).unwrap().is_unset());
    }

    #[test]
    fn cache_budgets_error_is_a_validation_error_naming_the_fix() {
        let eff = CacheDefaults {
            content_cache_size: Some(kopiur_api::common::CacheSize::Percent(40)),
            ..Default::default()
        };
        let err = cache_budgets(Some(&eff), None).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, crate::error::Error::Validation(_)), "{msg}");
        assert!(msg.contains("Fix:"), "{msg}");
    }

    #[test]
    fn configured_capacity_only_for_sized_per_run_volumes() {
        assert_eq!(configured_capacity(&CacheVolume::EmptyDir), None);
        assert_eq!(
            configured_capacity(&CacheVolume::Ephemeral {
                capacity: "8Gi".into(),
                storage_class: None
            }),
            Some("8Gi")
        );
        assert_eq!(
            configured_capacity(&CacheVolume::Pvc {
                claim_name: "c".into()
            }),
            None
        );
    }

    #[test]
    fn resize_events_say_what_why_and_fix_within_the_note_limit() {
        use crate::io::CachePvcResize;
        let claim = "kopiur-cache-some-very-long-policy-name-ns-abcdef";
        assert_eq!(
            cache_pvc_resize_event(claim, &CachePvcResize::Unchanged),
            None
        );

        let grown = cache_pvc_resize_event(
            claim,
            &CachePvcResize::Expanded {
                from: "1Gi".into(),
                to: "2Gi".into(),
            },
        )
        .unwrap();
        assert_eq!(grown.severity, ResizeEventSeverity::Normal);
        assert_eq!(grown.reason, "CachePvcExpanded");

        let refused = cache_pvc_resize_event(
            claim,
            &CachePvcResize::ExpansionNotAllowed {
                current: "1Gi".into(),
                requested: "2Gi".into(),
                reason: "its StorageClass `standard` does not set `allowVolumeExpansion: true`"
                    .into(),
            },
        )
        .unwrap();
        let shrink = cache_pvc_resize_event(
            claim,
            &CachePvcResize::ShrinkRefused {
                current: "4Gi".into(),
                requested: "2Gi".into(),
            },
        )
        .unwrap();
        for (ev, reason) in [
            (&refused, "CachePvcExpansionNotAllowed"),
            (&shrink, "CachePvcShrinkRefused"),
        ] {
            assert_eq!(ev.severity, ResizeEventSeverity::Warning);
            assert_eq!(ev.reason, reason);
            assert!(ev.note.contains(claim), "names the PVC: {}", ev.note);
            assert!(ev.note.contains("Fix:"), "carries the fix: {}", ev.note);
            assert!(ev.note.len() <= 1024, "Event note limit: {}", ev.note.len());
        }
        assert!(refused.note.contains("allowVolumeExpansion"));
        assert!(shrink.note.contains("cannot shrink"));
    }

    // --- #707 review follow-ups --------------------------------------------------

    /// Greptile on #707: a Persistent cache grown 1Gi→2Gi with
    /// `contentCacheSize: { size: 1Gi }` passes admission, but while the claim still
    /// reports 1Gi (the filesystem only finishes growing when a pod mounts it —
    /// `FileSystemResizePending`) resolving against 1Gi failed, BEFORE the mover Job
    /// that would finish the resize was created. Every retry hit the same check.
    /// Budgets now resolve against the configured capacity and shrink to fit.
    #[test]
    fn a_pending_resize_never_blocks_the_run_budgets_shrink_to_fit() {
        let eff = CacheDefaults {
            capacity: Some("2Gi".into()),
            mode: Some(CacheVolumeMode::Persistent),
            content_cache_size: Some(kopiur_api::common::CacheSize::Size("1Gi".into())),
            ..Default::default()
        };
        // What the old code did: resolve against the claim's current size.
        assert!(cache_budgets(Some(&eff), Some("1Gi")).is_err());
        let volume = ResolvedCacheVolume {
            volume: CacheVolume::Pvc {
                claim_name: "c".into(),
            },
            capacity: Some("1Gi".into()),
            configured: Some("2Gi".into()),
        };
        let r = cache_budgets_for_volume(Some(&eff), &volume).expect("the run must proceed");
        assert_eq!(r.content_cache_size_mb, Some(512));
        assert_eq!(r.metadata_cache_size_mb, Some(153));
        assert_eq!(r.capacity.as_deref(), Some("1Gi"));
        // Once the claim catches up, the full budgets apply.
        let caught_up = ResolvedCacheVolume {
            capacity: Some("2Gi".into()),
            ..volume
        };
        let r = cache_budgets_for_volume(Some(&eff), &caught_up).unwrap();
        assert_eq!(r.content_cache_size_mb, Some(1024));
    }

    /// Greptile on #707: both replication kinds accepted `mover.cache` but their Jobs
    /// used an unsized emptyDir and kopia's default budgets. A RepositoryReplication
    /// now gets the resolved per-run cache volume (Persistent coerced to a fresh
    /// ephemeral volume, as for verification — a replication run must not share a
    /// ReadWriteOnce claim) and budgets sized to it.
    #[test]
    fn repository_replication_gets_the_resolved_cache() {
        let eff = CacheDefaults {
            capacity: Some("2Gi".into()),
            storage_class_name: Some("fast".into()),
            mode: Some(CacheVolumeMode::Persistent),
            ..Default::default()
        };
        let (volume, tuning) = replication_cache(Some(&eff), std::num::NonZeroU32::MIN).unwrap();
        assert_eq!(
            volume,
            CacheVolume::Ephemeral {
                capacity: "2Gi".into(),
                storage_class: Some("fast".into()),
            }
        );
        assert_eq!(tuning.content_cache_size_mb, Some(1228));
        assert_eq!(tuning.metadata_cache_size_mb, Some(307));
        // No cache config → emptyDir + kopia defaults, exactly as before.
        let (volume, tuning) = replication_cache(None, std::num::NonZeroU32::MIN).unwrap();
        assert_eq!(volume, CacheVolume::EmptyDir);
        assert!(tuning.is_unset());
    }

    /// A SnapshotReplication connects BOTH repositories, each with its own kopia
    /// cache on the one cache volume, so each gets half: the two together still fit.
    #[test]
    fn snapshot_replication_splits_the_volume_between_its_two_caches() {
        let eff = CacheDefaults {
            capacity: Some("2Gi".into()),
            ..Default::default()
        };
        let (_, tuning) =
            replication_cache(Some(&eff), std::num::NonZeroU32::new(2).unwrap()).unwrap();
        assert_eq!(tuning.content_cache_size_mb, Some(614));
        assert_eq!(tuning.metadata_cache_size_mb, Some(153));
        let both = 2 * (614 + 153);
        assert!(
            both * 100 <= 2048 * 90,
            "two caches must fit the volume: {both}"
        );
    }

    /// Greptile on #714: with `capacity: 3Mi` a SnapshotReplication split the
    /// volume into 1MiB shares, but each of its two caches kept 1MiB + 1MiB — 4MiB
    /// of budgets on a 3MiB volume. Every cache on a shared volume now fits, and a
    /// volume too small to give each connection 1MiB + 1MiB is refused with a fix.
    #[test]
    fn shared_cache_volumes_fit_every_cache_or_are_refused() {
        let two = std::num::NonZeroU32::new(2).unwrap();
        let tiny = CacheDefaults {
            capacity: Some("3Mi".into()),
            ..Default::default()
        };
        let err = replication_cache(Some(&tiny), two).unwrap_err().to_string();
        assert!(err.contains("too small") && err.contains("6Mi"), "{err}");
        for cap in [6, 7, 10, 33, 100, 1000] {
            let eff = CacheDefaults {
                capacity: Some(format!("{cap}Mi")),
                ..Default::default()
            };
            let (_, t) = replication_cache(Some(&eff), two).unwrap();
            let all = 2 * (t.content_cache_size_mb.unwrap() + t.metadata_cache_size_mb.unwrap());
            assert!(all * 100 <= cap * 90, "{cap}Mi: two caches total {all}MiB");
        }
    }
}
