//! Mover kopia-cache resolution: combine a repository's `cacheDefaults` with a
//! run's `mover.cache`, and project the result onto the two consumers — the
//! connect-time cache budgets ([`kopiur_kopia::CacheTuning`]) and (in the Job
//! builder) the cache volume. One place so backup/restore/maintenance resolve the
//! cache identically (ADR §3.1).

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kopiur_api::common::{CacheDefaults, CacheVolumeMode, ScratchDefaults};
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

/// The kopia connect-time cache budgets (`--content/metadata-cache-size-mb`) from an
/// effective cache config. Empty (kopia defaults) when unset.
pub fn cache_tuning(effective: Option<&CacheDefaults>) -> CacheTuning {
    effective
        .map(|c| CacheTuning {
            content_cache_size_mb: c.content_cache_size_mb,
            metadata_cache_size_mb: c.metadata_cache_size_mb,
        })
        .unwrap_or_default()
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
pub async fn resolve_cache_volume(
    client: &kube::Client,
    ns: &str,
    cache_owner: OwnerReference,
    claim_name: &str,
    effective: Option<&CacheDefaults>,
) -> Result<CacheVolume> {
    let Some(c) = effective else {
        return Ok(CacheVolume::EmptyDir);
    };
    // A sized volume needs a capacity; without one, fall back to an emptyDir.
    let Some(capacity) = c.capacity.clone() else {
        return Ok(CacheVolume::EmptyDir);
    };
    match c.effective_mode() {
        CacheVolumeMode::Persistent => {
            let claim = crate::io::ensure_cache_pvc(
                client,
                ns,
                claim_name,
                cache_owner,
                &capacity,
                c.storage_class_name.as_deref(),
            )
            .await?;
            Ok(CacheVolume::Pvc { claim_name: claim })
        }
        CacheVolumeMode::Ephemeral => Ok(CacheVolume::Ephemeral {
            capacity,
            storage_class: c.storage_class_name.clone(),
        }),
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
            repo_namespace: Some("ns".into()),
            identity_defaults: None,
            schedule_defaults: None,
            on_namespace_delete: Default::default(),
            mode: Default::default(),
            credential_projection_allowed: false,
            owner_ref: Default::default(),
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
        let tuning = cache_tuning(eff.as_ref());
        assert_eq!(tuning.metadata_cache_size_mb, Some(1024));
        assert_eq!(tuning.content_cache_size_mb, Some(4096));

        // Mover overrides content only → metadata still inherited from the repo.
        let mover = CacheDefaults {
            content_cache_size_mb: Some(16384),
            ..Default::default()
        };
        let eff = effective_cache(&repo, Some(&mover));
        let tuning = cache_tuning(eff.as_ref());
        assert_eq!(tuning.content_cache_size_mb, Some(16384));
        assert_eq!(tuning.metadata_cache_size_mb, Some(1024));
    }

    #[test]
    fn no_cache_anywhere_is_kopia_defaults() {
        let repo = repo_with(None);
        assert!(cache_tuning(effective_cache(&repo, None).as_ref()).is_unset());
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
}
