//! #707 review follow-ups (#613) for the admission-side cache rules.

use super::*;
use crate::maintenance::MaintenanceSpec;
use crate::snapshot_policy::SnapshotPolicySpec;
use crate::testutil::from_yaml;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

fn joined(errs: &[ValidationError]) -> String {
    errs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn policy(cache: &str) -> SnapshotPolicySpec {
    from_yaml(&format!(
        "repository: {{ kind: Repository, name: r }}\n\
         sources: [ {{ pvc: {{ name: data }} }} ]\n\
         mover:\n\
         \x20 cache: {cache}\n"
    ))
}

fn maintenance(cache: &str) -> MaintenanceSpec {
    from_yaml(&format!(
        "repository: {{ kind: Repository, name: r }}\n\
         schedule: {{ quick: {{ cron: '0 */6 * * *' }}, full: {{ cron: '0 3 * * *' }} }}\n\
         ownership: {{ owner: kopiur/r, takeoverPolicy: PromptCondition }}\n\
         mover: {{ cache: {cache} }}\n"
    ))
}

fn owner(kind: &str, controller: bool) -> OwnerReference {
    OwnerReference {
        api_version: "kopiur.home-operations.com/v1alpha1".into(),
        kind: kind.into(),
        name: "r".into(),
        uid: "u".into(),
        controller: Some(controller),
        block_owner_deletion: None,
    }
}

/// Greptile on #707: a stored Repository with maintenance disabled and an
/// oversized legacy `spec.maintenance.mover.cache` re-enables maintenance without
/// touching that block. The Repository passes (unchanged block), but the operator
/// then CREATES the managed `Maintenance` copying it; that create has no old
/// object, so the legacy bound refused it and maintenance never started. A
/// verified projection of an already-admitted repository is exempt.
#[test]
fn a_verified_repository_projection_is_not_refused_for_inherited_legacy_budgets() {
    let spec = maintenance("{ capacity: 2Gi, contentCacheSizeMb: 5000 }");
    let parent_cache = spec.mover.as_ref().and_then(|m| m.cache.clone());
    // A user-authored Maintenance with these budgets is refused on CREATE…
    assert!(!validate_maintenance_cache_legacy_budgets(&spec, None, None).is_empty());
    // …but the operator's projection — whose parent repository (verified by the
    // webhook: controller owner ref, uid match) carries the SAME cache block — is not.
    assert!(
        validate_maintenance_cache_legacy_budgets(&spec, None, Some(parent_cache.as_ref()))
            .is_empty()
    );
}

/// Greptile on #714 (security): owner references are user-editable, so claiming a
/// repository controller reference must not by itself skip the bound. The
/// exemption needs the parent repository's own `spec.maintenance.mover.cache` to
/// match: a user-authored Maintenance that names a repository whose cache differs
/// (or has none) is still checked.
#[test]
fn a_spoofed_projection_is_still_checked() {
    let spec = maintenance("{ capacity: 2Gi, contentCacheSizeMb: 5000 }");
    let other: crate::common::CacheDefaults = from_yaml("capacity: 2Gi\n");
    assert!(
        !validate_maintenance_cache_legacy_budgets(&spec, None, Some(Some(&other))).is_empty(),
        "a parent with a different cache block does not vouch for this one"
    );
    assert!(
        !validate_maintenance_cache_legacy_budgets(&spec, None, Some(None)).is_empty(),
        "a parent with no maintenance cache does not vouch for this one"
    );
}

/// Only a CONTROLLER owner reference to a kopiur Repository/ClusterRepository
/// names the parent the webhook verifies against.
#[test]
fn repository_controller_ref_picks_only_a_kopiur_repository_controller() {
    assert!(repository_controller_ref(&[]).is_none());
    assert!(repository_controller_ref(&[owner("Repository", false)]).is_none());
    assert!(repository_controller_ref(&[owner("SnapshotPolicy", true)]).is_none());
    let mut foreign = owner("Repository", true);
    foreign.api_version = "example.com/v1".into();
    assert!(repository_controller_ref(&[foreign]).is_none());
    for kind in ["Repository", "ClusterRepository"] {
        let refs = [owner(kind, true)];
        assert_eq!(
            repository_controller_ref(&refs).map(|o| o.kind.as_str()),
            Some(kind)
        );
    }
}

/// Greptile on #707: a capacity too small to bound both caches (under 3Mi) is
/// refused on a new apply even with no budgets set, while an unchanged stored
/// block and the shared aggregate (the reconciler's view) are left alone.
#[test]
fn a_capacity_too_small_to_bound_both_caches_is_refused_on_apply_only() {
    let spec = policy("{ capacity: 2Mi }");
    let msg = joined(&validate_cache_legacy_budgets_for(&spec, None));
    assert!(msg.contains("too small"), "{msg}");
    assert!(validate_cache_legacy_budgets_for(&spec, Some(&spec)).is_empty());
    assert!(validate_backup_config(&spec).is_empty(), "ratchet pin");
    assert!(validate_cache_legacy_budgets_for(&policy("{ capacity: 3Mi }"), None).is_empty());
}

fn snapshot_replication(cache: &str) -> crate::snapshot_replication::SnapshotReplicationSpec {
    from_yaml(&format!(
        "sourceRef: {{ name: nas-primary }}\n\
         destinationRef: {{ name: offsite }}\n\
         schedule: {{ cron: '0 6 * * *' }}\n\
         mover: {{ cache: {cache} }}\n"
    ))
}

/// Greptile on #714: a SnapshotReplication keeps TWO kopia caches (source and
/// destination) on its one cache volume, so the per-cache 3Mi minimum doubles. A
/// smaller capacity is refused on apply; an unchanged stored block is left alone.
#[test]
fn a_snapshot_replication_cache_must_hold_both_repositories_caches() {
    let small = snapshot_replication("{ capacity: 4Mi }");
    let msg = joined(&validate_snapshot_replication_cache_minimum(&small, None));
    assert!(msg.contains("6Mi") && msg.contains("both"), "{msg}");
    assert!(validate_snapshot_replication_cache_minimum(&small, Some(&small)).is_empty());
    assert!(
        validate_snapshot_replication(&small).is_empty(),
        "ratchet pin"
    );
    let ok = snapshot_replication("{ capacity: 6Mi }");
    assert!(validate_snapshot_replication_cache_minimum(&ok, None).is_empty());
    let no_capacity = snapshot_replication("{ contentCacheSizeMb: 100 }");
    assert!(validate_snapshot_replication_cache_minimum(&no_capacity, None).is_empty());
}
