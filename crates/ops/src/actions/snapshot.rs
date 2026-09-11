//! Builds the `Snapshot` CR an on-demand backup run applies.
//!
//! The invocation ("run this recipe now") is expressed as a
//! [`SnapshotNowRequest`], deliberately free of any CLI type: `kubectl kopiur
//! snapshot now` builds one from its flags, a web UI from a form, and both get
//! byte-identical children — including the `pvcSelector`/multi-repository
//! fan-out, whose cells come from [`kopiur_api::expand`] shared verbatim with
//! the `SnapshotSchedule` reconciler.

use chrono::{DateTime, Utc};
use kopiur_api::common::{DeletionPolicy, FailurePolicy};
use kopiur_api::consts::{CONFIG_LABEL, ORIGIN_LABEL};
use kopiur_api::expand::MintCell;
use kopiur_api::{Origin, PolicyRef, Snapshot, SnapshotPhase, SnapshotPolicy, SnapshotSpec};
use kube::api::{Api, PostParams};

use crate::ctx::OpsCtx;
use crate::error::{OpsError, classify_kube};
use crate::format::human_bytes;

/// One "run this recipe now" invocation: the recipe plus the per-run overrides.
///
/// Sub-objects, not loose leaves, for the policy surfaces (`failure_policy`) —
/// a new mover-Job control slots in without changing this struct's shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotNowRequest {
    /// The `SnapshotPolicy` (recipe) to run.
    pub policy: String,
    /// Name for the created `Snapshot`; `None` derives
    /// `<policy>-manual-<timestamp>`.
    pub name: Option<String>,
    /// kopia snapshot tags, as `(key, value)` pairs.
    pub tags: Vec<(String, String)>,
    /// Lifecycle of the kopia snapshot when this CR is deleted; `None` leaves
    /// the operator's origin-aware default.
    pub deletion_policy: Option<DeletionPolicy>,
    /// Pin the snapshot, exempting it from GFS retention until unpinned.
    pub pin: bool,
    /// Free-form text recorded on the kopia snapshot manifest.
    pub description: Option<String>,
    /// Mover `Job` failure controls; `None` leaves the operator's defaults.
    pub failure_policy: Option<FailurePolicy>,
    /// For a multi-repository policy: back up into this ONE repository (by
    /// name) instead of fanning out to all of them.
    pub repository: Option<String>,
}

/// Build the manual `Snapshot` CR for this invocation. Pure — `now` is
/// injected so names are deterministic under test. Mirrors what a
/// `SnapshotSchedule` stamps on its children (origin + config labels) so the
/// rest of the tooling (`snapshots list --policy`, the controller's
/// `resolve_origin`) treats it uniformly.
pub fn build_snapshot(req: &SnapshotNowRequest, namespace: &str, now: DateTime<Utc>) -> Snapshot {
    build_snapshot_for(req, namespace, now, None)
}

/// [`build_snapshot`] for one cell of a fan-out (`pvcSelector` member and/or
/// multi-repository dimension, #368).
///
/// `cell` carries the child's deterministic name, the `spec.source` pin saying
/// which PVC it covers, and — for a child of a multi-repo policy — the
/// normalized `spec.repository` pin saying which repository it targets. `None`
/// is the ordinary single-source, single-repo case.
pub fn build_snapshot_for(
    req: &SnapshotNowRequest,
    namespace: &str,
    now: DateTime<Utc>,
    cell: Option<&MintCell>,
) -> Snapshot {
    let base = default_name(req, now);
    let (name, source, repository) = match cell {
        Some(c) => (c.name.clone(), c.source.clone(), c.repository.clone()),
        None => (base, None, None),
    };
    let mut snapshot = Snapshot::new(
        &name,
        SnapshotSpec {
            repository,
            source,
            policy_ref: Some(PolicyRef {
                name: req.policy.clone(),
                namespace: None,
            }),
            tags: if req.tags.is_empty() {
                None
            } else {
                Some(req.tags.iter().cloned().collect())
            },
            failure_policy: req.failure_policy.clone(),
            deletion_policy: req.deletion_policy,
            // `snapshot now` creates a manual Snapshot with no owning schedule.
            on_schedule_delete: None,
            pin: req.pin,
            description: req.description.clone(),
        },
    );
    snapshot.metadata.namespace = Some(namespace.to_string());
    snapshot.metadata.labels = Some(
        [
            (
                ORIGIN_LABEL.to_string(),
                Origin::Manual.label_value().to_string(),
            ),
            (CONFIG_LABEL.to_string(), req.policy.clone()),
        ]
        .into(),
    );
    snapshot
}

/// The requested name, or the `<policy>-manual-<timestamp>` derivation that is
/// also the fan-out's base name. Pure.
fn default_name(req: &SnapshotNowRequest, now: DateTime<Utc>) -> String {
    req.name
        .clone()
        .unwrap_or_else(|| format!("{}-manual-{}", req.policy, now.format("%Y%m%d%H%M%S")))
}

/// What a terminal phase means for this command. Exhaustive over
/// [`SnapshotPhase`]: a new phase cannot compile until classified.
pub fn terminal(snapshot: &Snapshot) -> Option<Result<Box<Snapshot>, Box<Snapshot>>> {
    match snapshot.status.as_ref().and_then(|s| s.phase.as_ref())? {
        SnapshotPhase::Pending | SnapshotPhase::Running => None,
        // `Unchanged` is a SUCCESS: the source was read and hashed, and kopia
        // declined to write a second identical manifest. Exiting non-zero here
        // would fail every `kubectl kopiur snapshot now --wait` in a CI/GitOps
        // job the moment a source stopped changing — the healthy case.
        SnapshotPhase::Succeeded | SnapshotPhase::Unchanged => Some(Ok(Box::new(snapshot.clone()))),
        SnapshotPhase::Failed => Some(Err(Box::new(snapshot.clone()))),
        // A manual Snapshot can only be Deleting if someone deleted it mid-run;
        // surface that as the failure path (the wait also catches the delete
        // event itself). Discovered is unreachable for a CR we just created
        // with a policyRef.
        SnapshotPhase::Deleting | SnapshotPhase::Discovered => {
            Some(Err(Box::new(snapshot.clone())))
        }
        // Never a terminal answer: `--wait` keeps waiting (bounded by its own
        // timeout) rather than exiting 0 (a success we cannot substantiate) or
        // 1 (a failure that may not have happened).
        SnapshotPhase::Unknown(_) => None,
    }
}

/// One-line success summary from the terminal object's status.
///
/// An `Unchanged` run gets its own line rather than the usual one: it has no
/// kopia id, no size and no duration to report, so the shared formatter would
/// print `kopia id ?, ?, took ?` and read like something went wrong.
pub fn success_summary(snapshot: &Snapshot) -> String {
    let status = snapshot.status.as_ref();
    if status.and_then(|s| s.phase.as_ref()) == Some(&SnapshotPhase::Unchanged) {
        let name = snapshot.metadata.name.as_deref().unwrap_or("?");
        return format!(
            "snapshot {name}: no files changed since the previous snapshot, so no new \
             snapshot was created (the previous one is still the restore point)\n"
        );
    }
    let id = status
        .and_then(|s| s.snapshot.as_ref())
        .map(|i| i.kopia_snapshot_id.as_str())
        .unwrap_or("?");
    let size = status
        .and_then(|s| s.stats.as_ref())
        .and_then(|s| s.size_bytes)
        .map(human_bytes)
        .unwrap_or_else(|| "?".into());
    let duration = status
        .and_then(|s| s.timing.as_ref())
        .and_then(|t| t.duration_seconds)
        .map(|d| format!("{d}s"))
        .unwrap_or_else(|| "?".into());
    let name = snapshot.metadata.name.as_deref().unwrap_or("?");
    format!("snapshot {name} succeeded: kopia id {id}, {size}, took {duration}\n")
}

/// Failure detail from the terminal object's status (what/why + kopia stderr
/// tail + logTail), for stderr.
pub fn failure_detail(snapshot: &Snapshot) -> String {
    let name = snapshot.metadata.name.as_deref().unwrap_or("?");
    let mut out = format!("snapshot {name} failed");
    if let Some(f) = snapshot.status.as_ref().and_then(|s| s.failure.as_ref()) {
        out.push_str(&format!(" ({}): {}", f.kopia_error_class, f.message));
        if let Some(stderr) = &f.stderr_tail {
            out.push_str(&format!("\n--- kopia stderr tail ---\n{stderr}"));
        }
    }
    if let Some(tail) = snapshot.status.as_ref().and_then(|s| s.log_tail.as_ref()) {
        out.push_str(&format!("\n--- log tail ---\n{tail}"));
    }
    out.push('\n');
    out
}

/// The `Snapshot` CRs this invocation should create.
///
/// One, for an ordinary single-source recipe. N — one per matched PVC — when
/// the recipe uses a `pvcSelector` (#346). This layer expands rather than
/// letting the operator guess, for the same reason a `SnapshotSchedule` does: a
/// `Snapshot` must never pick one of N volumes on the user's behalf.
///
/// The decisions (names, pins, the same-path collision guard) come from
/// `kopiur_api::expand`, shared verbatim with the controller. Only the PVC
/// listing is here, because the shared layer cannot depend on the controller
/// crate.
///
/// A selector that currently matches nothing is an error, not an empty plan:
/// creating zero snapshots and reporting success would look like a backup that
/// ran.
pub async fn plan_snapshots(
    ctx: &OpsCtx,
    req: &SnapshotNowRequest,
    policy: &SnapshotPolicy,
    namespace: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Snapshot>, OpsError> {
    let base = default_name(req, now);
    let matched = match_pvcs(ctx, policy, namespace).await?;

    let members = kopiur_api::expand::expand_sources(policy, &base, &matched).map_err(|e| {
        OpsError::AdmissionDenied {
            message: e.to_string(),
        }
    })?;
    let cells = planned_cells(
        policy,
        &req.policy,
        &base,
        members,
        req.repository.as_deref(),
    )?;
    let planned = match &cells[..] {
        // The single legacy cell keeps the exact pre-fan-out builder path.
        [cell] if cell.source.is_none() && cell.repository.is_none() => {
            vec![build_snapshot(req, namespace, now)]
        }
        _ => cells
            .iter()
            .map(|c| build_snapshot_for(req, namespace, now, Some(c)))
            .collect(),
    };
    if planned.is_empty() {
        return Err(OpsError::SelectorMatchedNothing {
            policy: req.policy.clone(),
            namespace: namespace.to_string(),
        });
    }
    Ok(planned)
}

/// List the PVCs each `pvcSelector` source of `policy` currently matches, keyed
/// by source index — the input `kopiur_api::expand::expand_sources` needs.
/// Sources without a selector are absent (they expand to themselves).
async fn match_pvcs(
    ctx: &OpsCtx,
    policy: &SnapshotPolicy,
    namespace: &str,
) -> Result<std::collections::BTreeMap<usize, Vec<kopiur_api::PvcTargetRef>>, OpsError> {
    use std::collections::BTreeMap;

    let mut matched: BTreeMap<usize, Vec<kopiur_api::PvcTargetRef>> = BTreeMap::new();
    for (index, source) in policy.spec.sources.iter().enumerate() {
        let Some(selector) = source.pvc_selector.as_ref() else {
            continue;
        };
        let namespaces: Vec<String> = match selector.namespace_selector.as_ref() {
            Some(n) if !n.match_names.is_empty() => n.match_names.clone(),
            _ => vec![namespace.to_string()],
        };
        let label_selector = selector
            .label_selector
            .as_ref()
            .map(kopiur_api::expand::label_selector_string)
            .unwrap_or_default();
        let mut found = Vec::new();
        for ns in &namespaces {
            let api: Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
                Api::namespaced(ctx.client.clone(), ns);
            let mut lp = kube::api::ListParams::default();
            if !label_selector.is_empty() {
                lp = lp.labels(&label_selector);
            }
            let list = api.list(&lp).await.map_err(|e| {
                classify_kube(
                    "list",
                    "PersistentVolumeClaim",
                    "persistentvolumeclaims",
                    Some(ns),
                    None,
                    e,
                )
            })?;
            for pvc in list.items {
                if let Some(name) = pvc.metadata.name {
                    found.push(kopiur_api::PvcTargetRef {
                        namespace: ns.clone(),
                        name,
                    });
                }
            }
        }
        // Deterministic: the child names derive from this order.
        found.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
        found.dedup();
        matched.insert(index, found);
    }
    Ok(matched)
}

/// **Pure.** The mint cells for one `snapshot now` invocation: the members ×
/// repositories cross product (#368) via the SAME
/// [`kopiur_api::expand::mint_cells`] the `SnapshotSchedule` uses — so a
/// manual run and a scheduled slot mint byte-identical sets — optionally
/// restricted by [`SnapshotNowRequest::repository`].
///
/// That restriction must name one of the policy's repositories (unknown names
/// are refused, listing the valid ones). Against a multi-repo policy it keeps
/// only that repository's pinned cells; against a single-repo policy the
/// (validated) name is the policy's one repository, and the minted child stays
/// unpinned exactly as a single-repo child always is.
fn planned_cells(
    policy: &SnapshotPolicy,
    policy_name: &str,
    base: &str,
    members: Option<Vec<kopiur_api::expand::ExpandedMember>>,
    repository: Option<&str>,
) -> Result<Vec<MintCell>, OpsError> {
    let mut cells = kopiur_api::expand::mint_cells(policy, base, members);
    if let Some(repo_name) = repository {
        let valid: Vec<&str> = kopiur_api::repository_refs(&policy.spec)
            .map(|r| r.name.as_str())
            .collect();
        if !valid.contains(&repo_name) {
            return Err(OpsError::UnknownPolicyRepository {
                given: repo_name.to_string(),
                policy: policy_name.to_string(),
                valid: valid.join(", "),
            });
        }
        cells.retain(|c| c.repository.as_ref().is_none_or(|r| r.name == repo_name));
    }
    Ok(cells)
}

/// Create the planned `Snapshot`s, in order, returning what the API server
/// stored. Stops at the first failure — a partially-created fan-out is
/// reported as the error it is, not silently completed.
pub async fn create_snapshots(
    ctx: &OpsCtx,
    namespace: &str,
    planned: &[Snapshot],
) -> Result<Vec<Snapshot>, OpsError> {
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
    let mut created = Vec::with_capacity(planned.len());
    for snapshot in planned {
        let name = snapshot.metadata.name.as_deref().unwrap_or("<unnamed>");
        created.push(
            api.create(&PostParams::default(), snapshot)
                .await
                .map_err(|e| {
                    classify_kube(
                        "create",
                        "Snapshot",
                        "snapshots",
                        Some(namespace),
                        Some(name),
                        e,
                    )
                })?,
        );
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn req() -> SnapshotNowRequest {
        SnapshotNowRequest {
            policy: "nightly".into(),
            name: None,
            tags: vec![],
            deletion_policy: None,
            pin: false,
            description: None,
            failure_policy: None,
            repository: None,
        }
    }

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 11, 3, 0, 12).unwrap()
    }

    #[test]
    fn builds_a_minimal_manual_snapshot_with_canonical_labels() {
        let snap = build_snapshot(&req(), "media", at());
        // Round-trip the cluster's way: typed → JSON → typed.
        let wire = serde_json::to_value(&snap).unwrap();
        assert_eq!(wire["metadata"]["name"], "nightly-manual-20260611030012");
        assert_eq!(wire["metadata"]["namespace"], "media");
        assert_eq!(
            wire["metadata"]["labels"]["kopiur.home-operations.com/origin"],
            "manual"
        );
        assert_eq!(
            wire["metadata"]["labels"]["kopiur.home-operations.com/config"],
            "nightly"
        );
        assert_eq!(wire["spec"]["policyRef"]["name"], "nightly");
        // Unset options must be ABSENT on the wire, not null/false noise.
        for key in [
            "tags",
            "failurePolicy",
            "deletionPolicy",
            "pin",
            "description",
        ] {
            assert!(wire["spec"].get(key).is_none(), "{key} should be absent");
        }
        let reparsed: Snapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(reparsed.spec, snap.spec);
    }

    #[test]
    fn every_request_field_lands_in_the_spec() {
        // The restore-options-dropped bug class: each field must round-trip.
        let mut r = req();
        r.name = Some("pre-upgrade".into());
        r.tags = vec![("reason".into(), "pre-upgrade".into())];
        r.deletion_policy = Some(DeletionPolicy::Retain);
        r.pin = true;
        r.description = Some("pre-upgrade snapshot".into());
        r.failure_policy = Some(FailurePolicy {
            backoff_limit: Some(0),
            active_deadline_seconds: Some(120),
            pod_startup_deadline_seconds: None,
        });
        let wire = serde_json::to_value(build_snapshot(&r, "media", at())).unwrap();
        assert_eq!(wire["metadata"]["name"], "pre-upgrade");
        assert_eq!(wire["spec"]["tags"]["reason"], "pre-upgrade");
        assert_eq!(wire["spec"]["deletionPolicy"], "Retain");
        assert_eq!(wire["spec"]["pin"], true);
        assert_eq!(wire["spec"]["description"], "pre-upgrade snapshot");
        assert_eq!(wire["spec"]["failurePolicy"]["backoffLimit"], 0);
        assert_eq!(wire["spec"]["failurePolicy"]["activeDeadlineSeconds"], 120);
    }

    fn with_phase(phase: &str) -> Snapshot {
        let v = serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": "s", "namespace": "media" },
            "spec": {},
            "status": { "phase": phase }
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn terminal_classification_is_exhaustive_and_correct() {
        assert!(terminal(&with_phase("Pending")).is_none());
        assert!(terminal(&with_phase("Running")).is_none());
        assert!(matches!(terminal(&with_phase("Succeeded")), Some(Ok(_))));
        assert!(matches!(terminal(&with_phase("Failed")), Some(Err(_))));
        assert!(matches!(terminal(&with_phase("Deleting")), Some(Err(_))));
    }

    #[test]
    fn success_summary_reports_id_size_duration() {
        let v = serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": "s" },
            "spec": {},
            "status": {
                "phase": "Succeeded",
                "snapshot": { "kopiaSnapshotID": "abc123", "identity": { "username": "u", "hostname": "h" } },
                "stats": { "sizeBytes": 1536 },
                "timing": { "durationSeconds": 42 }
            }
        });
        let snap: Snapshot = serde_json::from_value(v).unwrap();
        assert_eq!(
            success_summary(&snap),
            "snapshot s succeeded: kopia id abc123, 1.5 KiB, took 42s\n"
        );
    }

    #[test]
    fn failure_detail_includes_class_message_and_tails() {
        let v = serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": "s" },
            "spec": {},
            "status": {
                "phase": "Failed",
                "failure": {
                    "kopiaErrorClass": "AuthFailure",
                    "message": "credentials rejected; fix the Secret",
                    "stderrTail": "access denied",
                    "retryRecommended": false
                },
                "logTail": "last lines"
            }
        });
        let snap: Snapshot = serde_json::from_value(v).unwrap();
        let detail = failure_detail(&snap);
        assert!(detail.contains("snapshot s failed (AuthFailure): credentials rejected"));
        assert!(detail.contains("--- kopia stderr tail ---\naccess denied"));
        assert!(detail.contains("--- log tail ---\nlast lines"));
    }

    // --- multi-repo fan-out (#368): planned cells + the pin on the wire -----

    fn multi_repo_policy() -> SnapshotPolicy {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": "nightly", "namespace": "media" },
            "spec": {
                "repositories": [
                    { "kind": "Repository", "name": "nas" },
                    { "kind": "ClusterRepository", "name": "offsite" },
                ],
                "sources": [ { "pvc": { "name": "data" } } ],
            }
        }))
        .expect("multi-repo policy fixture")
    }

    fn single_repo_policy() -> SnapshotPolicy {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": "nightly", "namespace": "media" },
            "spec": {
                "repository": { "kind": "Repository", "name": "nas" },
                "sources": [ { "pvc": { "name": "data" } } ],
            }
        }))
        .expect("single-repo policy fixture")
    }

    #[test]
    fn planned_cells_single_repo_is_the_one_legacy_cell() {
        let cells = planned_cells(&single_repo_policy(), "nightly", "base", None, None).unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].name, "base");
        assert!(cells[0].source.is_none() && cells[0].repository.is_none());
        // A repository restriction naming the single repo is accepted and
        // changes nothing.
        let restricted =
            planned_cells(&single_repo_policy(), "nightly", "base", None, Some("nas")).unwrap();
        assert_eq!(restricted, cells);
    }

    #[test]
    fn planned_cells_multi_repo_crosses_and_restricts() {
        let all = planned_cells(&multi_repo_policy(), "nightly", "base", None, None).unwrap();
        assert_eq!(all.len(), 2, "one child per repository");
        assert!(all.iter().all(|c| c.repository.is_some()));

        let only_nas =
            planned_cells(&multi_repo_policy(), "nightly", "base", None, Some("nas")).unwrap();
        assert_eq!(only_nas.len(), 1);
        assert_eq!(only_nas[0].repository.as_ref().unwrap().name, "nas");
    }

    #[test]
    fn planned_cells_refuses_an_unknown_repository_listing_valid_ones() {
        let err =
            planned_cells(&multi_repo_policy(), "nightly", "base", None, Some("typo")).unwrap_err();
        match err {
            OpsError::UnknownPolicyRepository {
                given,
                policy,
                valid,
            } => {
                assert_eq!(given, "typo");
                assert_eq!(policy, "nightly");
                assert_eq!(valid, "nas, offsite");
            }
            other => panic!("expected UnknownPolicyRepository, got {other:?}"),
        }
    }

    #[test]
    fn a_pinned_cell_lands_spec_repository_on_the_wire() {
        let cells = planned_cells(&multi_repo_policy(), "nightly", "base", None, None).unwrap();
        let nas_cell = cells
            .iter()
            .find(|c| c.repository.as_ref().unwrap().name == "nas")
            .unwrap();
        let snap = build_snapshot_for(&req(), "media", at(), Some(nas_cell));
        let wire = serde_json::to_value(&snap).unwrap();
        assert_eq!(wire["metadata"]["name"], nas_cell.name);
        // The pin is stamped NORMALIZED: the namespaced member carries the
        // policy's namespace explicitly.
        assert_eq!(
            wire["spec"]["repository"],
            serde_json::json!({ "kind": "Repository", "name": "nas", "namespace": "media" })
        );
        // Manual children keep the manual origin + config labels.
        assert_eq!(
            wire["metadata"]["labels"]["kopiur.home-operations.com/origin"],
            "manual"
        );
    }
}
