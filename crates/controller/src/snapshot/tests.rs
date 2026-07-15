use super::*;
use crate::consts::SKIP_SNAPSHOT_CLEANUP_ANNOTATION;
use kopiur_api::common::NamespaceDeletePolicy;

#[test]
fn terminal_steady_requeue_is_bounded_reconcile_qps_relief() {
    // Issue #249: terminal Snapshots re-reconcile on this interval for their whole
    // retention window, so it must stay well above the old 600s (QPS relief) but
    // under an hour (a terminal CR still self-checks a couple of times per hour).
    let secs = TERMINAL_SNAPSHOT_STEADY_REQUEUE.as_secs();
    assert!(
        (30 * 60..=60 * 60).contains(&secs),
        "terminal requeue {secs}s must be 30–60 min (was 600s before #249)"
    );
}

fn ann(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// --- §11 ReadOnly gate: ReadOnly refuses backups; restores allowed elsewhere ---

#[test]
fn readonly_repository_refuses_backup_writes() {
    use kopiur_api::common::RepositoryMode;
    // The pure gate the reconciler branches on.
    assert!(!RepositoryMode::ReadOnly.allows_writes());
    assert!(RepositoryMode::ReadWrite.allows_writes());
    // The refusal message names the repo, the mode, and how to fix.
    let msg = readonly_backup_message("nas");
    assert!(msg.contains("nas"));
    assert!(msg.contains("ReadOnly"));
    assert!(msg.contains("ReadWrite"));
}

// --- Readiness gate: a not-Ready repository holds the backup in Pending ---

#[test]
fn not_ready_repository_holds_backup_pending() {
    use crate::consts::REPOSITORY_NOT_READY_REASON;
    use kopiur_api::common::PhaseLabel;
    use kopiur_api::snapshot::SnapshotPhase;

    // Pending is Reconciling (not Stalled), so the backup resumes on reconnect.
    assert_eq!(
        snapshot_ready_outcome(SnapshotPhase::Pending),
        io::ReadyOutcome::Reconciling
    );
    assert_eq!(SnapshotPhase::Pending.label(), "Pending");
    // The hold message names the repo; it's a wait, not a refusal.
    let msg = repository_not_ready_message("nas");
    assert!(msg.contains("nas"));
    assert!(msg.contains("Ready"));
    assert!(!msg.contains("refusing"));
    assert_eq!(REPOSITORY_NOT_READY_REASON, "RepositoryNotReady");
}

// --- §13(c) pin decision (pure: spec.pin vs observed → pin/unpin/noop) ---

#[test]
fn pin_decision_covers_every_case() {
    // Desired pinned, never reconciled → apply the pin.
    assert_eq!(pin_decision(true, None), PinAction::Pin);
    // Desired pinned, observed unpinned → apply.
    assert_eq!(pin_decision(true, Some(false)), PinAction::Pin);
    // Desired pinned, already pinned → no-op (never issue a redundant pin).
    assert_eq!(pin_decision(true, Some(true)), PinAction::NoOp);
    // Desired unpinned, currently pinned → remove.
    assert_eq!(pin_decision(false, Some(true)), PinAction::Unpin);
    // Desired unpinned, never pinned / observed unpinned → no-op.
    assert_eq!(pin_decision(false, None), PinAction::NoOp);
    assert_eq!(pin_decision(false, Some(false)), PinAction::NoOp);
}

// --- one-shot run decision: terminal phases never mint another mover Job ---
// Regression guard for the TTL-reap loop: every Snapshot in a live cluster
// re-ran its backup each `ttlSecondsAfterFinished` because the reconciler
// keyed "work is done" on the (self-reaping) Job's existence, not the phase.

#[test]
fn run_decision_covers_every_phase() {
    use kopiur_api::snapshot::SnapshotPhase;
    // Not started / in flight → drive the Job.
    assert_eq!(run_decision(None), RunDecision::Run);
    assert_eq!(run_decision(Some(SnapshotPhase::Pending)), RunDecision::Run);
    assert_eq!(run_decision(Some(SnapshotPhase::Running)), RunDecision::Run);
    // Succeeded → steady state (pin/staged only); NEVER a new mover Job.
    assert_eq!(
        run_decision(Some(SnapshotPhase::Succeeded)),
        RunDecision::SucceededSteadyState
    );
    // Failed → terminal until the spec changes; no TTL-driven retry loop.
    assert_eq!(
        run_decision(Some(SnapshotPhase::Failed)),
        RunDecision::TerminalFailed
    );
    // Phases owned by earlier gates → wait, don't act on a desynced view.
    assert_eq!(
        run_decision(Some(SnapshotPhase::Deleting)),
        RunDecision::Wait
    );
    assert_eq!(
        run_decision(Some(SnapshotPhase::Discovered)),
        RunDecision::Wait
    );
}

#[test]
fn should_run_preflight_only_at_first_launch() {
    use kopiur_api::snapshot::SnapshotPhase;
    // First launch: gate runs.
    assert!(should_run_preflight(None));
    assert!(should_run_preflight(Some(SnapshotPhase::Pending)));
    // A Running snapshot whose Job vanished resumes — preflight must NOT re-gate it.
    assert!(!should_run_preflight(Some(SnapshotPhase::Running)));
    // Terminal/other phases never reach the gate, but be explicit.
    assert!(!should_run_preflight(Some(SnapshotPhase::Succeeded)));
    assert!(!should_run_preflight(Some(SnapshotPhase::Failed)));
}

#[test]
fn preflight_expired_anchors_on_since_and_honors_indefinite() {
    use chrono::{DateTime, Duration as ChronoDuration};
    use std::time::Duration;
    let t0 = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let since = t0.to_rfc3339();
    let timeout = Some(Duration::from_secs(600));
    // Not yet elapsed.
    assert!(!preflight_expired(
        Some(&since),
        timeout,
        t0 + ChronoDuration::seconds(300)
    ));
    // Exactly/over the deadline → expired.
    assert!(preflight_expired(
        Some(&since),
        timeout,
        t0 + ChronoDuration::seconds(600)
    ));
    assert!(preflight_expired(
        Some(&since),
        timeout,
        t0 + ChronoDuration::seconds(9000)
    ));
    // No anchor yet (failure just started) → never expired.
    assert!(!preflight_expired(
        None,
        timeout,
        t0 + ChronoDuration::seconds(9000)
    ));
    // Indefinite (timeout None, e.g. spec `timeout: 0`) → never expired.
    assert!(!preflight_expired(
        Some(&since),
        None,
        t0 + ChronoDuration::seconds(9_000_000)
    ));
}

// --- §2 phase → Ready mapping ---

#[test]
fn snapshot_ready_outcome_maps_every_phase() {
    use kopiur_api::snapshot::SnapshotPhase;
    assert_eq!(
        snapshot_ready_outcome(SnapshotPhase::Succeeded),
        io::ReadyOutcome::Ready
    );
    assert_eq!(
        snapshot_ready_outcome(SnapshotPhase::Discovered),
        io::ReadyOutcome::Ready
    );
    assert_eq!(
        snapshot_ready_outcome(SnapshotPhase::Failed),
        io::ReadyOutcome::Stalled
    );
    for p in [
        SnapshotPhase::Pending,
        SnapshotPhase::Running,
        SnapshotPhase::Deleting,
    ] {
        assert_eq!(snapshot_ready_outcome(p), io::ReadyOutcome::Reconciling);
    }
}

#[test]
fn policy_args_for_threads_flattened_knobs() {
    // §13(b)/§13(f) end-to-end at the controller seam: the SnapshotPolicy policy
    // knobs become non-empty work-spec policy args.
    use kopiur_api::snapshot_policy::{Compression, ErrorHandling};
    let mut sp = sample_policy();
    sp.spec.compression = Some(Compression {
        compressor: Some("zstd".into()),
        never_compress: vec![],
    });
    sp.spec.error_handling = Some(ErrorHandling {
        ignore_file_errors: true,
        ..Default::default()
    });
    let p = policy_args_for(&sp);
    assert!(!p.is_empty());
    assert_eq!(p.compression.as_deref(), Some("zstd"));
    assert_eq!(p.ignore_file_errors, Some(true));
}

fn sample_policy() -> kopiur_api::SnapshotPolicy {
    kopiur_api::SnapshotPolicy::new(
        "pg",
        kopiur_api::SnapshotPolicySpec {
            repository: kopiur_api::common::RepositoryRef {
                kind: Default::default(),
                name: "r".into(),
                namespace: None,
            },
            identity: None,
            sources: vec![],
            copy_method: Default::default(),
            volume_snapshot_class_name: None,
            staging: None,
            group_by: None,
            retention: None,
            default_deletion_policy: None,
            compression: None,
            files: None,
            extra_args: vec![],
            error_handling: None,
            upload: None,
            verification: None,
            preflight: None,
            suspend: false,
            hooks: None,
            mover: None,
            credential_projection: None,
        },
    )
}

// --- delete_projection_enabled (cascade never projects, #231) ------------

#[test]
fn delete_projection_follows_the_recipe_in_the_snapshots_own_namespace() {
    let mut policy = sample_policy();
    policy.spec.credential_projection =
        Some(kopiur_api::common::CredentialProjection { enabled: true });
    assert!(delete_projection_enabled(false, Some(&policy)));
    policy.spec.credential_projection = None;
    assert!(!delete_projection_enabled(false, Some(&policy)));
    assert!(
        !delete_projection_enabled(false, None),
        "gone recipe defaults off"
    );
}

#[test]
fn delete_projection_is_hard_off_for_the_cross_namespace_cascade() {
    // Even a still-live recipe with the opt-in set must not project on the
    // cascade path: the copy would be owned by the repository CR, which can be
    // cluster-scoped — an invalid ownerRef on a namespaced Secret, never GC'd.
    let mut policy = sample_policy();
    policy.spec.credential_projection =
        Some(kopiur_api::common::CredentialProjection { enabled: true });
    assert!(!delete_projection_enabled(true, Some(&policy)));
}

// backend_to_repository_connect's exhaustive every-variant test moved with
// the fn to `kopiur_mover::repo_meta`.

// --- build_backup_run: the source volume (PVC vs inline NFS) glue ----------

fn resolved_s3_repo() -> io::ResolvedRepository {
    use kopiur_api::backend::S3Backend;
    use kopiur_api::common::{Encryption, SecretKeyRef};
    io::ResolvedRepository {
        // An object-store repo so there is no repo volume to mount — isolates
        // the SOURCE-volume assertion.
        backend: Backend::S3(S3Backend {
            bucket: "b".into(),
            prefix: None,
            endpoint: None,
            region: None,
            auth: None,
            tls: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: "creds".into(),
                namespace: None,
                key: Some("KOPIA_PASSWORD".into()),
            },
        },
        repo_namespace: Some("media-ns".into()),
        mover_defaults: None,
        identity_defaults: None,
        schedule_defaults: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        credential_projection_allowed: false,
        owner_ref: Default::default(),
    }
}

fn config_with_source(name: &str, source: kopiur_api::snapshot_policy::Source) -> SnapshotPolicy {
    use kopiur_api::common::{RepositoryKind, RepositoryRef};
    use kopiur_api::snapshot_policy::SnapshotPolicySpec;
    SnapshotPolicy::new(
        name,
        SnapshotPolicySpec {
            repository: RepositoryRef {
                kind: RepositoryKind::Repository,
                name: "repo".into(),
                namespace: None,
            },
            identity: None,
            sources: vec![source],
            copy_method: Default::default(),
            volume_snapshot_class_name: None,
            staging: None,
            group_by: None,
            retention: None,
            default_deletion_policy: None,
            compression: None,
            files: None,
            extra_args: vec![],
            error_handling: None,
            upload: None,
            verification: None,
            preflight: None,
            suspend: false,
            hooks: None,
            mover: None,
            credential_projection: None,
        },
    )
}

fn dummy_backup() -> Snapshot {
    Snapshot::new(
        "b1",
        kopiur_api::snapshot::SnapshotSpec {
            policy_ref: None,
            tags: None,
            failure_policy: None,
            deletion_policy: None,
            pin: false,
            description: None,
        },
    )
}

#[test]
fn build_backup_run_maps_nfs_source_to_inline_nfs_mount() {
    use crate::jobs::MountSource;
    use kopiur_api::backend::NfsVolume;
    use kopiur_api::snapshot_policy::Source;
    let cfg = config_with_source(
        "media",
        Source {
            pvc: None,
            pvc_selector: None,
            nfs: Some(NfsVolume {
                server: "expanse.internal".into(),
                path: "/mnt/eros/Media".into(),
            }),
            source_path_override: None,
            source_path_strategy: None,
        },
    );
    let repo = resolved_s3_repo();
    let (ws, source_volume, repo_volume, _creds) =
        build_backup_run(&dummy_backup(), &cfg, &repo, "media-ns", "media").unwrap();

    // The NFS export becomes an inline-NFS source mount (read-only), mounted at
    // and snapshotted under the export path (no override → defaults to it).
    let src = source_volume.expect("an NFS source mount");
    assert_eq!(
        src.source,
        MountSource::Nfs {
            server: "expanse.internal".into(),
            path: "/mnt/eros/Media".into(),
        }
    );
    assert_eq!(src.mount_path, "/mnt/eros/Media");
    assert!(src.read_only, "a backup source is mounted read-only");
    // kopia records the export path as the snapshot source path.
    match ws.operation {
        Operation::Snapshot(op) => assert_eq!(op.source_path, "/mnt/eros/Media"),
        other => panic!("expected a Snapshot operation, got {other:?}"),
    }
    // Object-store repo → no repo volume to mount.
    assert!(repo_volume.is_none());
}

#[test]
fn build_backup_run_honors_source_path_override_for_nfs() {
    use kopiur_api::backend::NfsVolume;
    use kopiur_api::snapshot_policy::Source;
    let cfg = config_with_source(
        "media",
        Source {
            pvc: None,
            pvc_selector: None,
            nfs: Some(NfsVolume {
                server: "nas.lan".into(),
                path: "/export/media".into(),
            }),
            source_path_override: Some("/data".into()),
            source_path_strategy: None,
        },
    );
    let repo = resolved_s3_repo();
    let (ws, source_volume, _repo, _creds) =
        build_backup_run(&dummy_backup(), &cfg, &repo, "ns", "media").unwrap();
    // The override drives both the mount path and the recorded source path.
    assert_eq!(source_volume.unwrap().mount_path, "/data");
    match ws.operation {
        Operation::Snapshot(op) => assert_eq!(op.source_path, "/data"),
        other => panic!("expected a Snapshot operation, got {other:?}"),
    }
}

#[test]
fn build_backup_run_remaps_nfs_pseudo_root_source_off_container_rootfs() {
    // Regression: an NFSv4 pseudo-root export (`path: "/"`) was mounted at
    // the container "/" — the mover pod then failed to start with
    // `error mounting ... to rootfs at "/": mountpoint ... is on the top of
    // rootfs`. The server-side export path stays "/", but the in-container
    // mount path (and kopia source path) must be a safe non-root path.
    use crate::jobs::MountSource;
    use kopiur_api::backend::NfsVolume;
    use kopiur_api::snapshot_policy::Source;
    let cfg = config_with_source(
        "media",
        Source {
            pvc: None,
            pvc_selector: None,
            nfs: Some(NfsVolume {
                server: "10.0.0.5".into(),
                path: "/".into(),
            }),
            source_path_override: None,
            source_path_strategy: None,
        },
    );
    let repo = resolved_s3_repo();
    let (ws, source_volume, _repo, _creds) =
        build_backup_run(&dummy_backup(), &cfg, &repo, "ns", "media").unwrap();
    let src = source_volume.expect("an NFS source mount");
    // The NFS volume still exports the server-side pseudo-root.
    assert_eq!(
        src.source,
        MountSource::Nfs {
            server: "10.0.0.5".into(),
            path: "/".into(),
        }
    );
    // ...but it is NOT mounted at the container rootfs.
    assert_ne!(
        src.mount_path, "/",
        "must not mount over the container rootfs"
    );
    assert_eq!(src.mount_path, crate::consts::NFS_SOURCE_MOUNT_PATH);
    match ws.operation {
        Operation::Snapshot(op) => {
            assert_eq!(op.source_path, crate::consts::NFS_SOURCE_MOUNT_PATH)
        }
        other => panic!("expected a Snapshot operation, got {other:?}"),
    }
}

#[test]
fn build_backup_run_maps_pvc_source_to_pvc_mount() {
    use crate::jobs::MountSource;
    use kopiur_api::snapshot_policy::{PvcSource, Source};
    let cfg = config_with_source(
        "pg",
        Source {
            pvc: Some(PvcSource {
                name: "pg-data".into(),
            }),
            pvc_selector: None,
            nfs: None,
            source_path_override: None,
            source_path_strategy: None,
        },
    );
    let repo = resolved_s3_repo();
    let (_ws, source_volume, _repo, _creds) =
        build_backup_run(&dummy_backup(), &cfg, &repo, "ns", "pg").unwrap();
    let src = source_volume.expect("a PVC source mount");
    assert_eq!(
        src.source,
        MountSource::Pvc {
            claim_name: "pg-data".into()
        }
    );
    assert_eq!(src.mount_path, "/pvc/pg-data");
}

#[test]
fn build_backup_run_renders_ns_dot_cluster_hostname_for_a_namespaced_repo_with_cluster_identity() {
    // M5: a namespaced Repository's `identityDefaults.cluster` now flows through
    // `io::ResolvedRepository` (previously always `None`) with ZERO further glue
    // changes — `resolve_identity_for`/`build_backup_run` were already kind-agnostic.
    use kopiur_api::common::IdentityDefaults;
    use kopiur_api::snapshot_policy::{PvcSource, Source};
    let mut repo = resolved_s3_repo();
    repo.identity_defaults = Some(IdentityDefaults {
        cluster: Some("east".into()),
        hostname_expr: None,
        username_expr: None,
    });
    let cfg = config_with_source(
        "pg",
        Source {
            pvc: Some(PvcSource {
                name: "pg-data".into(),
            }),
            pvc_selector: None,
            nfs: None,
            source_path_override: None,
            source_path_strategy: None,
        },
    );
    let (ws, ..) = build_backup_run(&dummy_backup(), &cfg, &repo, "billing", "pg").unwrap();
    assert_eq!(ws.identity.hostname, "billing.east");
}

#[test]
fn build_backup_run_maps_snapshot_create_knobs_and_keeps_them_off_policy_args() {
    // M4 flag sweep (issue #216 category sweep): `failFast`/`limitMb` (the
    // recipe's `SnapshotPolicy.spec.{errorHandling,upload}`) and `description`
    // (this run's `Snapshot.spec`) all land on `SnapshotOp` — they are
    // `snapshot create` argv flags, not `policy set` knobs, so they must NOT
    // appear on `op.policy` (`PolicyArgsSpec`).
    use kopiur_api::snapshot_policy::{ErrorHandling, PvcSource, Source, Upload};
    let mut cfg = config_with_source(
        "pg",
        Source {
            pvc: Some(PvcSource {
                name: "pg-data".into(),
            }),
            pvc_selector: None,
            nfs: None,
            source_path_override: None,
            source_path_strategy: None,
        },
    );
    cfg.spec.error_handling = Some(ErrorHandling {
        fail_fast: true,
        ..Default::default()
    });
    cfg.spec.upload = Some(Upload {
        limit_mb: Some(250),
        ..Default::default()
    });
    let mut backup = dummy_backup();
    backup.spec.description = Some("pre-upgrade snapshot".into());
    let repo = resolved_s3_repo();
    let (ws, _source_volume, _repo, _creds) =
        build_backup_run(&backup, &cfg, &repo, "ns", "pg").unwrap();
    let op = match &ws.operation {
        Operation::Snapshot(op) => op,
        other => panic!("expected a Snapshot op, got {}", other.kind_str()),
    };
    assert_eq!(op.fail_fast, Some(true));
    assert_eq!(op.upload_limit_mb, Some(250));
    assert_eq!(op.description.as_deref(), Some("pre-upgrade snapshot"));

    // The non-leak guard: `PolicyArgsSpec` structurally has no failFast/limitMb
    // fields, and this proves it at the JSON boundary too.
    let policy_json = serde_json::to_value(&op.policy).unwrap();
    assert!(policy_json.get("failFast").is_none());
    assert!(policy_json.get("limitMb").is_none());

    // Absent errorHandling/upload/description ⇒ None on SnapshotOp (kopia's
    // own defaults, today's argv).
    let (ws2, ..) = build_backup_run(
        &dummy_backup(),
        &config_with_source(
            "pg2",
            Source {
                pvc: Some(PvcSource {
                    name: "pg-data".into(),
                }),
                pvc_selector: None,
                nfs: None,
                source_path_override: None,
                source_path_strategy: None,
            },
        ),
        &repo,
        "ns",
        "pg2",
    )
    .unwrap();
    match &ws2.operation {
        Operation::Snapshot(op2) => {
            assert_eq!(op2.fail_fast, None);
            assert_eq!(op2.upload_limit_mb, None);
            assert_eq!(op2.description, None);
        }
        other => panic!("expected a Snapshot op, got {}", other.kind_str()),
    }
}

#[test]
fn build_backup_run_rejects_a_source_with_neither_pvc_nor_nfs() {
    use kopiur_api::snapshot_policy::Source;
    // pvcSelector-only / empty single source: the single-source mover path
    // needs an explicit pvc or nfs (the webhook rejects this earlier; the
    // controller defends against it rather than building a bogus Job).
    let cfg = config_with_source(
        "x",
        Source {
            pvc: None,
            pvc_selector: None,
            nfs: None,
            source_path_override: None,
            source_path_strategy: None,
        },
    );
    let repo = resolved_s3_repo();
    assert!(build_backup_run(&dummy_backup(), &cfg, &repo, "ns", "x").is_err());
}

// --- matches_snapshot_identity: the resolve_succeeded_snapshot predicate ---
// (M0a: same path alone must never cross-match a different source's snapshot)

fn list_entry(
    id: &str,
    path: &str,
    user_name: &str,
    host: &str,
) -> kopiur_kopia::SnapshotListEntry {
    let now = chrono::Utc::now();
    kopiur_kopia::SnapshotListEntry {
        id: id.to_string(),
        source: kopiur_kopia::SnapshotSource {
            host: host.into(),
            user_name: user_name.into(),
            path: path.to_string(),
        },
        description: String::new(),
        start_time: now,
        end_time: now,
        stats: Default::default(),
        root_entry: None,
        retention_reason: vec![],
    }
}

fn identity_of(
    username: &str,
    hostname: &str,
    source_path: &str,
) -> kopiur_mover::workspec::ResolvedIdentity {
    kopiur_mover::workspec::ResolvedIdentity {
        username: username.into(),
        hostname: hostname.into(),
        source_path: source_path.into(),
    }
}

#[test]
fn matches_snapshot_identity_excludes_same_path_different_identity() {
    // The exact hazard this predicate exists to close: two sources (e.g.
    // different namespaces, or different clusters sharing a repository)
    // wrote to the SAME path. Path alone must not match.
    let identity = identity_of("app", "cluster-a", "/pvc/data");
    let entry = list_entry("theirs", "/pvc/data", "someone-else", "cluster-b");
    assert!(!matches_snapshot_identity(&entry, &identity));
}

#[test]
fn matches_snapshot_identity_accepts_same_path_same_identity() {
    let identity = identity_of("app", "cluster-a", "/pvc/data");
    let entry = list_entry("mine", "/pvc/data", "app", "cluster-a");
    assert!(matches_snapshot_identity(&entry, &identity));
}

#[test]
fn matches_snapshot_identity_excludes_different_path_same_identity() {
    // Identity alone isn't sufficient either — path still has to match.
    let identity = identity_of("app", "cluster-a", "/pvc/data");
    let entry = list_entry("elsewhere", "/pvc/other", "app", "cluster-a");
    assert!(!matches_snapshot_identity(&entry, &identity));
}

// --- plan_deletion: exhaustive over every DeletionPolicy ----------------

#[test]
fn delete_policy_plans_snapshot_delete() {
    assert_eq!(
        plan_deletion(DeletionPolicy::Delete, &BTreeMap::new()),
        DeletionPlan::DeleteSnapshot
    );
}

#[test]
fn retain_policy_plans_retain() {
    assert_eq!(
        plan_deletion(DeletionPolicy::Retain, &BTreeMap::new()),
        DeletionPlan::RetainSnapshot
    );
}

#[test]
fn orphan_policy_plans_orphan() {
    assert_eq!(
        plan_deletion(DeletionPolicy::Orphan, &BTreeMap::new()),
        DeletionPlan::OrphanSnapshot
    );
}

#[test]
fn skip_annotation_overrides_delete_to_orphan() {
    // The repo-offline escape hatch: even Delete becomes Orphan so we never
    // contact a dead repository.
    let a = ann(&[(SKIP_SNAPSHOT_CLEANUP_ANNOTATION, "true")]);
    assert_eq!(
        plan_deletion(DeletionPolicy::Delete, &a),
        DeletionPlan::OrphanSnapshot
    );
}

#[test]
fn skip_annotation_overrides_every_policy() {
    let a = ann(&[(SKIP_SNAPSHOT_CLEANUP_ANNOTATION, "")]);
    for p in [
        DeletionPolicy::Delete,
        DeletionPolicy::Retain,
        DeletionPolicy::Orphan,
    ] {
        assert_eq!(plan_deletion(p, &a), DeletionPlan::OrphanSnapshot);
    }
}

#[test]
fn unrelated_annotations_do_not_trigger_skip() {
    let a = ann(&[("kopiur.home-operations.com/other", "x")]);
    assert_eq!(
        plan_deletion(DeletionPolicy::Delete, &a),
        DeletionPlan::DeleteSnapshot
    );
}

// --- namespace_delete_plan (ADR-0005 §5 data-loss prevention) -----------

#[test]
fn non_terminating_namespace_keeps_the_per_snapshot_plan() {
    // A lone `kubectl delete snapshot` (namespace healthy) honors the Snapshot's
    // own plan regardless of the repository's onNamespaceDelete policy.
    for policy in [NamespaceDeletePolicy::Orphan, NamespaceDeletePolicy::Delete] {
        for base in [
            DeletionPlan::DeleteSnapshot,
            DeletionPlan::RetainSnapshot,
            DeletionPlan::OrphanSnapshot,
        ] {
            assert_eq!(namespace_delete_plan(policy, false, base), base);
        }
    }
}

#[test]
fn terminating_namespace_orphan_policy_forces_orphan() {
    // The fail-safe default: a deleted namespace must not run `kopia snapshot
    // delete`, even when the Snapshot's own plan was DeleteSnapshot.
    for base in [
        DeletionPlan::DeleteSnapshot,
        DeletionPlan::RetainSnapshot,
        DeletionPlan::OrphanSnapshot,
    ] {
        assert_eq!(
            namespace_delete_plan(NamespaceDeletePolicy::Orphan, true, base),
            DeletionPlan::OrphanSnapshot
        );
    }
}

#[test]
fn terminating_namespace_delete_policy_cascades_to_base_plan() {
    // Opt-in cascade: with onNamespaceDelete=Delete, the per-Snapshot plan applies
    // (so a produced Snapshot still runs the snapshot delete).
    assert_eq!(
        namespace_delete_plan(
            NamespaceDeletePolicy::Delete,
            true,
            DeletionPlan::DeleteSnapshot
        ),
        DeletionPlan::DeleteSnapshot
    );
    // ...and a Retain/Orphan base is preserved unchanged.
    assert_eq!(
        namespace_delete_plan(
            NamespaceDeletePolicy::Delete,
            true,
            DeletionPlan::RetainSnapshot
        ),
        DeletionPlan::RetainSnapshot
    );
}

// --- delete_job_placement (the cascade Job cannot run in a terminating ns)

#[test]
fn non_terminating_delete_runs_in_the_snapshots_own_namespace() {
    // Status quo for a lone `kubectl delete snapshot`: the Job runs (and is
    // GC'd) next to the Snapshot, whatever the repository's shape.
    for repo_ns in [None, Some("repo-ns"), Some("app")] {
        assert_eq!(
            delete_job_placement(false, "app", repo_ns, Some("kopiur-system")),
            DeleteJobPlacement::RunIn("app".into())
        );
    }
}

#[test]
fn terminating_cluster_repo_runs_in_the_operator_namespace() {
    // The regression the e2e cascade test caught: NamespaceLifecycle rejects
    // creating the delete Job in the terminating namespace, so the cascade
    // must run it where the ClusterRepository's canonical Secret lives.
    assert_eq!(
        delete_job_placement(true, "app", None, Some("kopiur-system")),
        DeleteJobPlacement::RunIn("kopiur-system".into())
    );
}

#[test]
fn terminating_cluster_repo_without_operator_namespace_orphans() {
    // Nowhere survivable to run the Job: fail safe (release the finalizer,
    // keep the snapshot) rather than wedge namespace deletion forever, and
    // tell the operator admin exactly what to set.
    match delete_job_placement(true, "app", None, None) {
        DeleteJobPlacement::OrphanFallback { reason } => {
            assert!(reason.contains("KOPIUR_NAMESPACE"), "actionable: {reason}");
        }
        other => panic!("expected OrphanFallback, got {other:?}"),
    }
}

#[test]
fn terminating_namespaced_repo_in_another_namespace_runs_there() {
    // A cross-namespace Repository ref: its credential Secret (and any repo
    // PVC) live in the repository's namespace, which survives the cascade.
    assert_eq!(
        delete_job_placement(true, "app", Some("storage"), Some("kopiur-system")),
        DeleteJobPlacement::RunIn("storage".into())
    );
}

#[test]
fn terminating_namespaced_repo_in_the_same_namespace_orphans() {
    // The Repository dies with the namespace — its Secret/PVC are going
    // away too, so there is nothing survivable to clean against.
    match delete_job_placement(true, "app", Some("app"), Some("kopiur-system")) {
        DeleteJobPlacement::OrphanFallback { reason } => {
            assert!(
                reason.contains("kopia snapshot delete"),
                "actionable: {reason}"
            );
        }
        other => panic!("expected OrphanFallback, got {other:?}"),
    }
}

#[test]
fn terminating_operator_namespace_itself_orphans() {
    // Deleting the operator's own namespace: the fallback host is the
    // terminating namespace, so the cascade must orphan, not wedge.
    match delete_job_placement(true, "kopiur-system", None, Some("kopiur-system")) {
        DeleteJobPlacement::OrphanFallback { reason } => {
            assert!(
                reason.contains("kopiur-system"),
                "names the namespace: {reason}"
            );
        }
        other => panic!("expected OrphanFallback, got {other:?}"),
    }
}

// --- pinned_repository_ref (status.resolved freezes the run's repo) ------

#[test]
fn pinned_ref_defaults_a_namespaced_repository_to_the_recipe_namespace() {
    use kopiur_api::common::{RepositoryKind, RepositoryRef};
    let r = RepositoryRef {
        kind: RepositoryKind::Repository,
        name: "nas".into(),
        namespace: None,
    };
    let pinned = pinned_repository_ref(&r, "billing");
    assert_eq!(pinned.namespace.as_deref(), Some("billing"));
    // An explicit cross-namespace ref is preserved as-is.
    let r = RepositoryRef {
        namespace: Some("storage".into()),
        ..r
    };
    assert_eq!(
        pinned_repository_ref(&r, "billing").namespace.as_deref(),
        Some("storage")
    );
}

#[test]
fn pinned_ref_never_pins_a_namespace_for_cluster_repositories() {
    use kopiur_api::common::{RepositoryKind, RepositoryRef};
    // The webhook forbids `namespace` on ClusterRepository refs; the pinned
    // ref must stay valid against the same validator.
    let r = RepositoryRef {
        kind: RepositoryKind::ClusterRepository,
        name: "shared".into(),
        namespace: Some("ignored".into()),
    };
    assert_eq!(pinned_repository_ref(&r, "billing").namespace, None);
}

// --- resolved_run_status (status.resolved frozen at run time, ADR §3.4) --

#[test]
fn resolved_run_status_pins_repository_and_concrete_source() {
    use kopiur_api::snapshot_policy::{PvcSource, Source};
    let cfg = config_with_source(
        "media",
        Source {
            pvc: Some(PvcSource {
                name: "media-data".into(),
            }),
            pvc_selector: None,
            nfs: None,
            source_path_override: None,
            source_path_strategy: None,
        },
    );
    let repo = resolved_s3_repo();
    let (ws, _, _, _) =
        build_backup_run(&dummy_backup(), &cfg, &repo, "media-ns", "media").unwrap();
    let resolved = resolved_run_status(&cfg, "media-ns", &ws);
    // The deletion path re-resolves the repo from this pinned ref alone, so
    // it must carry the namespace the recipe resolved against.
    let pinned = resolved.repository.expect("repository pinned");
    assert_eq!(pinned.name, "repo");
    assert_eq!(pinned.namespace.as_deref(), Some("media-ns"));
    // ...and the concrete source the run snapshotted.
    assert_eq!(resolved.sources.len(), 1);
    assert_eq!(
        resolved.sources[0].pvc.as_deref(),
        Some("media-ns/media-data")
    );
    assert_eq!(
        resolved.sources[0].source_path.as_deref(),
        Some(ws.identity.source_path.as_str())
    );
}

// --- effective_deletion_policy ------------------------------------------

#[test]
fn discovered_is_forced_to_retain_regardless_of_spec() {
    for p in [
        None,
        Some(DeletionPolicy::Delete),
        Some(DeletionPolicy::Orphan),
        Some(DeletionPolicy::Retain),
    ] {
        assert_eq!(
            effective_deletion_policy(p, Origin::Discovered),
            DeletionPolicy::Retain
        );
    }
}

#[test]
fn produced_defaults_to_delete_when_unset() {
    assert_eq!(
        effective_deletion_policy(None, Origin::Scheduled),
        DeletionPolicy::Delete
    );
    assert_eq!(
        effective_deletion_policy(None, Origin::Manual),
        DeletionPolicy::Delete
    );
}

#[test]
fn produced_honors_explicit_spec_policy() {
    assert_eq!(
        effective_deletion_policy(Some(DeletionPolicy::Orphan), Origin::Manual),
        DeletionPolicy::Orphan
    );
    assert_eq!(
        effective_deletion_policy(Some(DeletionPolicy::Retain), Origin::Scheduled),
        DeletionPolicy::Retain
    );
}

fn job_with_status(status: Option<k8s_openapi::api::batch::v1::JobStatus>) -> Job {
    Job {
        status,
        ..Default::default()
    }
}

fn job_condition(type_: &str, status: &str) -> k8s_openapi::api::batch::v1::JobCondition {
    k8s_openapi::api::batch::v1::JobCondition {
        type_: type_.to_string(),
        status: status.to_string(),
        ..Default::default()
    }
}

// #103: the mover stamps `Succeeded` before its Job is terminal, so the
// SucceededSteadyState reap must wait until the Job is Complete/Failed/gone.
#[test]
fn staged_teardown_waits_for_a_still_running_job() {
    use k8s_openapi::api::batch::v1::JobStatus;
    let running = job_with_status(Some(JobStatus {
        active: Some(1),
        ..Default::default()
    }));
    assert!(
        !staged_teardown_ready(Some(&running)),
        "must not reap while the mover Job is still Active (#103)"
    );
    // No status at all is also non-terminal.
    assert!(!staged_teardown_ready(Some(&job_with_status(None))));
}

#[test]
fn staged_teardown_proceeds_on_terminal_or_absent_job() {
    use k8s_openapi::api::batch::v1::JobStatus;
    let complete = job_with_status(Some(JobStatus {
        conditions: Some(vec![job_condition("Complete", "True")]),
        succeeded: Some(1),
        ..Default::default()
    }));
    let failed = job_with_status(Some(JobStatus {
        conditions: Some(vec![job_condition("Failed", "True")]),
        ..Default::default()
    }));
    assert!(staged_teardown_ready(Some(&complete)), "Complete → reap");
    assert!(staged_teardown_ready(Some(&failed)), "Failed → reap");
    // TTL-reaped, or a discovered Snapshot that never owned a Job.
    assert!(staged_teardown_ready(None), "absent Job → reap");
}

// The pin-Job lookup gate: never-pinned Snapshots (the overwhelmingly common
// case) skip the per-heartbeat GET; anything that ever spawned a pin mover —
// including one whose spec.pin was toggled back mid-flight — stays findable
// via the `Pinned` condition upserted before every pin Job is applied.
#[test]
fn pin_job_may_exist_covers_every_marker() {
    fn snap(pin: bool, pinned: Option<bool>, with_condition: bool) -> Snapshot {
        let mut s = dummy_backup();
        s.spec.pin = pin;
        let conditions = if with_condition {
            io::upsert_condition_status(
                &[],
                crate::consts::PINNED_CONDITION,
                "Unknown",
                "PinJobRunning",
                "a SnapshotPin mover Job is applying spec.pin",
                None,
            )
        } else {
            Vec::new()
        };
        s.status = Some(kopiur_api::snapshot::SnapshotStatus {
            pinned,
            conditions,
            ..Default::default()
        });
        s
    }
    assert!(
        !pin_job_may_exist(&snap(false, None, false)),
        "never pinned"
    );
    assert!(pin_job_may_exist(&snap(true, None, false)), "spec.pin set");
    assert!(
        pin_job_may_exist(&snap(false, Some(false), false)),
        "pin recorded"
    );
    assert!(
        pin_job_may_exist(&snap(false, None, true)),
        "mid-flight toggle-back: the spawn-time condition keeps the Job findable"
    );
}

// A FAILED pin Job is kept (as the TTL-based retry-backoff marker) only while
// its direction is still what the decision wants; a stale one is consumed so
// it can't block — or mis-satisfy — a future toggle.
#[test]
fn pin_job_still_wanted_matrix() {
    // Direction matches the pending action → keep as backoff.
    assert!(pin_job_still_wanted(Some(true), PinAction::Pin));
    assert!(pin_job_still_wanted(Some(false), PinAction::Unpin));
    // Direction contradicts the pending action → stale, consume.
    assert!(!pin_job_still_wanted(Some(false), PinAction::Pin));
    assert!(!pin_job_still_wanted(Some(true), PinAction::Unpin));
    // Nothing pending at all → any failed Job is stale.
    assert!(!pin_job_still_wanted(Some(true), PinAction::NoOp));
    assert!(!pin_job_still_wanted(None, PinAction::NoOp));
    // Legacy direction-less Job: assume it was this action's attempt.
    assert!(pin_job_still_wanted(None, PinAction::Pin));
    assert!(pin_job_still_wanted(None, PinAction::Unpin));
}

// A pin Job is consumed by the direction it APPLIED (the annotation), never by
// the currently-desired spec.pin — a stale Job must not satisfy the opposite
// toggle.
#[test]
fn pin_job_target_reads_the_direction_annotation() {
    let mut job = Job::default();
    assert_eq!(pin_job_target(&job), None, "legacy Job: unattributable");
    job.metadata.annotations = Some(std::collections::BTreeMap::from([(
        crate::consts::PIN_TARGET_ANNOTATION.to_string(),
        "true".to_string(),
    )]));
    assert_eq!(pin_job_target(&job), Some(true));
    job.metadata.annotations = Some(std::collections::BTreeMap::from([(
        crate::consts::PIN_TARGET_ANNOTATION.to_string(),
        "false".to_string(),
    )]));
    assert_eq!(pin_job_target(&job), Some(false));
    job.metadata.annotations = Some(std::collections::BTreeMap::from([(
        crate::consts::PIN_TARGET_ANNOTATION.to_string(),
        "garbage".to_string(),
    )]));
    assert_eq!(pin_job_target(&job), None, "unparseable = unattributable");
}

// --- staged_watchdog_budget: the pinned bind budget's three states ---

#[test]
fn staged_watchdog_budget_pins_zero_absent_and_positive() {
    // Explicit 0 = the user's "wait indefinitely".
    assert_eq!(staged_watchdog_budget(Some(0)), None);
    // A positive pin is used verbatim.
    assert_eq!(
        staged_watchdog_budget(Some(90)),
        Some(std::time::Duration::from_secs(90))
    );
    // A legacy stamp without the field (or a nonsense negative) gets the default
    // budget — never an accidental infinite wait.
    assert_eq!(
        staged_watchdog_budget(None),
        Some(crate::consts::DEFAULT_STAGING_TIMEOUT)
    );
    assert_eq!(
        staged_watchdog_budget(Some(-5)),
        Some(crate::consts::DEFAULT_STAGING_TIMEOUT)
    );
}
