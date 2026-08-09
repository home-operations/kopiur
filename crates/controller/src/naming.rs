//! Deterministic, bounded name construction shared by the reconcilers.
//!
//! Kubernetes object names generated from user-chosen CR names must be (a)
//! **stable** — identical across reconciles, controller restarts, and HA
//! replicas, so re-applies converge instead of leaking a renamed sibling —
//! and (b) **bounded** — a Job name must fit the 63-char DNS-label limit (and
//! leave headroom when it is re-embedded in labels/pod names). Both helpers
//! hash with FNV-1a, chosen over `std`'s `DefaultHasher` because the latter
//! (SipHash) is explicitly not guaranteed stable across Rust releases — a
//! toolchain upgrade must never rename cluster objects.

/// Cap a generated object name at the 63-character DNS-label limit while keeping
/// it unique and deterministic: long names keep their leading 46 characters plus
/// a stable FNV-1a hash of the full name. A cross-namespace cascade-delete Job
/// embeds the source namespace in its name (two namespaces can each hold a
/// `Snapshot` of the same name), which can exceed the limit.
pub fn capped_name(full: &str) -> String {
    if full.len() <= 63 {
        return full.to_string();
    }
    let prefix: String = full.chars().take(46).collect();
    format!("{}-{:016x}", prefix.trim_end_matches('-'), fnv1a(full))
}

/// A short, stable (run-independent) 8-hex-char FNV-1a hash for name truncation.
pub fn short_hash(s: &str) -> String {
    format!("{:08x}", (fnv1a(s) & 0xffff_ffff))
}

/// The stable 6-hex per-repository tag used by multi-repo verify Job names
/// (`<policy>-vfy-<q|d>-<r6>-<unix>`) and the [`crate::consts::VERIFY_REPO_LABEL`]
/// value: [`short_hash`] over the normalized repo key
/// ([`kopiur_api::common::repo_key`]), truncated to 6. Six hex disambiguates a
/// policy's ≤8 repositories with room to spare (same budget reasoning as
/// `kopiur_api::expand::cache_pvc_name`'s h6) while keeping the Job-name slug
/// legible; label-safe where the raw `Kind/ns/name` key is not.
pub fn repo_tag6(repo_key: &str) -> String {
    short_hash(repo_key).chars().take(6).collect()
}

/// 64-bit FNV-1a over the string's bytes.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- capped_name (generated names stay valid DNS labels) -----------------

    #[test]
    fn capped_name_passes_short_names_through() {
        assert_eq!(capped_name("app-nightly-delete"), "app-nightly-delete");
    }

    #[test]
    fn capped_name_caps_long_names_uniquely_and_deterministically() {
        let long_a = format!("{}-snap-a-delete", "n".repeat(80));
        let long_b = format!("{}-snap-b-delete", "n".repeat(80));
        let a = capped_name(&long_a);
        assert!(a.len() <= 63, "{} chars", a.len());
        assert_eq!(a, capped_name(&long_a), "deterministic across reconciles");
        assert_ne!(a, capped_name(&long_b), "distinct inputs stay distinct");
    }

    #[test]
    fn capped_name_hash_is_pinned_across_toolchains() {
        // The exact FNV-1a output is load-bearing: it names on-cluster objects,
        // so an accidental algorithm change would rename (and leak) them on
        // upgrade. Pin a literal.
        let long_a = format!("{}-snap-a-delete", "n".repeat(80));
        assert!(capped_name(&long_a).ends_with("-78b92bff1e6d5c26"));
    }

    // --- short_hash -----------------------------------------------------------

    #[test]
    fn short_hash_is_pinned_across_toolchains() {
        // Same reasoning: maintenance/verify/replication Job names embed this
        // hash when the CR name exceeds the budget. Pin a literal.
        assert_eq!(short_hash("postgres-data"), "5a5d7a49");
    }

    // --- repo_tag6 ------------------------------------------------------------

    #[test]
    fn repo_tag6_is_pinned_stable_and_label_safe() {
        // Names on-cluster Jobs and rides a label value: pin a literal so an
        // algorithm change can never silently rename verify slots on upgrade.
        let tag = repo_tag6("Repository/backups/nas");
        assert_eq!(tag, &short_hash("Repository/backups/nas")[..6]);
        assert_eq!(tag.len(), 6);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(tag, repo_tag6("Repository/backups/nas"), "deterministic");
        assert_ne!(
            tag,
            repo_tag6("ClusterRepository/nas"),
            "distinct repos stay distinct"
        );
    }
}
