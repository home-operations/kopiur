use super::*;
use crate::consts::SKIP_SNAPSHOT_CLEANUP_ANNOTATION;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
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
        snapshot_ready_outcome(&SnapshotPhase::Pending),
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
    assert_eq!(
        run_decision(Some(&SnapshotPhase::Pending)),
        RunDecision::Run
    );
    assert_eq!(
        run_decision(Some(&SnapshotPhase::Running)),
        RunDecision::Run
    );
    // Succeeded → steady state (pin/staged only); NEVER a new mover Job.
    assert_eq!(
        run_decision(Some(&SnapshotPhase::Succeeded)),
        RunDecision::SucceededSteadyState
    );
    // Failed → terminal until the spec changes; no TTL-driven retry loop.
    assert_eq!(
        run_decision(Some(&SnapshotPhase::Failed)),
        RunDecision::TerminalFailed
    );
    // Phases owned by earlier gates → wait, don't act on a desynced view.
    assert_eq!(
        run_decision(Some(&SnapshotPhase::Deleting)),
        RunDecision::Wait
    );
    assert_eq!(
        run_decision(Some(&SnapshotPhase::Discovered)),
        RunDecision::Wait
    );
    // Unchanged → terminal steady state, same as Succeeded: the mover ran and
    // no further Job may launch. The two differ in what they OWN, which the
    // steady-state arm decides — not here. Crucially it must NOT be `Run`, or a
    // deduped Snapshot would mint a fresh mover Job on every reconcile.
    assert_eq!(
        run_decision(Some(&SnapshotPhase::Unchanged)),
        RunDecision::SucceededSteadyState
    );
    // A phase written by a NEWER operator: hold. Launching a Job could duplicate
    // work that phase already represents; calling it terminal would strand a run
    // this build simply cannot read.
    assert_eq!(
        run_decision(Some(&SnapshotPhase::Unknown("Quiescing".into()))),
        RunDecision::Wait
    );
}

#[test]
fn unchanged_is_ready_not_stalled() {
    use crate::io::ReadyOutcome;
    use crate::snapshot::plan::snapshot_ready_outcome;
    use kopiur_api::snapshot::SnapshotPhase;
    // The source IS protected — by the previous snapshot. Anything but Ready
    // would fail `kubectl wait --for=condition=Ready` and every Flux/Argo health
    // check on a perfectly healthy dedupe (#351).
    assert_eq!(
        snapshot_ready_outcome(&SnapshotPhase::Unchanged),
        ReadyOutcome::Ready
    );
}

#[test]
fn should_run_preflight_only_at_first_launch() {
    use kopiur_api::snapshot::SnapshotPhase;
    // First launch: gate runs.
    assert!(should_run_preflight(None));
    assert!(should_run_preflight(Some(&SnapshotPhase::Pending)));
    // A Running snapshot whose Job vanished resumes — preflight must NOT re-gate it.
    assert!(!should_run_preflight(Some(&SnapshotPhase::Running)));
    // Terminal/other phases never reach the gate, but be explicit.
    assert!(!should_run_preflight(Some(&SnapshotPhase::Succeeded)));
    assert!(!should_run_preflight(Some(&SnapshotPhase::Failed)));
    // Not knowably "at first launch": never re-open the gate on a phase this
    // build cannot place in the lifecycle.
    assert!(!should_run_preflight(Some(&SnapshotPhase::Unknown(
        "Quiescing".into()
    ))));
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
        snapshot_ready_outcome(&SnapshotPhase::Succeeded),
        io::ReadyOutcome::Ready
    );
    assert_eq!(
        snapshot_ready_outcome(&SnapshotPhase::Discovered),
        io::ReadyOutcome::Ready
    );
    assert_eq!(
        snapshot_ready_outcome(&SnapshotPhase::Failed),
        io::ReadyOutcome::Stalled
    );
    for p in [
        SnapshotPhase::Pending,
        SnapshotPhase::Running,
        SnapshotPhase::Deleting,
        // Never Ready, never Stalled — `kubectl wait` keeps waiting.
        SnapshotPhase::Unknown("Quiescing".into()),
    ] {
        assert_eq!(snapshot_ready_outcome(&p), io::ReadyOutcome::Reconciling);
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
            deletion: None,
            adoption: None,
        },
    )
}

// --- deletion-path credential helpers ------------------------------------
//
// The per-CR `delete_projection_enabled` gate is retired with the per-CR delete
// Job (M5a): the per-repository batch delete runs at the repository's home
// namespace, where its canonical Secret already lives, so it NEVER projects
// (projection is hardcoded off). `stuck_finalizer_hint` survives — it enriches
// the batch path's credential-resolution error with the escape hatch.

fn projection(enabled: bool) -> kopiur_api::common::CredentialProjection {
    kopiur_api::common::CredentialProjection { enabled }
}

#[test]
fn stuck_finalizer_hint_names_the_escape_hatch_and_keeps_the_original_message() {
    // The shared creds messages cannot mention a Snapshot-only remedy, so the delete
    // path adds it. This is the ONLY way out when the repository owner revokes
    // `credentialProjection.allowed` — the live gate no pin can (or should) reopen.
    let msg = super::stuck_finalizer_hint(
        "credentials Secret `repo-pw` does not exist in namespace `team-a`.",
        "team-a",
        "nightly-1",
    );
    assert!(
        msg.starts_with("credentials Secret `repo-pw` does not exist in namespace `team-a`."),
        "the underlying cause must survive verbatim: {msg}"
    );
    assert!(msg.contains(crate::consts::SKIP_SNAPSHOT_CLEANUP_ANNOTATION));
    assert!(msg.contains("team-a/nightly-1"));
    // The user must know the kopia snapshot survives the escape hatch.
    assert!(msg.contains("WITHOUT deleting the kopia snapshot"));
}

#[test]
fn projection_to_pin_records_an_absent_opt_in_as_an_explicit_off() {
    // `None` on the pin means "this Snapshot predates the pin", which the delete path
    // resolves by falling back to the live recipe. A recipe that simply never opted in
    // must therefore pin `enabled: false` rather than `None` — otherwise the backfill
    // guard never goes false and re-reads the recipe on every steady-state pass.
    let mut policy = sample_policy();
    policy.spec.credential_projection = None;
    assert_eq!(super::plan::projection_to_pin(&policy), projection(false));
    policy.spec.credential_projection = Some(projection(true));
    assert_eq!(super::plan::projection_to_pin(&policy), projection(true));
}

// --- repository-ref backfill (mass-deletion breaker per-repo counting) ----

/// A produced `Snapshot` (has `policyRef`), optionally already carrying a
/// `status.resolved.repository` pin.
fn backup_with_policy_ref(pinned_repo: Option<kopiur_api::common::RepositoryRef>) -> Snapshot {
    let mut backup = Snapshot::new(
        "b1",
        kopiur_api::snapshot::SnapshotSpec {
            source: None,
            policy_ref: Some(kopiur_api::common::PolicyRef {
                name: "pg".into(),
                namespace: None,
            }),
            tags: None,
            failure_policy: None,
            deletion_policy: None,
            on_schedule_delete: None,
            pin: false,
            description: None,
        },
    );
    if let Some(repository) = pinned_repo {
        backup.status = Some(kopiur_api::snapshot::SnapshotStatus {
            resolved: Some(kopiur_api::snapshot::ResolvedSnapshot {
                repository: Some(repository),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    backup
}

#[test]
fn needs_repository_backfill_when_pin_absent_but_recipe_reachable() {
    use kopiur_api::common::{RepositoryKind, RepositoryRef};

    assert!(super::plan::needs_repository_backfill(
        &backup_with_policy_ref(None)
    ));
    let already_pinned = backup_with_policy_ref(Some(RepositoryRef {
        kind: RepositoryKind::Repository,
        name: "r".into(),
        namespace: Some("ns".into()),
    }));
    assert!(!super::plan::needs_repository_backfill(&already_pinned));
}

#[test]
fn needs_repository_backfill_is_false_without_a_policy_ref() {
    // Discovered/manual Snapshots have no recipe to pin from; they stay in the
    // conservative unpinned bucket forever, by design — not a bug.
    assert!(!super::plan::needs_repository_backfill(&dummy_backup()));
}

#[test]
fn backfill_patch_body_is_none_when_neither_pin_needs_backfilling() {
    let policy = sample_policy();
    assert_eq!(
        super::plan::backfill_patch_body(&policy, "ns", false, false),
        None
    );
}

#[test]
fn backfill_patch_body_includes_only_the_keys_that_need_backfilling() {
    let policy = sample_policy();

    // Only the repository pin needs backfilling: the body carries just that key.
    let repo_only = super::plan::backfill_patch_body(&policy, "ns", false, true)
        .expect("repository backfill needed");
    let resolved = repo_only["resolved"].as_object().expect("object");
    assert!(!resolved.contains_key("credentialProjection"));
    assert_eq!(resolved["repository"]["name"], "r");
    // The pinned ref carries the fallback namespace ("ns") since the recipe's
    // own repository ref and metadata both leave the namespace unset.
    assert_eq!(resolved["repository"]["namespace"], "ns");

    // Only the projection pin needs backfilling: the body carries just that key.
    let projection_only = super::plan::backfill_patch_body(&policy, "ns", true, false)
        .expect("projection backfill needed");
    let resolved = projection_only["resolved"].as_object().expect("object");
    assert!(!resolved.contains_key("repository"));
    assert_eq!(resolved["credentialProjection"]["enabled"], false);

    // Both needed: both keys present.
    let both = super::plan::backfill_patch_body(&policy, "ns", true, true).expect("both needed");
    let resolved = both["resolved"].as_object().expect("object");
    assert!(resolved.contains_key("credentialProjection"));
    assert!(resolved.contains_key("repository"));
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
        deletion_protection: None,
        mass_deletion_ack: None,
        catalog: None,
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
            deletion: None,
            adoption: None,
        },
    )
}

fn dummy_backup() -> Snapshot {
    Snapshot::new(
        "b1",
        kopiur_api::snapshot::SnapshotSpec {
            source: None,
            policy_ref: None,
            tags: None,
            failure_policy: None,
            deletion_policy: None,
            on_schedule_delete: None,
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
            ..Default::default()
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
    assert!(
        src.read_only,
        "a backup source is mounted read-only unless the recipe opts out"
    );
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
            ..Default::default()
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
fn build_backup_run_honors_source_read_only_false() {
    // #254: kopia only reads the source, so read-only is the default — but the kubelet
    // skips its recursive fsGroup chgrp on a read-only mount, which makes a mover's
    // fsGroup/fsGroupChangePolicy silently inert. `readOnly: false` re-enables it.
    use crate::jobs::MountSource;
    use kopiur_api::snapshot_policy::{PvcSource, Source};
    let cfg = config_with_source(
        "data",
        Source {
            pvc: Some(PvcSource {
                name: "app-data".into(),
            }),
            read_only: Some(false),
            ..Default::default()
        },
    );
    let repo = resolved_s3_repo();
    let (_ws, source_volume, _repo, _creds) =
        build_backup_run(&dummy_backup(), &cfg, &repo, "ns", "data").unwrap();
    let src = source_volume.expect("a PVC source mount");
    assert_eq!(
        src.source,
        MountSource::Pvc {
            claim_name: "app-data".into()
        }
    );
    assert!(
        !src.read_only,
        "readOnly: false must reach the mount — one VolumeMountSpec.read_only drives BOTH \
         the PVC volume source's readOnly and the container volumeMount's readOnly, and \
         fsGroup needs both"
    );
}

#[test]
fn build_backup_run_defaults_an_unset_source_read_only_to_true() {
    // The CRD advertises `default: true` for sources[].readOnly. That schema default is
    // only honest because this resolver maps absent to exactly the same value — the
    // apiserver does not default a field whose parent object the user omitted.
    use kopiur_api::snapshot_policy::{PvcSource, Source};
    let cfg = config_with_source(
        "data",
        Source {
            pvc: Some(PvcSource {
                name: "app-data".into(),
            }),
            read_only: None,
            ..Default::default()
        },
    );
    let repo = resolved_s3_repo();
    let (_ws, source_volume, _repo, _creds) =
        build_backup_run(&dummy_backup(), &cfg, &repo, "ns", "data").unwrap();
    assert!(source_volume.expect("a PVC source mount").read_only);
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
                ..Default::default()
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
    // A genuinely empty source: admission rejects this, and the controller
    // defends against it rather than building a bogus Job.
    let cfg = config_with_source(
        "x",
        Source {
            pvc: None,
            pvc_selector: None,
            nfs: None,
            source_path_override: None,
            source_path_strategy: None,
            ..Default::default()
        },
    );
    let repo = resolved_s3_repo();
    assert!(build_backup_run(&dummy_backup(), &cfg, &repo, "ns", "x").is_err());
}

#[test]
fn an_unpinned_snapshot_against_a_selector_policy_names_the_fix_not_a_bug_report() {
    use kopiur_api::snapshot_policy::{PvcSelector, Source};
    // #346 as users actually hit it. The old code matched `(&source.pvc,
    // &source.nfs)`, so a pvcSelector fell into the `_` arm and produced
    // "invariant violated ... This is likely a bug in kopiur — please report
    // it". The previous version of THIS test asserted only `is_err()`, and its
    // comment claimed the webhook rejected selectors earlier, which was false —
    // `validate_source` accepted them. So the whole bug sat behind a green test.
    let cfg = config_with_source(
        "x",
        Source {
            pvc: None,
            pvc_selector: Some(PvcSelector {
                namespace_selector: None,
                label_selector: None,
            }),
            nfs: None,
            source_path_override: None,
            source_path_strategy: None,
            ..Default::default()
        },
    );
    let repo = resolved_s3_repo();
    let err = build_backup_run(&dummy_backup(), &cfg, &repo, "ns", "x")
        .expect_err("an unpinned Snapshot against a selector policy cannot run");
    let msg = err.to_string();
    assert!(
        !msg.contains("invariant violated"),
        "this is a user configuration problem, not a kopiur bug: {msg}"
    );
    assert!(msg.contains("pvcSelector"), "must name the cause: {msg}");
    assert!(msg.contains("snapshot now"), "must name the fix: {msg}");
}

#[test]
fn a_pinned_snapshot_against_a_selector_policy_builds_its_own_pvc_mount() {
    use kopiur_api::snapshot_policy::{PvcSelector, Source, SourcePathStrategy};
    // The other half: with `spec.source` naming the PVC, a selector policy
    // builds an ordinary single-PVC run — which is the whole design.
    let cfg = config_with_source(
        "x",
        Source {
            pvc: None,
            pvc_selector: Some(PvcSelector {
                namespace_selector: None,
                label_selector: None,
            }),
            nfs: None,
            source_path_override: None,
            source_path_strategy: Some(SourcePathStrategy::PvcNamespacedName),
            ..Default::default()
        },
    );
    let mut backup = dummy_backup();
    backup.spec.source = Some(kopiur_api::SnapshotSourceRef {
        source_index: 0,
        target: kopiur_api::SnapshotSourceTarget::Pvc(kopiur_api::PvcTargetRef {
            namespace: "web".into(),
            name: "assets".into(),
        }),
        group: None,
    });
    let repo = resolved_s3_repo();
    let (ws, mount, _repo_vol, _creds) =
        build_backup_run(&backup, &cfg, &repo, "ns", "x").expect("builds");
    let mount = mount.expect("a PVC source always mounts something");
    assert_eq!(mount.mount_path, "/pvc/web/assets");
    match &ws.operation {
        Operation::Snapshot(op) => assert_eq!(op.source_path, "/pvc/web/assets"),
        other => panic!("expected a Snapshot op, got {}", other.kind_str()),
    }
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
        tags: Default::default(),
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

// --- plan_deletion: the mass-deletion-protection decision table (M2) ----
//
// Decision order under test (see `plan_deletion`'s doc comment for the full
// table): (1) skip-cleanup annotation, absolute; (2) namespace-terminating
// cascade; (3) schedule cascade guard; (4) operator prune; (5)/(6) external
// destructive/non-destructive with the mass-deletion breaker.

use kopiur_api::common::ScheduleDeletePolicy;
use kopiur_api::consts::PRUNED_BY_ANNOTATION;
use kopiur_api::snapshot::PrunedBy;

/// Base facts: external (live owner), live namespace, breaker allowed, policy
/// Delete. Each test overrides only the fields it's exercising — keeps every
/// call site legible, matching `DeletionFacts`'s own rationale.
fn base_facts(annotations: &BTreeMap<String, String>) -> DeletionFacts<'_> {
    DeletionFacts {
        policy: DeletionPolicy::Delete,
        annotations,
        owner: OwnerState::Alive,
        cascade: ScheduleDeletePolicy::Retain,
        ns_terminating: false,
        ns_policy: None,
        breaker: BreakerState::Allowed,
    }
}

/// Annotations carrying a `pruned-by: <kind>` stamp.
fn pruned(kind: PrunedBy) -> BTreeMap<String, String> {
    ann(&[(PRUNED_BY_ANNOTATION, kind.annotation_value())])
}

// -- ported: the pre-M2 plan_deletion(policy, annotations) tests ---------

#[test]
fn delete_policy_plans_snapshot_delete() {
    let a = BTreeMap::new();
    assert_eq!(
        plan_deletion(DeletionFacts {
            policy: DeletionPolicy::Delete,
            ..base_facts(&a)
        }),
        DeletionPlan::DeleteSnapshot
    );
}

#[test]
fn retain_policy_plans_retain() {
    let a = BTreeMap::new();
    assert_eq!(
        plan_deletion(DeletionFacts {
            policy: DeletionPolicy::Retain,
            ..base_facts(&a)
        }),
        DeletionPlan::RetainSnapshot
    );
}

#[test]
fn orphan_policy_plans_orphan() {
    let a = BTreeMap::new();
    assert_eq!(
        plan_deletion(DeletionFacts {
            policy: DeletionPolicy::Orphan,
            ..base_facts(&a)
        }),
        DeletionPlan::OrphanSnapshot
    );
}

#[test]
fn skip_annotation_overrides_delete_to_orphan() {
    // The repo-offline escape hatch: even Delete becomes Orphan so we never
    // contact a dead repository.
    let a = ann(&[(SKIP_SNAPSHOT_CLEANUP_ANNOTATION, "true")]);
    assert_eq!(
        plan_deletion(DeletionFacts {
            policy: DeletionPolicy::Delete,
            ..base_facts(&a)
        }),
        DeletionPlan::OrphanSnapshot
    );
}

#[test]
fn skip_annotation_overrides_every_policy() {
    let a = ann(&[(SKIP_SNAPSHOT_CLEANUP_ANNOTATION, "")]);
    for policy in [
        DeletionPolicy::Delete,
        DeletionPolicy::Retain,
        DeletionPolicy::Orphan,
    ] {
        assert_eq!(
            plan_deletion(DeletionFacts {
                policy,
                ..base_facts(&a)
            }),
            DeletionPlan::OrphanSnapshot,
            "{policy:?}"
        );
    }
}

#[test]
fn unrelated_annotations_do_not_trigger_skip() {
    let a = ann(&[("kopiur.home-operations.com/other", "x")]);
    assert_eq!(
        plan_deletion(DeletionFacts {
            policy: DeletionPolicy::Delete,
            ..base_facts(&a)
        }),
        DeletionPlan::DeleteSnapshot
    );
}

// -- ported: the pre-M2 namespace_delete_plan tests ----------------------

#[test]
fn non_terminating_namespace_keeps_the_per_snapshot_plan() {
    // A lone `kubectl delete snapshot` (namespace healthy) honors the
    // Snapshot's own plan regardless of the repository's onNamespaceDelete
    // policy (or whether it could even be resolved).
    let a = BTreeMap::new();
    for ns_policy in [
        None,
        Some(NamespaceDeletePolicy::Orphan),
        Some(NamespaceDeletePolicy::Delete),
    ] {
        for (policy, expected) in [
            (DeletionPolicy::Delete, DeletionPlan::DeleteSnapshot),
            (DeletionPolicy::Retain, DeletionPlan::RetainSnapshot),
            (DeletionPolicy::Orphan, DeletionPlan::OrphanSnapshot),
        ] {
            let plan = plan_deletion(DeletionFacts {
                policy,
                ns_terminating: false,
                ns_policy,
                ..base_facts(&a)
            });
            assert_eq!(plan, expected, "{policy:?}/{ns_policy:?}");
        }
    }
}

// -- named intent tests (approved semantics) -----------------------------

#[test]
fn external_owner_gone_delete_downgrades_to_cascade_guard_retain() {
    let a = BTreeMap::new();
    let plan = plan_deletion(DeletionFacts {
        owner: OwnerState::GoneOrReplaced,
        cascade: ScheduleDeletePolicy::Retain,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::RetainSnapshotOnScheduleDelete);
}

#[test]
fn external_owner_exists_but_terminating_downgrades() {
    // Foreground-cascade: the schedule still exists (same uid) but is itself
    // terminating — GC hasn't reaped this Snapshot yet, but the schedule is
    // as good as gone.
    let owner_ref = schedule_owner_ref_fixture("sched-uid");
    let sched = schedule_fixture("sched-uid", true);
    let owner = owner_state_from(Some(&sched), &owner_ref);
    assert_eq!(owner, OwnerState::GoneOrReplaced);

    let a = BTreeMap::new();
    let plan = plan_deletion(DeletionFacts {
        owner,
        cascade: ScheduleDeletePolicy::Retain,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::RetainSnapshotOnScheduleDelete);
}

#[test]
fn external_owner_uid_mismatch_downgrades() {
    // kro-style delete+recreate: a same-name schedule exists but with a
    // different uid, so the ownerRef points at a schedule that's really gone.
    let owner_ref = schedule_owner_ref_fixture("old-uid");
    let sched = schedule_fixture("new-uid", false);
    let owner = owner_state_from(Some(&sched), &owner_ref);
    assert_eq!(owner, OwnerState::GoneOrReplaced);

    let a = BTreeMap::new();
    let plan = plan_deletion(DeletionFacts {
        owner,
        cascade: ScheduleDeletePolicy::Retain,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::RetainSnapshotOnScheduleDelete);
}

#[test]
fn cascade_delete_opt_in_still_deletes_when_owner_gone() {
    let a = BTreeMap::new();
    let allowed = plan_deletion(DeletionFacts {
        owner: OwnerState::GoneOrReplaced,
        cascade: ScheduleDeletePolicy::Delete,
        breaker: BreakerState::Allowed,
        ..base_facts(&a)
    });
    assert_eq!(allowed, DeletionPlan::DeleteSnapshot);

    // ...and still holdable: the opt-in cascade is still an external
    // deletion, so the breaker applies to it exactly as it would without the
    // cascade in play.
    let held = plan_deletion(DeletionFacts {
        owner: OwnerState::GoneOrReplaced,
        cascade: ScheduleDeletePolicy::Delete,
        breaker: BreakerState::Held,
        ..base_facts(&a)
    });
    assert_eq!(held, DeletionPlan::HoldSnapshotDeletion);
}

#[test]
fn single_external_delete_with_live_schedule_unchanged() {
    // Status quo: an Alive owner never puts the cascade guard in play, so a
    // lone external delete plans exactly as it did pre-M2.
    let a = BTreeMap::new();
    let plan = plan_deletion(DeletionFacts {
        owner: OwnerState::Alive,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::DeleteSnapshot);
}

#[test]
fn operator_pruned_bypasses_guard_and_breaker() {
    for kind in [PrunedBy::Retention, PrunedBy::FailedHistory] {
        let a = pruned(kind);
        let plan = plan_deletion(DeletionFacts {
            owner: OwnerState::GoneOrReplaced, // would otherwise trigger the guard
            cascade: ScheduleDeletePolicy::Retain, // would otherwise downgrade
            breaker: BreakerState::Held,       // would otherwise hold
            ..base_facts(&a)
        });
        assert_eq!(plan, DeletionPlan::DeleteSnapshot, "{kind:?}");
    }
}

// -- plan_prune: the variant-aware (PrunedBy × DeletionPolicy) 3×3 matrix (M2) --

#[test]
fn plan_prune_matrix_covers_every_prunedby_x_policy_cell() {
    let cases = [
        (
            PrunedBy::Retention,
            DeletionPolicy::Delete,
            DeletionPlan::DeleteSnapshot,
        ),
        (
            PrunedBy::Retention,
            DeletionPolicy::Retain,
            DeletionPlan::RetainSnapshot,
        ),
        (
            PrunedBy::Retention,
            DeletionPolicy::Orphan,
            DeletionPlan::OrphanSnapshot,
        ),
        (
            PrunedBy::FailedHistory,
            DeletionPolicy::Delete,
            DeletionPlan::DeleteSnapshot,
        ),
        (
            PrunedBy::FailedHistory,
            DeletionPolicy::Retain,
            DeletionPlan::RetainSnapshot,
        ),
        (
            PrunedBy::FailedHistory,
            DeletionPolicy::Orphan,
            DeletionPlan::OrphanSnapshot,
        ),
        // The loud downgrade: a policy-cascade prune under an effective
        // `Delete` policy NEVER contacts the repository.
        (
            PrunedBy::PolicyCascade,
            DeletionPolicy::Delete,
            DeletionPlan::RetainSnapshotOnPolicyDelete,
        ),
        (
            PrunedBy::PolicyCascade,
            DeletionPolicy::Retain,
            DeletionPlan::RetainSnapshot,
        ),
        (
            PrunedBy::PolicyCascade,
            DeletionPolicy::Orphan,
            DeletionPlan::OrphanSnapshot,
        ),
    ];
    for (kind, policy, expected) in cases {
        let a = pruned(kind);
        // Every operator prune bypasses BOTH the schedule cascade guard and
        // the breaker — set both to what would otherwise divert the
        // decision, so this test also proves the prune path short-circuits
        // them for every kind, not just Retention/FailedHistory.
        let plan = plan_deletion(DeletionFacts {
            policy,
            owner: OwnerState::GoneOrReplaced,
            cascade: ScheduleDeletePolicy::Retain,
            breaker: BreakerState::Held,
            ..base_facts(&a)
        });
        assert_eq!(plan, expected, "{kind:?}/{policy:?}");
    }
}

#[test]
fn policy_cascade_stamp_bypasses_schedule_cascade_guard_not_just_the_breaker() {
    // A composite of the two "bypass" mechanisms: a policy-cascade stamp
    // must resolve via the prune path (RetainSnapshotOnPolicyDelete), NOT
    // the schedule cascade guard, even when the owner is gone/replaced and
    // the schedule cascade is set to Retain (which would otherwise produce
    // the DIFFERENT plan RetainSnapshotOnScheduleDelete).
    let a = pruned(PrunedBy::PolicyCascade);
    let plan = plan_deletion(DeletionFacts {
        policy: DeletionPolicy::Delete,
        owner: OwnerState::GoneOrReplaced,
        cascade: ScheduleDeletePolicy::Retain,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::RetainSnapshotOnPolicyDelete);
}

#[test]
fn skip_cleanup_annotation_wins_over_a_policy_cascade_stamp() {
    // Decision step 1 (skip-cleanup) is absolute — it wins even over an
    // operator prune stamp.
    let a = ann(&[
        (
            PRUNED_BY_ANNOTATION,
            PrunedBy::PolicyCascade.annotation_value(),
        ),
        (SKIP_SNAPSHOT_CLEANUP_ANNOTATION, "true"),
    ]);
    let plan = plan_deletion(DeletionFacts {
        policy: DeletionPolicy::Delete,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::OrphanSnapshot);
}

#[test]
fn unknown_pruned_by_value_treated_external() {
    let a = ann(&[(PRUNED_BY_ANNOTATION, "garbage")]);
    let plan = plan_deletion(DeletionFacts {
        owner: OwnerState::GoneOrReplaced,
        cascade: ScheduleDeletePolicy::Retain,
        ..base_facts(&a)
    });
    // Unrecognized value parses to None => external => the cascade guard
    // applies exactly as if no pruned-by annotation were present at all.
    assert_eq!(plan, DeletionPlan::RetainSnapshotOnScheduleDelete);
}

#[test]
fn skip_cleanup_wins_even_over_held() {
    let a = ann(&[(SKIP_SNAPSHOT_CLEANUP_ANNOTATION, "true")]);
    let plan = plan_deletion(DeletionFacts {
        breaker: BreakerState::Held,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::OrphanSnapshot);
}

#[test]
fn ns_terminating_default_orphan_beats_cascade_guard() {
    let a = BTreeMap::new();
    let plan = plan_deletion(DeletionFacts {
        owner: OwnerState::GoneOrReplaced, // would otherwise trigger the guard
        cascade: ScheduleDeletePolicy::Delete, // would otherwise cascade-delete
        ns_terminating: true,
        ns_policy: Some(NamespaceDeletePolicy::Orphan),
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::OrphanSnapshot);
}

#[test]
fn ns_terminating_unresolved_repo_orphans() {
    // Ports the pre-M2 fail-safe: when the repository can't be resolved while
    // the namespace terminates, the caller passes `ns_policy: None`, and that
    // must orphan regardless of the Snapshot's own policy.
    let a = BTreeMap::new();
    for policy in [
        DeletionPolicy::Delete,
        DeletionPolicy::Retain,
        DeletionPolicy::Orphan,
    ] {
        let plan = plan_deletion(DeletionFacts {
            policy,
            ns_terminating: true,
            ns_policy: None,
            ..base_facts(&a)
        });
        assert_eq!(plan, DeletionPlan::OrphanSnapshot, "{policy:?}");
    }
}

#[test]
fn ns_terminating_delete_optin_bypasses_guard_but_not_breaker() {
    let a = BTreeMap::new();
    // Guard bypassed: owner gone + cascade Retain would normally downgrade to
    // RetainSnapshotOnScheduleDelete, but ns_policy: Delete skips straight to
    // the breaker check instead.
    let allowed = plan_deletion(DeletionFacts {
        owner: OwnerState::GoneOrReplaced,
        cascade: ScheduleDeletePolicy::Retain,
        ns_terminating: true,
        ns_policy: Some(NamespaceDeletePolicy::Delete),
        breaker: BreakerState::Allowed,
        ..base_facts(&a)
    });
    assert_eq!(allowed, DeletionPlan::DeleteSnapshot);

    // Not the breaker: still holdable.
    let held = plan_deletion(DeletionFacts {
        owner: OwnerState::GoneOrReplaced,
        cascade: ScheduleDeletePolicy::Retain,
        ns_terminating: true,
        ns_policy: Some(NamespaceDeletePolicy::Delete),
        breaker: BreakerState::Held,
        ..base_facts(&a)
    });
    assert_eq!(held, DeletionPlan::HoldSnapshotDeletion);
}

#[test]
fn ns_terminating_delete_optin_overrides_policy_cascade_stamp() {
    // Failure-1 regression guard (PR #272): during namespace teardown the
    // SnapshotPolicy cleanup finalizer stamps its live children `pruned-by:
    // policy-cascade` (the default `onPolicyDelete: Retain`). That IMPLICIT stamp
    // must NOT override an EXPLICIT `onNamespaceDelete: Delete` opt-in — the
    // stamped child resolves as an ordinary external destructive deletion, so a
    // `deletionPolicy: Delete` snapshot IS reclaimed, not quietly retained. On
    // HEAD (before the fix) this returned RetainSnapshotOnPolicyDelete: the bug.
    let a = pruned(PrunedBy::PolicyCascade);
    let allowed = plan_deletion(DeletionFacts {
        ns_terminating: true,
        ns_policy: Some(NamespaceDeletePolicy::Delete),
        breaker: BreakerState::Allowed,
        ..base_facts(&a)
    });
    assert_eq!(allowed, DeletionPlan::DeleteSnapshot);

    // Still subject to the mass-deletion breaker, exactly like an unstamped
    // external delete under the same opt-in.
    let held = plan_deletion(DeletionFacts {
        ns_terminating: true,
        ns_policy: Some(NamespaceDeletePolicy::Delete),
        breaker: BreakerState::Held,
        ..base_facts(&a)
    });
    assert_eq!(held, DeletionPlan::HoldSnapshotDeletion);
}

#[test]
fn ns_terminating_policy_cascade_stays_nondestructive_under_default_ns_policy() {
    // The complement to the opt-in override: a `policy-cascade`-stamped child in
    // a terminating namespace under the DEFAULT ns policy (Orphan) or an
    // unresolved repository (None) stays non-destructive — the fix must not make
    // the default namespace-delete path start reclaiming data. Effective policy
    // is Delete (base_facts), so this proves the ns policy, not the stamp, wins.
    // (Passes on HEAD too — the ns-terminating Orphan/None arms short-circuit to
    // OrphanSnapshot before any prune check; this pins that invariant is kept.)
    let a = pruned(PrunedBy::PolicyCascade);
    for ns_policy in [None, Some(NamespaceDeletePolicy::Orphan)] {
        let plan = plan_deletion(DeletionFacts {
            ns_terminating: true,
            ns_policy,
            ..base_facts(&a)
        });
        assert_eq!(plan, DeletionPlan::OrphanSnapshot, "{ns_policy:?}");
    }
}

#[test]
fn ns_terminating_retention_prune_keeps_prune_semantics_never_held() {
    // A genuine OPERATOR prune (Retention/FailedHistory) in a terminating
    // namespace under `onNamespaceDelete: Delete` keeps its prune semantics: it
    // deletes on effective `Delete` and is NEVER held by the breaker, even with
    // the breaker tripping. `plan_ns_delete` routes it to `plan_prune`, not
    // `plan_external`, so the ns-teardown opt-in does not turn retention into a
    // breaker-gated mass deletion — retention must keep working during an
    // incident. This is the invariant the PolicyCascade fix must not disturb.
    for kind in [PrunedBy::Retention, PrunedBy::FailedHistory] {
        let a = pruned(kind);
        let plan = plan_deletion(DeletionFacts {
            ns_terminating: true,
            ns_policy: Some(NamespaceDeletePolicy::Delete),
            breaker: BreakerState::Held,
            ..base_facts(&a)
        });
        assert_eq!(plan, DeletionPlan::DeleteSnapshot, "{kind:?} never held");
    }
}

#[test]
fn breaker_never_holds_retain_or_orphan() {
    let a = BTreeMap::new();
    for policy in [DeletionPolicy::Retain, DeletionPolicy::Orphan] {
        let plan = plan_deletion(DeletionFacts {
            policy,
            breaker: BreakerState::Held,
            ..base_facts(&a)
        });
        assert_ne!(plan, DeletionPlan::HoldSnapshotDeletion, "{policy:?}");
    }
}

#[test]
fn orphaned_ownerref_children_get_their_own_policy() {
    // NoScheduleOwner: manual/discovered/`--cascade=orphan` children never
    // consult the cascade guard, whatever `cascade` happens to carry.
    let a = BTreeMap::new();
    for (policy, expected) in [
        (DeletionPolicy::Delete, DeletionPlan::DeleteSnapshot),
        (DeletionPolicy::Retain, DeletionPlan::RetainSnapshot),
        (DeletionPolicy::Orphan, DeletionPlan::OrphanSnapshot),
    ] {
        let plan = plan_deletion(DeletionFacts {
            policy,
            owner: OwnerState::NoScheduleOwner,
            cascade: ScheduleDeletePolicy::Retain,
            ..base_facts(&a)
        });
        assert_eq!(plan, expected, "{policy:?}");
    }
}

// -- Adopted origin: deletion planning (M5) -------------------------------
//
// `Origin::Adopted` rows run through `plan_deletion` exactly like a produced
// (Scheduled/Manual) row once `effective_deletion_policy` has resolved their
// policy — unlike Discovered, they are NOT forced to Retain. These lock that
// in end-to-end: `effective_deletion_policy(_, Origin::Adopted)` feeding
// `plan_deletion`/`counts_toward_breaker`.

#[test]
fn adopted_external_delete_with_no_schedule_owner_deletes() {
    // Adopted rows are NOT forced-Retain like Discovered: an external delete
    // with no schedule owner and the breaker allowed deletes exactly like a
    // produced row's would.
    let a = BTreeMap::new();
    let policy = effective_deletion_policy(None, Origin::Adopted);
    assert_eq!(policy, DeletionPolicy::Delete);
    let plan = plan_deletion(DeletionFacts {
        policy,
        owner: OwnerState::NoScheduleOwner,
        breaker: BreakerState::Allowed,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::DeleteSnapshot);
}

#[test]
fn adopted_external_delete_counts_toward_and_is_held_by_the_breaker() {
    // Adopted external deletes count toward the per-repo mass-deletion breaker
    // exactly like produced ones — the breaker doesn't special-case origin.
    let a = BTreeMap::new();
    let policy = effective_deletion_policy(None, Origin::Adopted);

    assert!(counts_toward_breaker(DeletionFacts {
        policy,
        owner: OwnerState::NoScheduleOwner,
        ..base_facts(&a)
    }));

    // Over threshold + unacked ⇒ held, never silently deleted.
    let plan = plan_deletion(DeletionFacts {
        policy,
        owner: OwnerState::NoScheduleOwner,
        breaker: BreakerState::Held,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::HoldSnapshotDeletion);
}

#[test]
fn adopted_retention_prune_deletes_never_held() {
    // Retention is the whole point of adoption: an operator retention prune on
    // an adopted row deletes even with the breaker Held (operator prunes always
    // bypass the breaker, same as any produced row).
    let a = pruned(PrunedBy::Retention);
    let policy = effective_deletion_policy(Some(DeletionPolicy::Delete), Origin::Adopted);
    let plan = plan_deletion(DeletionFacts {
        policy,
        owner: OwnerState::NoScheduleOwner,
        breaker: BreakerState::Held,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::DeleteSnapshot);
}

#[test]
fn adopted_policy_cascade_prune_retains_kopia_data() {
    // A SnapshotPolicy-deletion cascade keeps kopia data even for an adopted
    // row: the loud downgrade (RetainSnapshotOnPolicyDelete) fires for Adopted
    // exactly as it does for a produced row under an effective Delete policy.
    let a = pruned(PrunedBy::PolicyCascade);
    let policy = effective_deletion_policy(None, Origin::Adopted);
    let plan = plan_deletion(DeletionFacts {
        policy,
        ..base_facts(&a)
    });
    assert_eq!(plan, DeletionPlan::RetainSnapshotOnPolicyDelete);
}

// -- table test: every meaningful row of the decision matrix -------------

/// One row of the decision matrix: the facts that matter for that row plus
/// the expected [`DeletionPlan`]. `skip`/`pruned` build the annotations map;
/// everything else maps 1:1 onto [`DeletionFacts`].
struct Row {
    policy: DeletionPolicy,
    owner: OwnerState,
    cascade: ScheduleDeletePolicy,
    pruned: Option<PrunedBy>,
    skip: bool,
    ns_terminating: bool,
    ns_policy: Option<NamespaceDeletePolicy>,
    breaker: BreakerState,
    expected: DeletionPlan,
}

#[test]
fn plan_deletion_table_covers_every_meaningful_row() {
    use DeletionPlan::*;
    use DeletionPolicy as P;
    use NamespaceDeletePolicy as NDP;
    use OwnerState as O;
    use ScheduleDeletePolicy as SDP;

    let rows = [
        // -- namespace terminating, ns_policy unresolved (None): always Orphan,
        // whatever the per-CR policy.
        Row {
            policy: P::Delete,
            owner: O::Alive,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: true,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        Row {
            policy: P::Retain,
            owner: O::Alive,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: true,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        Row {
            policy: P::Orphan,
            owner: O::Alive,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: true,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        // -- namespace terminating, ns_policy Orphan (default): always Orphan.
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Delete,
            pruned: None,
            skip: false,
            ns_terminating: true,
            ns_policy: Some(NDP::Orphan),
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        // -- namespace terminating, ns_policy Delete (opt-in cascade): bypasses
        // the schedule cascade guard, but NOT the breaker/prune logic.
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: true,
            ns_policy: Some(NDP::Delete),
            breaker: BreakerState::Allowed,
            expected: DeleteSnapshot,
        },
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: true,
            ns_policy: Some(NDP::Delete),
            breaker: BreakerState::Held,
            expected: HoldSnapshotDeletion,
        },
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: Some(PrunedBy::Retention),
            skip: false,
            ns_terminating: true,
            ns_policy: Some(NDP::Delete),
            breaker: BreakerState::Held,
            expected: DeleteSnapshot,
        },
        Row {
            policy: P::Retain,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: true,
            ns_policy: Some(NDP::Delete),
            breaker: BreakerState::Allowed,
            expected: RetainSnapshot,
        },
        Row {
            policy: P::Orphan,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: true,
            ns_policy: Some(NDP::Delete),
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        // -- live namespace, owner gone/replaced, external (no prune): cascade
        // guard applies.
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: RetainSnapshotOnScheduleDelete,
        },
        Row {
            policy: P::Retain,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: RetainSnapshot,
        },
        Row {
            policy: P::Orphan,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Delete,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: DeleteSnapshot,
        },
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Delete,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Held,
            expected: HoldSnapshotDeletion,
        },
        Row {
            policy: P::Retain,
            owner: O::GoneOrReplaced,
            cascade: SDP::Delete,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: RetainSnapshot,
        },
        Row {
            policy: P::Orphan,
            owner: O::GoneOrReplaced,
            cascade: SDP::Delete,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        // -- live namespace, owner gone/replaced, operator-pruned: guard bypassed.
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: Some(PrunedBy::Retention),
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Held,
            expected: DeleteSnapshot,
        },
        Row {
            policy: P::Retain,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: Some(PrunedBy::FailedHistory),
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: RetainSnapshot,
        },
        Row {
            policy: P::Orphan,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: Some(PrunedBy::Retention),
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        // -- live namespace, owner Alive: plain external, breaker gates Delete
        // only.
        Row {
            policy: P::Delete,
            owner: O::Alive,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: DeleteSnapshot,
        },
        Row {
            policy: P::Delete,
            owner: O::Alive,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Held,
            expected: HoldSnapshotDeletion,
        },
        Row {
            policy: P::Retain,
            owner: O::Alive,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Held,
            expected: RetainSnapshot,
        },
        Row {
            policy: P::Orphan,
            owner: O::Alive,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Held,
            expected: OrphanSnapshot,
        },
        // -- live namespace, no schedule owner at all: same as Alive.
        Row {
            policy: P::Delete,
            owner: O::NoScheduleOwner,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: DeleteSnapshot,
        },
        Row {
            policy: P::Delete,
            owner: O::NoScheduleOwner,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Held,
            expected: HoldSnapshotDeletion,
        },
        Row {
            policy: P::Retain,
            owner: O::NoScheduleOwner,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: RetainSnapshot,
        },
        Row {
            policy: P::Orphan,
            owner: O::NoScheduleOwner,
            cascade: SDP::Retain,
            pruned: None,
            skip: false,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
        // -- skip-cleanup: absolute, beats everything else including Held and
        // ns-terminating.
        Row {
            policy: P::Delete,
            owner: O::GoneOrReplaced,
            cascade: SDP::Retain,
            pruned: None,
            skip: true,
            ns_terminating: false,
            ns_policy: None,
            breaker: BreakerState::Held,
            expected: OrphanSnapshot,
        },
        Row {
            policy: P::Delete,
            owner: O::Alive,
            cascade: SDP::Retain,
            pruned: None,
            skip: true,
            ns_terminating: true,
            ns_policy: None,
            breaker: BreakerState::Allowed,
            expected: OrphanSnapshot,
        },
    ];

    for (i, row) in rows.iter().enumerate() {
        let mut a = BTreeMap::new();
        if let Some(p) = row.pruned {
            a.insert(
                PRUNED_BY_ANNOTATION.to_string(),
                p.annotation_value().to_string(),
            );
        }
        if row.skip {
            a.insert(
                SKIP_SNAPSHOT_CLEANUP_ANNOTATION.to_string(),
                "true".to_string(),
            );
        }
        let plan = plan_deletion(DeletionFacts {
            policy: row.policy,
            annotations: &a,
            owner: row.owner,
            cascade: row.cascade,
            ns_terminating: row.ns_terminating,
            ns_policy: row.ns_policy,
            breaker: row.breaker,
        });
        assert_eq!(plan, row.expected, "row {i}");
    }
}

// -- schedule_owner_ref / owner_state_from --------------------------------

fn schedule_owner_ref_fixture(uid: &str) -> OwnerReference {
    OwnerReference {
        api_version: crate::consts::API_VERSION.to_string(),
        kind: "SnapshotSchedule".to_string(),
        name: "nightly".to_string(),
        uid: uid.to_string(),
        controller: Some(true),
        block_owner_deletion: Some(false),
    }
}

fn schedule_fixture(uid: &str, terminating: bool) -> kopiur_api::SnapshotSchedule {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    let mut sched = kopiur_api::SnapshotSchedule::new(
        "nightly",
        kopiur_api::SnapshotScheduleSpec {
            policy_ref: Some(kopiur_api::common::PolicyRef {
                name: "pg".into(),
                namespace: None,
            }),
            policy_selector: None,
            schedule: kopiur_api::ScheduleSpec {
                cron: "H 2 * * *".into(),
                jitter: None,
                timezone: None,
                run_on_create: false,
                suspend: false,
                concurrency_policy: kopiur_api::ConcurrencyPolicy::Forbid,
                starting_deadline_seconds: None,
            },
            failed_jobs_history_limit: None,
            deletion: None,
        },
    );
    sched.metadata.namespace = Some("media".into());
    sched.metadata.uid = Some(uid.to_string());
    if terminating {
        sched.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
    }
    sched
}

#[test]
fn owner_state_from_same_uid_live_is_alive() {
    let owner = schedule_owner_ref_fixture("uid-1");
    let sched = schedule_fixture("uid-1", false);
    assert_eq!(owner_state_from(Some(&sched), &owner), OwnerState::Alive);
}

#[test]
fn owner_state_from_404_is_gone_or_replaced() {
    let owner = schedule_owner_ref_fixture("uid-1");
    assert_eq!(owner_state_from(None, &owner), OwnerState::GoneOrReplaced);
}

#[test]
fn owner_state_from_terminating_is_gone_or_replaced() {
    let owner = schedule_owner_ref_fixture("uid-1");
    let sched = schedule_fixture("uid-1", true);
    assert_eq!(
        owner_state_from(Some(&sched), &owner),
        OwnerState::GoneOrReplaced
    );
}

#[test]
fn owner_state_from_uid_mismatch_is_gone_or_replaced() {
    let owner = schedule_owner_ref_fixture("old-uid");
    let sched = schedule_fixture("new-uid", false);
    assert_eq!(
        owner_state_from(Some(&sched), &owner),
        OwnerState::GoneOrReplaced
    );
}

#[test]
fn schedule_owner_ref_finds_the_controller_ownerref() {
    let mut backup = dummy_backup();
    backup.metadata.owner_references = Some(vec![schedule_owner_ref_fixture("uid-1")]);
    let found = schedule_owner_ref(&backup).expect("found");
    assert_eq!(found.uid, "uid-1");
    assert_eq!(found.kind, "SnapshotSchedule");
}

#[test]
fn schedule_owner_ref_is_none_without_a_schedule_owner() {
    // No ownerRef at all.
    let backup = dummy_backup();
    assert!(schedule_owner_ref(&backup).is_none());

    // A foreign-kind ownerRef (e.g. a Repository) doesn't count.
    let mut foreign = dummy_backup();
    foreign.metadata.owner_references = Some(vec![OwnerReference {
        api_version: crate::consts::API_VERSION.to_string(),
        kind: "Repository".to_string(),
        name: "nas".to_string(),
        uid: "uid-1".to_string(),
        controller: Some(true),
        block_owner_deletion: Some(false),
    }]);
    assert!(schedule_owner_ref(&foreign).is_none());
}

// -- breaker_state / clamp_ack --------------------------------------------

fn t(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(secs, 0).unwrap()
}

#[test]
fn breaker_state_below_and_at_threshold() {
    let now = t(1_700_000_000);
    assert_eq!(breaker_state(4, 5, now, None), BreakerState::Allowed);
    assert_eq!(breaker_state(5, 5, now, None), BreakerState::Held);
}

#[test]
fn breaker_state_threshold_zero_disables() {
    let now = t(1_700_000_000);
    assert_eq!(
        breaker_state(1_000_000, 0, now, None),
        BreakerState::Allowed
    );
}

#[test]
fn breaker_state_ack_releases_at_or_before_deletion_timestamp_only() {
    let ack = t(1_700_000_000);
    let at_ack = t(1_700_000_000);
    let before_ack = t(1_699_999_000);
    let after_ack = t(1_700_000_001);
    assert_eq!(
        breaker_state(10, 5, at_ack, Some(ack)),
        BreakerState::Allowed
    );
    assert_eq!(
        breaker_state(10, 5, before_ack, Some(ack)),
        BreakerState::Allowed
    );
    // Newer than the ack: stays held despite the ack existing.
    assert_eq!(
        breaker_state(10, 5, after_ack, Some(ack)),
        BreakerState::Held
    );
}

#[test]
fn clamp_ack_pulls_future_acks_back_to_now() {
    let now = t(1_700_000_000);
    let future = t(1_700_001_000);
    let past = t(1_699_000_000);
    assert_eq!(clamp_ack(Some(future), now), Some(now));
    assert_eq!(clamp_ack(Some(past), now), Some(past));
    assert_eq!(clamp_ack(None, now), None);
}

// -- counts_toward_breaker -------------------------------------------------

#[test]
fn counts_toward_breaker_external_delete_is_true() {
    let a = BTreeMap::new();
    assert!(counts_toward_breaker(DeletionFacts {
        policy: DeletionPolicy::Delete,
        ..base_facts(&a)
    }));
}

#[test]
fn counts_toward_breaker_pruned_is_false() {
    let a = pruned(PrunedBy::Retention);
    assert!(!counts_toward_breaker(DeletionFacts {
        policy: DeletionPolicy::Delete,
        ..base_facts(&a)
    }));
}

#[test]
fn counts_toward_breaker_retain_and_orphan_are_false() {
    let a = BTreeMap::new();
    for policy in [DeletionPolicy::Retain, DeletionPolicy::Orphan] {
        assert!(
            !counts_toward_breaker(DeletionFacts {
                policy,
                ..base_facts(&a)
            }),
            "{policy:?}"
        );
    }
}

#[test]
fn counts_toward_breaker_skip_annotated_is_false() {
    let a = ann(&[(SKIP_SNAPSHOT_CLEANUP_ANNOTATION, "true")]);
    assert!(!counts_toward_breaker(DeletionFacts {
        policy: DeletionPolicy::Delete,
        ..base_facts(&a)
    }));
}

#[test]
fn counts_toward_breaker_cascade_retained_is_false() {
    // The cascade-wave-does-not-inflate-count guarantee: an owner-gone Retain
    // downgrade must not itself count as a pending destructive deletion.
    let a = BTreeMap::new();
    assert!(!counts_toward_breaker(DeletionFacts {
        policy: DeletionPolicy::Delete,
        owner: OwnerState::GoneOrReplaced,
        cascade: ScheduleDeletePolicy::Retain,
        ..base_facts(&a)
    }));
}

#[test]
fn counts_toward_breaker_policy_cascade_stamp_is_false_in_live_namespace() {
    // In a LIVE namespace a `policy-cascade` stamp is quiet-retained
    // (RetainSnapshotOnPolicyDelete), never a destructive delete, so it does not
    // count toward the breaker — even though `PolicyCascade` (unlike
    // Retention/FailedHistory) is no longer blanket-exempted. The plan check,
    // not a stamp short-circuit, is what excludes it here.
    let a = pruned(PrunedBy::PolicyCascade);
    assert!(!counts_toward_breaker(DeletionFacts {
        policy: DeletionPolicy::Delete,
        ..base_facts(&a)
    }));
}

#[test]
fn counts_toward_breaker_policy_cascade_stamp_is_true_under_ns_teardown_delete() {
    // The gap this fix closes (PR #272): during a namespace teardown with the
    // explicit `onNamespaceDelete: Delete` opt-in, a `policy-cascade`-stamped
    // child resolves to an external destructive DeleteSnapshot
    // (plan_ns_delete → plan_external), so it MUST count toward the per-repo
    // mass-deletion breaker exactly like an unstamped external child — otherwise
    // a large teardown that stamps all its children mass-deletes kopia data with
    // NO breaker hold and NO ack. On HEAD this returned false (any pruned-by was
    // blanket-exempt), silently nullifying the breaker in the common case.
    let a = pruned(PrunedBy::PolicyCascade);
    assert!(counts_toward_breaker(DeletionFacts {
        policy: DeletionPolicy::Delete,
        ns_terminating: true,
        ns_policy: Some(NamespaceDeletePolicy::Delete),
        ..base_facts(&a)
    }));
}

#[test]
fn counts_toward_breaker_retention_prune_stays_exempt_even_under_ns_teardown_delete() {
    // The stamp-exemption exists for OPERATOR prunes: Retention (and
    // FailedHistory) are bounded, deliberate, steady-state deletes that must
    // keep working during an incident and are NEVER held. That exemption holds
    // EVERYWHERE — including a terminating namespace under `Delete` — so an
    // operator prune never trips the breaker.
    for kind in [PrunedBy::Retention, PrunedBy::FailedHistory] {
        let a = pruned(kind);
        assert!(
            !counts_toward_breaker(DeletionFacts {
                policy: DeletionPolicy::Delete,
                ns_terminating: true,
                ns_policy: Some(NamespaceDeletePolicy::Delete),
                ..base_facts(&a)
            }),
            "{kind:?} must stay breaker-exempt under ns-teardown Delete"
        );
    }
}

#[test]
fn counts_toward_breaker_policy_cascade_stamp_is_false_under_ns_teardown_orphan() {
    // Complement to the Delete case: under the DEFAULT ns policy (Orphan) or an
    // unresolved repository (None), a terminating `policy-cascade` child plans
    // OrphanSnapshot — non-destructive — so it must NOT count toward the breaker.
    let a = pruned(PrunedBy::PolicyCascade);
    for ns_policy in [None, Some(NamespaceDeletePolicy::Orphan)] {
        assert!(
            !counts_toward_breaker(DeletionFacts {
                policy: DeletionPolicy::Delete,
                ns_terminating: true,
                ns_policy,
                ..base_facts(&a)
            }),
            "{ns_policy:?}"
        );
    }
}

// -- breaker_stores_ready / parse_mass_deletion_ack ------------------------

#[test]
fn breaker_stores_ready_requires_present_and_synced() {
    // The store-unset path must behave IDENTICALLY to the not-yet-synced path:
    // both are "can't count yet", never "nothing pending".
    assert!(!breaker_stores_ready(false, false));
    assert!(!breaker_stores_ready(false, true)); // unset, even if a flag says synced
    assert!(!breaker_stores_ready(true, false)); // present but cold
    assert!(breaker_stores_ready(true, true)); // the only trustworthy case
}

#[test]
fn parse_mass_deletion_ack_absent_is_none_not_invalid() {
    let now = chrono::Utc::now();
    assert_eq!(parse_mass_deletion_ack(None, now), (None, false));
}

#[test]
fn parse_mass_deletion_ack_parses_and_clamps_to_now() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    // A past value passes through; a future value is clamped back to now.
    let (past, invalid) = parse_mass_deletion_ack(Some("2025-06-01T00:00:00Z"), now);
    assert!(!invalid);
    assert_eq!(
        past,
        Some(
            chrono::DateTime::parse_from_rfc3339("2025-06-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)
        )
    );
    let (future, invalid) = parse_mass_deletion_ack(Some("2027-06-01T00:00:00Z"), now);
    assert!(!invalid);
    assert_eq!(future, Some(now));
}

#[test]
fn parse_mass_deletion_ack_unparseable_is_ignored_and_flagged() {
    // Fail-safe: a garbage value never disarms the breaker (None) and signals the
    // caller to warn (invalid = true).
    let now = chrono::Utc::now();
    assert_eq!(
        parse_mass_deletion_ack(Some("not-a-date"), now),
        (None, true)
    );
}

// -- should_emit_held_event (transition-only) ------------------------------

#[test]
fn should_emit_held_event_only_on_transition_into_held() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
    let cond = |status: &str| Condition {
        type_: crate::consts::DELETION_HELD_CONDITION.into(),
        status: status.into(),
        reason: crate::consts::MASS_DELETION_BREAKER_REASON.into(),
        message: String::new(),
        last_transition_time: Time(k8s_openapi::jiff::Timestamp::now()),
        observed_generation: None,
    };
    // No prior condition, or a prior `False`, → emit (this is the transition).
    assert!(should_emit_held_event(&[]));
    assert!(should_emit_held_event(&[cond("False")]));
    // Already `True` → suppress (no re-emit while it stays held).
    assert!(!should_emit_held_event(&[cond("True")]));
}

// -- mass_deletion_ack_command / hold message ------------------------------

fn rref(kind: kopiur_api::common::RepositoryKind, name: &str, ns: Option<&str>) -> RepositoryRef {
    RepositoryRef {
        kind,
        name: name.into(),
        namespace: ns.map(Into::into),
    }
}

#[test]
fn ack_command_uses_namespaced_verb_for_a_repository() {
    use kopiur_api::common::RepositoryKind;
    let cmd = mass_deletion_ack_command(
        &rref(RepositoryKind::Repository, "nas", Some("backups")),
        "2026-01-02T03:04:05Z",
    );
    assert_eq!(
        cmd,
        "kubectl -n backups annotate repository/nas \
         kopiur.home-operations.com/allow-mass-deletion=\"2026-01-02T03:04:05Z\" --overwrite"
    );
}

#[test]
fn ack_command_uses_cluster_verb_and_no_namespace_for_a_cluster_repository() {
    use kopiur_api::common::RepositoryKind;
    let cmd = mass_deletion_ack_command(
        &rref(RepositoryKind::ClusterRepository, "shared", None),
        "2026-01-02T03:04:05Z",
    );
    assert_eq!(
        cmd,
        "kubectl annotate clusterrepository/shared \
         kopiur.home-operations.com/allow-mass-deletion=\"2026-01-02T03:04:05Z\" --overwrite"
    );
}

#[test]
fn hold_message_carries_counts_repo_ack_command_and_escape_hatch() {
    use kopiur_api::common::RepositoryKind;
    let msg = mass_deletion_hold_message(
        &rref(RepositoryKind::Repository, "nas", Some("backups")),
        12,
        10,
        "2026-01-02T03:04:05Z",
    );
    assert!(msg.contains("12 pending"), "count: {msg}");
    assert!(msg.contains("threshold of 10"), "threshold: {msg}");
    assert!(msg.contains("Repository `nas`"), "repo: {msg}");
    assert!(
        msg.contains("kubectl -n backups annotate repository/nas"),
        "ack command: {msg}"
    );
    assert!(msg.contains("2026-01-02T03:04:05Z"), "ack value: {msg}");
    assert!(
        msg.contains(SKIP_SNAPSHOT_CLEANUP_ANNOTATION),
        "escape hatch: {msg}"
    );
}

// -- schedule_cascade_retained_message (RetainSnapshotOnScheduleDelete executor) --

#[test]
fn schedule_cascade_retained_message_names_cr_and_opt_in() {
    let msg = schedule_cascade_retained_message("backups", "nightly-1");
    assert!(msg.contains("backups/nightly-1"), "cr name: {msg}");
    assert!(msg.contains("RETAINED"), "states retained: {msg}");
    assert!(
        msg.contains("spec.deletion.onScheduleDelete: Delete"),
        "names the opt-in: {msg}"
    );
}

#[test]
fn schedule_cascade_retained_message_does_not_promise_a_refresh_interval() {
    // Regression: the message used to promise rediscovery "within the
    // repository's catalog refresh interval", which is misleading now that
    // periodicRefresh defaults off — nothing runs on a timer unless the user
    // opted in. The real triggers are a catalog scan (bootstrap, spec change,
    // or a recreated policy's scan request), followed by default auto-adoption.
    let msg = schedule_cascade_retained_message("backups", "nightly-1");
    assert!(
        !msg.contains("within the repository's catalog refresh interval"),
        "must not promise a timer-driven refresh: {msg}"
    );
    assert!(
        msg.contains("next catalog scan"),
        "names the real trigger: {msg}"
    );
    assert!(
        msg.contains("auto-adopted"),
        "states the post-rediscovery outcome: {msg}"
    );
}

// -- policy_cascade_retained_message (RetainSnapshotOnPolicyDelete executor) --

#[test]
fn policy_cascade_retained_message_names_cr_retained_state_and_opt_in() {
    let msg = policy_cascade_retained_message("backups", "nightly-1", true);
    assert!(msg.contains("backups/nightly-1"), "cr name: {msg}");
    assert!(
        msg.contains("RETAINED in the repository"),
        "states retained: {msg}"
    );
    assert!(
        msg.contains("rediscoverable/adoptable"),
        "states rediscoverable: {msg}"
    );
    assert!(
        msg.contains("spec.deletion.onPolicyDelete: Delete"),
        "names the opt-in: {msg}"
    );
}

#[test]
fn policy_cascade_retained_message_cancelled_mid_flight_names_no_completed_snapshot() {
    // A live child cascaded before its mover Job ever finished: there is no
    // kopia snapshot to "retain" — the message must say so, not lie about a
    // snapshot that never existed.
    let msg = policy_cascade_retained_message("backups", "nightly-2", false);
    assert!(
        msg.contains("never completed") && msg.contains("cancelled mid-flight"),
        "states not-completed: {msg}"
    );
    assert!(!msg.contains("RETAINED in the repository"), "{msg}");
    assert!(
        msg.contains("rediscoverable/adoptable"),
        "still states rediscoverable: {msg}"
    );
    assert!(
        msg.contains("spec.deletion.onPolicyDelete: Delete"),
        "still names the opt-in: {msg}"
    );
}

// -- repo_mass_deletion_condition (repo-side, both kinds) ------------------

#[test]
fn repo_mass_deletion_condition_held_at_or_above_threshold() {
    use kopiur_api::common::RepositoryKind;
    let held = repo_mass_deletion_condition(
        &rref(RepositoryKind::Repository, "nas", Some("backups")),
        10,
        10,
        Some("2026-01-02T03:04:05Z"),
    );
    assert!(held.held);
    assert_eq!(
        held.reason,
        crate::consts::MASS_DELETION_THRESHOLD_EXCEEDED_REASON
    );
    assert!(
        held.message
            .contains("kubectl -n backups annotate repository/nas")
    );
}

#[test]
fn repo_mass_deletion_condition_clear_below_threshold_and_when_disabled() {
    use kopiur_api::common::RepositoryKind;
    let repo = rref(RepositoryKind::ClusterRepository, "shared", None);
    let below = repo_mass_deletion_condition(&repo, 9, 10, None);
    assert!(!below.held);
    assert_eq!(
        below.reason,
        crate::consts::MASS_DELETION_BELOW_THRESHOLD_REASON
    );
    // threshold 0 disables the breaker: never held even with a huge count.
    let disabled = repo_mass_deletion_condition(&repo, 1_000, 0, None);
    assert!(!disabled.held);
}

// -- effective_on_schedule_delete ------------------------------------------

#[test]
fn effective_on_schedule_delete_none_is_retain() {
    assert_eq!(
        effective_on_schedule_delete(None),
        ScheduleDeletePolicy::Retain
    );
    assert_eq!(
        effective_on_schedule_delete(Some(ScheduleDeletePolicy::Delete)),
        ScheduleDeletePolicy::Delete
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

// --- batch_job_placement (mirrors delete_job_placement, no per-Snapshot
// namespace fallback — a batch always runs at the repository's home) --------

#[test]
fn batch_placement_runs_in_the_namespaced_repos_home_namespace() {
    assert_eq!(
        batch_job_placement(Some("storage"), Some("kopiur-system"), None),
        DeleteJobPlacement::RunIn("storage".into())
    );
    // Terminating elsewhere doesn't matter.
    assert_eq!(
        batch_job_placement(Some("storage"), Some("kopiur-system"), Some("app")),
        DeleteJobPlacement::RunIn("storage".into())
    );
}

#[test]
fn batch_placement_runs_in_the_operator_namespace_for_cluster_repo() {
    assert_eq!(
        batch_job_placement(None, Some("kopiur-system"), None),
        DeleteJobPlacement::RunIn("kopiur-system".into())
    );
}

#[test]
fn batch_placement_orphans_when_repo_namespace_is_terminating() {
    match batch_job_placement(Some("storage"), Some("kopiur-system"), Some("storage")) {
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
fn batch_placement_orphans_when_operator_namespace_is_terminating() {
    match batch_job_placement(None, Some("kopiur-system"), Some("kopiur-system")) {
        DeleteJobPlacement::OrphanFallback { reason } => {
            assert!(
                reason.contains("kopiur-system"),
                "names the namespace: {reason}"
            );
        }
        other => panic!("expected OrphanFallback, got {other:?}"),
    }
}

#[test]
fn batch_placement_orphans_when_operator_namespace_unknown() {
    match batch_job_placement(None, None, None) {
        DeleteJobPlacement::OrphanFallback { reason } => {
            assert!(reason.contains("KOPIUR_NAMESPACE"), "actionable: {reason}");
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
            ..Default::default()
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

#[test]
fn adopted_defaults_to_delete_like_produced_not_retain() {
    // M5: an adopted row is managed like any produced backup (Scheduled/Manual),
    // NOT forced to Retain like Discovered — retention is the whole point of
    // adoption, so the default must be Delete when the spec leaves it unset.
    assert_eq!(
        effective_deletion_policy(None, Origin::Adopted),
        DeletionPolicy::Delete
    );
    assert_eq!(
        effective_deletion_policy(Some(DeletionPolicy::Orphan), Origin::Adopted),
        DeletionPolicy::Orphan
    );
}

// --- resolve_origin: status-first precedence over the origin label (M5) ----

/// A bare `Snapshot` with `status.origin`/the origin label set as given, for
/// exercising [`resolve_origin`]'s precedence. `None`/`None` mirrors a raw
/// `kubectl create` (no status yet, no label stamped).
fn backup_with_origin(status_origin: Option<Origin>, label: Option<&str>) -> Snapshot {
    let mut backup = dummy_backup();
    if let Some(o) = status_origin {
        backup.status = Some(kopiur_api::snapshot::SnapshotStatus {
            origin: Some(o),
            ..Default::default()
        });
    }
    if let Some(l) = label {
        backup
            .labels_mut()
            .insert(crate::consts::ORIGIN_LABEL.to_string(), l.to_string());
    }
    backup
}

#[test]
fn resolve_origin_defaults_to_manual_with_no_status_or_label() {
    assert_eq!(
        resolve_origin(&backup_with_origin(None, None)),
        Origin::Manual
    );
}

#[test]
fn resolve_origin_status_wins_over_a_conflicting_label() {
    // status.origin is canonical: a stale/mismatched `discovered` label (e.g.
    // from before an M6 adoption re-stamped status) must never demote an
    // already-adopted row back to Discovered.
    let backup = backup_with_origin(Some(Origin::Adopted), Some("discovered"));
    assert_eq!(resolve_origin(&backup), Origin::Adopted);
}

#[test]
fn resolve_origin_reads_adopted_from_the_label_when_status_is_unset() {
    // Label-only adoption (status not yet stamped, e.g. mid-reconcile) still
    // resolves to Adopted — the label is the fallback, not just Discovered's.
    let backup = backup_with_origin(None, Some("adopted"));
    assert_eq!(resolve_origin(&backup), Origin::Adopted);
}

// --- needs_terminal_pin: shared idempotence gate for the Discovered/Adopted
// steady-state pin arms (M5) -------------------------------------------------
//
// `reconcile_inner`'s `pin_discovered_row`/`pin_adopted_row` are thin async IO
// wrappers around a `snapshot_ready_status` patch; the one piece of actual
// decision logic they contain — whether a patch is needed at all this pass —
// is this pure predicate, extracted so the "only pin when unset/divergent,
// never re-patch once converged" idempotence is unit-tested without a cluster.
// The wrapper functions' IO (the patch calls, `ensure_finalizer`, the terminal
// requeue) is exercised live by the M8 e2e suite, not here.

#[test]
fn needs_terminal_pin_true_when_phase_unset() {
    assert!(super::plan::needs_terminal_pin(
        None,
        &SnapshotPhase::Discovered
    ));
    assert!(super::plan::needs_terminal_pin(
        None,
        &SnapshotPhase::Succeeded
    ));
}

#[test]
fn needs_terminal_pin_true_when_phase_diverges_from_target() {
    assert!(super::plan::needs_terminal_pin(
        Some(&SnapshotPhase::Pending),
        &SnapshotPhase::Succeeded
    ));
    assert!(super::plan::needs_terminal_pin(
        Some(&SnapshotPhase::Succeeded),
        &SnapshotPhase::Discovered
    ));
}

#[test]
fn needs_terminal_pin_false_once_converged() {
    // The idempotence both `pin_discovered_row` and `pin_adopted_row` rely on:
    // once the observed phase matches the arm's own target, no further patch.
    assert!(!super::plan::needs_terminal_pin(
        Some(&SnapshotPhase::Discovered),
        &SnapshotPhase::Discovered
    ));
    assert!(!super::plan::needs_terminal_pin(
        Some(&SnapshotPhase::Succeeded),
        &SnapshotPhase::Succeeded
    ));
}

#[test]
fn adopted_row_has_provenance_gates_the_succeeded_pin() {
    use kopiur_api::common::ResolvedIdentity;
    use kopiur_api::snapshot::{SnapshotInfo, SnapshotStatus};
    // I1: a user-applied BARE `origin: adopted` label with NO `status.snapshot`
    // resolves `Adopted` (label fallback) but carries NO controller-written
    // provenance — `pin_adopted_row` must NOT pin it `Succeeded`. A phantom
    // Succeeded row would enter GFS retention (displacing a real snapshot) and set
    // `has_history` (suppressing a recreated policy's scan). On HEAD
    // `pin_adopted_row` pinned ANY Adopted-resolving row.
    let forged = backup_with_origin(None, Some("adopted"));
    assert_eq!(resolve_origin(&forged), Origin::Adopted);
    assert!(
        !super::plan::adopted_row_has_provenance(&forged),
        "a bare origin:adopted label carries no provenance"
    );
    // A genuine adopted row (adopt_one wrote `status.snapshot`) IS pinned.
    let mut genuine = dummy_backup();
    genuine.status = Some(SnapshotStatus {
        origin: Some(Origin::Adopted),
        snapshot: Some(SnapshotInfo {
            kopia_snapshot_id: "k-abc".into(),
            identity: ResolvedIdentity {
                username: "u".into(),
                hostname: "h".into(),
                source_path: Some("/d".into()),
            },
            description: None,
        }),
        ..Default::default()
    });
    assert!(
        super::plan::adopted_row_has_provenance(&genuine),
        "a controller-written adopted row carries provenance"
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

// --- tags_for: user tags merged under the reserved operator tags -----------

mod tags_for_tests {
    use super::*;

    fn backup_with_tags(pairs: &[(&str, &str)]) -> kopiur_api::Snapshot {
        // SnapshotSpec derives no Default; an empty spec via the wire shape.
        let empty: kopiur_api::SnapshotSpec =
            serde_json::from_value(serde_json::json!({})).unwrap();
        let mut snap = kopiur_api::Snapshot::new("b", empty);
        if !pairs.is_empty() {
            snap.spec.tags = Some(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            );
        }
        snap
    }

    #[test]
    fn no_user_tags_yields_only_the_reserved_config_tag() {
        let tags = tags_for(&backup_with_tags(&[]), &sample_policy());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags.get("kopiur:config").map(String::as_str), Some("pg"));
    }

    #[test]
    fn user_tags_are_merged_and_reserved_wins_last() {
        let backup = backup_with_tags(&[("reason", "pre-upgrade"), ("team", "billing")]);
        let tags = tags_for(&backup, &sample_policy());
        assert_eq!(tags.get("reason").map(String::as_str), Some("pre-upgrade"));
        assert_eq!(tags.get("team").map(String::as_str), Some("billing"));
        assert_eq!(
            tags.get("kopiur:config").map(String::as_str),
            Some("pg"),
            "reserved tag must be present and authoritative"
        );
    }

    #[test]
    fn invalid_stored_keys_are_skipped_never_fatal() {
        // Pre-validator stored objects can carry anything; the build path skips
        // (warn) rather than failing the backup of a stored object.
        let long_key = "k".repeat(64);
        let long_value = "v".repeat(257);
        let backup = backup_with_tags(&[
            ("env:prod", "colon"),        // first-colon mangled + reserved-collision risk
            ("kopiur-meta", "spoof"),     // reserved prefix
            (long_key.as_str(), "v"),     // oversize key
            ("big", long_value.as_str()), // oversize value
            ("", "empty"),                // empty key
            ("ok", "kept"),
        ]);
        let tags = tags_for(&backup, &sample_policy());
        assert_eq!(tags.get("ok").map(String::as_str), Some("kept"));
        assert_eq!(
            tags.len(),
            2,
            "only the valid user tag + the reserved tag: {tags:?}"
        );
    }

    #[test]
    fn user_tags_are_capped_at_the_admission_bound() {
        let pairs: Vec<(String, String)> = (0..15)
            .map(|i| (format!("k{i:02}"), "v".to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let tags = tags_for(&backup_with_tags(&refs), &sample_policy());
        // 10 user tags + the reserved config tag.
        assert_eq!(tags.len(), kopiur_api::validate::MAX_SNAPSHOT_TAGS + 1);
        assert!(tags.contains_key("kopiur:config"));
    }
}

// --- recorded_meta: the kopiur-meta value stamped on every produced run -----

mod recorded_meta_tests {
    use super::*;
    use crate::io::InheritOutcome;
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};
    use kopiur_api::common::{MOVER_NONROOT_ID, MoverDefaults, MoverSpec, resolve_mover};
    use kopiur_api::{KOPIUR_META_SCHEMA_V1, RecordedSrc};

    fn inherited(uid: Option<i64>, pins_identity: bool) -> InheritOutcome {
        InheritOutcome::Inherited {
            pod: "app-0".into(),
            container: Some("app".into()),
            uid,
            pins_identity,
        }
    }

    fn sc_uid(uid: i64) -> SecurityContext {
        SecurityContext {
            run_as_user: Some(uid),
            ..Default::default()
        }
    }

    #[test]
    fn inherited_won_records_src_inherited() {
        // The workload pinned 1000 and the merge kept it.
        let resolved = resolve_mover(None, Some(&sc_uid(1000)), None, None, None, None);
        let meta = recorded_meta(&resolved, &inherited(Some(1000), true), None);
        assert_eq!(meta.schema, KOPIUR_META_SCHEMA_V1);
        assert_eq!(meta.src, RecordedSrc::Inherited);
        assert_eq!(meta.uid, Some(1000));
        assert_eq!(meta.fs_group, Some(MOVER_NONROOT_ID), "hardened fsGroup");
    }

    #[test]
    fn restore_only_snapshot_outcome_never_records_src_inherited() {
        // Defensive classification of the restore-only `InheritedFromSnapshot`
        // outcome (unreachable from a backup — validators): replaying a recorded
        // identity is NOT reading the live workload, so even if it ever reached a
        // backup's recorder it must not claim `src: inherited`.
        let resolved = resolve_mover(None, Some(&sc_uid(3001)), None, None, None, None);
        let meta = recorded_meta(
            &resolved,
            &InheritOutcome::InheritedFromSnapshot {
                snapshot: "app/pg-b1".into(),
                uid: Some(3001),
                src: RecordedSrc::Inherited,
            },
            None,
        );
        assert_ne!(meta.src, RecordedSrc::Inherited);
        assert_eq!(
            meta.src,
            RecordedSrc::Defaults,
            "no explicit context either"
        );
    }

    #[test]
    fn explicit_displacing_an_inherited_uid_records_src_explicit() {
        // The workload pinned 0 but the recipe's explicit context won with 3001.
        let mover = MoverSpec {
            security_context: Some(sc_uid(3001)),
            ..Default::default()
        };
        let resolved = resolve_mover(None, Some(&sc_uid(3001)), None, None, None, None);
        let meta = recorded_meta(&resolved, &inherited(Some(0), true), Some(&mover));
        assert_eq!(meta.src, RecordedSrc::Explicit);
        assert_eq!(meta.uid, Some(3001));
    }

    #[test]
    fn fallback_on_an_explicit_pod_level_uid_records_src_explicit() {
        // Inheritance failed; the run proceeded on the recipe's explicit
        // POD-level uid (the promotion carries it to the effective identity).
        let psc = PodSecurityContext {
            run_as_user: Some(2000),
            ..Default::default()
        };
        let mover = MoverSpec {
            pod_security_context: Some(psc.clone()),
            ..Default::default()
        };
        let resolved = resolve_mover(None, None, Some(&psc), None, None, None);
        let meta = recorded_meta(
            &resolved,
            &InheritOutcome::Fallback {
                reason: "no pod matches".into(),
            },
            Some(&mover),
        );
        assert_eq!(meta.src, RecordedSrc::Explicit);
        assert_eq!(meta.uid, Some(2000));
    }

    #[test]
    fn moverdefaults_pinned_uid_records_src_defaults() {
        let defaults = MoverDefaults {
            security_context: Some(sc_uid(2000)),
            ..Default::default()
        };
        let resolved = resolve_mover(Some(&defaults), None, None, None, None, None);
        let meta = recorded_meta(&resolved, &InheritOutcome::NotRequested, None);
        assert_eq!(meta.src, RecordedSrc::Defaults);
        assert_eq!(meta.uid, Some(2000));
    }

    #[test]
    fn nothing_pinned_records_absent_uid_and_src_defaults() {
        // Absent uid = image-determined, recorded honestly (never baked to 65532).
        let resolved = resolve_mover(None, None, None, None, None, None);
        let meta = recorded_meta(&resolved, &InheritOutcome::NotRequested, None);
        assert_eq!(meta.src, RecordedSrc::Defaults);
        assert_eq!(meta.uid, None);
        assert_eq!(meta.gid, None);
        assert_eq!(meta.fs_group, Some(MOVER_NONROOT_ID));
    }

    #[test]
    fn inherit_that_pinned_nothing_over_moverdefaults_records_src_defaults() {
        // pins_identity: false — inheriting was a no-op; moverDefaults' uid is
        // what the mover ran as, and claiming `inherited` would be a lie.
        let defaults = MoverDefaults {
            security_context: Some(sc_uid(2000)),
            ..Default::default()
        };
        let resolved = resolve_mover(Some(&defaults), None, None, None, None, None);
        let meta = recorded_meta(&resolved, &inherited(None, false), None);
        assert_eq!(meta.src, RecordedSrc::Defaults);
        assert_eq!(meta.uid, Some(2000));
    }

    #[test]
    fn gid_records_the_effective_run_as_group() {
        let sc = SecurityContext {
            run_as_user: Some(3001),
            run_as_group: Some(3002),
            ..Default::default()
        };
        let mover = MoverSpec {
            security_context: Some(sc.clone()),
            ..Default::default()
        };
        let resolved = resolve_mover(None, Some(&sc), None, None, None, None);
        let meta = recorded_meta(&resolved, &InheritOutcome::NotRequested, Some(&mover));
        assert_eq!(meta.src, RecordedSrc::Explicit);
        assert_eq!(meta.gid, Some(3002));
    }
}

// --- inherit_verdict: what `inheritSecurityContextFrom` actually achieved. ---
//
// Pure, so every arm is exercised without a cluster. The property that matters most is that
// EVERY requested-inherit path yields a verdict — including the healthy one. An arm that
// returned `None` on success would make the condition write-once-and-stick: a user who fixed
// their recipe would keep a stale `InheritOverridden` forever.

#[cfg(test)]
mod inherit_verdict_tests {
    use super::*;
    use crate::io::InheritOutcome;
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};
    use kopiur_api::common::{MoverDefaults, MoverSpec, ResolvedMover, resolve_mover};

    /// A `ResolvedMover` whose effective uid is `uid` (container-level).
    fn resolved_with_uid(uid: Option<i64>) -> ResolvedMover {
        let sc = uid.map(|u| SecurityContext {
            run_as_user: Some(u),
            ..Default::default()
        });
        resolve_mover(None, sc.as_ref(), None, None, None, None)
    }

    fn inherited(uid: Option<i64>, pins_identity: bool) -> InheritOutcome {
        InheritOutcome::Inherited {
            pod: "app-7c9d8f5b6".into(),
            container: Some("app".into()),
            uid,
            pins_identity,
        }
    }

    #[test]
    fn no_inheritance_requested_yields_no_condition_at_all() {
        assert!(
            inherit_verdict(
                &InheritOutcome::NotRequested,
                &resolved_with_uid(None),
                None
            )
            .is_none(),
            "the condition must not appear on recipes that never asked to inherit"
        );
    }

    #[test]
    fn restore_only_snapshot_outcome_is_defensively_ignored_on_backups() {
        // `InheritedFromSnapshot` is restore-only (the variant is admission-rejected
        // on SnapshotPolicy and the backup reconciler passes no recorded source), so
        // the backup verdict must not invent a condition for it — the restore
        // reconciler owns that reporting.
        assert!(
            inherit_verdict(
                &InheritOutcome::InheritedFromSnapshot {
                    snapshot: "app/pg-b1".into(),
                    uid: Some(3001),
                    src: kopiur_api::recorded::RecordedSrc::Inherited,
                },
                &resolved_with_uid(Some(3001)),
                None
            )
            .is_none(),
            "a backup must never report the restore-only recorded-inherit outcome"
        );
    }

    #[test]
    fn a_working_inherit_reports_positively_so_a_stale_warning_clears() {
        // THE STICKY-CONDITION GUARD. If this arm returned None, a user who fixed an
        // InheritOverridden recipe would keep the stale False forever — nothing would flip it.
        let v = inherit_verdict(
            &inherited(Some(1000), true),
            &resolved_with_uid(Some(1000)),
            None,
        )
        .expect("a resolved inherit must still produce a verdict");
        assert!(
            v.ok,
            "inheritance resolved and stuck — this is the healthy state"
        );
        assert_eq!(v.reason, INHERIT_APPLIED_REASON);
        assert!(
            v.message.contains("app-7c9d8f5b6") && v.message.contains("uid 1000"),
            "the positive verdict should name the pod and the identity: {}",
            v.message
        );
    }

    #[test]
    fn groups_only_inherit_is_healthy_not_a_warning() {
        // No UID, but the workload contributed a group — the mover reads 0640 data through the
        // group bit. Legitimate; warning here would flag a working setup.
        let v = inherit_verdict(&inherited(None, true), &resolved_with_uid(None), None).unwrap();
        assert!(v.ok);
        assert_eq!(v.reason, INHERIT_APPLIED_REASON);
    }

    #[test]
    fn inherit_that_pinned_nothing_warns_and_names_the_real_identity() {
        let v = inherit_verdict(&inherited(None, false), &resolved_with_uid(None), None).unwrap();
        assert!(!v.ok);
        assert_eq!(v.reason, INHERIT_PINNED_NO_UID_REASON);
        assert!(
            v.message.contains("its own image's uid 65532"),
            "with no layer pinning a UID the mover really does run as 65532: {}",
            v.message
        );
    }

    #[test]
    fn pinned_nothing_does_not_claim_65532_when_moverdefaults_supplied_a_uid() {
        // The message used to hardcode "runs as its own image's UID 65532". If moverDefaults
        // pins a UID, the mover runs as THAT — saying 65532 would be a plain lie in the very
        // message meant to explain the problem.
        let defaults = MoverDefaults {
            security_context: Some(SecurityContext {
                run_as_user: Some(2000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = resolve_mover(Some(&defaults), None, None, None, None, None);
        let v = inherit_verdict(&inherited(None, false), &resolved, None).unwrap();
        assert!(!v.ok);
        assert!(
            v.message.contains("uid 2000") && !v.message.contains("65532"),
            "must name the uid the mover actually runs as: {}",
            v.message
        );
    }

    #[test]
    fn an_overridden_inherit_names_the_recipe_when_the_recipe_won() {
        let explicit = MoverSpec {
            security_context: Some(SecurityContext {
                run_as_user: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = inherit_verdict(
            &inherited(Some(2500), true),
            &resolved_with_uid(Some(1000)),
            Some(&explicit),
        )
        .unwrap();
        assert!(!v.ok);
        assert_eq!(v.reason, INHERIT_OVERRIDDEN_REASON);
        assert!(
            v.message
                .contains("this recipe's explicit mover.securityContext.runAsUser")
                && v.message.contains("Remove mover.securityContext.runAsUser"),
            "{}",
            v.message
        );
    }

    #[test]
    fn moverdefaults_cannot_displace_an_inherited_uid_so_the_verdict_is_healthy() {
        // The matter-server regression, through the REAL fold pipeline: the workload
        // pins uid 2500 at the POD level, moverDefaults pins 1000 at the container
        // level. Pre-fix the moverDefaults value shadowed the inherited one across
        // dimensions and this reported InheritOverridden blaming moverDefaults "by
        // design"; post-fix the inherited identity wins every layer and the verdict is
        // the healthy InheritApplied. (This is also why the moverDefaults message
        // branch was deleted: the state it described is unrepresentable now.)
        let defaults = MoverDefaults {
            security_context: Some(SecurityContext {
                run_as_user: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inherited_psc = PodSecurityContext {
            run_as_user: Some(2500),
            ..Default::default()
        };
        // What resolve_mover_security_contexts produces: inherited ⊂ explicit(unset).
        let (recipe_sc, recipe_psc) =
            kopiur_api::common::merge_context_pair(None, Some(&inherited_psc), None, None);
        let resolved = resolve_mover(
            Some(&defaults),
            recipe_sc.as_ref(),
            recipe_psc.as_ref(),
            None,
            None,
            None,
        );
        let v = inherit_verdict(&inherited(Some(2500), true), &resolved, None).unwrap();
        assert!(v.ok, "the inherited uid won every layer: {}", v.message);
        assert!(v.message.contains("uid 2500"), "{}", v.message);
    }

    #[test]
    fn an_overridden_inherit_names_the_pod_level_field_when_that_is_what_won() {
        // The recipe can displace the inherited uid from either dimension; the remedy
        // must name the field the user actually wrote.
        let explicit = MoverSpec {
            pod_security_context: Some(PodSecurityContext {
                run_as_user: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inherited_sc = SecurityContext {
            run_as_user: Some(2500),
            ..Default::default()
        };
        let (recipe_sc, recipe_psc) = kopiur_api::common::merge_context_pair(
            Some(&inherited_sc),
            None,
            explicit.security_context.as_ref(),
            explicit.pod_security_context.as_ref(),
        );
        let resolved = resolve_mover(
            None,
            recipe_sc.as_ref(),
            recipe_psc.as_ref(),
            None,
            None,
            None,
        );
        let v = inherit_verdict(&inherited(Some(2500), true), &resolved, Some(&explicit)).unwrap();
        assert!(!v.ok);
        assert_eq!(v.reason, INHERIT_OVERRIDDEN_REASON);
        assert!(
            v.message.contains("mover.podSecurityContext.runAsUser"),
            "must name the pod-level field that actually won: {}",
            v.message
        );
    }

    #[test]
    fn a_fallback_names_the_identity_it_stood_in_with() {
        let v = inherit_verdict(
            &InheritOutcome::Fallback {
                reason: "no running workload pod mounts the backup source PVC `data`".into(),
            },
            &resolved_with_uid(Some(1000)),
            None,
        )
        .unwrap();
        assert!(!v.ok);
        assert_eq!(v.reason, INHERIT_FALLBACK_REASON);
        assert!(v.message.contains("uid 1000") && v.message.contains("not tracking the workload"));
    }
}

// --- repository_shaped_failure: the gate on the terminal-failure reverify
// nudge (#345). Truth table: only a repository-shaped failure (the connect op,
// or a RepositoryUnavailable class whatever the op) — or NO failure block at
// all (fail-safe) — may nudge; a source-level failure must not.

mod repository_shaped_failure_gate {
    use super::*;

    fn fb(op: Option<&str>, class: &str) -> FailureBlock {
        FailureBlock {
            kopia_error_class: class.to_string(),
            message: "boom".into(),
            stderr_tail: None,
            exit_code: Some(1),
            retry_recommended: false,
            op: op.map(str::to_string),
        }
    }

    #[test]
    fn no_failure_block_is_fail_safe_true() {
        // A controller-stamped MoverJobFailed with no mover-written failure:
        // no evidence it was source-level, so still nudge.
        assert!(repository_shaped_failure(None));
    }

    #[test]
    fn repository_connect_op_nudges_whatever_the_class() {
        // The connect op is repository-level by definition — even a class the
        // gate wouldn't otherwise care about (NotFound: missing repo path).
        assert!(repository_shaped_failure(Some(&fb(
            Some("repository connect"),
            "NotFound"
        ))));
        assert!(repository_shaped_failure(Some(&fb(
            Some("repository connect"),
            "RepositoryUnavailable"
        ))));
    }

    #[test]
    fn repository_unavailable_class_nudges_whatever_the_op() {
        // A backend that went away mid-backup surfaces on `snapshot create`.
        assert!(repository_shaped_failure(Some(&fb(
            Some("snapshot create"),
            "RepositoryUnavailable"
        ))));
    }

    #[test]
    fn source_level_failures_do_not_nudge() {
        // A broken PVC path (NotFound on `snapshot create`) proves nothing
        // about the backend: the probe would succeed anyway (#345).
        assert!(!repository_shaped_failure(Some(&fb(
            Some("snapshot create"),
            "NotFound"
        ))));
        assert!(!repository_shaped_failure(Some(&fb(
            Some("snapshot create"),
            "SourceError"
        ))));
    }

    #[test]
    fn gate_labels_match_the_producing_enums() {
        // The gate compares against the enums' stable labels; pin the exact
        // strings the mover persists so a label rename cannot silently
        // decouple the gate from what lands in status.failure.
        assert_eq!(
            kopiur_mover::error::KopiaOp::RepositoryConnect.as_str(),
            "repository connect"
        );
        assert_eq!(
            kopiur_kopia::KopiaErrorClass::RepositoryUnavailable.as_str(),
            "RepositoryUnavailable"
        );
    }

    #[test]
    fn mover_failed_message_is_byte_stable_and_names_op_and_class() {
        // Derived only from status.failure fields — repeated reconciles of the
        // same outcome must produce byte-identical text (no status churn).
        let f = fb(Some("repository connect"), "RepositoryUnavailable");
        let msg = mover_failed_message(Some(&f));
        assert_eq!(
            msg,
            "the backup failed (repository connect, RepositoryUnavailable): see \
             status.failure and the mover Job/pod logs"
        );
        assert_eq!(msg, mover_failed_message(Some(&f)), "must be deterministic");
        // No op recorded (a non-kopia mover failure): class only.
        let msg = mover_failed_message(Some(&fb(None, "Unknown")));
        assert_eq!(
            msg,
            "the backup failed (Unknown): see status.failure and the mover Job/pod logs"
        );
        // No failure block at all: the original generic text.
        assert_eq!(
            mover_failed_message(None),
            "the backup failed; see status.failure and the mover Job/pod logs"
        );
    }
}
