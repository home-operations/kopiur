//! Auto-adoption of discovered snapshots into identity-matching
//! `SnapshotPolicy`s (fixes #210).
//!
//! A `discovered` `Snapshot` row is an immortal catalog entry for a kopia
//! snapshot the operator did not produce. When such a row's kopia identity
//! EXACTLY matches a live `SnapshotPolicy`'s resolved identity, the policy
//! *adopts* it: a fresh `origin: Adopted` `Snapshot` is created in the policy's
//! namespace carrying the policy's config label, then the discovered row is
//! deleted. The adopted row is now GFS-governed like any produced backup — it
//! participates in `spec.retention` and is eventually pruned — instead of
//! sitting in the catalog forever.
//!
//! This module is the **pure decision layer** ([`adoption_candidates`],
//! [`plan_adoption`], [`build_adopted_snapshot`]); the kube LIST/create/delete
//! is the thin IO in [`crate::snapshot_policy`]. Every rule below is unit-tested
//! here.
//!
//! ## Safety invariants (each maps to a test)
//!
//! 1. **Cluster-wide LIST** (IO side): discovered candidates are LISTed via
//!    `crate::controllers::scoped_api` so a `ClusterRepository`'s rows in other
//!    namespaces are seen — a policy-namespaced LIST would silently miss them.
//! 2. **Structured-identity equality** ([`identities_match`]): username AND
//!    hostname AND `sourcePath` must all match exactly. NEVER via
//!    `identity_string` (which omits the path when `None`, while kopia rows
//!    always carry one — a false match).
//! 3. **Foreign-cluster hard refusal** ([`is_foreign_cluster`]): even on an
//!    exact identity match, a candidate whose hostname classifies
//!    [`HostClass::ForeignCluster`] under the repository's
//!    `identityDefaults.cluster` is refused (defense in depth).
//! 4. **Recreate, never relabel** (IO side): the adopted row is created FIRST,
//!    then the discovered row deleted — so a crash between the two is healed by
//!    the next wave, never losing the catalog entry.
//! 5. **No ownerReference** on adopted rows ([`build_adopted_snapshot`]) — a
//!    repository owner-ref plus `deletionPolicy: Delete` would turn `kubectl
//!    delete repository` into a GC-driven kopia-deletion wave.
//! 6. **Adoption AFTER retention** (IO side), consuming SEPARATE LISTs.
//! 7. **Batching**: at most [`POLICY_ADOPTION_BATCH`] adoptions per pass, in
//!    deterministic (snapshot-id) order.
//! 8. **Retention-aware under `Retain`/`Orphan`** ([`AdoptionRetentionGate`]):
//!    a candidate GFS would immediately prune is never adopted — the adopted
//!    CR would be deleted on the next retention pass while the kopia snapshot
//!    survives, be re-discovered by the next catalog scan, and be re-adopted:
//!    an adopt→prune→rediscover cycle that never converges and recycles the
//!    repository's discovery Job forever. Under `Delete` every candidate is
//!    adopted (the #210 drain: retention then genuinely prunes kopia data, so
//!    the pool empties).

use std::collections::{BTreeMap, BTreeSet};

use kube::ResourceExt;

use kopiur_api::common::{
    DeletionPolicy, PolicyRef, ResolvedIdentity, Retention, SnapshotAdoption,
};
use kopiur_api::retention::select_kept;
use kopiur_api::snapshot::{
    ResolvedSnapshot, SnapshotInfo, SnapshotSpec, SnapshotStats, SnapshotStatus, SnapshotTiming,
};
use kopiur_api::{
    HostClass, Origin, Snapshot, SnapshotPhase, SnapshotPolicy, classify_hostname, identity_string,
};

use crate::consts::{CONFIG_LABEL, ORIGIN_LABEL, REPOSITORY_UID_LABEL, SNAPSHOT_ID_LABEL};
use crate::snapshot_policy::SnapshotRetentionView;

/// At most this many discovered snapshots are adopted per reconcile pass. This
/// bound is what keeps the (unbatched) retention delete loop bounded per pass:
/// each pass re-LISTs and re-plans, so any remainder is picked up next pass.
pub const POLICY_ADOPTION_BATCH: usize = 50;

/// A discovered `Snapshot` row eligible for adoption, distilled to the fields the
/// planner and builder need. Extracted from a `discovered`-origin `Snapshot` CR
/// by [`adoption_candidates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionCandidate {
    /// Namespace the discovered row lives in (where the delete lands — may differ
    /// from the policy's namespace for a `ClusterRepository`).
    pub namespace: String,
    /// The discovered row's CR name.
    pub name: String,
    /// The kopia snapshot id this row represents (`status.snapshot.kopiaSnapshotID`).
    pub snapshot_id: String,
    /// The kopia identity recorded on the row (`status.snapshot.identity`).
    pub identity: ResolvedIdentity,
    /// The row's `status.timing`, carried onto the adopted row verbatim.
    pub timing: Option<SnapshotTiming>,
    /// The row's `status.stats`, carried onto the adopted row verbatim.
    pub stats: Option<SnapshotStats>,
    /// The row's `spec.pin`, carried onto the adopted row.
    pub pinned: bool,
    /// The row's decoded `kopiur-meta` metadata (`status.recorded`), carried
    /// onto the adopted row verbatim so adoption never drops recorded identity.
    pub recorded: Option<kopiur_api::RecordedSnapshotMeta>,
    /// The row's `status.snapshot.description`, carried onto the adopted row.
    pub description: Option<String>,
}

/// Extract adoption candidates from a `Snapshot` population: `discovered`-origin
/// rows of THIS repository (`repo_uid`) that are not terminating and carry a
/// resolvable kopia id + identity in status. Pure.
///
/// Takes borrows (`impl IntoIterator<Item = &Snapshot>`) so the reflector-store
/// path can feed it `Arc`-held rows without wholesale-cloning full objects
/// (#382 M3, C8) — only the candidate's distilled fields are cloned. The scope
/// is deliberately NOT namespace-filtered: discovered rows are matched across
/// every namespace the caller's population covers (install scope), matching
/// the live `origin=discovered,repository-uid=<uid>` LIST semantics.
///
/// SKIPs (returns nothing for):
/// - rows not labeled `origin: discovered` or not for `repo_uid`;
/// - **terminating** rows (`metadata.deletionTimestamp` set) — a row already
///   being expired must not be re-created under a new identity;
/// - rows missing `status.snapshot` (no id/identity to match on).
pub fn adoption_candidates<'a>(
    repo_uid: &str,
    rows: impl IntoIterator<Item = &'a Snapshot>,
) -> Vec<AdoptionCandidate> {
    rows.into_iter()
        .filter_map(|s| {
            let labels = s.labels();
            if labels.get(ORIGIN_LABEL).map(String::as_str)
                != Some(Origin::Discovered.label_value())
            {
                return None;
            }
            if labels.get(REPOSITORY_UID_LABEL).map(String::as_str) != Some(repo_uid) {
                return None;
            }
            // SKIP terminating rows — never re-create one that is being expired.
            if s.metadata.deletion_timestamp.is_some() {
                return None;
            }
            let status = s.status.as_ref()?;
            let info = status.snapshot.as_ref()?;
            if info.kopia_snapshot_id.is_empty() {
                return None;
            }
            Some(AdoptionCandidate {
                namespace: s.namespace().unwrap_or_default(),
                name: s.name_any(),
                snapshot_id: info.kopia_snapshot_id.clone(),
                identity: info.identity.clone(),
                timing: status.timing.clone(),
                stats: status.stats.clone(),
                pinned: s.spec.pin,
                recorded: status.recorded.clone(),
                description: info.description.clone(),
            })
        })
        .collect()
}

/// What one adoption pass decided: the rows to adopt (already filtered, sorted,
/// and capped) and whether to request an on-demand catalog scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionPlan {
    /// Candidates to adopt this pass (≤ [`POLICY_ADOPTION_BATCH`], snapshot-id order).
    pub adopt: Vec<AdoptionCandidate>,
    /// Whether to stamp the on-demand catalog-scan request on the repository.
    pub request_scan: bool,
    /// Identity-matching, non-owned candidates deliberately left `discovered`
    /// because [`AdoptionRetentionGate`] proved GFS would immediately prune
    /// them (inv. 8). Surfaced via `status.adoption.skippedByRetention`.
    pub skipped_by_retention: u64,
    /// The repository's catalog holds discovered snapshots, and NONE of them
    /// match this policy's identity (issue #380).
    ///
    /// Silent by construction otherwise, and dangerous after a disaster
    /// recovery: `spec.seed` brings a whole repository's history back, but the
    /// identity fork guards are Update-gated and cannot fire on a fresh-cluster
    /// CREATE — so a recovered `SnapshotPolicy` whose identity differs even
    /// slightly from the pre-disaster one adopts NOTHING while looking
    /// perfectly healthy, and starts a brand-new chain beside the history it
    /// was supposed to reclaim.
    ///
    /// Gated by the SAME once-per-(policy, identity) latch as the no-match scan
    /// request, so a policy that legitimately has no history to adopt warns
    /// once rather than on every reconcile.
    pub no_adoptable_history: bool,
}

/// What this policy already carries and has already asked for — the inputs to
/// the own-id filter and the once-per-identity scan-request arm.
pub struct AdoptionHistory<'a> {
    /// Kopia ids of the policy's existing config-labeled `Snapshot` children.
    pub own_snapshot_ids: &'a BTreeSet<String>,
    /// Whether the policy has ANY terminal-successful history.
    pub has_history: bool,
    /// `status.adoption.scanRequestedIdentity` — the identity a no-match scan
    /// was already requested for, so that arm fires once per (policy, identity).
    pub scan_requested_identity: Option<&'a str>,
}

/// The retention context [`plan_adoption`] needs to enforce invariant 8. Built
/// by `snapshot_policy::run_adoption` from the SAME pre-prune `backups` LIST
/// the retention pass just evaluated.
pub struct AdoptionRetentionGate<'a> {
    /// The policy's GFS retention (`spec.retention`). `None` = the policy never
    /// prunes, so adopting cannot loop — no gating.
    pub retention: Option<&'a Retention>,
    /// The `deletionPolicy` adopted rows will carry — resolve it with
    /// [`effective_deletion_policy`] so this gate and [`build_adopted_snapshot`]
    /// can never disagree on which convergence regime applies.
    pub deletion_policy: DeletionPolicy,
    /// Retention views of the policy's current config-labeled children.
    pub own_views: &'a [SnapshotRetentionView],
    /// The policy CR name — candidate views take the FUTURE adopted CR name
    /// ([`adopted_cr_name`]) as their retention id, so `select_kept`'s
    /// equal-`end_time` tie-break resolves identically at gate time and on the
    /// next retention pass.
    pub policy_name: &'a str,
}

/// Decide the adoption plan for one pass. **Pure** — exhaustive over
/// [`SnapshotAdoption`], no `_ =>`.
///
/// - [`SnapshotAdoption::Ignore`] → adopt nothing, request no scan.
/// - [`SnapshotAdoption::Adopt`] → adopt every candidate that (inv. 2) matches
///   the policy's live-resolved identity structurally AND (inv. 3) is not a
///   foreign-cluster hostname AND is not already carried by a config-label
///   `Snapshot` (`own_snapshot_ids`) AND (inv. 8) would survive the policy's
///   retention under an effective `Retain`/`Orphan` deletion policy (`gate`);
///   sorted by snapshot id and capped at [`POLICY_ADOPTION_BATCH`] (inv. 7).
///
/// `request_scan` is true when this pass adopted anything (a wave implies more
/// may exist behind the catalog's `retain` caps), OR — for a brand-new /
/// delete-then-recreated policy — when NO candidate matched at all, the policy
/// has no history yet, and no scan was already requested for THIS identity
/// (`scan_requested_identity`). That last arm fires exactly once per (policy,
/// identity): a scan that yields nothing stays quiet forever after. A wave
/// whose every candidate was withheld by the retention gate requests NO scan
/// (`adopt` is empty, and gated candidates count as `matched_any`) — this is
/// what lets the pathological adopt→prune→rediscover cycle terminate.
pub fn plan_adoption(
    mode: SnapshotAdoption,
    policy_identity: &ResolvedIdentity,
    repo_cluster: Option<&str>,
    candidates: Vec<AdoptionCandidate>,
    history: &AdoptionHistory<'_>,
    gate: &AdoptionRetentionGate<'_>,
) -> AdoptionPlan {
    match mode {
        SnapshotAdoption::Ignore => AdoptionPlan {
            adopt: Vec::new(),
            request_scan: false,
            skipped_by_retention: 0,
            no_adoptable_history: false,
        },
        SnapshotAdoption::Adopt => {
            // Identity-matching, non-foreign candidates (inv. 2 + 3). `matched_any`
            // is computed BEFORE the own-id filter: a candidate that matched but is
            // already ours means there IS relevant history, so it must not trigger a
            // "nothing matched" scan request. The retention gate (inv. 8) runs after
            // BOTH — a retention-withheld candidate is still "history exists".
            // Whether the repository's catalog held ANY discovered candidate at
            // all, captured before the filters consume it — the difference
            // between "nothing to adopt" and "history exists but none of it is
            // yours", which is the whole point of the #380 warning below.
            let catalog_non_empty = !candidates.is_empty();
            let mut matched: Vec<AdoptionCandidate> = candidates
                .into_iter()
                .filter(|c| {
                    identities_match(policy_identity, &c.identity)
                        && !is_foreign_cluster(&c.identity.hostname, repo_cluster)
                })
                .collect();
            let matched_any = !matched.is_empty();
            matched.retain(|c| !history.own_snapshot_ids.contains(&c.snapshot_id));
            let skipped_by_retention = apply_retention_gate(&mut matched, gate);
            // Deterministic order (inv. 7) so a capped pass always adopts the same
            // prefix, and the batch cap.
            matched.sort_by(|a, b| a.snapshot_id.cmp(&b.snapshot_id));
            matched.truncate(POLICY_ADOPTION_BATCH);
            let adopt = matched;

            let already_requested =
                history.scan_requested_identity == Some(identity_string(policy_identity).as_str());
            let no_match_first_time = !matched_any && !history.has_history && !already_requested;
            let request_scan = !adopt.is_empty() || no_match_first_time;
            AdoptionPlan {
                adopt,
                request_scan,
                skipped_by_retention,
                // A catalog that is EMPTY says nothing (the scan may not have
                // run yet) — only a populated one none of whose entries match
                // is evidence of an identity mismatch.
                no_adoptable_history: no_match_first_time && catalog_non_empty,
            }
        }
    }
}

/// Invariant 8: withhold every candidate GFS would immediately prune when the
/// adopted row's `deletionPolicy` would be `Retain`/`Orphan` (a CR-only prune —
/// the kopia snapshot survives, re-discovers, and re-adopts forever). Under
/// `Delete` the adopt-then-prune cycle performs real kopia deletion and
/// converges, so nothing is withheld. Exhaustive over [`DeletionPolicy`].
/// Returns how many candidates were withheld.
///
/// The gate evaluates `select_kept` over own views ∪ candidate views. This
/// pre-prune evaluation is stable across passes because (a) `select_kept` is
/// time-invariant (buckets derive purely from end times — no `now`), (b)
/// removing a non-kept row never changes any other row's bucket outcome, and
/// (c) equal-`end_time` ties break by id, and candidate views use the future
/// adopted CR name — so the gate and the next retention pass resolve ties
/// identically.
fn apply_retention_gate(
    matched: &mut Vec<AdoptionCandidate>,
    gate: &AdoptionRetentionGate<'_>,
) -> u64 {
    let retention = match gate.deletion_policy {
        DeletionPolicy::Delete => return 0,
        DeletionPolicy::Retain | DeletionPolicy::Orphan => match gate.retention {
            None => return 0,
            Some(r) => r,
        },
    };
    let before = matched.len();
    let mut views: Vec<SnapshotRetentionView> = gate.own_views.to_vec();
    views.extend(
        matched
            .iter()
            .filter_map(|c| candidate_view(gate.policy_name, c)),
    );
    let kept: BTreeSet<String> = select_kept(&views, retention).keep.into_iter().collect();
    matched.retain(|c| kept.contains(&adopted_cr_name(gate.policy_name, &c.snapshot_id)));
    (before - matched.len()) as u64
}

/// The retention view the adopted CR for `candidate` WOULD have: id = the
/// future adopted CR name, end time from the discovered row's timing, pin
/// carried. A candidate with no parseable end time yields `None` and is thus
/// withheld by the gate — its kept-ness is unprovable, so it stays a truthful
/// discovered row. (Defensive: the catalog always writes timing on discovered
/// rows.)
fn candidate_view(
    policy_name: &str,
    candidate: &AdoptionCandidate,
) -> Option<SnapshotRetentionView> {
    let end = candidate.timing.as_ref()?.end_time.as_deref()?;
    let end_time = chrono::DateTime::parse_from_rfc3339(end)
        .ok()?
        .with_timezone(&chrono::Utc);
    Some(SnapshotRetentionView {
        name: adopted_cr_name(policy_name, &candidate.snapshot_id),
        end_time,
        pinned: candidate.pinned,
    })
}

/// Structured identity equality (inv. 2): username AND hostname AND `sourcePath`
/// all equal. Deliberately field-by-field, NOT [`identity_string`] — the string
/// form drops the path when it is `None`, so `alice@host` would spuriously match
/// `alice@host:/data` (kopia rows always carry a path).
fn identities_match(policy: &ResolvedIdentity, candidate: &ResolvedIdentity) -> bool {
    policy.username == candidate.username
        && policy.hostname == candidate.hostname
        && policy.source_path == candidate.source_path
}

/// Whether a hostname is another cluster's under this repository's
/// `identityDefaults.cluster` (inv. 3). Only [`HostClass::ForeignCluster`]
/// refuses; `Bare`/`OwnCluster` do not.
fn is_foreign_cluster(hostname: &str, cluster: Option<&str>) -> bool {
    matches!(
        classify_hostname(hostname, cluster),
        HostClass::ForeignCluster { .. }
    )
}

/// Build the adopted `Snapshot` (spec + metadata) and its `status` for a
/// candidate, in the `policy`'s namespace. **Pure.** The status is returned
/// separately so the caller creates the CR then PATCHes the status subresource.
///
/// - **Name**: `<policy>-adopted-<first16(snapshot_id)>-<hash8(snapshot_id)>`,
///   length-capped by [`crate::naming::capped_name`]. The first-16 prefix is
///   kept for human correlation with the kopia id; the trailing
///   [`crate::naming::short_hash`] of the FULL id makes the name
///   collision-free even when two ids in the same policy share a ≥16-char
///   prefix (two ids differing only after char 16 previously minted the SAME
///   name — see the `adopted_row_matches_candidate` name-collision guard in
///   `snapshot_policy.rs`, which this fix makes unreachable for that case).
/// - **Labels**: mirror the discovered-row set — `origin: adopted`,
///   `SNAPSHOT_ID_LABEL`, `REPOSITORY_UID_LABEL` — plus `CONFIG_LABEL` (the
///   policy name) so retention governs it.
/// - **Spec**: `policyRef` = the policy; `deletionPolicy` =
///   `spec.defaultDeletionPolicy` (else `Delete`); `pin` carried from the
///   candidate; NO `onScheduleDelete`.
/// - **Meta**: NO ownerReferences (inv. 5).
/// - **Status**: `phase: Succeeded`, `origin: Adopted`, the kopia id + identity,
///   timing + stats verbatim, and `resolved.repository` pinned via
///   [`crate::snapshot::pinned_repository_ref`] — the pin that makes deletion,
///   batching, and `produced_ids_for` attribute the row to its repository.
pub fn build_adopted_snapshot(
    policy: &SnapshotPolicy,
    repo_uid: &str,
    candidate: &AdoptionCandidate,
    repo_ref: &kopiur_api::common::RepositoryRef,
) -> (Snapshot, SnapshotStatus) {
    let policy_name = policy.name_any();
    let namespace = policy.namespace().unwrap_or_default();
    let cr_name = adopted_cr_name(&policy_name, &candidate.snapshot_id);

    let mut labels = BTreeMap::new();
    labels.insert(
        ORIGIN_LABEL.to_string(),
        Origin::Adopted.label_value().to_string(),
    );
    labels.insert(CONFIG_LABEL.to_string(), policy_name.clone());
    labels.insert(SNAPSHOT_ID_LABEL.to_string(), candidate.snapshot_id.clone());
    labels.insert(REPOSITORY_UID_LABEL.to_string(), repo_uid.to_string());

    let deletion_policy = effective_deletion_policy(policy);

    // `repo_ref` is threaded by the caller (pure fn, no error path here): the
    // repository the row was DISCOVERED in. Normalized once and used for both
    // pins below.
    let pinned_repo = crate::snapshot::pinned_repository_ref(repo_ref, &namespace);
    // The mint-time spec pin (#368): stamped only when the policy is
    // multi-repo — the retention buckets key on it, and the row must land in
    // the bucket of the repository it actually lives in. Single-repo adopted
    // rows stay unpinned, byte-identical to the pre-multi-repo wire.
    let spec_pin = kopiur_api::is_multi_repo(&policy.spec).then(|| pinned_repo.clone());

    let mut snapshot = Snapshot::new(
        &cr_name,
        SnapshotSpec {
            repository: spec_pin,
            source: None,
            policy_ref: Some(PolicyRef {
                name: policy_name.clone(),
                namespace: None,
            }),
            tags: None,
            failure_policy: None,
            deletion_policy: Some(deletion_policy),
            // Adopted rows have no owning schedule.
            on_schedule_delete: None,
            pin: candidate.pinned,
            description: None,
        },
    );
    // NO ownerReferences (inv. 5).
    snapshot.metadata = crate::io::child_meta(&cr_name, &namespace, labels, None);

    let status = SnapshotStatus {
        phase: Some(SnapshotPhase::Succeeded),
        origin: Some(Origin::Adopted),
        snapshot: Some(SnapshotInfo {
            kopia_snapshot_id: candidate.snapshot_id.clone(),
            identity: candidate.identity.clone(),
            description: candidate.description.clone(),
        }),
        timing: candidate.timing.clone(),
        stats: candidate.stats.clone(),
        recorded: candidate.recorded.clone(),
        resolved: Some(ResolvedSnapshot {
            repository: Some(pinned_repo),
            sources: Vec::new(),
            // Pin the recipe's projection opt-in like a produced row, so the
            // deletion path re-projects credentials correctly (never reads the
            // adopted row as "predates the pin").
            credential_projection: Some(crate::snapshot::projection_to_pin(policy)),
        }),
        ..Default::default()
    };

    (snapshot, status)
}

/// The name the adopted `Snapshot` CR for `snapshot_id` gets:
/// `<policy>-adopted-<first16(id)>-<hash8(full id)>`, length-capped. The
/// first-16 prefix keeps human correlation with the kopia id; the trailing
/// hash of the FULL id keeps names distinct for ids sharing a ≥16-char prefix.
/// Shared by [`build_adopted_snapshot`] and the retention gate's candidate
/// views (inv. 8 tie-break determinism) — keep them on this one helper.
pub fn adopted_cr_name(policy_name: &str, snapshot_id: &str) -> String {
    let short: String = snapshot_id.chars().take(16).collect();
    let hash = crate::naming::short_hash(snapshot_id);
    crate::naming::capped_name(&format!("{policy_name}-adopted-{short}-{hash}"))
}

/// The `deletionPolicy` an adopted row will carry: the policy's
/// `spec.defaultDeletionPolicy`, else `Delete`. One resolver shared by
/// [`build_adopted_snapshot`] and [`AdoptionRetentionGate::deletion_policy`]
/// so the builder and the gate can never disagree on which convergence regime
/// (inv. 8) applies.
pub fn effective_deletion_policy(policy: &SnapshotPolicy) -> DeletionPolicy {
    policy
        .spec
        .default_deletion_policy
        .unwrap_or(DeletionPolicy::Delete)
}

/// The `AdoptionSkippedByRetention` Normal-Event note (inv. 8). **Pure** so the
/// required contents are unit-asserted: the count, the identity, why the rows
/// stay discovered, and every lever that changes the outcome.
pub fn adoption_skipped_event_message(count: u64, identity: &str) -> String {
    format!(
        "Left {count} discovered snapshot(s) matching identity {identity} unadopted: \
         spec.retention would prune them immediately, and this policy's effective \
         deletionPolicy (Retain/Orphan) deletes only the Snapshot CR — the kopia snapshot \
         would be re-discovered and re-adopted in an endless loop. They remain in the \
         catalog as discovered rows. To change this: widen spec.retention, set \
         spec.defaultDeletionPolicy: Delete (adopted rows then genuinely prune kopia \
         data), pin a snapshot via spec.pin on its discovered row, or set spec.adoption: \
         Ignore."
    )
}

/// The `SnapshotsAdopted` Normal-Event note. **Pure** so the required contents
/// are unit-asserted: the count, the identity, that the rows are now GFS-governed
/// and WILL be pruned per `spec.retention`, and BOTH opt-outs.
pub fn adoption_event_message(count: u64, identity: &str) -> String {
    format!(
        "Adopted {count} discovered snapshot(s) matching identity {identity} into this \
         SnapshotPolicy. They are now governed by GFS retention (spec.retention) and WILL be \
         pruned like any snapshot this policy produces. To opt out of automatic adoption, set \
         spec.adoption: Ignore on this SnapshotPolicy, or spec.catalog.adoption: Ignore on the \
         referenced repository."
    )
}

/// The `NoAdoptableHistory` Warning Event message (#380): the repository holds
/// discovered snapshots, and none of them belong to this policy's identity.
///
/// Pure so the exact text is asserted, and volatile-free so a repeat is
/// byte-identical. Names what was found, why it matters, and the two things
/// that actually fix it.
pub fn no_adoptable_history_message(identity: &str, repository: &str) -> String {
    format!(
        "Repository {repository} holds discovered snapshots, but NONE of them match this \
         SnapshotPolicy's identity {identity}, so there is nothing for it to adopt. After a \
         disaster recovery (spec.seed) this almost always means the recovered identity does not \
         match the one that wrote the history — kopiur's identity-fork guards are update-gated \
         and cannot fire on a freshly-created SnapshotPolicy, so the mismatch is otherwise \
         silent, and this policy will start a NEW backup chain beside history you could have \
         reclaimed. Fix: compare this policy's identity (spec.identity and the repository's \
         identityDefaults) with the pre-disaster configuration byte for byte, and re-apply it \
         once they match. If the mismatch is intentional, set spec.adoption: Ignore to silence \
         this."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::common::RepositoryKind;
    use kopiur_api::snapshot_policy::SnapshotPolicySpec;

    /// An all-default (empty) `SnapshotSpec` (the type derives no `Default`).
    fn empty_spec() -> SnapshotSpec {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }

    fn identity(user: &str, host: &str, path: Option<&str>) -> ResolvedIdentity {
        ResolvedIdentity {
            username: user.into(),
            hostname: host.into(),
            source_path: path.map(String::from),
        }
    }

    /// A discovered `Snapshot` CR fixture: labels + status carrying id/identity.
    fn discovered_row(repo_uid: &str, name: &str, id: &str, ident: ResolvedIdentity) -> Snapshot {
        let mut labels = BTreeMap::new();
        labels.insert(ORIGIN_LABEL.to_string(), "discovered".to_string());
        labels.insert(REPOSITORY_UID_LABEL.to_string(), repo_uid.to_string());
        labels.insert(SNAPSHOT_ID_LABEL.to_string(), id.to_string());
        let mut s = Snapshot::new(name, empty_spec());
        s.metadata.namespace = Some("disc-ns".to_string());
        s.metadata.labels = Some(labels);
        s.status = Some(SnapshotStatus {
            phase: Some(SnapshotPhase::Discovered),
            origin: Some(Origin::Discovered),
            snapshot: Some(SnapshotInfo {
                kopia_snapshot_id: id.to_string(),
                identity: ident,
                description: None,
            }),
            timing: Some(SnapshotTiming {
                start_time: Some("2026-01-01T00:00:00Z".into()),
                end_time: Some("2026-01-01T00:01:00Z".into()),
                duration_seconds: Some(60),
            }),
            stats: Some(SnapshotStats {
                size_bytes: Some(1234),
                ..Default::default()
            }),
            ..Default::default()
        });
        s
    }

    fn candidate(id: &str, ident: ResolvedIdentity, pinned: bool) -> AdoptionCandidate {
        AdoptionCandidate {
            namespace: "disc-ns".into(),
            name: format!("row-{id}"),
            snapshot_id: id.into(),
            identity: ident,
            timing: None,
            stats: None,
            pinned,
            recorded: None,
            description: None,
        }
    }

    /// A Delete/no-retention gate — the regime in which invariant 8 never
    /// withholds anything, so the legacy truth tables hold unchanged.
    fn delete_gate() -> AdoptionRetentionGate<'static> {
        AdoptionRetentionGate {
            retention: None,
            deletion_policy: DeletionPolicy::Delete,
            own_views: &[],
            policy_name: "pol",
        }
    }

    fn adopt_plan(
        policy_identity: &ResolvedIdentity,
        repo_cluster: Option<&str>,
        candidates: Vec<AdoptionCandidate>,
        own: &BTreeSet<String>,
        has_history: bool,
        requested: Option<&str>,
    ) -> AdoptionPlan {
        plan_adoption(
            SnapshotAdoption::Adopt,
            policy_identity,
            repo_cluster,
            candidates,
            &AdoptionHistory {
                own_snapshot_ids: own,
                has_history,
                scan_requested_identity: requested,
            },
            &delete_gate(),
        )
    }

    // -- adoption_candidates extraction --------------------------------------

    #[test]
    fn candidates_extract_matching_rows_and_skip_the_rest() {
        let ident = identity("app", "billing", Some("/data"));
        let mut rows = vec![
            discovered_row("repo-1", "keep", "aaa", ident.clone()),
            // wrong repo uid
            discovered_row("repo-2", "wrong-repo", "bbb", ident.clone()),
        ];
        // Not a discovered row (origin label scheduled).
        let mut scheduled = discovered_row("repo-1", "scheduled", "ccc", ident.clone());
        scheduled
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .insert(ORIGIN_LABEL.to_string(), "scheduled".to_string());
        rows.push(scheduled);
        // Terminating row — skipped.
        let mut terminating = discovered_row("repo-1", "terminating", "ddd", ident.clone());
        terminating.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
            ));
        rows.push(terminating);
        // Missing status.snapshot — skipped.
        let mut no_status = discovered_row("repo-1", "no-status", "eee", ident.clone());
        no_status.status.as_mut().unwrap().snapshot = None;
        rows.push(no_status);

        let got = adoption_candidates("repo-1", &rows);
        assert_eq!(
            got.len(),
            1,
            "only the one matching, live, status-carrying row"
        );
        assert_eq!(got[0].snapshot_id, "aaa");
        assert_eq!(got[0].namespace, "disc-ns");
        assert_eq!(got[0].identity, ident);
    }

    #[test]
    fn candidates_are_install_scope_wide_and_borrow_store_rows() {
        // #382 M3: the reflector-store path replaces an INSTALL-SCOPE-WIDE
        // live LIST, so — unlike the namespaced child populations (C4) — a
        // discovered row in ANY namespace is a candidate; only the two label
        // gates scope the set. The extractor takes borrowed rows (here: refs
        // into Arc-held storage) so full objects are never wholesale-cloned.
        let ident = identity("app", "billing", Some("/data"));
        let mut other_ns = discovered_row("repo-1", "elsewhere", "bbb", ident.clone());
        other_ns.metadata.namespace = Some("another-ns".to_string());
        let arcs: Vec<std::sync::Arc<Snapshot>> = vec![
            std::sync::Arc::new(discovered_row("repo-1", "here", "aaa", ident.clone())),
            std::sync::Arc::new(other_ns),
            std::sync::Arc::new(discovered_row("repo-2", "wrong-repo", "ccc", ident)),
        ];

        let mut got: Vec<(String, String)> =
            adoption_candidates("repo-1", arcs.iter().map(std::sync::Arc::as_ref))
                .into_iter()
                .map(|c| (c.namespace, c.name))
                .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("another-ns".to_string(), "elsewhere".to_string()),
                ("disc-ns".to_string(), "here".to_string()),
            ],
            "both namespaces' discovered rows count; the other repo's never does"
        );
    }

    #[test]
    fn candidates_carry_the_pin_flag() {
        let ident = identity("app", "billing", Some("/data"));
        let mut row = discovered_row("repo-1", "pinned", "aaa", ident);
        row.spec.pin = true;
        let got = adoption_candidates("repo-1", &[row]);
        assert_eq!(got.len(), 1);
        assert!(got[0].pinned, "spec.pin is carried onto the candidate");
    }

    #[test]
    fn candidates_carry_recorded_meta_and_description() {
        // A discovered row whose scan decoded kopiur-meta (+ description) must
        // not lose either through adoption.
        let ident = identity("app", "billing", Some("/data"));
        let mut row = discovered_row("repo-1", "meta", "aaa", ident);
        let status = row.status.as_mut().unwrap();
        status.recorded = Some(kopiur_api::RecordedSnapshotMeta {
            schema: 1,
            src: kopiur_api::RecordedSrc::Inherited,
            uid: Some(1000),
            gid: Some(1000),
            fs_group: Some(1000),
        });
        status.snapshot.as_mut().unwrap().description = Some("from the repo".into());
        let got = adoption_candidates("repo-1", &[row]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].recorded.as_ref().unwrap().uid, Some(1000));
        assert_eq!(got[0].description.as_deref(), Some("from the repo"));
    }

    // -- plan_adoption: matching + refusals ----------------------------------

    #[test]
    fn exact_identity_match_is_adopted() {
        let id = identity("app", "billing", Some("/data"));
        let plan = adopt_plan(
            &id,
            None,
            vec![candidate("aaa", id.clone(), false)],
            &BTreeSet::new(),
            false,
            None,
        );
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].snapshot_id, "aaa");
        assert!(
            plan.request_scan,
            "a non-empty adopt always requests a rescan"
        );
    }

    #[test]
    fn near_miss_on_username_hostname_or_path_is_not_adopted() {
        let policy = identity("app", "billing", Some("/data"));
        let cands = vec![
            candidate("u", identity("app-2", "billing", Some("/data")), false),
            candidate("h", identity("app", "billing-2", Some("/data")), false),
            candidate("p", identity("app", "billing", Some("/other")), false),
            // path present vs absent must NOT match (the identity_string trap).
            candidate("none", identity("app", "billing", None), false),
        ];
        let plan = adopt_plan(&policy, None, cands, &BTreeSet::new(), false, None);
        assert!(plan.adopt.is_empty(), "no near-miss is adopted");
    }

    #[test]
    fn foreign_cluster_hostname_is_refused_even_on_exact_match() {
        // The candidate's identity is byte-identical to the policy's, but the
        // hostname classifies ForeignCluster under the repo's cluster — refused.
        let policy = identity("app", "prod.west", Some("/data"));
        let cand = candidate("aaa", identity("app", "prod.west", Some("/data")), false);
        let plan = adopt_plan(
            &policy,
            Some("east"),
            vec![cand],
            &BTreeSet::new(),
            false,
            None,
        );
        assert!(
            plan.adopt.is_empty(),
            "an exact match that is another cluster's is still refused"
        );
        // With no cluster identity the same host is Bare → adopted.
        let plan = adopt_plan(
            &policy,
            None,
            vec![candidate(
                "aaa",
                identity("app", "prod.west", Some("/data")),
                false,
            )],
            &BTreeSet::new(),
            false,
            None,
        );
        assert_eq!(
            plan.adopt.len(),
            1,
            "no cluster mode: host is bare, adopted"
        );
    }

    #[test]
    fn already_carried_id_is_skipped() {
        let id = identity("app", "billing", Some("/data"));
        let own: BTreeSet<String> = ["aaa".to_string()].into_iter().collect();
        let plan = adopt_plan(
            &id,
            None,
            vec![candidate("aaa", id.clone(), false)],
            &own,
            true,
            None,
        );
        assert!(
            plan.adopt.is_empty(),
            "an already-owned kopia id is not re-adopted"
        );
        assert!(
            !plan.request_scan,
            "a matched-but-owned candidate is not 'nothing matched', and history exists"
        );
    }

    #[test]
    fn pinned_candidate_is_carried_into_the_adopt_set() {
        let id = identity("app", "billing", Some("/data"));
        let plan = adopt_plan(
            &id,
            None,
            vec![candidate("aaa", id.clone(), true)],
            &BTreeSet::new(),
            false,
            None,
        );
        assert!(plan.adopt[0].pinned);
    }

    #[test]
    fn ignore_mode_is_inert() {
        let id = identity("app", "billing", Some("/data"));
        let plan = plan_adoption(
            SnapshotAdoption::Ignore,
            &id,
            None,
            vec![candidate("aaa", id.clone(), false)],
            &AdoptionHistory {
                own_snapshot_ids: &BTreeSet::new(),
                has_history: false,
                scan_requested_identity: None,
            },
            &delete_gate(),
        );
        assert!(plan.adopt.is_empty());
        assert!(!plan.request_scan);
    }

    #[test]
    fn empty_candidates_with_history_adopts_nothing_and_stays_quiet() {
        let id = identity("app", "billing", Some("/data"));
        let plan = adopt_plan(&id, None, vec![], &BTreeSet::new(), true, None);
        assert!(plan.adopt.is_empty());
        assert!(!plan.request_scan);
    }

    // -- request_scan truth table --------------------------------------------

    #[test]
    fn request_scan_truth_table() {
        let id = identity("app", "billing", Some("/data"));
        let none: Vec<AdoptionCandidate> = vec![];

        // no history + never requested → true (ask exactly once).
        assert!(adopt_plan(&id, None, none.clone(), &BTreeSet::new(), false, None).request_scan);
        // no history + already requested for THIS identity → false.
        assert!(
            !adopt_plan(
                &id,
                None,
                none.clone(),
                &BTreeSet::new(),
                false,
                Some(&identity_string(&id))
            )
            .request_scan
        );
        // has history + no matches → false.
        assert!(!adopt_plan(&id, None, none.clone(), &BTreeSet::new(), true, None).request_scan);
        // non-empty adopt → true (regardless of history/requested).
        assert!(
            adopt_plan(
                &id,
                None,
                vec![candidate("aaa", id.clone(), false)],
                &BTreeSet::new(),
                true,
                Some(&identity_string(&id))
            )
            .request_scan
        );
        // A DIFFERENT identity's prior request does not suppress this one.
        assert!(
            adopt_plan(
                &id,
                None,
                none,
                &BTreeSet::new(),
                false,
                Some("other@host:/x")
            )
            .request_scan
        );
    }

    // -- batching + deterministic order --------------------------------------

    #[test]
    fn batch_cap_and_deterministic_order() {
        let id = identity("app", "billing", Some("/data"));
        // 60 matching candidates, ids "id00".."id59" inserted in reverse.
        let mut cands: Vec<AdoptionCandidate> = (0..60)
            .rev()
            .map(|i| candidate(&format!("id{i:02}"), id.clone(), false))
            .collect();
        // Shuffle-ish: also push a duplicate-order perturbation.
        cands.rotate_left(7);
        let plan = adopt_plan(&id, None, cands, &BTreeSet::new(), false, None);
        assert_eq!(plan.adopt.len(), POLICY_ADOPTION_BATCH, "capped at 50");
        // Sorted ascending by snapshot id, so the first 50 are id00..id49.
        let ids: Vec<&str> = plan.adopt.iter().map(|c| c.snapshot_id.as_str()).collect();
        let want: Vec<String> = (0..POLICY_ADOPTION_BATCH)
            .map(|i| format!("id{i:02}"))
            .collect();
        assert_eq!(ids, want.iter().map(String::as_str).collect::<Vec<_>>());
    }

    // -- retention gate (inv. 8) ---------------------------------------------

    fn timed_candidate(
        id: &str,
        ident: ResolvedIdentity,
        end: &str,
        pinned: bool,
    ) -> AdoptionCandidate {
        let mut c = candidate(id, ident, pinned);
        c.timing = Some(SnapshotTiming {
            start_time: None,
            end_time: Some(end.into()),
            duration_seconds: None,
        });
        c
    }

    fn own_view(name: &str, end: &str, pinned: bool) -> SnapshotRetentionView {
        SnapshotRetentionView {
            name: name.into(),
            end_time: chrono::DateTime::parse_from_rfc3339(end)
                .unwrap()
                .with_timezone(&chrono::Utc),
            pinned,
        }
    }

    fn retention_of(v: serde_json::Value) -> Retention {
        serde_json::from_value(v).unwrap()
    }

    fn retain_gate<'a>(
        retention: Option<&'a Retention>,
        own_views: &'a [SnapshotRetentionView],
    ) -> AdoptionRetentionGate<'a> {
        AdoptionRetentionGate {
            retention,
            deletion_policy: DeletionPolicy::Retain,
            own_views,
            policy_name: "pol",
        }
    }

    fn gated_plan(
        policy_identity: &ResolvedIdentity,
        candidates: Vec<AdoptionCandidate>,
        own: &BTreeSet<String>,
        has_history: bool,
        gate: &AdoptionRetentionGate<'_>,
    ) -> AdoptionPlan {
        plan_adoption(
            SnapshotAdoption::Adopt,
            policy_identity,
            None,
            candidates,
            &AdoptionHistory {
                own_snapshot_ids: own,
                has_history,
                scan_requested_identity: None,
            },
            gate,
        )
    }

    fn adopt_ids(plan: &AdoptionPlan) -> Vec<&str> {
        plan.adopt.iter().map(|c| c.snapshot_id.as_str()).collect()
    }

    #[test]
    fn retain_gate_skips_candidates_gfs_would_immediately_prune() {
        let id = identity("app", "billing", Some("/data"));
        let cands = vec![
            timed_candidate("ccc", id.clone(), "2026-01-01T00:00:00Z", false),
            timed_candidate("aaa", id.clone(), "2026-01-03T00:00:00Z", false),
            timed_candidate("bbb", id.clone(), "2026-01-02T00:00:00Z", false),
        ];
        let ret = retention_of(serde_json::json!({ "keepLatest": 1 }));
        let gate = retain_gate(Some(&ret), &[]);
        let plan = gated_plan(&id, cands, &BTreeSet::new(), false, &gate);
        assert_eq!(adopt_ids(&plan), vec!["aaa"], "only the GFS-kept newest");
        assert_eq!(plan.skipped_by_retention, 2);
        assert!(
            plan.request_scan,
            "a non-empty wave still drains the catalog"
        );
    }

    #[test]
    fn retain_gate_all_skipped_requests_no_scan_and_terminates_the_loop() {
        // THE loop-termination pin: every candidate is older than the own row
        // keepLatest:1 keeps, so the whole wave is withheld — and the plan must
        // request NO scan even with `has_history: false` and no prior request
        // (withheld candidates count as matched history). On pre-fix code this
        // input re-stamped a scan token every pass, recycling the discovery Job
        // seconds after each completion, forever.
        let id = identity("app", "billing", Some("/data"));
        let cands = vec![
            timed_candidate("old1", id.clone(), "2026-01-01T00:00:00Z", false),
            timed_candidate("old2", id.clone(), "2026-01-02T00:00:00Z", false),
        ];
        let own = [own_view("pol-live", "2026-01-10T00:00:00Z", false)];
        let ret = retention_of(serde_json::json!({ "keepLatest": 1 }));
        let gate = retain_gate(Some(&ret), &own);
        let plan = gated_plan(&id, cands, &BTreeSet::new(), false, &gate);
        assert!(plan.adopt.is_empty());
        assert_eq!(plan.skipped_by_retention, 2);
        assert!(
            !plan.request_scan,
            "a fully-withheld wave must go quiet, or the scan/adopt loop never converges"
        );
    }

    #[test]
    fn delete_mode_adopts_everything_regardless_of_retention() {
        // The #210 drain design: under Delete, adopt-then-prune performs real
        // kopia deletion, so the gate must not withhold anything.
        let id = identity("app", "billing", Some("/data"));
        let cands = vec![
            timed_candidate("aaa", id.clone(), "2026-01-03T00:00:00Z", false),
            timed_candidate("bbb", id.clone(), "2026-01-02T00:00:00Z", false),
            timed_candidate("ccc", id.clone(), "2026-01-01T00:00:00Z", false),
        ];
        let ret = retention_of(serde_json::json!({ "keepLatest": 1 }));
        let gate = AdoptionRetentionGate {
            retention: Some(&ret),
            deletion_policy: DeletionPolicy::Delete,
            own_views: &[],
            policy_name: "pol",
        };
        let plan = gated_plan(&id, cands, &BTreeSet::new(), false, &gate);
        assert_eq!(plan.adopt.len(), 3);
        assert_eq!(plan.skipped_by_retention, 0);
    }

    #[test]
    fn orphan_mode_gates_like_retain() {
        // Orphan also deletes only the CR (no repository contact), so adopting
        // a would-be-pruned candidate is the same non-converging no-op.
        let id = identity("app", "billing", Some("/data"));
        let cands = vec![
            timed_candidate("aaa", id.clone(), "2026-01-03T00:00:00Z", false),
            timed_candidate("bbb", id.clone(), "2026-01-02T00:00:00Z", false),
        ];
        let ret = retention_of(serde_json::json!({ "keepLatest": 1 }));
        let gate = AdoptionRetentionGate {
            retention: Some(&ret),
            deletion_policy: DeletionPolicy::Orphan,
            own_views: &[],
            policy_name: "pol",
        };
        let plan = gated_plan(&id, cands, &BTreeSet::new(), false, &gate);
        assert_eq!(adopt_ids(&plan), vec!["aaa"]);
        assert_eq!(plan.skipped_by_retention, 1);
    }

    #[test]
    fn no_retention_configured_adopts_everything_under_retain() {
        // retention: None = the policy never prunes = adopting cannot loop.
        let id = identity("app", "billing", Some("/data"));
        let cands = vec![
            timed_candidate("aaa", id.clone(), "2026-01-03T00:00:00Z", false),
            timed_candidate("bbb", id.clone(), "2026-01-02T00:00:00Z", false),
        ];
        let gate = retain_gate(None, &[]);
        let plan = gated_plan(&id, cands, &BTreeSet::new(), false, &gate);
        assert_eq!(plan.adopt.len(), 2);
        assert_eq!(plan.skipped_by_retention, 0);
    }

    #[test]
    fn empty_retention_object_skips_all_unpinned_candidates_under_retain() {
        // `retention: {}` keeps nothing, so every unpinned candidate would be
        // pruned instantly — the correct terminal state is: none adopted, all
        // stay discovered. Pinned candidates are GFS-exempt and still adopt.
        let id = identity("app", "billing", Some("/data"));
        let cands = vec![
            timed_candidate("aaa", id.clone(), "2026-01-03T00:00:00Z", false),
            timed_candidate("bbb", id.clone(), "2026-01-02T00:00:00Z", false),
            timed_candidate("pin", id.clone(), "2026-01-01T00:00:00Z", true),
        ];
        let ret = retention_of(serde_json::json!({}));
        let gate = retain_gate(Some(&ret), &[]);
        let plan = gated_plan(&id, cands, &BTreeSet::new(), false, &gate);
        assert_eq!(adopt_ids(&plan), vec!["pin"]);
        assert_eq!(plan.skipped_by_retention, 2);
    }

    #[test]
    fn pinned_candidate_is_adopted_even_when_outside_every_bucket() {
        let id = identity("app", "billing", Some("/data"));
        let cands = vec![
            timed_candidate("new", id.clone(), "2026-01-03T00:00:00Z", false),
            timed_candidate("mid", id.clone(), "2026-01-02T00:00:00Z", false),
            timed_candidate("pin", id.clone(), "2026-01-01T00:00:00Z", true),
        ];
        let ret = retention_of(serde_json::json!({ "keepLatest": 1 }));
        let gate = retain_gate(Some(&ret), &[]);
        let plan = gated_plan(&id, cands, &BTreeSet::new(), false, &gate);
        assert_eq!(adopt_ids(&plan), vec!["new", "pin"]);
        assert_eq!(plan.skipped_by_retention, 1);
    }

    #[test]
    fn candidate_without_end_time_is_skipped_under_retain_gating() {
        // No parseable end time → kept-ness unprovable → withheld (defensive;
        // the catalog always writes timing on discovered rows).
        let id = identity("app", "billing", Some("/data"));
        let cands = vec![candidate("aaa", id.clone(), false)];
        let ret = retention_of(serde_json::json!({ "keepLatest": 5 }));
        let gate = retain_gate(Some(&ret), &[]);
        let plan = gated_plan(&id, cands, &BTreeSet::new(), false, &gate);
        assert!(plan.adopt.is_empty());
        assert_eq!(plan.skipped_by_retention, 1);
        assert!(!plan.request_scan);
    }

    #[test]
    fn retain_gate_is_stable_across_passes() {
        let id = identity("app", "billing", Some("/data"));
        let ret = retention_of(serde_json::json!({ "keepLatest": 1 }));

        // Pass 1: no history; three candidates → adopt only the GFS-kept newest.
        let cands = || {
            vec![
                timed_candidate("aaa", id.clone(), "2026-01-03T00:00:00Z", false),
                timed_candidate("bbb", id.clone(), "2026-01-02T00:00:00Z", false),
                timed_candidate("ccc", id.clone(), "2026-01-01T00:00:00Z", false),
            ]
        };
        let gate1 = retain_gate(Some(&ret), &[]);
        let plan1 = gated_plan(&id, cands(), &BTreeSet::new(), false, &gate1);
        assert_eq!(adopt_ids(&plan1), vec!["aaa"]);

        // Pass 2: the adopted row is an own child under its adopted name; the
        // remaining candidates are re-offered. Nothing more is adopted, nothing
        // is requested — the plan is a fixed point.
        let own = [own_view(
            &adopted_cr_name("pol", "aaa"),
            "2026-01-03T00:00:00Z",
            false,
        )];
        let own_ids: BTreeSet<String> = ["aaa".to_string()].into_iter().collect();
        let gate2 = retain_gate(Some(&ret), &own);
        let plan2 = gated_plan(&id, cands(), &own_ids, true, &gate2);
        assert!(plan2.adopt.is_empty());
        assert!(!plan2.request_scan);
        assert_eq!(plan2.skipped_by_retention, 2);

        // Displaced-own guard: an own row that already LOST keepLatest:1 to a
        // newer child gets (Retain-)pruned, re-discovers, and is re-offered as
        // a candidate — it must not be re-adopted (no oscillation).
        let displaced = timed_candidate("old-displaced", id.clone(), "2026-01-01T00:00:00Z", false);
        let plan3 = gated_plan(&id, vec![displaced], &own_ids, true, &gate2);
        assert!(plan3.adopt.is_empty());
        assert_eq!(plan3.skipped_by_retention, 1);
        assert!(!plan3.request_scan);

        // Exact end-time tie at the boundary: the candidate competes under its
        // FUTURE adopted name, so the id tie-break resolves identically at gate
        // time and after adoption — the winner, once adopted, keeps winning.
        let tied_end = "2026-01-05T00:00:00Z";
        let own_tied = [own_view("zzz-own", tied_end, false)];
        let cand_tied = timed_candidate("t1", id.clone(), tied_end, false);
        assert!(
            adopted_cr_name("pol", "t1").as_str() < "zzz-own",
            "fixture precondition: the adopted name wins the id tie-break"
        );
        let gate_tied = retain_gate(Some(&ret), &own_tied);
        let plan_tied = gated_plan(&id, vec![cand_tied], &BTreeSet::new(), true, &gate_tied);
        assert_eq!(
            adopt_ids(&plan_tied),
            vec!["t1"],
            "tie goes to the smaller id"
        );
        // After adoption the same population re-planned (winner now own, loser
        // re-offered) reproduces itself: the displaced row is not re-adopted.
        let own_after = [own_view(&adopted_cr_name("pol", "t1"), tied_end, false)];
        let own_ids_after: BTreeSet<String> = ["t1".to_string()].into_iter().collect();
        // The pruned "zzz-own" re-discovers under its kopia id ("zzz"), so it
        // now competes under adopted_cr_name("pol","zzz") — still a loser.
        let re_offered = timed_candidate("zzz", id.clone(), tied_end, false);
        let gate_after = retain_gate(Some(&ret), &own_after);
        let plan_after = gated_plan(&id, vec![re_offered], &own_ids_after, true, &gate_after);
        assert!(
            plan_after.adopt.is_empty(),
            "no oscillation after the tie resolves"
        );
        assert!(!plan_after.request_scan);
    }

    #[test]
    fn adopted_cr_name_matches_build_adopted_snapshot() {
        // Refactor pin: the gate's candidate views and the builder must mint
        // the SAME name, or tie-breaks diverge between planning and reality.
        let policy = policy_fixture(None);
        let cand = candidate(
            "0123456789abcdef0123",
            identity("app", "billing", Some("/data")),
            false,
        );
        let (snap, _) = build_adopted_snapshot(
            &policy,
            "repo-uid-1",
            &cand,
            kopiur_api::single_repository_ref(&policy.spec).unwrap(),
        );
        assert_eq!(
            snap.name_any(),
            adopted_cr_name("app", "0123456789abcdef0123")
        );
    }

    // -- build_adopted_snapshot ----------------------------------------------

    fn policy_fixture(deletion: Option<DeletionPolicy>) -> SnapshotPolicy {
        let spec: SnapshotPolicySpec = serde_json::from_value(serde_json::json!({
            "repository": { "kind": "Repository", "name": "nas" },
            "sources": [{ "pvc": { "name": "data" } }],
        }))
        .unwrap();
        let mut p = SnapshotPolicy::new("app", spec);
        p.spec.default_deletion_policy = deletion;
        p.metadata.namespace = Some("billing".into());
        p
    }

    #[test]
    fn build_adopted_snapshot_field_by_field() {
        let policy = policy_fixture(None);
        let cand = AdoptionCandidate {
            namespace: "disc-ns".into(),
            name: "row-aaa".into(),
            snapshot_id: "0123456789abcdef0123".into(),
            identity: identity("app", "billing", Some("/data")),
            timing: Some(SnapshotTiming {
                start_time: Some("2026-01-01T00:00:00Z".into()),
                end_time: Some("2026-01-01T00:01:00Z".into()),
                duration_seconds: Some(60),
            }),
            stats: Some(SnapshotStats {
                size_bytes: Some(4096),
                ..Default::default()
            }),
            pinned: true,
            recorded: Some(kopiur_api::RecordedSnapshotMeta {
                schema: kopiur_api::KOPIUR_META_SCHEMA_V1,
                src: kopiur_api::RecordedSrc::Explicit,
                uid: Some(3001),
                gid: None,
                fs_group: Some(65532),
            }),
            description: Some("kept description".into()),
        };
        let (snap, status) = build_adopted_snapshot(
            &policy,
            "repo-uid-1",
            &cand,
            kopiur_api::single_repository_ref(&policy.spec).unwrap(),
        );

        // Name: <policy>-adopted-<first16(id)>-<hash8(full id)>, in the policy's
        // namespace. The trailing hash is over the FULL id (not just the
        // first-16 prefix), so it stays distinct for ids sharing that prefix.
        assert_eq!(snap.name_any(), "app-adopted-0123456789abcdef-f9c550c9");
        assert_eq!(snap.namespace().as_deref(), Some("billing"));

        // Labels mirror the discovered set + CONFIG_LABEL.
        let labels = snap.labels();
        assert_eq!(
            labels.get(ORIGIN_LABEL).map(String::as_str),
            Some("adopted")
        );
        assert_eq!(labels.get(CONFIG_LABEL).map(String::as_str), Some("app"));
        assert_eq!(
            labels.get(SNAPSHOT_ID_LABEL).map(String::as_str),
            Some("0123456789abcdef0123")
        );
        assert_eq!(
            labels.get(REPOSITORY_UID_LABEL).map(String::as_str),
            Some("repo-uid-1")
        );

        // Spec: policyRef, deletionPolicy default = Delete, pin carried, no onScheduleDelete.
        // A SINGLE-repo policy's adopted row carries NO spec.repository pin —
        // byte-identical to the pre-multi-repo wire (#368).
        assert!(snap.spec.repository.is_none());
        assert_eq!(snap.spec.policy_ref.as_ref().unwrap().name, "app");
        assert_eq!(snap.spec.deletion_policy, Some(DeletionPolicy::Delete));
        assert!(snap.spec.pin, "candidate.pinned is carried");
        assert!(snap.spec.on_schedule_delete.is_none());

        // Meta: NO ownerReferences (inv. 5).
        assert!(
            snap.metadata.owner_references.is_none(),
            "adopted rows carry NO ownerReference"
        );

        // Status: phase/origin, id+identity, timing+stats verbatim, resolved-repo pin.
        assert_eq!(status.phase, Some(SnapshotPhase::Succeeded));
        assert_eq!(status.origin, Some(Origin::Adopted));
        let info = status.snapshot.as_ref().unwrap();
        assert_eq!(info.kopia_snapshot_id, "0123456789abcdef0123");
        assert_eq!(info.identity, cand.identity);
        assert_eq!(status.timing, cand.timing);
        assert_eq!(status.stats, cand.stats);
        // Recorded identity + description survive adoption verbatim.
        assert_eq!(status.recorded, cand.recorded);
        assert_eq!(info.description, cand.description);
        let pinned = status
            .resolved
            .as_ref()
            .unwrap()
            .repository
            .as_ref()
            .unwrap();
        assert_eq!(pinned.kind, RepositoryKind::Repository);
        assert_eq!(pinned.name, "nas");
        // A namespaced Repository ref pins the namespace it resolved against.
        assert_eq!(pinned.namespace.as_deref(), Some("billing"));
    }

    /// Multi-repo fan-out (#368): an adopted row of a multi-repo policy stamps
    /// `spec.repository` = the repository it was DISCOVERED in (normalized) —
    /// the pin the (source, repo) retention buckets and deletion batching key
    /// on. Spec constructed directly — the same shape admission accepts now
    /// that the M7 feature gate is lifted.
    #[test]
    fn build_adopted_snapshot_pins_the_discovered_repository_for_multi_repo() {
        let spec: SnapshotPolicySpec = serde_json::from_value(serde_json::json!({
            "repositories": [
                { "kind": "Repository", "name": "nas" },
                { "kind": "ClusterRepository", "name": "offsite" },
            ],
            "sources": [{ "pvc": { "name": "data" } }],
        }))
        .unwrap();
        let mut policy = SnapshotPolicy::new("app", spec);
        policy.metadata.namespace = Some("billing".into());
        let cand = candidate("aaa", identity("app", "billing", Some("/data")), false);

        // Discovered in the second member (the cluster-scoped repo).
        let (snap, status) = build_adopted_snapshot(
            &policy,
            "repo-uid-2",
            &cand,
            &kopiur_api::common::RepositoryRef {
                kind: RepositoryKind::ClusterRepository,
                name: "offsite".into(),
                namespace: None,
            },
        );
        let pin = snap.spec.repository.as_ref().expect("spec pin stamped");
        assert_eq!(pin.kind, RepositoryKind::ClusterRepository);
        assert_eq!(pin.name, "offsite");
        assert_eq!(pin.namespace, None, "cluster refs normalize namespace-free");
        // Spec pin and status pin agree (one normalization).
        assert_eq!(
            Some(pin),
            status.resolved.as_ref().unwrap().repository.as_ref()
        );

        // Discovered in the namespaced member: the pin carries the EFFECTIVE
        // namespace explicitly.
        let (snap, _) = build_adopted_snapshot(
            &policy,
            "repo-uid-1",
            &cand,
            &kopiur_api::common::RepositoryRef {
                kind: RepositoryKind::Repository,
                name: "nas".into(),
                namespace: None,
            },
        );
        let pin = snap.spec.repository.as_ref().expect("spec pin stamped");
        assert_eq!(pin.namespace.as_deref(), Some("billing"));
    }

    #[test]
    fn build_adopted_snapshot_honors_default_deletion_policy() {
        let policy = policy_fixture(Some(DeletionPolicy::Orphan));
        let cand = candidate("aaa", identity("app", "billing", Some("/data")), false);
        let (snap, _) = build_adopted_snapshot(
            &policy,
            "repo-uid-1",
            &cand,
            kopiur_api::single_repository_ref(&policy.spec).unwrap(),
        );
        assert_eq!(snap.spec.deletion_policy, Some(DeletionPolicy::Orphan));
    }

    /// The P1 regression test: two kopia snapshot ids that share a ≥16-char
    /// prefix but differ afterward previously minted the SAME adopted-row
    /// name (`<policy>-adopted-<first16>`), which drove the second adoption
    /// into the create-409 branch in `snapshot_policy::adopt_one` — either
    /// clobbering the first row's status (pre-91093e7) or, post-91093e7,
    /// permanently wedging the second candidate behind the
    /// `adopted_row_matches_candidate` stranger guard (skipped + re-warned
    /// every pass, its discovered row never cleaned up). Appending a hash of
    /// the FULL id makes the two names distinct.
    #[test]
    fn build_adopted_snapshot_names_differ_for_ids_sharing_a_16char_prefix() {
        let policy = policy_fixture(None);
        let ident = identity("app", "billing", Some("/data"));
        let id_a = "0123456789abcdef-AAAA-tail-one";
        let id_b = "0123456789abcdef-BBBB-tail-two";
        assert_eq!(
            &id_a[..16],
            &id_b[..16],
            "fixture precondition: shared 16-char prefix"
        );
        let cand_a = candidate(id_a, ident.clone(), false);
        let cand_b = candidate(id_b, ident, false);
        let repo_ref = kopiur_api::single_repository_ref(&policy.spec).unwrap();
        let (snap_a, _) = build_adopted_snapshot(&policy, "repo-uid-1", &cand_a, repo_ref);
        let (snap_b, _) = build_adopted_snapshot(&policy, "repo-uid-1", &cand_b, repo_ref);
        assert_ne!(
            snap_a.name_any(),
            snap_b.name_any(),
            "distinct full ids must never collide on their first-16 prefix alone"
        );
    }

    #[test]
    fn build_adopted_snapshot_name_is_deterministic_for_the_same_full_id() {
        let policy = policy_fixture(None);
        let ident = identity("app", "billing", Some("/data"));
        let cand = candidate("0123456789abcdef0123", ident, false);
        let (snap_1, _) = build_adopted_snapshot(
            &policy,
            "repo-uid-1",
            &cand,
            kopiur_api::single_repository_ref(&policy.spec).unwrap(),
        );
        let (snap_2, _) = build_adopted_snapshot(
            &policy,
            "repo-uid-1",
            &cand,
            kopiur_api::single_repository_ref(&policy.spec).unwrap(),
        );
        assert_eq!(
            snap_1.name_any(),
            snap_2.name_any(),
            "same (policy, full id) must always name the same, so idempotent \
             re-adoption (already-carried-id skip; 409 -> ensure-status) converges"
        );
    }

    #[test]
    fn build_adopted_snapshot_name_stays_capped_for_long_policy_and_id() {
        let mut policy = policy_fixture(None);
        policy.metadata.name = Some("n".repeat(80));
        let ident = identity("app", "billing", Some("/data"));
        let long_id = "9".repeat(200);
        let cand = candidate(&long_id, ident, false);
        let (snap, _) = build_adopted_snapshot(
            &policy,
            "repo-uid-1",
            &cand,
            kopiur_api::single_repository_ref(&policy.spec).unwrap(),
        );
        let name = snap.name_any();
        assert!(
            name.len() <= 63,
            "capped_name must still cap a long policy name + long id: {} chars ({name})",
            name.len()
        );
    }

    // -- event message -------------------------------------------------------

    #[test]
    fn adoption_event_message_states_count_identity_governance_and_both_opt_outs() {
        let msg = adoption_event_message(3, "app@billing:/data");
        assert!(msg.contains('3'), "count");
        assert!(msg.contains("app@billing:/data"), "identity");
        assert!(msg.contains("spec.retention"), "GFS governance / pruning");
        assert!(msg.contains("pruned"), "states rows WILL be pruned");
        assert!(msg.contains("spec.adoption: Ignore"), "policy opt-out");
        assert!(
            msg.contains("spec.catalog.adoption: Ignore"),
            "repository opt-out"
        );
    }

    #[test]
    fn adoption_skipped_event_message_states_count_identity_and_all_levers() {
        let msg = adoption_skipped_event_message(2, "app@billing:/data");
        assert!(msg.contains('2'), "count");
        assert!(msg.contains("app@billing:/data"), "identity");
        assert!(msg.contains("spec.retention"), "lever: widen retention");
        assert!(
            msg.contains("spec.defaultDeletionPolicy: Delete"),
            "lever: switch to real pruning"
        );
        assert!(msg.contains("spec.pin"), "lever: pin a snapshot");
        assert!(msg.contains("spec.adoption: Ignore"), "lever: opt out");
        assert!(msg.contains("discovered"), "states where the rows remain");
    }
}
