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

use std::collections::{BTreeMap, BTreeSet};

use kube::ResourceExt;

use kopiur_api::common::{DeletionPolicy, PolicyRef, ResolvedIdentity, SnapshotAdoption};
use kopiur_api::snapshot::{
    ResolvedSnapshot, SnapshotInfo, SnapshotSpec, SnapshotStats, SnapshotStatus, SnapshotTiming,
};
use kopiur_api::{
    HostClass, Origin, Snapshot, SnapshotPhase, SnapshotPolicy, classify_hostname, identity_string,
};

use crate::consts::{CONFIG_LABEL, ORIGIN_LABEL, REPOSITORY_UID_LABEL, SNAPSHOT_ID_LABEL};

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
}

/// Extract adoption candidates from a `Snapshot` LIST: `discovered`-origin rows
/// of THIS repository (`repo_uid`) that are not terminating and carry a
/// resolvable kopia id + identity in status. Pure.
///
/// SKIPs (returns nothing for):
/// - rows not labeled `origin: discovered` or not for `repo_uid`;
/// - **terminating** rows (`metadata.deletionTimestamp` set) — a row already
///   being expired must not be re-created under a new identity;
/// - rows missing `status.snapshot` (no id/identity to match on).
pub fn adoption_candidates(repo_uid: &str, rows: &[Snapshot]) -> Vec<AdoptionCandidate> {
    rows.iter()
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
}

/// Decide the adoption plan for one pass. **Pure** — exhaustive over
/// [`SnapshotAdoption`], no `_ =>`.
///
/// - [`SnapshotAdoption::Ignore`] → adopt nothing, request no scan.
/// - [`SnapshotAdoption::Adopt`] → adopt every candidate that (inv. 2) matches
///   the policy's live-resolved identity structurally AND (inv. 3) is not a
///   foreign-cluster hostname AND is not already carried by a config-label
///   `Snapshot` (`own_snapshot_ids`); sorted by snapshot id and capped at
///   [`POLICY_ADOPTION_BATCH`] (inv. 7).
///
/// `request_scan` is true when this pass adopted anything (a wave implies more
/// may exist behind the catalog's `retain` caps), OR — for a brand-new /
/// delete-then-recreated policy — when NO candidate matched at all, the policy
/// has no history yet, and no scan was already requested for THIS identity
/// (`scan_requested_identity`). That last arm fires exactly once per (policy,
/// identity): a scan that yields nothing stays quiet forever after.
pub fn plan_adoption(
    mode: SnapshotAdoption,
    policy_identity: &ResolvedIdentity,
    repo_cluster: Option<&str>,
    candidates: Vec<AdoptionCandidate>,
    own_snapshot_ids: &BTreeSet<String>,
    has_history: bool,
    scan_requested_identity: Option<&str>,
) -> AdoptionPlan {
    match mode {
        SnapshotAdoption::Ignore => AdoptionPlan {
            adopt: Vec::new(),
            request_scan: false,
        },
        SnapshotAdoption::Adopt => {
            // Identity-matching, non-foreign candidates (inv. 2 + 3). `matched_any`
            // is computed BEFORE the own-id filter: a candidate that matched but is
            // already ours means there IS relevant history, so it must not trigger a
            // "nothing matched" scan request.
            let mut matched: Vec<AdoptionCandidate> = candidates
                .into_iter()
                .filter(|c| {
                    identities_match(policy_identity, &c.identity)
                        && !is_foreign_cluster(&c.identity.hostname, repo_cluster)
                })
                .collect();
            let matched_any = !matched.is_empty();
            matched.retain(|c| !own_snapshot_ids.contains(&c.snapshot_id));
            // Deterministic order (inv. 7) so a capped pass always adopts the same
            // prefix, and the batch cap.
            matched.sort_by(|a, b| a.snapshot_id.cmp(&b.snapshot_id));
            matched.truncate(POLICY_ADOPTION_BATCH);
            let adopt = matched;

            let already_requested =
                scan_requested_identity == Some(identity_string(policy_identity).as_str());
            let request_scan =
                !adopt.is_empty() || (!matched_any && !has_history && !already_requested);
            AdoptionPlan {
                adopt,
                request_scan,
            }
        }
    }
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
/// - **Name**: `<policy>-adopted-<first16(snapshot_id)>`, length-capped by
///   [`crate::naming::capped_name`].
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
) -> (Snapshot, SnapshotStatus) {
    let policy_name = policy.name_any();
    let namespace = policy.namespace().unwrap_or_default();
    let short: String = candidate.snapshot_id.chars().take(16).collect();
    let cr_name = crate::naming::capped_name(&format!("{policy_name}-adopted-{short}"));

    let mut labels = BTreeMap::new();
    labels.insert(
        ORIGIN_LABEL.to_string(),
        Origin::Adopted.label_value().to_string(),
    );
    labels.insert(CONFIG_LABEL.to_string(), policy_name.clone());
    labels.insert(SNAPSHOT_ID_LABEL.to_string(), candidate.snapshot_id.clone());
    labels.insert(REPOSITORY_UID_LABEL.to_string(), repo_uid.to_string());

    let deletion_policy = policy
        .spec
        .default_deletion_policy
        .unwrap_or(DeletionPolicy::Delete);

    let mut snapshot = Snapshot::new(
        &cr_name,
        SnapshotSpec {
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

    let pinned_repo = crate::snapshot::pinned_repository_ref(&policy.spec.repository, &namespace);
    let status = SnapshotStatus {
        phase: Some(SnapshotPhase::Succeeded),
        origin: Some(Origin::Adopted),
        snapshot: Some(SnapshotInfo {
            kopia_snapshot_id: candidate.snapshot_id.clone(),
            identity: candidate.identity.clone(),
        }),
        timing: candidate.timing.clone(),
        stats: candidate.stats.clone(),
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
            own,
            has_history,
            requested,
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
    fn candidates_carry_the_pin_flag() {
        let ident = identity("app", "billing", Some("/data"));
        let mut row = discovered_row("repo-1", "pinned", "aaa", ident);
        row.spec.pin = true;
        let got = adoption_candidates("repo-1", &[row]);
        assert_eq!(got.len(), 1);
        assert!(got[0].pinned, "spec.pin is carried onto the candidate");
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
            &BTreeSet::new(),
            false,
            None,
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
        };
        let (snap, status) = build_adopted_snapshot(&policy, "repo-uid-1", &cand);

        // Name: <policy>-adopted-<first16(id)>, in the policy's namespace.
        assert_eq!(snap.name_any(), "app-adopted-0123456789abcdef");
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

    #[test]
    fn build_adopted_snapshot_honors_default_deletion_policy() {
        let policy = policy_fixture(Some(DeletionPolicy::Orphan));
        let cand = candidate("aaa", identity("app", "billing", Some("/data")), false);
        let (snap, _) = build_adopted_snapshot(&policy, "repo-uid-1", &cand);
        assert_eq!(snap.spec.deletion_policy, Some(DeletionPolicy::Orphan));
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
}
