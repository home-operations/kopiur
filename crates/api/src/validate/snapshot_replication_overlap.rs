//! The pure identity-overlap decision for `SnapshotReplication` admission
//! (issue #368).
//!
//! A replication copies SOURCE identities into the destination repository. When
//! a destination-side `SnapshotPolicy` writes **directly** into that same
//! repository under an identity the replication's `spec.selection` would also
//! select, replicated copies and the policy's own snapshots interleave in one
//! kopia identity's history — and with `pruning: mirrorSource` a source-side
//! deletion cascades into an identity the destination does not merely mirror
//! (the data-loss combination the webhook denies).
//!
//! This module is only the **decision**: the webhook lists the destination's
//! policies, resolves their identities, and calls
//! [`replication_identity_overlap`]. Matching semantics are byte-identical to
//! the replication mover's selection (`crates/mover/src/replicate.rs`): an
//! identity is selected when it matches at least one `include` matcher (an
//! empty `include` list means *everything*) and no `exclude` matcher — exclude
//! always wins — and a fully-empty matcher defensively matches NOTHING (the
//! webhook refuses one upstream, but an invalid matcher must never select or
//! exclude the world).

use crate::common::ResolvedIdentity;
use crate::identity_string;
use crate::snapshot_replication::{IdentityMatcher, component_glob_matches};

/// The destination-side identities (rendered as kopia's
/// `username@hostname[:path]`) that `include`/`exclude` would select — i.e.
/// identities this replication would copy INTO while a destination policy also
/// writes them directly. Empty means no overlap. Sorted and de-duplicated so
/// the admission message (and any test) is deterministic.
///
/// ```
/// use kopiur_api::common::ResolvedIdentity;
/// use kopiur_api::snapshot_replication::IdentityMatcher;
/// use kopiur_api::validate::replication_identity_overlap;
///
/// let dest = vec![ResolvedIdentity {
///     username: "pg".into(),
///     hostname: "billing".into(),
///     source_path: Some("/pvc/data".into()),
/// }];
/// // An absent selection (empty include) selects everything → overlap.
/// assert_eq!(
///     replication_identity_overlap(&[], &[], &dest),
///     vec!["pg@billing:/pvc/data".to_string()],
/// );
/// // Excluding the identity clears it.
/// let exclude = vec![IdentityMatcher { username: Some("pg".into()), ..Default::default() }];
/// assert!(replication_identity_overlap(&[], &exclude, &dest).is_empty());
/// ```
pub fn replication_identity_overlap(
    include: &[IdentityMatcher],
    exclude: &[IdentityMatcher],
    identities: &[ResolvedIdentity],
) -> Vec<String> {
    let mut out: Vec<String> = identities
        .iter()
        .filter(|id| {
            let included =
                include.is_empty() || include.iter().any(|m| overlap_matcher_matches(m, id));
            included && !exclude.iter().any(|m| overlap_matcher_matches(m, id))
        })
        .map(identity_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One matcher against one resolved identity: every PRESENT component must
/// [`component_glob_matches`] its counterpart (an absent matcher component
/// matches anything); an all-absent matcher matches nothing (defensive — see
/// module doc). An identity with no `source_path` matches a `sourcePath`
/// pattern only when the pattern covers the empty string (`"*"` does).
fn overlap_matcher_matches(m: &IdentityMatcher, id: &ResolvedIdentity) -> bool {
    if m.username.is_none() && m.hostname.is_none() && m.source_path.is_none() {
        return false;
    }
    m.username
        .as_deref()
        .is_none_or(|p| component_glob_matches(p, &id.username))
        && m.hostname
            .as_deref()
            .is_none_or(|p| component_glob_matches(p, &id.hostname))
        && m.source_path
            .as_deref()
            .is_none_or(|p| component_glob_matches(p, id.source_path.as_deref().unwrap_or("")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(username: &str, hostname: &str, path: Option<&str>) -> ResolvedIdentity {
        ResolvedIdentity {
            username: username.into(),
            hostname: hostname.into(),
            source_path: path.map(str::to_string),
        }
    }

    fn matcher(
        username: Option<&str>,
        hostname: Option<&str>,
        source_path: Option<&str>,
    ) -> IdentityMatcher {
        IdentityMatcher {
            username: username.map(str::to_string),
            hostname: hostname.map(str::to_string),
            source_path: source_path.map(str::to_string),
        }
    }

    #[test]
    fn empty_include_selects_every_identity() {
        let ids = [
            id("pg", "billing", Some("/pvc/data")),
            id("redis", "cache", Some("/pvc/redis")),
        ];
        assert_eq!(
            replication_identity_overlap(&[], &[], &ids),
            vec![
                "pg@billing:/pvc/data".to_string(),
                "redis@cache:/pvc/redis".to_string(),
            ],
        );
    }

    #[test]
    fn include_glob_narrows_and_all_set_components_must_match() {
        let ids = [
            id("pg-main", "billing", Some("/pvc/data")),
            id("pg-replica", "other", Some("/pvc/data")),
            id("redis", "billing", Some("/pvc/redis")),
        ];
        let include = [matcher(Some("pg-*"), Some("billing"), None)];
        assert_eq!(
            replication_identity_overlap(&include, &[], &ids),
            vec!["pg-main@billing:/pvc/data".to_string()],
        );
    }

    #[test]
    fn exclude_wins_over_include() {
        let ids = [
            id("pg", "billing", Some("/pvc/data")),
            id("pg", "staging", Some("/pvc/data")),
        ];
        let include = [matcher(Some("pg"), None, None)];
        let exclude = [matcher(None, Some("staging"), None)];
        assert_eq!(
            replication_identity_overlap(&include, &exclude, &ids),
            vec!["pg@billing:/pvc/data".to_string()],
        );
    }

    #[test]
    fn all_empty_matcher_matches_nothing_in_either_list() {
        let ids = [id("pg", "billing", Some("/pvc/data"))];
        let empty = [matcher(None, None, None)];
        // As an include entry it selects nothing (include non-empty, no match).
        assert!(replication_identity_overlap(&empty, &[], &ids).is_empty());
        // As an exclude entry it excludes nothing.
        assert_eq!(
            replication_identity_overlap(&[], &empty, &ids).len(),
            1,
            "a defensively-inert empty matcher must not exclude the world"
        );
    }

    #[test]
    fn absent_source_path_matches_only_patterns_covering_empty() {
        let ids = [id("cfg", "ns", None)];
        let star = [matcher(None, None, Some("*"))];
        assert_eq!(replication_identity_overlap(&star, &[], &ids).len(), 1);
        let concrete = [matcher(None, None, Some("/pvc/*"))];
        assert!(replication_identity_overlap(&concrete, &[], &ids).is_empty());
    }

    #[test]
    fn result_is_sorted_and_deduplicated() {
        let ids = [
            id("z", "h", Some("/p")),
            id("a", "h", Some("/p")),
            id("a", "h", Some("/p")),
        ];
        assert_eq!(
            replication_identity_overlap(&[], &[], &ids),
            vec!["a@h:/p".to_string(), "z@h:/p".to_string()],
        );
    }
}
