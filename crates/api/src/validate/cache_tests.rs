//! #613: admission rules for the mover cache budgets, parsed the cluster's way
//! (YAML → JSON → typed). Two tiers, mirroring `admission.rs`:
//!
//! * the NEW fields (`contentCacheSize` / `metadataCacheSize`) are checked by the
//!   shared aggregates the reconcilers also re-run — no stored object can carry them;
//! * the capacity bound on the ORIGINAL `*CacheSizeMb` fields is admission-only and
//!   only fires when the cache block changes, so a stored CR that predates it keeps
//!   reconciling. Every rejection here is paired with that ratchet pin.

use super::*;
use crate::cluster_repository::ClusterRepositorySpec;
use crate::maintenance::MaintenanceSpec;
use crate::repository::RepositorySpec;
use crate::restore::RestoreSpec;
use crate::snapshot::{Origin, SnapshotSpec};
use crate::snapshot_policy::SnapshotPolicySpec;
use crate::testutil::from_yaml;

fn policy(cache: &str) -> SnapshotPolicySpec {
    from_yaml(&format!(
        "repository: {{ kind: Repository, name: r }}\n\
         sources: [ {{ pvc: {{ name: data }} }} ]\n\
         mover:\n\
         \x20 cache: {cache}\n"
    ))
}

fn repository(cache: &str) -> RepositorySpec {
    from_yaml(&format!(
        "backend: {{ filesystem: {{ path: /repo, volume: {{ pvc: {{ name: repo }} }} }} }}\n\
         encryption: {{ passwordSecretRef: {{ name: s }} }}\n\
         moverDefaults:\n\
         \x20 cache: {cache}\n"
    ))
}

fn joined(errs: &[ValidationError]) -> String {
    errs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

// --- shared tier: the new percent/size fields --------------------------------

#[test]
fn a_valid_percent_policy_is_admitted() {
    let spec = policy(
        "{ capacity: 2Gi, contentCacheSize: { percent: 63 }, metadataCacheSize: { percent: 12 } }",
    );
    assert!(
        validate_backup_config(&spec).is_empty(),
        "{}",
        joined(&validate_backup_config(&spec))
    );
    assert!(validate_cache_legacy_budgets_for(&spec, None).is_empty());
}

#[test]
fn a_percent_may_draw_on_an_inherited_capacity() {
    // No capacity on this layer: it may come from the repository's moverDefaults,
    // so only the shape is checked here and the merged config is resolved at run time.
    let spec = policy("{ contentCacheSize: { percent: 50 } }");
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn shared_aggregate_rejects_bad_new_budgets() {
    for (cache, needle) in [
        (
            "{ capacity: 1Gi, contentCacheSize: { percent: 0 } }",
            "between 1 and 100",
        ),
        (
            "{ capacity: 1Gi, contentCacheSize: { percent: 101 } }",
            "between 1 and 100",
        ),
        (
            "{ contentCacheSize: { percent: 80 }, metadataCacheSize: { percent: 20 } }",
            "100% of capacity",
        ),
        (
            "{ capacity: 2Gi, contentCacheSize: { size: 3Gi } }",
            "does not fit",
        ),
        (
            "{ capacity: 2Gi, contentCacheSize: { size: 1500Mi }, metadataCacheSize: { size: 400Mi } }",
            "over 90%",
        ),
        (
            "{ contentCacheSize: { size: big } }",
            "not a Kubernetes quantity",
        ),
        (
            "{ capacity: 1Gi, contentCacheSize: { percent: 50 }, contentCacheSizeMb: 10 }",
            "cannot both be set",
        ),
    ] {
        let errs = validate_backup_config(&policy(cache));
        let msg = joined(&errs);
        assert!(
            msg.contains(needle),
            "{cache}: expected {needle:?}, got:\n{msg}"
        );
    }
}

#[test]
fn every_kind_with_a_mover_validates_its_cache() {
    let bad = "{ capacity: 1Gi, contentCacheSize: { percent: 150 } }";
    let snapshot: SnapshotSpec = from_yaml(&format!(
        "policyRef: {{ name: pg }}\nmover: {{ cache: {bad} }}\n"
    ));
    assert!(!validate_backup(&snapshot, Some(Origin::Manual)).is_empty());
    let restore: RestoreSpec = from_yaml(&format!(
        "source: {{ snapshotRef: {{ name: s }} }}\n\
         target: {{ pvc: {{ name: t, capacity: 1Gi }} }}\n\
         mover: {{ cache: {bad} }}\n"
    ));
    assert!(!validate_restore_spec(&restore).is_empty());
    let maintenance: MaintenanceSpec = from_yaml(&format!(
        "repository: {{ kind: Repository, name: r }}\n\
         schedule: {{ quick: {{ cron: '0 */6 * * *' }}, full: {{ cron: '0 3 * * *' }} }}\n\
         ownership: {{ owner: kopiur/r, takeoverPolicy: PromptCondition }}\n\
         mover: {{ cache: {bad} }}\n"
    ));
    assert!(!validate_maintenance(&maintenance).is_empty());
}

#[test]
fn repository_mover_defaults_and_maintenance_caches_are_validated() {
    let errs = validate_repository(&repository(
        "{ contentCacheSize: { percent: 70 }, metadataCacheSize: { percent: 30 } }",
    ));
    assert!(
        joined(&errs).contains("spec.moverDefaults.cache"),
        "{}",
        joined(&errs)
    );

    let cluster: ClusterRepositorySpec = from_yaml(
        "backend: { filesystem: { path: /repo } }\n\
         encryption: { passwordSecretRef: { name: creds, namespace: kopiur-system } }\n\
         allowedNamespaces: { all: true }\n\
         maintenance:\n\
         \x20 mover: { cache: { capacity: 1Gi, metadataCacheSize: { size: 2Gi } } }\n",
    );
    let errs = validate_cluster_repository(&cluster);
    assert!(
        joined(&errs).contains("spec.maintenance.mover.cache"),
        "{}",
        joined(&errs)
    );
}

// --- admission-only tier: the original *CacheSizeMb fields --------------------

#[test]
fn legacy_mb_over_capacity_is_refused_at_admission_but_not_by_the_shared_aggregate() {
    // The #613 reporter's shape: budgets that add up past the cache volume.
    let spec = policy("{ capacity: 2Gi, contentCacheSizeMb: 1900, metadataCacheSizeMb: 500 }");
    let errs = validate_cache_legacy_budgets_for(&spec, None);
    let msg = joined(&errs);
    assert!(msg.contains("over 90%"), "{msg}");
    assert!(msg.contains("spec.mover.cache"), "{msg}");
    // Ratchet pin: the aggregate the reconciler re-runs still accepts it, so a
    // stored CR keeps backing up exactly as before.
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn legacy_mb_check_skips_an_unchanged_cache_block() {
    let spec = policy("{ capacity: 2Gi, contentCacheSizeMb: 5000 }");
    // An UPDATE that does not touch the cache block (a label, a finalizer, the
    // controller's own writes) is never refused by this rule…
    assert!(validate_cache_legacy_budgets_for(&spec, Some(&spec)).is_empty());
    // …but CREATE, or an edit to the block, is.
    assert!(!validate_cache_legacy_budgets_for(&spec, None).is_empty());
    let older = policy("{ capacity: 1Gi, contentCacheSizeMb: 5000 }");
    assert!(!validate_cache_legacy_budgets_for(&spec, Some(&older)).is_empty());
}

#[test]
fn legacy_mb_without_capacity_or_within_it_is_admitted() {
    // No capacity: an emptyDir, so there is nothing to bound against.
    assert!(
        validate_cache_legacy_budgets_for(&policy("{ contentCacheSizeMb: 50000 }"), None)
            .is_empty()
    );
    // Within 90% of capacity: the pre-#613 manifests in docs/examples.
    let ok = policy("{ capacity: 16Gi, contentCacheSizeMb: 10000, metadataCacheSizeMb: 2000 }");
    assert!(validate_cache_legacy_budgets_for(&ok, None).is_empty());
}

#[test]
fn cache_blocks_cover_both_repository_caches() {
    let spec: RepositorySpec = from_yaml(
        "backend: { filesystem: { path: /repo, volume: { pvc: { name: repo } } } }\n\
         encryption: { passwordSecretRef: { name: s } }\n\
         maintenance:\n\
         \x20 mover: { cache: { capacity: 1Gi, contentCacheSizeMb: 5000 } }\n",
    );
    let blocks = spec.cache_blocks();
    assert_eq!(
        blocks.iter().map(|b| b.0).collect::<Vec<_>>(),
        ["spec.moverDefaults.cache", "spec.maintenance.mover.cache"]
    );
    let errs = validate_cache_legacy_budgets_for(&spec, None);
    assert!(
        joined(&errs).contains("spec.maintenance.mover.cache"),
        "{}",
        joined(&errs)
    );
}
