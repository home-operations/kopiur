//! Per-repository mover-Job **pool membership**: which flavors of mover Job
//! count toward a repository's `spec.concurrency.maxConcurrentJobs`, and the
//! label that makes the pool countable with one selector.
//!
//! Membership is a policy decision, so it lives in exactly ONE place: the
//! exhaustive `match` in [`counts_toward_repo_pool`]. Every mover-spawning path
//! names its [`MoverJobKind`] there, so "does a pin Job compete with backups?"
//! has a single answer that a test can read off, rather than N label blocks
//! that can silently disagree. A new mover flavor cannot be added to
//! [`MoverJobKind`] without deciding — the `match` has no `_ =>` arm.
//!
//! The label itself is [`kopiur_api::consts::REPO_POOL_LABEL`], stamped through
//! `inputs.labels` so `build_job` mirrors it onto both the `Job` and its pod
//! template.

use kopiur_api::common::RepositoryRef;
use kopiur_api::consts::REPO_POOL_LABEL;

use crate::naming::repo_label;

/// Every flavor of mover `Job` kopiur spawns, as the pool-membership decision
/// sees them. One variant per job-build path.
///
/// Deliberately its own enum rather than a reuse of `OP_*` label constants: the
/// question here is "does this run compete for a repository's concurrency
/// budget", which is not the same partition as the op string the mover
/// dispatches on (a restore's direct and populator Jobs are one kind here; a
/// bootstrap and a catalog re-scan are one kind here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoverJobKind {
    /// `Snapshot` backup mover (`crate::snapshot`).
    Backup,
    /// `Restore` mover — both the direct and the populator Job, which share one
    /// build path (`crate::restore::run_restore_mover`).
    Restore,
    /// `RepositoryReplication` `kopia repository sync-to` mirror.
    RepositoryReplication,
    /// `SnapshotReplication` `kopia snapshot migrate` copy.
    SnapshotReplication,
    /// `Maintenance` quick/full run.
    Maintenance,
    /// `SnapshotPolicy` verification (quick or deep).
    Verification,
    /// `Snapshot` pin/unpin.
    Pin,
    /// Batched `snapdel-*` snapshot-delete Job.
    SnapshotDeleteBatch,
    /// Repository bootstrap / catalog re-scan (`<repo>-discovery`), for both
    /// repository kinds.
    Discovery,
    /// The CLI's read-only `kopiur browse` session pod.
    BrowseSession,
}

/// Does this flavor of mover Job count toward its repository's
/// `spec.concurrency.maxConcurrentJobs`?
///
/// **The whole decision table.** `true` for the flavors that do the bulk data
/// work a user throttles a backend against: backups, restores, and the SOURCE
/// side of either replication.
///
/// `false` for everything else, and each `false` is deliberate:
///
/// - **Maintenance** is the *cure* for an overloaded repository (it compacts
///   indexes and drops unreferenced blobs). Queuing it behind a saturated
///   backup pool would make a struggling repository permanently unmaintainable.
/// - **Verification** and **Pin** are cheap metadata operations; a pin is a
///   single manifest rewrite, and both are operator/housekeeping-driven rather
///   than user-scheduled load.
/// - **SnapshotDeleteBatch** is already single-flighted per repository by its
///   own dispatcher, and it *reduces* repository load; holding a delete behind
///   backups grows the backlog it exists to drain.
/// - **Discovery** (bootstrap / catalog re-scan) is what makes a repository
///   `Ready` in the first place. Gating it on a pool that only fills once the
///   repository IS ready would deadlock a fresh repository.
/// - **BrowseSession** is an interactive `kubectl kopiur browse` pod a human is
///   waiting on; parking it behind a nightly backup queue would read as a hung
///   command.
///
/// Exhaustive: a new [`MoverJobKind`] cannot compile until it is classified.
pub fn counts_toward_repo_pool(kind: MoverJobKind) -> bool {
    match kind {
        MoverJobKind::Backup
        | MoverJobKind::Restore
        | MoverJobKind::RepositoryReplication
        | MoverJobKind::SnapshotReplication => true,
        MoverJobKind::Maintenance
        | MoverJobKind::Verification
        | MoverJobKind::Pin
        | MoverJobKind::SnapshotDeleteBatch
        | MoverJobKind::Discovery
        | MoverJobKind::BrowseSession => false,
    }
}

/// The `(key, value)` [`REPO_POOL_LABEL`] entry for a run of `kind` against
/// `repo_ref` — or `None` when the flavor is excluded from the pool, so
/// `labels.extend(repo_pool_label(..))` leaves an excluded Job's metadata
/// byte-identical to what it carried before pooling existed.
///
/// **`repo_ref` must be NORMALIZED** — the ref a
/// `crate::io::ResolvedRepository::repository_ref` (equivalently
/// `kopiur_api::common::normalized_repository_ref`) produced, never a raw spec
/// ref. [`repo_label`] hashes `repository:{ns}/{name}`, so a `Repository` ref
/// carrying `namespace: None` hashes `repository:/{name}` and lands the run in
/// a pool of its own — splitting one repository's cap in two, silently. Every
/// caller therefore passes the ref it actually RESOLVED against.
pub fn repo_pool_label(kind: MoverJobKind, repo_ref: &RepositoryRef) -> Option<(String, String)> {
    counts_toward_repo_pool(kind).then(|| (REPO_POOL_LABEL.to_string(), repo_label(repo_ref)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::common::RepositoryKind;

    /// Every variant, so the truth-table tests below are provably exhaustive.
    /// Kept honest by [`assert_all_is_every_variant`], which destructures the
    /// enum exhaustively — a variant added to `MoverJobKind` but not to this
    /// list fails to COMPILE, rather than slipping through a length check.
    const ALL: &[MoverJobKind] = &[
        MoverJobKind::Backup,
        MoverJobKind::Restore,
        MoverJobKind::RepositoryReplication,
        MoverJobKind::SnapshotReplication,
        MoverJobKind::Maintenance,
        MoverJobKind::Verification,
        MoverJobKind::Pin,
        MoverJobKind::SnapshotDeleteBatch,
        MoverJobKind::Discovery,
        MoverJobKind::BrowseSession,
    ];

    const IN_POOL: &[MoverJobKind] = &[
        MoverJobKind::Backup,
        MoverJobKind::Restore,
        MoverJobKind::RepositoryReplication,
        MoverJobKind::SnapshotReplication,
    ];

    const OUT_OF_POOL: &[MoverJobKind] = &[
        MoverJobKind::Maintenance,
        MoverJobKind::Verification,
        MoverJobKind::Pin,
        MoverJobKind::SnapshotDeleteBatch,
        MoverJobKind::Discovery,
        MoverJobKind::BrowseSession,
    ];

    fn nas() -> RepositoryRef {
        RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: Some("backups".into()),
        }
    }

    /// The COMPILE-TIME half of the exhaustiveness guard.
    ///
    /// `ALL` is a hand-written list, so a length check alone proves nothing: a
    /// new `MoverJobKind` added to none of the three consts would keep
    /// `ALL.len() == IN_POOL.len() + OUT_OF_POOL.len()` true and sail through.
    /// This `match` has no `_ =>` arm and names each variant exactly as `ALL`
    /// does, so adding a variant to the enum breaks the BUILD here until it is
    /// listed — which is what forces it into a partition, and from there into
    /// [`counts_toward_repo_pool`]'s truth table below.
    fn assert_all_is_every_variant(k: MoverJobKind) {
        // Mirrors `ALL`, in order. Keep the two in sync.
        match k {
            MoverJobKind::Backup => assert_eq!(ALL[0], k),
            MoverJobKind::Restore => assert_eq!(ALL[1], k),
            MoverJobKind::RepositoryReplication => assert_eq!(ALL[2], k),
            MoverJobKind::SnapshotReplication => assert_eq!(ALL[3], k),
            MoverJobKind::Maintenance => assert_eq!(ALL[4], k),
            MoverJobKind::Verification => assert_eq!(ALL[5], k),
            MoverJobKind::Pin => assert_eq!(ALL[6], k),
            MoverJobKind::SnapshotDeleteBatch => assert_eq!(ALL[7], k),
            MoverJobKind::Discovery => assert_eq!(ALL[8], k),
            MoverJobKind::BrowseSession => assert_eq!(ALL[9], k),
        }
    }

    #[test]
    fn every_kind_is_covered_by_the_truth_tables() {
        // Compile-time: `ALL` really is every variant, in the stated order (a
        // new variant fails to compile in `assert_all_is_every_variant`; a
        // REORDERED or duplicated `ALL` fails the index assertions here).
        assert_eq!(ALL.len(), 10, "ALL and the destructure must stay in step");
        for k in ALL {
            assert_all_is_every_variant(*k);
        }
        // Run-time: each variant sits in exactly one partition, and
        // `counts_toward_repo_pool` agrees with that partition.
        assert_eq!(ALL.len(), IN_POOL.len() + OUT_OF_POOL.len());
        for k in ALL {
            assert!(
                IN_POOL.contains(k) ^ OUT_OF_POOL.contains(k),
                "{k:?} must be in exactly one partition"
            );
            assert_eq!(counts_toward_repo_pool(*k), IN_POOL.contains(k), "{k:?}");
        }
    }

    // --- presence: the four pooled job-build paths ------------------------

    #[test]
    fn backup_jobs_carry_the_pool_label_with_the_repo_label_value() {
        assert_eq!(
            repo_pool_label(MoverJobKind::Backup, &nas()),
            Some((REPO_POOL_LABEL.to_string(), repo_label(&nas()))),
        );
    }

    #[test]
    fn restore_jobs_carry_the_pool_label_with_the_repo_label_value() {
        assert_eq!(
            repo_pool_label(MoverJobKind::Restore, &nas()),
            Some((REPO_POOL_LABEL.to_string(), repo_label(&nas()))),
        );
    }

    #[test]
    fn repository_replication_jobs_carry_the_pool_label_with_the_repo_label_value() {
        assert_eq!(
            repo_pool_label(MoverJobKind::RepositoryReplication, &nas()),
            Some((REPO_POOL_LABEL.to_string(), repo_label(&nas()))),
        );
    }

    #[test]
    fn snapshot_replication_jobs_carry_the_pool_label_with_the_repo_label_value() {
        assert_eq!(
            repo_pool_label(MoverJobKind::SnapshotReplication, &nas()),
            Some((REPO_POOL_LABEL.to_string(), repo_label(&nas()))),
        );
    }

    #[test]
    fn every_pooled_kind_shares_one_value_so_one_selector_counts_the_pool() {
        // The point of the label: a backup, a restore and a replication against
        // the SAME repository must be countable with a single selector.
        let values: std::collections::BTreeSet<String> = IN_POOL
            .iter()
            .map(|k| repo_pool_label(*k, &nas()).expect("pooled").1)
            .collect();
        assert_eq!(values.len(), 1, "one repository ⇒ one pool value");
    }

    // --- absence: housekeeping / interactive Jobs stay out of the pool -----

    #[test]
    fn housekeeping_jobs_carry_no_pool_label_at_all() {
        // Regression pin: maintenance, verification, pin, the snapdel batch,
        // bootstrap/discovery and the browse session must not be countable in
        // the pool, and `None` keeps their Job metadata byte-identical.
        for kind in OUT_OF_POOL {
            assert_eq!(repo_pool_label(*kind, &nas()), None, "{kind:?}");
        }
    }

    // --- the normalization invariant --------------------------------------

    #[test]
    fn a_denormalized_ref_would_split_the_pool() {
        // Why every call site keys off the RESOLVED identity: the same
        // repository named without a namespace hashes to a DIFFERENT value.
        // This test documents the failure mode the callers avoid.
        let raw = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: None,
        };
        assert_ne!(
            repo_pool_label(MoverJobKind::Backup, &raw),
            repo_pool_label(MoverJobKind::Backup, &nas()),
        );
    }

    #[test]
    fn normalizing_a_namespaceless_spec_ref_rejoins_the_pool() {
        // The invariant the reconcilers rely on: a spec ref with
        // `namespace: None`, normalized against the owning CR's namespace,
        // produces the SAME pool value as the resolved/pinned identity.
        let raw = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: None,
        };
        let normalized = kopiur_api::common::normalized_repository_ref(&raw, "backups");
        assert_eq!(normalized, nas());
        assert_eq!(
            repo_pool_label(MoverJobKind::Backup, &normalized),
            repo_pool_label(MoverJobKind::Backup, &nas()),
        );
        assert_eq!(
            crate::naming::pinned_repo_key(&normalized),
            crate::naming::pinned_repo_key(&nas()),
        );
    }

    #[test]
    fn a_cluster_repository_pool_is_distinct_from_a_namespaced_one_of_the_same_name() {
        let cluster = RepositoryRef {
            kind: RepositoryKind::ClusterRepository,
            name: "nas".into(),
            namespace: None,
        };
        assert_ne!(
            repo_pool_label(MoverJobKind::Backup, &cluster),
            repo_pool_label(MoverJobKind::Backup, &nas()),
        );
    }

    #[test]
    fn two_namespaces_holding_a_repository_of_one_name_get_distinct_pools() {
        let other = RepositoryRef {
            namespace: Some("other".into()),
            ..nas()
        };
        assert_ne!(
            repo_pool_label(MoverJobKind::Backup, &other),
            repo_pool_label(MoverJobKind::Backup, &nas()),
        );
    }

    #[test]
    fn the_pool_label_value_is_a_valid_label_value() {
        // Kubernetes label values: <=63 chars, alphanumerics plus `-_.`, and
        // must start/end alphanumeric. A long repository name must not produce
        // a Job the apiserver rejects.
        let long = RepositoryRef {
            name: "n".repeat(120),
            ..nas()
        };
        let (_, value) = repo_pool_label(MoverJobKind::Backup, &long).expect("pooled");
        assert!(value.len() <= 63, "{} chars", value.len());
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        );
        assert!(value.starts_with(|c: char| c.is_ascii_alphanumeric()));
        assert!(value.ends_with(|c: char| c.is_ascii_alphanumeric()));
    }
}
