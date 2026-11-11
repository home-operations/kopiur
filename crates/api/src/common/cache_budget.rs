//! kopia cache-budget resolution (#613): turn a mover cache config's
//! `contentCacheSize`/`metadataCacheSize` (percent or size), the original
//! `*CacheSizeMb` fields, and the capacity defaults into the two MiB values kopia
//! is given (`--content-cache-size-mb` / `--metadata-cache-size-mb`).
//!
//! [`resolve_cache_budgets`] is the ONLY place that math lives. The admission
//! validators, the controller's work-spec builders, and the VolSync translator all
//! call it, so what the webhook accepts is exactly what the mover runs with.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{CacheDefaults, CacheSize};
use crate::consts::{
    DEFAULT_CONTENT_CACHE_PERCENT, DEFAULT_METADATA_CACHE_PERCENT, MAX_CACHE_BUDGET_PERCENT,
};
use crate::error::ValidationError;

/// The kopia cache budgets a mover actually runs with, pinned into status
/// (`Snapshot.status.resolved.cache`) so the effective values are visible on the
/// object rather than only inside the mover Job (#613).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedCache {
    /// The cache volume size the budgets were resolved against. Absent for an
    /// `emptyDir` cache, which has no fixed size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<String>,
    /// The `--content-cache-size-mb` kopia was given; absent means kopia's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_cache_size_mb: Option<i64>,
    /// The `--metadata-cache-size-mb` kopia was given; absent means kopia's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_cache_size_mb: Option<i64>,
}

/// How [`resolve_cache_budgets`] treats the original `*CacheSizeMb` fields.
///
/// Those fields predate the capacity bound, so a stored object may carry values
/// that do not fit its `capacity`. The shared validator and the controller keep
/// such an object running exactly as before ([`LegacyBudgets::PassThrough`]); only
/// admission refuses a NEW apply that keeps them ([`LegacyBudgets::Enforce`], see
/// `validate::validate_cache_legacy_budgets`) — the admission-only ratchet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyBudgets {
    /// Hand `*CacheSizeMb` to kopia verbatim; do not bound it by `capacity`.
    PassThrough,
    /// Bound `*CacheSizeMb` by `capacity` exactly like `{ size: … }`.
    Enforce,
}

/// Where one resolved budget came from — decides whether the capacity bound applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// `{ percent }` / `{ size }`, or a `*CacheSizeMb` under [`LegacyBudgets::Enforce`].
    Bounded,
    /// A `*CacheSizeMb` under [`LegacyBudgets::PassThrough`].
    Legacy,
    /// Filled in from the default percentage.
    Defaulted,
}

#[derive(Clone, Copy, Debug)]
struct Budget {
    mb: i64,
    origin: Origin,
}

impl Budget {
    fn bounded(self) -> bool {
        self.origin == Origin::Bounded
    }
}

const MIB: i128 = 1024 * 1024;

/// Parse a Kubernetes quantity into whole MiB, rounding down. `None` when it does
/// not parse or is negative.
pub fn quantity_to_mib(q: &str) -> Option<i64> {
    let parsed = kube_quantity::ParsedQuantity::try_from(q.trim()).ok()?;
    let bytes = parsed.to_bytes_i128()?;
    if bytes < 0 {
        return None;
    }
    i64::try_from(bytes / MIB).ok()
}

/// Everything the per-budget helpers need, so each stays small.
struct Ctx<'a> {
    field: &'a str,
    capacity: Option<&'a str>,
    cap_mib: Option<i64>,
    legacy: LegacyBudgets,
    /// `false` only for [`validate_cache_shape`]: one layer checked in isolation,
    /// where a `{ percent }` may draw on a capacity inherited from another layer.
    percent_needs_capacity: bool,
}

impl Ctx<'_> {
    fn invalid(&self, sub: &str, reason: String) -> ValidationError {
        ValidationError::InvalidFieldValue {
            field: format!("{}.{sub}", self.field),
            reason,
        }
    }

    fn capacity_str(&self) -> &str {
        self.capacity.unwrap_or_default()
    }

    /// Ceiling for the two budgets together: `MAX_CACHE_BUDGET_PERCENT` of capacity.
    fn ceiling(cap: i64) -> i64 {
        cap * i64::from(MAX_CACHE_BUDGET_PERCENT) / 100
    }

    fn percent(&self, p: u32, sub: &str) -> Result<Budget, ValidationError> {
        if !(1..=100).contains(&p) {
            return Err(self.invalid(sub, format!("percent must be between 1 and 100, got {p}")));
        }
        let Some(cap) = self.cap_mib else {
            if !self.percent_needs_capacity {
                // Shape-only check: the share is resolved later against the merged
                // config's capacity; `0` is a placeholder that is never handed out.
                return Ok(Budget {
                    mb: 0,
                    origin: Origin::Bounded,
                });
            }
            return Err(self.invalid(
                sub,
                format!(
                    "{{ percent: {p} }} needs a cache `capacity` to take a share of, and none is \
                     set here or inherited from moverDefaults.cache. Fix: set `capacity`, or use \
                     {{ size: <quantity> }}"
                ),
            ));
        };
        let mb = cap * i64::from(p) / 100;
        if mb < 1 {
            return Err(self.invalid(
                sub,
                format!(
                    "{p}% of capacity `{}` is less than 1MiB. Fix: raise `capacity` or the percent",
                    self.capacity_str()
                ),
            ));
        }
        Ok(Budget {
            mb,
            origin: Origin::Bounded,
        })
    }

    fn size(&self, q: &str, sub: &str) -> Result<Budget, ValidationError> {
        match quantity_to_mib(q) {
            Some(mb) if mb >= 1 => Ok(Budget {
                mb,
                origin: Origin::Bounded,
            }),
            Some(_) => Err(self.invalid(
                sub,
                format!("size `{q}` is less than 1Mi; kopia cache budgets are whole MiB"),
            )),
            None => Err(self.invalid(
                sub,
                format!("size `{q}` is not a Kubernetes quantity (e.g. `500Mi`, `2Gi`)"),
            )),
        }
    }

    /// One budget as written, or `None` when unset.
    fn explicit(
        &self,
        sized: Option<&CacheSize>,
        mb: Option<i64>,
        sub: &str,
    ) -> Result<Option<Budget>, ValidationError> {
        match (sized, mb) {
            (Some(_), Some(_)) => Err(ValidationError::MutuallyExclusive {
                a: format!("{}.{sub}", self.field),
                b: format!("{}.{sub}Mb", self.field),
                context: "they are two spellings of one kopia cache budget; keep one".into(),
            }),
            (Some(CacheSize::Percent(p)), None) => self.percent(*p, sub).map(Some),
            (Some(CacheSize::Size(q)), None) => self.size(q, sub).map(Some),
            (None, Some(n)) => Ok(Some(Budget {
                mb: n,
                origin: match self.legacy {
                    LegacyBudgets::PassThrough => Origin::Legacy,
                    LegacyBudgets::Enforce => Origin::Bounded,
                },
            })),
            (None, None) => Ok(None),
        }
    }

    /// Reject two percents that already sum past the ceiling — checkable even when
    /// the capacity is inherited and unknown here.
    fn check_percent_sum(&self, cfg: &CacheDefaults) -> Result<(), ValidationError> {
        if let (Some(CacheSize::Percent(a)), Some(CacheSize::Percent(b))) =
            (&cfg.content_cache_size, &cfg.metadata_cache_size)
            && a + b > MAX_CACHE_BUDGET_PERCENT
        {
            return Err(self.invalid(
                "contentCacheSize",
                format!(
                    "content {a}% + metadata {b}% = {}% of capacity, over the \
                     {MAX_CACHE_BUDGET_PERCENT}% limit. kopia overshoots its cache budgets \
                     between sweeps, so budgets near 100% fill the volume. Fix: lower one so the \
                     two sum to at most {MAX_CACHE_BUDGET_PERCENT}",
                    a + b
                ),
            ));
        }
        Ok(())
    }

    /// A budget's default share — at least 1MiB, so a small capacity never rounds a
    /// cache down to "unset" (kopia's unbounded default) — lowered to the room
    /// `other` leaves under the ceiling. `None` only when there is no room at all.
    fn default_for(cap: i64, pct: u32, other: Option<Budget>) -> Option<Budget> {
        let room = Self::ceiling(cap) - other.map_or(0, |o| o.mb);
        let mb = (cap * i64::from(pct) / 100).max(1).min(room);
        (mb >= 1).then_some(Budget {
            mb,
            origin: Origin::Defaulted,
        })
    }

    /// At admission ([`LegacyBudgets::Enforce`]) a known capacity must bound BOTH
    /// caches: one left unset runs at kopia's multi-GiB default and can fill a small
    /// volume. Only a capacity under 3MiB (no room for 1MiB + 1MiB under the 90%
    /// ceiling) gets here; a stored object with one keeps running (pass-through).
    fn check_both_bounded(
        &self,
        content: Option<Budget>,
        metadata: Option<Budget>,
    ) -> Result<(), ValidationError> {
        if self.legacy == LegacyBudgets::Enforce && (content.is_none() || metadata.is_none()) {
            return Err(self.invalid(
                "capacity",
                format!(
                    "capacity `{}` is too small to bound both kopia caches (it needs at least \
                     3Mi), so one would run at kopia's unbounded default. Fix: raise `capacity`, \
                     or remove it for an unsized emptyDir cache",
                    self.capacity_str()
                ),
            ));
        }
        Ok(())
    }

    /// The capacity bound on resolved budgets (legacy pass-through values exempt).
    fn check_bounds(
        &self,
        cap: i64,
        content: Option<Budget>,
        metadata: Option<Budget>,
    ) -> Result<(), ValidationError> {
        for (sub, b) in [
            ("contentCacheSize", content),
            ("metadataCacheSize", metadata),
        ] {
            if let Some(b) = b.filter(|b| b.bounded() && b.mb > cap) {
                return Err(self.invalid(
                    sub,
                    format!(
                        "{}MiB does not fit in the cache volume's capacity `{}` ({cap}MiB). Fix: \
                         lower it, raise `capacity`, or use {{ percent: N }}",
                        b.mb,
                        self.capacity_str()
                    ),
                ));
            }
        }
        if !(content.is_some_and(Budget::bounded) || metadata.is_some_and(Budget::bounded)) {
            return Ok(());
        }
        // A bounded budget must leave the other one room, or kopia would fall back to
        // its own multi-GiB default on what may be a small volume.
        for (sub, other, b) in [
            ("metadataCacheSize", "contentCacheSize", metadata),
            ("contentCacheSize", "metadataCacheSize", content),
        ] {
            if b.is_none() {
                return Err(self.invalid(
                    other,
                    format!(
                        "leaves no room for the {sub} budget within {MAX_CACHE_BUDGET_PERCENT}% of \
                         capacity `{}`. Fix: lower it, or set {sub} explicitly",
                        self.capacity_str()
                    ),
                ));
            }
        }
        let (c, m) = (content.map_or(0, |b| b.mb), metadata.map_or(0, |b| b.mb));
        let ceiling = Self::ceiling(cap);
        if c + m > ceiling {
            return Err(self.invalid(
                "contentCacheSize",
                format!(
                    "content {c}MiB + metadata {m}MiB = {}MiB, over {MAX_CACHE_BUDGET_PERCENT}% of \
                     capacity `{}` ({ceiling}MiB). kopia overshoots its cache budgets between \
                     sweeps, so budgets near the volume size fill it. Fix: lower a budget or \
                     raise `capacity`",
                    c + m,
                    self.capacity_str()
                ),
            ));
        }
        Ok(())
    }
}

/// Resolve a cache config's two kopia budgets to MiB — THE single place budgets are
/// computed (#613). `capacity` is the size of the cache volume the mover actually
/// mounts (`None` for an `emptyDir`); `field` prefixes error paths (e.g.
/// `"spec.mover.cache"`).
///
/// 1. An explicit budget resolves as written: `{ percent: p }` → `capacity × p / 100`
///    (rounded down; needs a capacity), `{ size: q }` → `q` in MiB (rounded down),
///    `*CacheSizeMb: n` → `n`.
/// 2. With a capacity, an unset budget defaults to its share (content 60%, metadata
///    15%), lowered if needed so the pair stays within 90% of capacity. A default
///    that would round to 0MiB is left unset (kopia's own default).
/// 3. With a capacity, every bounded budget must fit: each ≤ capacity, the pair ≤ 90%
///    of it, and neither may crowd the other out. `*CacheSizeMb` values are exempt
///    under [`LegacyBudgets::PassThrough`] so stored objects keep running unchanged.
///
/// Guarantee (pinned by a property test): whenever this returns `Ok` and no legacy
/// pass-through value was involved, `content + metadata ≤ 90% × capacity`.
pub fn resolve_cache_budgets(
    cfg: &CacheDefaults,
    capacity: Option<&str>,
    legacy: LegacyBudgets,
    field: &str,
) -> Result<ResolvedCache, ValidationError> {
    let ctx = Ctx {
        field,
        capacity,
        // An unparseable capacity resolves as "no known capacity": the volume itself
        // is then rejected by the apiserver, which is the clearer error.
        cap_mib: capacity.and_then(quantity_to_mib).filter(|c| *c > 0),
        legacy,
        percent_needs_capacity: true,
    };
    ctx.check_percent_sum(cfg)?;
    let content = ctx.explicit(
        cfg.content_cache_size.as_ref(),
        cfg.content_cache_size_mb,
        "contentCacheSize",
    )?;
    let metadata = ctx.explicit(
        cfg.metadata_cache_size.as_ref(),
        cfg.metadata_cache_size_mb,
        "metadataCacheSize",
    )?;

    // A capacity that parses but rounds to 0MiB (e.g. `512Ki`) is a SIZED volume too
    // small to bound either cache — refuse it at admission rather than treat it as
    // "no capacity" (#714 review). Stored objects keep running (pass-through).
    if capacity.and_then(quantity_to_mib) == Some(0) {
        ctx.check_both_bounded(None, None)?;
    }
    let Some(cap) = ctx.cap_mib else {
        // No fixed-size volume: nothing to default against or bound by.
        return Ok(ResolvedCache {
            capacity: None,
            content_cache_size_mb: content.map(|b| b.mb),
            metadata_cache_size_mb: metadata.map(|b| b.mb),
        });
    };
    // Default content against metadata's explicit value, then metadata against the
    // RESOLVED content: the 1MiB floor means two defaults on a tiny volume could
    // otherwise overshoot the ceiling together.
    let content_r =
        content.or_else(|| Ctx::default_for(cap, DEFAULT_CONTENT_CACHE_PERCENT, metadata));
    let metadata_r =
        metadata.or_else(|| Ctx::default_for(cap, DEFAULT_METADATA_CACHE_PERCENT, content_r));
    ctx.check_bounds(cap, content_r, metadata_r)?;
    ctx.check_both_bounded(content_r, metadata_r)?;
    Ok(ResolvedCache {
        capacity: capacity.map(str::to_string),
        content_cache_size_mb: content_r.map(|b| b.mb),
        metadata_cache_size_mb: metadata_r.map(|b| b.mb),
    })
}

/// Check ONE cache layer in isolation (a `mover.cache` or `moverDefaults.cache` at
/// admission). With a `capacity` on this layer the budgets are fully resolved and
/// bounded against it; without one only their shape is checked (both spellings,
/// percent range, sum of percents, size syntax), because the capacity may be
/// inherited — the merged config is then resolved at run time by
/// [`resolve_cache_budgets`]. Legacy `*CacheSizeMb` values pass through.
pub fn validate_cache_shape(cfg: &CacheDefaults, field: &str) -> Result<(), ValidationError> {
    if cfg.capacity.is_some() {
        return resolve_cache_budgets(
            cfg,
            cfg.capacity.as_deref(),
            LegacyBudgets::PassThrough,
            field,
        )
        .map(drop);
    }
    let ctx = Ctx {
        field,
        capacity: None,
        cap_mib: None,
        legacy: LegacyBudgets::PassThrough,
        percent_needs_capacity: false,
    };
    ctx.check_percent_sum(cfg)?;
    ctx.explicit(
        cfg.content_cache_size.as_ref(),
        cfg.content_cache_size_mb,
        "contentCacheSize",
    )?;
    ctx.explicit(
        cfg.metadata_cache_size.as_ref(),
        cfg.metadata_cache_size_mb,
        "metadataCacheSize",
    )?;
    Ok(())
}

/// Scale resolved budgets down to fit `available` when it is smaller than the
/// capacity they were resolved against (#613 review follow-up).
///
/// Budgets are always resolved against the CONFIGURED capacity — the one admission
/// validated — so an edit that passed the webhook can never fail at run time just
/// because the volume has not caught up. The volume actually mounted can be
/// smaller: a grown PVC whose filesystem only finishes resizing when a pod mounts
/// it (`FileSystemResizePending`), a refused expansion, or two caches sharing one
/// volume (snapshot replication). Each budget is scaled by `available / capacity`
/// (rounded down, at least 1MiB so neither cache falls back to kopia's unbounded
/// default), which keeps the pair within 90% of `available`. A volume at least as
/// large as the resolved capacity, or a result with no capacity, is returned as is.
pub fn fit_cache_budgets(r: &ResolvedCache, available: &str) -> ResolvedCache {
    let (Some(resolved), Some(avail)) = (
        r.capacity.as_deref().and_then(quantity_to_mib),
        quantity_to_mib(available),
    ) else {
        return r.clone();
    };
    if avail >= resolved || resolved <= 0 {
        return r.clone();
    }
    let scale = |b: Option<i64>| b.map(|mb| (mb * avail / resolved).max(1));
    let (mut content, mut metadata) = (
        scale(r.content_cache_size_mb),
        scale(r.metadata_cache_size_mb),
    );
    // The 1MiB floor can push the pair past the ceiling on a small volume (#714
    // review): take the excess off the larger budget, never below 1MiB.
    let ceiling = avail * i64::from(MAX_CACHE_BUDGET_PERCENT) / 100;
    if let (Some(c), Some(m)) = (content.as_mut(), metadata.as_mut()) {
        let excess = *c + *m - ceiling;
        if excess > 0 {
            let larger = if *c >= *m { c } else { m };
            *larger = (*larger - excess).max(1);
        }
    }
    ResolvedCache {
        capacity: Some(available.to_string()),
        content_cache_size_mb: content,
        metadata_cache_size_mb: metadata,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod followup_tests;
