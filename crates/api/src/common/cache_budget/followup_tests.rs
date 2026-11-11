//! Regression tests for the #707 review follow-ups (#613): small capacities must
//! bound BOTH caches, and budgets must fit a mounted volume smaller than the
//! configured capacity (a pending resize, or two caches sharing one volume).

use super::*;
use crate::testutil::from_yaml;

const F: &str = "spec.mover.cache";

fn cfg(yaml: &str) -> CacheDefaults {
    from_yaml(yaml)
}

fn resolve(c: &CacheDefaults) -> Result<ResolvedCache, ValidationError> {
    resolve_cache_budgets(c, c.capacity.as_deref(), LegacyBudgets::PassThrough, F)
}

fn mb(r: &ResolvedCache) -> (Option<i64>, Option<i64>) {
    (r.content_cache_size_mb, r.metadata_cache_size_mb)
}

/// Greptile on #707: with `capacity: 5Mi` the metadata default rounded to 0 and
/// was left UNSET, so kopia kept its multi-GiB metadata default and could still
/// fill the volume. A default is never rounded down to nothing while there is room.
#[test]
fn small_capacity_defaults_bound_both_caches() {
    // 5MiB → content 3, metadata floored at 1 (fits the 4MiB ceiling).
    assert_eq!(
        mb(&resolve(&cfg("capacity: 5Mi\n")).unwrap()),
        (Some(3), Some(1))
    );
    // 3MiB is the smallest capacity that bounds both: 1 + 1 ≤ 2 (90% of 3).
    assert_eq!(
        mb(&resolve(&cfg("capacity: 3Mi\n")).unwrap()),
        (Some(1), Some(1))
    );
}

/// …and a capacity too small to bound BOTH caches is refused at admission (one
/// would be left at kopia's unbounded default), while a stored object with it
/// keeps running unchanged (pass-through).
#[test]
fn capacity_too_small_to_bound_both_caches_is_refused_at_admission() {
    let c = cfg("capacity: 2Mi\n");
    let err = resolve_cache_budgets(&c, Some("2Mi"), LegacyBudgets::Enforce, F).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("too small") && msg.contains("3Mi"), "{msg}");
    assert!(resolve(&c).is_ok(), "a stored object must keep running");
}

/// Greptile on #707: a Persistent cache grown 1Gi→2Gi whose filesystem only
/// finishes growing on pod mount (`FileSystemResizePending`) still reports 1Gi,
/// and resolving `{ size: 1Gi }` against that 1Gi failed every run, before the
/// mover pod that would finish the resize could start. Budgets are resolved
/// against the configured capacity (what admission validated) and then scaled
/// down to fit the volume actually available.
#[test]
fn budgets_resolved_against_configured_capacity_fit_a_smaller_mounted_volume() {
    let c = cfg("capacity: 2Gi\ncontentCacheSize: { size: 1Gi }\n");
    // Resolving against the still-small claim directly is what used to wedge.
    assert!(resolve_cache_budgets(&c, Some("1Gi"), LegacyBudgets::PassThrough, F).is_err());
    let r = resolve(&c).unwrap();
    assert_eq!(mb(&r), (Some(1024), Some(307)));
    let fitted = fit_cache_budgets(&r, "1Gi");
    assert_eq!(mb(&fitted), (Some(512), Some(153)));
    assert_eq!(fitted.capacity.as_deref(), Some("1Gi"));
    // A volume at least as large as configured leaves the budgets untouched.
    assert_eq!(fit_cache_budgets(&r, "2Gi"), r);
    assert_eq!(fit_cache_budgets(&r, "4Gi"), r);
    // Nothing resolved against a capacity (emptyDir) → nothing to scale.
    let empty = ResolvedCache::default();
    assert_eq!(fit_cache_budgets(&empty, "1Gi"), empty);
}

/// For every admissible config and every smaller mounted volume, the fitted
/// budgets stay bounded (both set) and within 90% of what is mounted.
#[test]
fn fitted_budgets_never_exceed_the_mounted_volume() {
    let budgets = [
        None,
        Some(CacheSize::Percent(10)),
        Some(CacheSize::Percent(60)),
        Some(CacheSize::Percent(80)),
        Some(CacheSize::Size("100Mi".into())),
        Some(CacheSize::Size("1Gi".into())),
    ];
    let mut checked = 0;
    for configured in ["1Gi", "2Gi", "10Gi"] {
        let c_mib = quantity_to_mib(configured).unwrap();
        for c in &budgets {
            for m in &budgets {
                let cfg = CacheDefaults {
                    capacity: Some(configured.into()),
                    content_cache_size: c.clone(),
                    metadata_cache_size: m.clone(),
                    ..Default::default()
                };
                let Ok(r) =
                    resolve_cache_budgets(&cfg, Some(configured), LegacyBudgets::Enforce, F)
                else {
                    continue;
                };
                for mounted in ["100Mi", "512Mi", "1Gi", "1500Mi"] {
                    let f = fit_cache_budgets(&r, mounted);
                    let (Some(fc), Some(fm)) = (f.content_cache_size_mb, f.metadata_cache_size_mb)
                    else {
                        panic!("{cfg:?} fitted to {mounted} left a cache unbounded: {f:?}");
                    };
                    let now = quantity_to_mib(mounted).unwrap().min(c_mib);
                    assert!(
                        (fc + fm) * 100 <= now * i64::from(MAX_CACHE_BUDGET_PERCENT),
                        "{cfg:?} fitted to {mounted}: {f:?}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 50, "{checked}");
}
