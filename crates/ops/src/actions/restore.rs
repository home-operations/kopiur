//! Builds the `Restore` CR a restore run applies.
//!
//! The invocation is a [`RestoreRequest`]: exactly one source (snapshot /
//! policy / raw identity) into exactly one target (created PVC / existing PVC /
//! populator), expressed with the CRD's own externally-tagged enums so the
//! exactly-one-of invariant is unrepresentable-if-wrong rather than validated.
//! `kubectl kopiur restore` maps its clap groups onto it; a web UI maps a form.

use chrono::{DateTime, Utc};
use kopiur_api::common::{CredentialProjection, FailurePolicy, RepositoryRef};
use kopiur_api::restore::{RestoreOptions, RestorePolicy, RestoreSpec};
use kopiur_api::{Restore, RestorePhase, RestoreSource, RestoreTarget};
use kube::api::{Api, PostParams};

use crate::actions::snapshot::human_bytes;
use crate::ctx::OpsCtx;
use crate::error::{OpsError, classify_kube};

/// One restore invocation: the exactly-one-of source/target pair plus the
/// optional policy sub-objects, all already in CRD shape.
///
/// `PartialEq` only, never `Eq`: `RestoreTarget::Pvc` reaches k8s-openapi types
/// that are `PartialEq` alone (api-conventions #2).
#[derive(Clone, Debug, PartialEq)]
pub struct RestoreRequest {
    /// Where the data comes from.
    pub source: RestoreSource,
    /// Where it lands.
    pub target: RestoreTarget,
    /// The repository holding the snapshot; required for a raw-identity
    /// source, otherwise derived by the operator from the source.
    pub repository: Option<RepositoryRef>,
    /// kopia restore tuning; `None` leaves the operator's defaults.
    pub options: Option<RestoreOptions>,
    /// Missing-snapshot behavior and the source-wait budget; `None` leaves the
    /// operator's defaults.
    pub policy: Option<RestorePolicy>,
    /// Project the repository's credential Secret(s) into the mover Job's
    /// namespace (gated by the owning `ClusterRepository`).
    pub credential_projection: bool,
    /// Mover `Job` failure controls; `None` leaves the operator's defaults.
    pub failure_policy: Option<FailurePolicy>,
    /// Name for the created `Restore`; `None` derives
    /// `restore-<source>-<timestamp>`.
    pub name: Option<String>,
}

/// The short source token used in the default `Restore` name. Exhaustive over
/// [`RestoreSource`] — a new source kind cannot compile until it decides how it
/// names itself.
fn source_token(source: &RestoreSource) -> &str {
    match source {
        RestoreSource::SnapshotRef(snapshot) => &snapshot.name,
        RestoreSource::FromPolicy(policy) => &policy.name,
        RestoreSource::Identity(identity) => &identity.username,
    }
}

/// Build the `Restore` CR from the request. Pure — `now` is injected so names
/// are deterministic under test. Every field of the request lands on the wire
/// (no field may be dropped — the restore-options bug class is regression-
/// tested below).
pub fn build_restore(req: &RestoreRequest, namespace: &str, now: DateTime<Utc>) -> Restore {
    let name = req.name.clone().unwrap_or_else(|| {
        format!(
            "restore-{}-{}",
            source_token(&req.source),
            now.format("%Y%m%d%H%M%S")
        )
    });
    let mut restore = Restore::new(
        &name,
        RestoreSpec {
            repository: req.repository.clone(),
            source: req.source.clone(),
            target: req.target.clone(),
            options: req.options.clone(),
            policy: req.policy.clone(),
            credential_projection: req
                .credential_projection
                .then_some(CredentialProjection { enabled: true }),
            mover: None,
            failure_policy: req.failure_policy.clone(),
        },
    );
    restore.metadata.namespace = Some(namespace.to_string());
    restore
}

/// Create the `Restore`, returning what the API server stored.
pub async fn create_restore(
    ctx: &OpsCtx,
    namespace: &str,
    restore: Restore,
) -> Result<Restore, OpsError> {
    let api: Api<Restore> = Api::namespaced(ctx.client.clone(), namespace);
    let name = restore.metadata.name.clone().expect("name set by builder");
    api.create(&PostParams::default(), &restore)
        .await
        .map_err(|e| {
            classify_kube(
                "create",
                "Restore",
                "restores",
                Some(namespace),
                Some(&name),
                e,
            )
        })
}

/// Terminal-phase classification. Exhaustive over [`RestorePhase`].
pub fn terminal(restore: &Restore) -> Option<Result<Box<Restore>, Box<Restore>>> {
    match restore.status.as_ref().and_then(|s| s.phase.as_ref())? {
        RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring => None,
        RestorePhase::Completed => Some(Ok(Box::new(restore.clone()))),
        RestorePhase::Failed => Some(Err(Box::new(restore.clone()))),
        // Never a terminal answer: `--wait` keeps waiting (bounded by its own
        // timeout) rather than exiting 0 or 1 on a phase it cannot interpret.
        RestorePhase::Unknown(_) => None,
    }
}

/// What one claim's pin says it restored, for the success summary: the kopia
/// snapshot id, or "empty volume" for a pinned deploy-or-restore `NoSnapshot`
/// decision, or `?` when nothing was recorded. Pure.
fn pinned_outcome(resolved: Option<&kopiur_api::restore::ResolvedRestore>) -> String {
    match resolved.and_then(|r| r.resolution) {
        Some(kopiur_api::ResolutionOutcome::NoSnapshot) => "empty volume (no snapshot)".into(),
        Some(kopiur_api::ResolutionOutcome::Snapshot) | None => resolved
            .and_then(|r| r.kopia_snapshot_id.as_deref())
            .map(|id| format!("kopia id {id}"))
            .unwrap_or_else(|| "kopia id ?".into()),
    }
}

/// One-line success summary from the terminal object's status.
///
/// A populator's state lives PER CLAIM under `status.claims.<pvc>` (#443), so
/// a fanned-out restore lists each claim's own kopia id and target rather than
/// the top-level mirror — which is absent with several claims and would read
/// `kopia id ?` (review wave 2, finding 8). A direct restore (no claims) keeps
/// the classic top-level shape.
pub fn success_summary(restore: &Restore) -> String {
    let status = restore.status.as_ref();
    let name = restore.metadata.name.as_deref().unwrap_or("?");
    let claims = status.map(|s| &s.claims).filter(|c| !c.is_empty());
    if let Some(claims) = claims {
        let per_claim: Vec<String> = claims
            .iter()
            .map(|(pvc, claim)| format!("pvc/{pvc}: {}", pinned_outcome(claim.resolved.as_ref())))
            .collect();
        let n = claims.len();
        return format!(
            "restore {name} completed: {n} claim{} — {}\n",
            if n == 1 { "" } else { "s" },
            per_claim.join("; ")
        );
    }
    let id = pinned_outcome(status.and_then(|s| s.resolved.as_ref()));
    let bytes = status
        .and_then(|s| s.progress.as_ref())
        .and_then(|p| p.bytes_restored)
        .map(human_bytes)
        .unwrap_or_else(|| "?".into());
    let files = status
        .and_then(|s| s.progress.as_ref())
        .and_then(|p| p.files_restored)
        .map(|f| f.to_string())
        .unwrap_or_else(|| "?".into());
    let target = status
        .and_then(|s| s.target.as_ref())
        .and_then(|t| t.pvc_ref.as_ref())
        .map(|p| format!("pvc/{}", p.name))
        .unwrap_or_else(|| "target".into());
    format!("restore {name} completed: {id}, {bytes} / {files} files into {target}\n")
}

/// Render one failure block + log tail pair (the mover's last words), shared by
/// the direct and per-claim shapes. Pure.
fn push_failure_and_tail(
    out: &mut String,
    failure: Option<&kopiur_api::common::FailureBlock>,
    log_tail: Option<&str>,
) {
    if let Some(f) = failure {
        out.push_str(&format!(" ({}): {}", f.kopia_error_class, f.message));
        if let Some(stderr) = &f.stderr_tail {
            out.push_str(&format!("\n--- kopia stderr tail ---\n{stderr}"));
        }
    }
    if let Some(tail) = log_tail {
        out.push_str(&format!("\n--- log tail ---\n{tail}"));
    }
}

/// Failure detail from the terminal object's status, for stderr.
///
/// Per claim for a populator (#443, review wave 2 finding 8): with ONE claim
/// the detail is that claim's own `failure`/`logTail`; with several, a
/// per-claim list ("claim <pvc>: <reason> — <message>") with each failed
/// claim's failure block and tail beneath it, so the user sees WHICH claim
/// stalled the restore. A direct restore reads the top-level fields.
pub fn failure_detail(restore: &Restore) -> String {
    let name = restore.metadata.name.as_deref().unwrap_or("?");
    let status = restore.status.as_ref();
    let mut out = format!("restore {name} failed");
    let claims = status.map(|s| &s.claims).filter(|c| !c.is_empty());
    match claims {
        None => push_failure_and_tail(
            &mut out,
            status.and_then(|s| s.failure.as_ref()),
            status.and_then(|s| s.log_tail.as_deref()),
        ),
        Some(claims) if claims.len() == 1 => {
            let (pvc, claim) = claims.iter().next().expect("one claim");
            out.push_str(&format!(" (claim {pvc})"));
            push_failure_and_tail(&mut out, claim.failure.as_ref(), claim.log_tail.as_deref());
        }
        Some(claims) => {
            for (pvc, claim) in claims {
                let phase = claim
                    .phase
                    .as_ref()
                    .map(|p| serde_json::to_value(p).ok())
                    .and_then(|v| v.and_then(|v| v.as_str().map(str::to_string)))
                    .unwrap_or_else(|| "?".into());
                out.push_str(&format!(
                    "\nclaim {pvc}: {phase}/{} — {}",
                    claim.reason.as_deref().unwrap_or("?"),
                    claim.message.as_deref().unwrap_or("")
                ));
                let mut detail = String::new();
                push_failure_and_tail(
                    &mut detail,
                    claim.failure.as_ref(),
                    claim.log_tail.as_deref(),
                );
                if !detail.is_empty() {
                    out.push_str(&format!("\n  claim {pvc} detail{detail}"));
                }
            }
        }
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use kopiur_api::common::RepositoryKind;
    use kopiur_api::common::{ObjectRef, PvcAccessMode};
    use kopiur_api::restore::{FromPolicy, IdentitySource, PvcTemplate};
    use kopiur_api::{OnMissingSnapshot, PopulatorTarget};

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 11, 3, 0, 12).unwrap()
    }

    /// A bare snapshot-into-existing-PVC request; each test overrides the one
    /// field it is about.
    fn req() -> RestoreRequest {
        RestoreRequest {
            source: RestoreSource::SnapshotRef(ObjectRef {
                name: "snap1".into(),
                namespace: None,
            }),
            target: RestoreTarget::PvcRef(ObjectRef {
                name: "data".into(),
                namespace: None,
            }),
            repository: None,
            options: None,
            policy: None,
            credential_projection: false,
            failure_policy: None,
            name: None,
        }
    }

    #[test]
    fn every_source_target_combination_builds_the_right_enums() {
        let sources = [
            RestoreSource::SnapshotRef(ObjectRef {
                name: "snap1".into(),
                namespace: None,
            }),
            RestoreSource::FromPolicy(FromPolicy {
                name: "pol1".into(),
                namespace: None,
                as_of: None,
                offset: 0,
                source_path: None,
            }),
            RestoreSource::Identity(IdentitySource {
                username: "u".into(),
                hostname: "h".into(),
                source_path: Some("/data".into()),
                snapshot_id: None,
                as_of: None,
                offset: None,
            }),
        ];
        let targets = [
            RestoreTarget::PvcRef(ObjectRef {
                name: "existing".into(),
                namespace: None,
            }),
            RestoreTarget::Pvc(PvcTemplate {
                name: "fresh".into(),
                storage_class_name: None,
                capacity: Some("1Gi".into()),
                access_modes: vec![],
            }),
            RestoreTarget::Populator(PopulatorTarget {}),
        ];
        for source in &sources {
            for target in &targets {
                let r = RestoreRequest {
                    source: source.clone(),
                    target: target.clone(),
                    ..req()
                };
                let restore = build_restore(&r, "media", at());
                assert_eq!(restore.spec.source.kind_str(), source.kind_str());
                assert_eq!(restore.spec.target.kind_str(), target.kind_str());
                // Round-trip through JSON (the cluster's encoding).
                let wire = serde_json::to_value(&restore).unwrap();
                let reparsed: Restore = serde_json::from_value(wire).unwrap();
                assert_eq!(reparsed.spec, restore.spec);
            }
        }
    }

    #[test]
    fn the_default_name_tokenizes_every_source_kind() {
        for (source, expected) in [
            (
                RestoreSource::SnapshotRef(ObjectRef {
                    name: "snap1".into(),
                    namespace: None,
                }),
                "restore-snap1-20260611030012",
            ),
            (
                RestoreSource::FromPolicy(FromPolicy {
                    name: "pol1".into(),
                    namespace: None,
                    as_of: None,
                    offset: 0,
                    source_path: None,
                }),
                "restore-pol1-20260611030012",
            ),
            (
                RestoreSource::Identity(IdentitySource {
                    username: "pg".into(),
                    hostname: "media".into(),
                    source_path: None,
                    snapshot_id: None,
                    as_of: None,
                    offset: None,
                }),
                "restore-pg-20260611030012",
            ),
        ] {
            let r = RestoreRequest { source, ..req() };
            let restore = build_restore(&r, "media", at());
            assert_eq!(restore.metadata.name.as_deref(), Some(expected));
        }
    }

    /// A request with EVERY field set, for the two "no field dropped" sweeps.
    /// They are split so the second one's 13 option asserts don't compound the
    /// first's cognitive complexity.
    fn full_request() -> RestoreRequest {
        RestoreRequest {
            source: RestoreSource::FromPolicy(FromPolicy {
                name: "pol1".into(),
                namespace: Some("other".into()),
                as_of: Some("2026-06-01T00:00:00Z".into()),
                offset: 2,
                source_path: Some("/pvc/postgres-data".into()),
            }),
            target: RestoreTarget::Pvc(PvcTemplate {
                name: "fresh".into(),
                storage_class_name: Some("fast".into()),
                capacity: Some("10Gi".into()),
                access_modes: vec![PvcAccessMode::ReadWriteOnce, PvcAccessMode::ReadOnlyMany],
            }),
            repository: Some(RepositoryRef {
                kind: RepositoryKind::ClusterRepository,
                name: "nas".into(),
                namespace: None,
            }),
            options: Some(RestoreOptions {
                enable_file_deletion: true,
                ignore_permission_errors: Some(false),
                write_files_atomically: Some(true),
                parallel: Some(4),
                write_sparse_files: Some(true),
                skip_owners: Some(true),
                skip_permissions: Some(false),
                skip_times: Some(true),
                overwrite_files: Some(false),
                overwrite_directories: Some(false),
                overwrite_symlinks: Some(true),
                ignore_errors: Some(false),
                skip_existing: Some(true),
            }),
            policy: Some(RestorePolicy {
                on_missing_snapshot: Some(OnMissingSnapshot::Continue),
                wait_timeout: Some("5m".into()),
            }),
            credential_projection: true,
            failure_policy: Some(FailurePolicy {
                backoff_limit: Some(1),
                active_deadline_seconds: Some(600),
                pod_startup_deadline_seconds: Some(300),
            }),
            name: Some("my-restore".into()),
        }
    }

    #[test]
    fn every_request_field_lands_in_the_spec() {
        // The restore-options-dropped bug class: EVERY field must round-trip.
        let wire = serde_json::to_value(build_restore(&full_request(), "media", at())).unwrap();
        assert_eq!(wire["metadata"]["name"], "my-restore");
        assert_eq!(wire["metadata"]["namespace"], "media");
        let spec = &wire["spec"];
        assert_eq!(spec["source"]["fromPolicy"]["name"], "pol1");
        assert_eq!(spec["source"]["fromPolicy"]["namespace"], "other");
        assert_eq!(spec["source"]["fromPolicy"]["asOf"], "2026-06-01T00:00:00Z");
        assert_eq!(spec["source"]["fromPolicy"]["offset"], 2);
        assert_eq!(
            spec["source"]["fromPolicy"]["sourcePath"],
            "/pvc/postgres-data"
        );
        assert_eq!(spec["target"]["pvc"]["name"], "fresh");
        assert_eq!(spec["target"]["pvc"]["capacity"], "10Gi");
        assert_eq!(spec["target"]["pvc"]["storageClassName"], "fast");
        assert_eq!(
            spec["target"]["pvc"]["accessModes"],
            serde_json::json!(["ReadWriteOnce", "ReadOnlyMany"])
        );
        assert_eq!(spec["policy"]["onMissingSnapshot"], "Continue");
        assert_eq!(spec["policy"]["waitTimeout"], "5m");
        assert_eq!(spec["failurePolicy"]["backoffLimit"], 1);
        assert_eq!(spec["failurePolicy"]["activeDeadlineSeconds"], 600);
        assert_eq!(spec["failurePolicy"]["podStartupDeadlineSeconds"], 300);
        assert_eq!(spec["repository"]["kind"], "ClusterRepository");
        assert_eq!(spec["repository"]["name"], "nas");
        assert_eq!(spec["credentialProjection"]["enabled"], true);
    }

    /// The same sweep for the 13 kopia restore options, split into its own test
    /// so its assert count doesn't compound the one above's.
    #[test]
    fn every_restore_option_lands_in_the_spec() {
        let wire = serde_json::to_value(build_restore(&full_request(), "media", at())).unwrap();
        let opts = &wire["spec"]["options"];
        assert_eq!(opts["enableFileDeletion"], true);
        assert_eq!(opts["ignorePermissionErrors"], false);
        assert_eq!(opts["writeFilesAtomically"], true);
        assert_eq!(opts["parallel"], 4);
        assert_eq!(opts["writeSparseFiles"], true);
        assert_eq!(opts["skipOwners"], true);
        assert_eq!(opts["skipPermissions"], false);
        assert_eq!(opts["skipTimes"], true);
        assert_eq!(opts["overwriteFiles"], false);
        assert_eq!(opts["overwriteDirectories"], false);
        assert_eq!(opts["overwriteSymlinks"], true);
        assert_eq!(opts["ignoreErrors"], false);
        assert_eq!(opts["skipExisting"], true);
    }

    #[test]
    fn minimal_restore_has_no_optional_noise_on_the_wire() {
        let wire = serde_json::to_value(build_restore(&req(), "media", at())).unwrap();
        assert_eq!(wire["metadata"]["name"], "restore-snap1-20260611030012");
        let spec = &wire["spec"];
        assert_eq!(spec["source"]["snapshotRef"]["name"], "snap1");
        assert_eq!(spec["target"]["pvcRef"]["name"], "data");
        for key in [
            "options",
            "policy",
            "failurePolicy",
            "repository",
            "mover",
            "credentialProjection",
        ] {
            assert!(spec.get(key).is_none(), "{key} should be absent");
        }
    }

    #[test]
    fn identity_snapshot_id_lands_with_the_adr_capitalization() {
        let r = RestoreRequest {
            source: RestoreSource::Identity(IdentitySource {
                username: "u".into(),
                hostname: "h".into(),
                source_path: Some("/p".into()),
                snapshot_id: Some("abc123".into()),
                as_of: None,
                offset: None,
            }),
            repository: Some(RepositoryRef {
                kind: RepositoryKind::Repository,
                name: "nas".into(),
                namespace: None,
            }),
            ..req()
        };
        let wire = serde_json::to_value(build_restore(&r, "media", at())).unwrap();
        assert_eq!(wire["spec"]["source"]["identity"]["snapshotID"], "abc123");
        assert_eq!(wire["spec"]["source"]["identity"]["username"], "u");
        assert_eq!(wire["spec"]["source"]["identity"]["sourcePath"], "/p");
    }

    fn with_phase(phase: &str) -> Restore {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": "r", "namespace": "media" },
            "spec": {
                "source": { "snapshotRef": { "name": "s" } },
                "target": { "pvcRef": { "name": "d" } }
            },
            "status": { "phase": phase }
        }))
        .unwrap()
    }

    #[test]
    fn terminal_classification_is_exhaustive_and_correct() {
        for pending in ["Pending", "Resolving", "Restoring"] {
            assert!(terminal(&with_phase(pending)).is_none(), "{pending}");
        }
        assert!(matches!(terminal(&with_phase("Completed")), Some(Ok(_))));
        assert!(matches!(terminal(&with_phase("Failed")), Some(Err(_))));
    }

    /// A fanned-out populator (#443) with two claims: one restored, one
    /// deploy-or-restore'd empty. Reused by the success and failure renderers.
    fn two_claims(phase: &str, claims: serde_json::Value) -> Restore {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": "app" },
            "spec": {
                "source": { "fromPolicy": { "name": "cfg" } },
                "target": { "populator": {} }
            },
            "status": { "phase": phase, "claims": claims }
        }))
        .unwrap()
    }

    /// Review wave 2, finding 8: this layer reads the PER-CLAIM records. With
    /// several claims the top-level `resolved` is absent by design, so the old
    /// renderer printed `kopia id ?`; now each claim's own id and target is listed.
    #[test]
    fn success_summary_lists_every_claims_own_id_and_target() {
        let restore = two_claims(
            "Completed",
            serde_json::json!({
                "data": {
                    "phase": "Populated", "reason": "RestoreSucceeded",
                    "resolved": { "resolution": "Snapshot", "kopiaSnapshotID": "abc123" }
                },
                "logs": {
                    "phase": "Populated", "reason": "NoSnapshotContinue",
                    "resolved": { "resolution": "NoSnapshot" }
                }
            }),
        );
        assert_eq!(
            success_summary(&restore),
            "restore app completed: 2 claims — pvc/data: kopia id abc123; \
             pvc/logs: empty volume (no snapshot)\n"
        );
        // One claim: still its OWN record, never `kopia id ?`.
        let one = two_claims(
            "Completed",
            serde_json::json!({
                "data": {
                    "phase": "Populated", "reason": "RestoreSucceeded",
                    "resolved": { "resolution": "Snapshot", "kopiaSnapshotID": "abc123" }
                }
            }),
        );
        assert_eq!(
            success_summary(&one),
            "restore app completed: 1 claim — pvc/data: kopia id abc123\n"
        );
    }

    /// Review wave 2, finding 8: a failed fan-out names WHICH claim failed, with
    /// that claim's failure block and log tail; the healthy sibling is listed
    /// without detail. One claim reads its own record directly.
    #[test]
    fn failure_detail_is_per_claim() {
        let restore = two_claims(
            "Failed",
            serde_json::json!({
                "data": {
                    "phase": "Populated", "reason": "RestoreSucceeded",
                    "message": "restored"
                },
                "logs": {
                    "phase": "Failed", "reason": "MoverJobFailed",
                    "message": "the populator restore mover Job `app-populate-deadbeef` failed",
                    "failure": {
                        "kopiaErrorClass": "PermissionDenied",
                        "message": "kopia: cannot write /data",
                        "retryRecommended": false,
                        "stderrTail": "ERROR permission denied"
                    },
                    "logTail": "mover: restore failed"
                }
            }),
        );
        let text = failure_detail(&restore);
        assert_eq!(
            text,
            "restore app failed\n\
             claim data: Populated/RestoreSucceeded — restored\n\
             claim logs: Failed/MoverJobFailed — the populator restore mover Job \
             `app-populate-deadbeef` failed\n  \
             claim logs detail (PermissionDenied): kopia: cannot write /data\n\
             --- kopia stderr tail ---\nERROR permission denied\n\
             --- log tail ---\nmover: restore failed\n"
        );

        let one = two_claims(
            "Failed",
            serde_json::json!({
                "logs": {
                    "phase": "Failed", "reason": "MoverJobFailed",
                    "failure": {
                        "kopiaErrorClass": "PermissionDenied",
                        "message": "kopia: cannot write /data",
                        "retryRecommended": false
                    },
                    "logTail": "mover: restore failed"
                }
            }),
        );
        assert_eq!(
            failure_detail(&one),
            "restore app failed (claim logs) (PermissionDenied): kopia: cannot write /data\n\
             --- log tail ---\nmover: restore failed\n"
        );

        // A direct restore keeps reading the top-level fields.
        let direct: Restore = serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": "r" },
            "spec": {
                "source": { "snapshotRef": { "name": "s" } },
                "target": { "pvcRef": { "name": "d" } }
            },
            "status": {
                "phase": "Failed",
                "failure": { "kopiaErrorClass": "AuthFailure", "message": "bad password",
                             "retryRecommended": false },
                "logTail": "tail"
            }
        }))
        .unwrap();
        assert_eq!(
            failure_detail(&direct),
            "restore r failed (AuthFailure): bad password\n--- log tail ---\ntail\n"
        );
    }

    #[test]
    fn success_summary_reports_id_bytes_files_and_target() {
        let restore: Restore = serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": "r" },
            "spec": {
                "source": { "snapshotRef": { "name": "s" } },
                "target": { "pvcRef": { "name": "d" } }
            },
            "status": {
                "phase": "Completed",
                "resolved": { "kopiaSnapshotID": "abc123" },
                "progress": { "bytesRestored": 1536, "filesRestored": 12 },
                "target": { "pvcRef": { "name": "d" } }
            }
        }))
        .unwrap();
        assert_eq!(
            success_summary(&restore),
            "restore r completed: kopia id abc123, 1.5 KiB / 12 files into pvc/d\n"
        );
    }
}
