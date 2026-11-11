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
/// `Maintenance` the operator manages for a repository is a projection of an
/// already-admitted repository, not a new user edit, so the bound is skipped.
#[test]
fn a_repository_managed_maintenance_is_not_refused_for_inherited_legacy_budgets() {
    let spec = maintenance("{ capacity: 2Gi, contentCacheSizeMb: 5000 }");
    // A user-authored Maintenance with these budgets is refused on CREATE…
    assert!(!validate_maintenance_cache_legacy_budgets(&spec, None, &[]).is_empty());
    assert!(
        !validate_maintenance_cache_legacy_budgets(&spec, None, &[owner("Repository", false)])
            .is_empty(),
        "a non-controller owner reference is not the operator's projection"
    );
    assert!(
        !validate_maintenance_cache_legacy_budgets(&spec, None, &[owner("SnapshotPolicy", true)])
            .is_empty(),
        "only a repository owner makes it a managed projection"
    );
    // …but the operator's managed projection of a Repository/ClusterRepository is not.
    for kind in ["Repository", "ClusterRepository"] {
        assert!(
            validate_maintenance_cache_legacy_budgets(&spec, None, &[owner(kind, true)]).is_empty(),
            "{kind}-managed Maintenance must be admitted"
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
