//! #613: cache-budget resolution math. These pin the rounding, the capacity
//! defaults, percent vs size vs legacy MiB, and — the property the issue is about —
//! that resolved budgets never add up past the cache volume.

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

// --- wire shape -------------------------------------------------------------

#[test]
fn cache_size_is_externally_tagged_percent_or_size() {
    let c = cfg(
        "capacity: 2Gi\ncontentCacheSize: { percent: 63 }\nmetadataCacheSize: { size: 256Mi }\n",
    );
    assert_eq!(c.content_cache_size, Some(CacheSize::Percent(63)));
    assert_eq!(c.metadata_cache_size, Some(CacheSize::Size("256Mi".into())));
    // Round-trips to the same wire shape.
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["contentCacheSize"], serde_json::json!({ "percent": 63 }));
    assert_eq!(
        v["metadataCacheSize"],
        serde_json::json!({ "size": "256Mi" })
    );
    // Both variants at once, or an unknown one, is not a CacheSize.
    let both: Result<CacheSize, _> =
        serde_json::from_value(serde_json::json!({ "percent": 5, "size": "1Gi" }));
    assert!(both.is_err());
    let unknown: Result<CacheSize, _> = serde_json::from_value(serde_json::json!({ "mb": 5 }));
    assert!(unknown.is_err());
}

#[test]
fn quantity_to_mib_rounds_down_across_suffixes() {
    assert_eq!(quantity_to_mib("1Gi"), Some(1024));
    assert_eq!(quantity_to_mib("1G"), Some(953)); // 10^9 B
    assert_eq!(quantity_to_mib("1536Ki"), Some(1));
    assert_eq!(quantity_to_mib("1Mi"), Some(1));
    assert_eq!(quantity_to_mib("1023Ki"), Some(0));
    assert_eq!(quantity_to_mib("garbage"), None);
}

// --- defaults ---------------------------------------------------------------

#[test]
fn capacity_alone_defaults_to_sixty_and_fifteen_percent() {
    let r = resolve(&cfg("capacity: 10Gi\n")).unwrap();
    // 10240MiB × 60% = 6144, × 15% = 1536.
    assert_eq!(mb(&r), (Some(6144), Some(1536)));
    assert_eq!(r.capacity.as_deref(), Some("10Gi"));
}

#[test]
fn no_capacity_and_no_budgets_is_kopia_defaults() {
    let r = resolve(&CacheDefaults::default()).unwrap();
    assert_eq!(mb(&r), (None, None));
    assert_eq!(r.capacity, None);
}

#[test]
fn no_capacity_passes_absolute_budgets_through() {
    let r = resolve(&cfg(
        "contentCacheSizeMb: 4096\nmetadataCacheSize: { size: 1Gi }\n",
    ))
    .unwrap();
    assert_eq!(mb(&r), (Some(4096), Some(1024)));
}

#[test]
fn defaults_round_down_on_small_capacities() {
    // 100MiB → 60 / 15.
    assert_eq!(
        mb(&resolve(&cfg("capacity: 100Mi\n")).unwrap()),
        (Some(60), Some(15))
    );
    // 1MiB → nothing fits under the ceiling: both left unset at run time, never an
    // error for a stored object (admission refuses it; see the follow-up tests).
    assert_eq!(mb(&resolve(&cfg("capacity: 1Mi\n")).unwrap()), (None, None));
}

#[test]
fn a_set_budget_lowers_the_other_default_to_fit_under_ninety_percent() {
    // content 80% of 1000MiB-ish: metadata default 15% would push to 95%, so it is
    // lowered to the room left (90% − 80% = 10%).
    let r = resolve(&cfg(
        "capacity: 1000Mi\ncontentCacheSize: { percent: 80 }\n",
    ))
    .unwrap();
    assert_eq!(mb(&r), (Some(800), Some(100)));
    // metadata 50% leaves 40% for content (below the 60% default).
    let r = resolve(&cfg(
        "capacity: 1000Mi\nmetadataCacheSize: { percent: 50 }\n",
    ))
    .unwrap();
    assert_eq!(mb(&r), (Some(400), Some(500)));
}

// --- explicit budgets ---------------------------------------------------------

#[test]
fn percent_resolves_against_capacity_rounding_down() {
    // The issue's example: 2Gi with 63% / 12%.
    let r = resolve(&cfg(
        "capacity: 2Gi\ncontentCacheSize: { percent: 63 }\nmetadataCacheSize: { percent: 12 }\n",
    ))
    .unwrap();
    // 2048 × 0.63 = 1290.24 → 1290; × 0.12 = 245.76 → 245.
    assert_eq!(mb(&r), (Some(1290), Some(245)));
}

#[test]
fn size_resolves_to_whole_mib() {
    let r = resolve(&cfg(
        "capacity: 10Gi\ncontentCacheSize: { size: 4Gi }\nmetadataCacheSize: { size: 1500M }\n",
    ))
    .unwrap();
    // 1500M = 1.5e9 B = 1430.5MiB → 1430.
    assert_eq!(mb(&r), (Some(4096), Some(1430)));
}

#[test]
fn legacy_mb_still_works_and_fills_the_other_default() {
    let r = resolve(&cfg("capacity: 8Gi\ncontentCacheSizeMb: 4096\n")).unwrap();
    // content as written; metadata 15% of 8192 = 1228.
    assert_eq!(mb(&r), (Some(4096), Some(1228)));
}

#[test]
fn stored_legacy_values_over_capacity_pass_through_unchanged() {
    // An object admitted before #613 must keep running exactly as before.
    let c = cfg("capacity: 2Gi\ncontentCacheSizeMb: 5000\nmetadataCacheSizeMb: 5000\n");
    assert_eq!(mb(&resolve(&c).unwrap()), (Some(5000), Some(5000)));
    // ...but admission (Enforce) refuses to re-admit them.
    let err = resolve_cache_budgets(&c, Some("2Gi"), LegacyBudgets::Enforce, F).unwrap_err();
    assert!(err.to_string().contains("does not fit"), "{err}");
}

// --- refusals ---------------------------------------------------------------

fn err(yaml: &str) -> String {
    resolve(&cfg(yaml)).unwrap_err().to_string()
}

#[test]
fn percent_out_of_range_is_refused() {
    assert!(err("capacity: 1Gi\ncontentCacheSize: { percent: 0 }\n").contains("between 1 and 100"));
    assert!(
        err("capacity: 1Gi\ncontentCacheSize: { percent: 101 }\n").contains("between 1 and 100")
    );
}

#[test]
fn percent_without_any_capacity_is_refused_with_the_fix() {
    let e = err("metadataCacheSize: { percent: 10 }\n");
    assert!(e.contains("needs a cache `capacity`"), "{e}");
    assert!(e.contains("Fix:"), "{e}");
}

#[test]
fn percents_summing_past_ninety_are_refused_even_without_capacity() {
    let e = err("contentCacheSize: { percent: 80 }\nmetadataCacheSize: { percent: 11 }\n");
    assert!(e.contains("91%"), "{e}");
    // Exactly 90 is fine.
    assert!(
        resolve(&cfg(
            "capacity: 1Gi\ncontentCacheSize: { percent: 75 }\nmetadataCacheSize: { percent: 15 }\n"
        ))
        .is_ok()
    );
}

#[test]
fn size_larger_than_capacity_is_refused() {
    let e = err("capacity: 2Gi\ncontentCacheSize: { size: 3Gi }\n");
    assert!(e.contains("does not fit"), "{e}");
}

#[test]
fn sizes_summing_past_ninety_percent_are_refused() {
    let e = err(
        "capacity: 2Gi\ncontentCacheSize: { size: 1500Mi }\nmetadataCacheSize: { size: 400Mi }\n",
    );
    assert!(e.contains("over 90%"), "{e}");
}

#[test]
fn a_budget_crowding_out_the_other_is_refused() {
    // 90% content leaves no room for the metadata default.
    let e = err("capacity: 1Gi\ncontentCacheSize: { percent: 90 }\n");
    assert!(e.contains("leaves no room"), "{e}");
}

#[test]
fn both_spellings_of_one_budget_are_refused() {
    let e = err("capacity: 1Gi\ncontentCacheSize: { percent: 50 }\ncontentCacheSizeMb: 100\n");
    assert!(e.contains("cannot both be set"), "{e}");
}

#[test]
fn bad_sizes_are_refused() {
    assert!(err("contentCacheSize: { size: lots }\n").contains("not a Kubernetes quantity"));
    assert!(err("contentCacheSize: { size: 512Ki }\n").contains("less than 1Mi"));
}

#[test]
fn tiny_capacity_with_a_percent_is_refused_not_rounded_to_zero() {
    let e = err("capacity: 4Mi\nmetadataCacheSize: { percent: 10 }\n");
    assert!(e.contains("less than 1MiB"), "{e}");
}

// --- merge: one budget, two spellings ----------------------------------------

#[test]
fn merge_treats_both_spellings_as_one_budget() {
    let base = cfg("capacity: 8Gi\ncontentCacheSizeMb: 4096\nmetadataCacheSizeMb: 512\n");
    let over = cfg("contentCacheSize: { percent: 50 }\n");
    let m = CacheDefaults::merge(Some(&base), Some(&over)).unwrap();
    // The override's percent REPLACES the inherited MiB — never both (a conflict).
    assert_eq!(m.content_cache_size, Some(CacheSize::Percent(50)));
    assert_eq!(m.content_cache_size_mb, None);
    // The untouched metadata budget is still inherited.
    assert_eq!(m.metadata_cache_size_mb, Some(512));
    assert_eq!(mb(&resolve(&m).unwrap()), (Some(4096), Some(512)));
}

// --- the property the issue is about ------------------------------------------

/// For every combination of capacity and budgets (percent, size, MiB, unset) that
/// admission accepts, the resolved budgets never exceed 90% of the cache volume —
/// so kopia's between-sweep overshoot stays on the disk.
#[test]
fn resolved_budgets_never_exceed_capacity_for_any_admissible_input() {
    let capacities = [
        "1Mi", "2Mi", "7Mi", "10Mi", "64Mi", "100Mi", "512Mi", "999Mi", "1Gi", "1500M", "2Gi",
        "10Gi", "100Gi",
    ];
    let budgets: Vec<Option<(Option<CacheSize>, Option<i64>)>> = {
        let mut v = vec![None];
        for p in [1, 5, 10, 15, 30, 45, 60, 75, 89, 90, 95, 100] {
            v.push(Some((Some(CacheSize::Percent(p)), None)));
        }
        for s in ["1Mi", "50Mi", "500Mi", "1Gi", "5Gi", "50Gi"] {
            v.push(Some((Some(CacheSize::Size(s.into())), None)));
        }
        for n in [1, 100, 1000, 5000, 50000] {
            v.push(Some((None, Some(n))));
        }
        v
    };
    let mut checked = 0;
    for cap in capacities {
        let cap_mib = quantity_to_mib(cap).unwrap();
        for c in &budgets {
            for m in &budgets {
                let cfg = CacheDefaults {
                    capacity: Some(cap.into()),
                    content_cache_size: c.as_ref().and_then(|b| b.0.clone()),
                    content_cache_size_mb: c.as_ref().and_then(|b| b.1),
                    metadata_cache_size: m.as_ref().and_then(|b| b.0.clone()),
                    metadata_cache_size_mb: m.as_ref().and_then(|b| b.1),
                    ..Default::default()
                };
                // "Admissible" = what the webhook accepts on a new apply.
                let Ok(r) = resolve_cache_budgets(&cfg, Some(cap), LegacyBudgets::Enforce, F)
                else {
                    continue;
                };
                let total =
                    r.content_cache_size_mb.unwrap_or(0) + r.metadata_cache_size_mb.unwrap_or(0);
                assert!(
                    total * 100 <= cap_mib * i64::from(MAX_CACHE_BUDGET_PERCENT),
                    "{cfg:?} resolved to {r:?}: {total}MiB > 90% of {cap_mib}MiB"
                );
                // …and neither cache is left at kopia's unbounded default.
                assert!(
                    r.content_cache_size_mb.is_some() && r.metadata_cache_size_mb.is_some(),
                    "{cfg:?} left a cache unbounded: {r:?}"
                );
                // The controller's pass-through view of an admissible object is identical.
                assert_eq!(
                    resolve_cache_budgets(&cfg, Some(cap), LegacyBudgets::PassThrough, F).unwrap(),
                    r,
                    "admission and controller must resolve an admitted object identically"
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 500,
        "the grid must exercise many admissible inputs, got {checked}"
    );
}
