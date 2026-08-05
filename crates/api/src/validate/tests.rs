use super::*;
use crate::cluster_repository::{AllowedNamespaces, ClusterRepositorySpec};
use crate::common::{
    DeletionPolicy, FailurePolicy, Identity, RepositoryKind, RepositoryMode, RepositoryRef,
    Retention, ScheduleDeletePolicy,
};
use crate::maintenance::RepositoryMaintenanceSpec;
use crate::repository::{RepositoryHealthSpec, RepositorySpec};
use crate::repository_replication::RepositoryReplicationSpec;
use crate::restore::{RestoreSource, RestoreSpec, RestoreTarget};
use crate::snapshot::{Origin, SnapshotSpec};
use crate::snapshot_policy::{Hook, HttpHeader, SnapshotPolicySpec, Source};
use crate::snapshot_schedule::SnapshotScheduleSpec;
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use std::collections::BTreeMap;

fn repo_ref(kind: RepositoryKind, ns: Option<&str>) -> RepositoryRef {
    RepositoryRef {
        kind,
        name: "r".to_string(),
        namespace: ns.map(String::from),
    }
}

// --- validate_repository_ref ---

#[test]
fn cluster_repo_ref_forbids_namespace() {
    let err = validate_repository_ref(&repo_ref(RepositoryKind::ClusterRepository, Some("x")))
        .unwrap_err();
    assert_eq!(
        err,
        ValidationError::ClusterRepoNamespaceForbidden {
            namespace: "x".to_string()
        }
    );
}

#[test]
fn cluster_repo_ref_without_namespace_ok() {
    assert!(validate_repository_ref(&repo_ref(RepositoryKind::ClusterRepository, None)).is_ok());
}

#[test]
fn namespaced_repo_ref_allows_namespace() {
    assert!(validate_repository_ref(&repo_ref(RepositoryKind::Repository, Some("other"))).is_ok());
    assert!(validate_repository_ref(&repo_ref(RepositoryKind::Repository, None)).is_ok());
}

// --- validate_consumer_against_cluster_repo ---

#[test]
fn consumer_allowed_via_list() {
    let allowed = AllowedNamespaces::List(vec!["billing".into(), "staging".into()]);
    assert!(validate_consumer_against_cluster_repo("billing", "repo", &allowed, None).is_ok());
}

#[test]
fn consumer_denied_via_list() {
    let allowed = AllowedNamespaces::List(vec!["billing".into()]);
    let err = validate_consumer_against_cluster_repo("evil", "repo", &allowed, None).unwrap_err();
    assert_eq!(
        err,
        ValidationError::ConsumerNamespaceNotAllowed {
            namespace: "evil".to_string(),
            repo: "repo".to_string()
        }
    );
}

#[test]
fn consumer_allowed_via_all_true_denied_via_all_false() {
    assert!(
        validate_consumer_against_cluster_repo("any", "repo", &AllowedNamespaces::All(true), None)
            .is_ok()
    );
    assert!(
        validate_consumer_against_cluster_repo("any", "repo", &AllowedNamespaces::All(false), None)
            .is_err()
    );
}

#[test]
fn consumer_allowed_via_selector_match() {
    let sel = LabelSelector {
        match_labels: Some(BTreeMap::from([(
            "kopiur.home-operations.com/tier".to_string(),
            "enterprise".to_string(),
        )])),
        ..Default::default()
    };
    let allowed = AllowedNamespaces::Selector(sel);
    let labels = BTreeMap::from([(
        "kopiur.home-operations.com/tier".to_string(),
        "enterprise".to_string(),
    )]);
    assert!(validate_consumer_against_cluster_repo("ns", "repo", &allowed, Some(&labels)).is_ok());
}

#[test]
fn consumer_denied_via_selector_mismatch() {
    let sel = LabelSelector {
        match_labels: Some(BTreeMap::from([(
            "kopiur.home-operations.com/tier".to_string(),
            "enterprise".to_string(),
        )])),
        ..Default::default()
    };
    let allowed = AllowedNamespaces::Selector(sel);
    let labels = BTreeMap::from([(
        "kopiur.home-operations.com/tier".to_string(),
        "free".to_string(),
    )]);
    assert!(validate_consumer_against_cluster_repo("ns", "repo", &allowed, Some(&labels)).is_err());
}

#[test]
fn selector_without_labels_fails_closed() {
    let allowed = AllowedNamespaces::Selector(LabelSelector::default());
    let err = validate_consumer_against_cluster_repo("ns", "repo", &allowed, None).unwrap_err();
    assert_eq!(
        err,
        ValidationError::SelectorLabelsUnavailable {
            namespace: "ns".to_string(),
            repo: "repo".to_string()
        }
    );
}

// --- validate_backup_deletion_policy ---

#[test]
fn discovered_accepts_none_and_retain() {
    assert!(validate_backup_deletion_policy(Origin::Discovered, None).is_ok());
    assert!(
        validate_backup_deletion_policy(Origin::Discovered, Some(DeletionPolicy::Retain)).is_ok()
    );
}

#[test]
fn discovered_rejects_delete_and_orphan() {
    assert_eq!(
        validate_backup_deletion_policy(Origin::Discovered, Some(DeletionPolicy::Delete))
            .unwrap_err(),
        ValidationError::DiscoveredMustRetain {
            got: "Delete".to_string()
        }
    );
    assert!(matches!(
        validate_backup_deletion_policy(Origin::Discovered, Some(DeletionPolicy::Orphan)),
        Err(ValidationError::DiscoveredMustRetain { .. })
    ));
}

#[test]
fn produced_origins_accept_any_policy() {
    for o in [Origin::Scheduled, Origin::Manual] {
        for p in [
            None,
            Some(DeletionPolicy::Delete),
            Some(DeletionPolicy::Retain),
            Some(DeletionPolicy::Orphan),
        ] {
            assert!(validate_backup_deletion_policy(o, p).is_ok());
        }
    }
}

#[test]
fn adopted_accepts_any_policy() {
    // Unlike `discovered`, an adopted row was deliberately re-attached to a
    // SnapshotPolicy so it is managed like a produced backup — any deletionPolicy
    // (including None/Delete/Orphan) is legal.
    for p in [
        None,
        Some(DeletionPolicy::Delete),
        Some(DeletionPolicy::Retain),
        Some(DeletionPolicy::Orphan),
    ] {
        assert!(validate_backup_deletion_policy(Origin::Adopted, p).is_ok());
    }
}

// --- validate_source ---

#[test]
fn source_with_both_pvc_and_selector_is_rejected() {
    use crate::snapshot_policy::{PvcSelector, PvcSource};
    let src = Source {
        pvc: Some(PvcSource { name: "p".into() }),
        pvc_selector: Some(PvcSelector {
            namespace_selector: None,
            label_selector: None,
        }),
        nfs: None,
        source_path_override: None,
        source_path_strategy: None,
        ..Default::default()
    };
    assert!(matches!(
        validate_source(&src),
        Err(ValidationError::MutuallyExclusive { .. })
    ));
}

#[test]
fn source_with_neither_is_rejected() {
    let src = Source {
        pvc: None,
        pvc_selector: None,
        nfs: None,
        source_path_override: None,
        source_path_strategy: None,
        ..Default::default()
    };
    assert!(matches!(
        validate_source(&src),
        Err(ValidationError::MissingRequiredField { .. })
    ));
}

#[test]
fn nfs_source_alone_is_accepted() {
    let src = Source {
        pvc: None,
        pvc_selector: None,
        nfs: Some(NfsVolume {
            server: "nas.lan".into(),
            path: "/export/media".into(),
        }),
        source_path_override: None,
        source_path_strategy: None,
        ..Default::default()
    };
    assert!(validate_source(&src).is_ok());
}

#[test]
fn nfs_source_with_pvc_is_mutually_exclusive() {
    use crate::snapshot_policy::PvcSource;
    let src = Source {
        pvc: Some(PvcSource { name: "p".into() }),
        pvc_selector: None,
        nfs: Some(NfsVolume {
            server: "nas.lan".into(),
            path: "/export/media".into(),
        }),
        source_path_override: None,
        source_path_strategy: None,
        ..Default::default()
    };
    assert!(matches!(
        validate_source(&src),
        Err(ValidationError::MutuallyExclusive { .. })
    ));
}

#[test]
fn nfs_source_with_relative_path_is_rejected() {
    let src = Source {
        pvc: None,
        pvc_selector: None,
        nfs: Some(NfsVolume {
            server: "nas.lan".into(),
            path: "export/media".into(),
        }),
        source_path_override: None,
        source_path_strategy: None,
        ..Default::default()
    };
    assert!(matches!(
        validate_source(&src),
        Err(ValidationError::InvalidFieldValue { .. })
    ));
}

// --- validate_backend (filesystem inline-NFS repo content) ---

#[test]
fn filesystem_nfs_repo_volume_valid_passes() {
    use crate::backend::{Backend, FilesystemBackend, NfsVolume, RepoVolume};
    let b = Backend::Filesystem(FilesystemBackend {
        path: "/repo".into(),
        volume: Some(RepoVolume::Nfs(NfsVolume {
            server: "nas.lan".into(),
            path: "/export/kopia".into(),
        })),
    });
    assert!(validate_backend(&b).is_ok());
}

#[test]
fn filesystem_nfs_repo_volume_relative_path_is_rejected() {
    use crate::backend::{Backend, FilesystemBackend, NfsVolume, RepoVolume};
    let b = Backend::Filesystem(FilesystemBackend {
        path: "/repo".into(),
        volume: Some(RepoVolume::Nfs(NfsVolume {
            server: "nas.lan".into(),
            path: "export/kopia".into(), // not absolute
        })),
    });
    assert!(matches!(
        validate_backend(&b),
        Err(ValidationError::InvalidFieldValue { .. })
    ));
}

#[test]
fn filesystem_pvc_and_object_backends_need_no_content_check() {
    use crate::backend::{Backend, FilesystemBackend, PvcVolume, RepoVolume, S3Backend};
    let pvc = Backend::Filesystem(FilesystemBackend {
        path: "/repo".into(),
        volume: Some(RepoVolume::Pvc(PvcVolume {
            name: "repo-pvc".into(),
        })),
    });
    let bare = Backend::Filesystem(FilesystemBackend {
        path: "/repo".into(),
        volume: None,
    });
    let s3 = Backend::S3(S3Backend {
        bucket: "b".into(),
        prefix: None,
        endpoint: None,
        region: None,
        auth: None,
        tls: None,
    });
    assert!(validate_backend(&pvc).is_ok());
    assert!(validate_backend(&bare).is_ok());
    assert!(validate_backend(&s3).is_ok());
}

#[test]
fn gdrive_requires_a_folder_id() {
    use crate::backend::{Backend, GdriveBackend};
    let ok = Backend::Gdrive(GdriveBackend {
        folder_id: "0ABC".into(),
        credentials_secret_ref: None,
    });
    assert!(validate_backend(&ok).is_ok());
    let empty = Backend::Gdrive(GdriveBackend {
        folder_id: "  ".into(),
        credentials_secret_ref: None,
    });
    assert!(validate_backend(&empty).is_err());
}

#[test]
fn rclone_startup_timeout_must_be_a_go_duration() {
    use crate::backend::{Backend, RcloneBackend};
    let mk = |t: Option<&str>| {
        Backend::Rclone(RcloneBackend {
            remote_path: "r:bucket".into(),
            config_secret_ref: None,
            startup_timeout: t.map(str::to_string),
        })
    };
    assert!(validate_backend(&mk(Some("2m"))).is_ok());
    assert!(validate_backend(&mk(None)).is_ok());
    assert!(validate_backend(&mk(Some("soon"))).is_err());
}

// --- validate_backend_auth / workload identity ---

fn s3_with_auth(auth: Option<crate::backend::BackendAuth>) -> crate::backend::Backend {
    use crate::backend::{Backend, S3Backend};
    Backend::S3(S3Backend {
        bucket: "b".into(),
        prefix: None,
        endpoint: None,
        region: None,
        auth,
        tls: None,
    })
}

fn wi(sa: &str) -> crate::backend::WorkloadIdentity {
    crate::backend::WorkloadIdentity {
        service_account_name: sa.into(),
    }
}

fn secret_ref(name: &str) -> crate::common::SecretRef {
    crate::common::SecretRef {
        name: name.into(),
        namespace: None,
    }
}

#[test]
fn backend_auth_secret_ref_xor_workload_identity() {
    use crate::backend::BackendAuth;
    // Either alone is fine; an empty block is fine (keys may ride the
    // password Secret); both together are ambiguous and rejected.
    let secret_only = s3_with_auth(Some(BackendAuth {
        secret_ref: Some(secret_ref("creds")),
        workload_identity: None,
    }));
    let wi_only = s3_with_auth(Some(BackendAuth {
        secret_ref: None,
        workload_identity: Some(wi("backup-mover")),
    }));
    let empty = s3_with_auth(Some(BackendAuth {
        secret_ref: None,
        workload_identity: None,
    }));
    assert!(validate_backend(&secret_only).is_ok());
    assert!(validate_backend(&wi_only).is_ok());
    assert!(validate_backend(&empty).is_ok());

    let both = s3_with_auth(Some(BackendAuth {
        secret_ref: Some(secret_ref("creds")),
        workload_identity: Some(wi("backup-mover")),
    }));
    let err = validate_backend(&both).unwrap_err();
    assert!(matches!(err, ValidationError::MutuallyExclusive { .. }));
    let msg = err.to_string();
    assert!(msg.contains("auth.secretRef"), "{msg}");
    assert!(msg.contains("auth.workloadIdentity"), "{msg}");
}

#[test]
fn workload_identity_service_account_name_must_be_dns1123() {
    use crate::backend::BackendAuth;
    for bad in ["", "Has-Caps", "trailing-", "-leading", "under_score"] {
        let b = s3_with_auth(Some(BackendAuth {
            secret_ref: None,
            workload_identity: Some(wi(bad)),
        }));
        let err = validate_backend(&b).expect_err(&format!("SA name {bad:?} must be rejected"));
        // The message names the full field path so the user knows where to look.
        assert!(
            err.to_string()
                .contains("auth.workloadIdentity.serviceAccountName"),
            "{err}"
        );
    }
    let ok = s3_with_auth(Some(BackendAuth {
        secret_ref: None,
        workload_identity: Some(wi("backup-mover.v2")),
    }));
    assert!(validate_backend(&ok).is_ok());
}

#[test]
fn azure_workload_identity_requires_storage_account() {
    use crate::backend::{AzureBackend, Backend, BackendAuth};
    let azure = |storage_account: Option<&str>| {
        Backend::Azure(AzureBackend {
            container: "c".into(),
            prefix: None,
            storage_account: storage_account.map(str::to_string),
            auth: Some(BackendAuth {
                secret_ref: None,
                workload_identity: Some(wi("backup-mover")),
            }),
        })
    };
    let err = validate_backend(&azure(None)).unwrap_err();
    let msg = err.to_string();
    // What/why/fix: the webhook injects tenant/client/token, not the account.
    assert!(msg.contains("storageAccount"), "{msg}");
    assert!(msg.contains("workloadIdentity"), "{msg}");
    assert!(validate_backend(&azure(Some("acct"))).is_ok());
}

#[test]
fn replication_auth_same_kind_static_wi_mix_is_rejected() {
    use crate::backend::BackendAuth;
    let static_side = s3_with_auth(Some(BackendAuth {
        secret_ref: Some(secret_ref("creds")),
        workload_identity: None,
    }));
    let wi_side = s3_with_auth(Some(BackendAuth {
        secret_ref: None,
        workload_identity: Some(wi("backup-mover")),
    }));
    // Both directions of the same-kind mix leak the static env into the
    // ambient chain and are rejected with the why in the message.
    for (src, dst) in [(&static_side, &wi_side), (&wi_side, &static_side)] {
        let err = validate_replication_auth(src, dst).unwrap_err();
        assert!(err.to_string().contains("ambient"), "{err}");
    }
    // Same-kind, same auth style on both sides is fine.
    assert!(validate_replication_auth(&static_side, &static_side).is_ok());
    assert!(validate_replication_auth(&wi_side, &wi_side).is_ok());
}

#[test]
fn replication_auth_cross_kind_and_gcs_mixes_are_allowed() {
    use crate::backend::{Backend, BackendAuth, GcsBackend};
    let s3_wi = s3_with_auth(Some(BackendAuth {
        secret_ref: None,
        workload_identity: Some(wi("backup-mover")),
    }));
    let gcs_static = Backend::Gcs(GcsBackend {
        bucket: "b".into(),
        prefix: None,
        auth: Some(BackendAuth {
            secret_ref: Some(secret_ref("gcs-creds")),
            workload_identity: None,
        }),
    });
    let gcs_wi = Backend::Gcs(GcsBackend {
        bucket: "b".into(),
        prefix: None,
        auth: Some(BackendAuth {
            secret_ref: None,
            workload_identity: Some(wi("backup-mover")),
        }),
    });
    // Cross-kind: the static side's env keys mean nothing to the other cloud.
    assert!(validate_replication_auth(&s3_wi, &gcs_static).is_ok());
    // GCS static creds travel as a --credentials-file path, never ambient env,
    // so even a same-kind GCS mix is safe.
    assert!(validate_replication_auth(&gcs_wi, &gcs_static).is_ok());
    assert!(validate_replication_auth(&gcs_static, &gcs_wi).is_ok());
}

#[test]
fn replication_auth_both_wi_must_share_the_service_account() {
    use crate::backend::BackendAuth;
    let wi_a = s3_with_auth(Some(BackendAuth {
        secret_ref: None,
        workload_identity: Some(wi("sa-a")),
    }));
    let wi_b = s3_with_auth(Some(BackendAuth {
        secret_ref: None,
        workload_identity: Some(wi("sa-b")),
    }));
    let err = validate_replication_auth(&wi_a, &wi_b).unwrap_err();
    let msg = err.to_string();
    // The message names both SAs and says the fix (one SA, both stores).
    assert!(msg.contains("sa-a") && msg.contains("sa-b"), "{msg}");
    assert!(msg.contains("same"), "{msg}");
}

#[test]
fn replication_destination_secret_in_another_namespace_is_rejected() {
    use crate::backend::BackendAuth;
    use crate::validate::backend::validate_replication_destination_secret_namespace;
    let dest = s3_with_auth(Some(BackendAuth {
        secret_ref: Some(crate::common::SecretRef {
            name: "r2-creds".into(),
            namespace: Some("other-ns".into()),
        }),
        workload_identity: None,
    }));
    let err = validate_replication_destination_secret_namespace(&dest, "team-a").unwrap_err();
    let msg = err.to_string();
    // What/why/fix: names the Secret, both namespaces, and why envFrom can't reach it.
    assert!(msg.contains("r2-creds"), "{msg}");
    assert!(msg.contains("other-ns") && msg.contains("team-a"), "{msg}");
    assert!(msg.contains("envFrom"), "{msg}");
}

#[test]
fn replication_destination_secret_same_or_absent_namespace_is_allowed() {
    use crate::backend::{Backend, BackendAuth, FilesystemBackend};
    use crate::validate::backend::validate_replication_destination_secret_namespace;
    // Absent namespace = same namespace as the CR.
    let absent = s3_with_auth(Some(BackendAuth {
        secret_ref: Some(secret_ref("r2-creds")),
        workload_identity: None,
    }));
    assert!(validate_replication_destination_secret_namespace(&absent, "team-a").is_ok());
    // Explicit same namespace.
    let same = s3_with_auth(Some(BackendAuth {
        secret_ref: Some(crate::common::SecretRef {
            name: "r2-creds".into(),
            namespace: Some("team-a".into()),
        }),
        workload_identity: None,
    }));
    assert!(validate_replication_destination_secret_namespace(&same, "team-a").is_ok());
    // Workload identity — no auth Secret at all.
    let wi_dest = s3_with_auth(Some(BackendAuth {
        secret_ref: None,
        workload_identity: Some(wi("backup-mover")),
    }));
    assert!(validate_replication_destination_secret_namespace(&wi_dest, "team-a").is_ok());
    // Filesystem — no credentials.
    let fs = Backend::Filesystem(FilesystemBackend {
        path: "/repo".into(),
        volume: None,
    });
    assert!(validate_replication_destination_secret_namespace(&fs, "team-a").is_ok());
}

#[test]
fn nfs_source_with_empty_server_is_rejected() {
    let src = Source {
        pvc: None,
        pvc_selector: None,
        nfs: Some(NfsVolume {
            server: "  ".into(),
            path: "/export/media".into(),
        }),
        source_path_override: None,
        source_path_strategy: None,
        ..Default::default()
    };
    assert!(matches!(
        validate_source(&src),
        Err(ValidationError::MissingRequiredField { .. })
    ));
}

// --- validate_restore ---

fn restore_with(source: RestoreSource, repo: Option<RepositoryRef>) -> RestoreSpec {
    use crate::common::ObjectRef;
    RestoreSpec {
        repository: repo,
        source,
        // A benign existing-PVC target (target is required, ADR-0005 §9); the
        // populator-specific rules are exercised in dedicated tests.
        target: RestoreTarget::PvcRef(ObjectRef {
            name: "tgt".into(),
            namespace: None,
        }),
        options: None,
        policy: None,
        credential_projection: None,
        mover: None,
        failure_policy: None,
    }
}

#[test]
fn restore_identity_requires_repository() {
    use crate::restore::IdentitySource;
    let spec = restore_with(
        RestoreSource::Identity(IdentitySource {
            username: "u".into(),
            hostname: "h".into(),
            source_path: None,
            snapshot_id: None,
            as_of: None,
            offset: None,
        }),
        None,
    );
    assert_eq!(
        validate_restore(&spec).unwrap_err(),
        ValidationError::RestoreSourceRepositoryRequired
    );
}

#[test]
fn restore_identity_with_repository_ok() {
    use crate::restore::IdentitySource;
    let spec = restore_with(
        RestoreSource::Identity(IdentitySource {
            username: "u".into(),
            hostname: "h".into(),
            source_path: None,
            snapshot_id: None,
            as_of: None,
            offset: None,
        }),
        Some(repo_ref(RepositoryKind::Repository, Some("backups"))),
    );
    assert!(validate_restore(&spec).is_ok());
}

#[test]
fn restore_backup_ref_does_not_require_repository() {
    use crate::common::ObjectRef;
    let spec = restore_with(
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        }),
        None,
    );
    assert!(validate_restore(&spec).is_ok());
}

#[test]
fn restore_pvc_target_requires_name() {
    use crate::common::ObjectRef;
    use crate::restore::PvcTemplate;
    let mut spec = restore_with(
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        }),
        None,
    );
    spec.target = RestoreTarget::Pvc(PvcTemplate {
        name: "  ".into(),
        storage_class_name: None,
        capacity: None,
        access_modes: vec![],
    });
    assert!(matches!(
        validate_restore(&spec),
        Err(ValidationError::MissingRequiredField { .. })
    ));
}

#[test]
fn restore_pvc_target_requires_capacity() {
    use crate::common::ObjectRef;
    use crate::restore::PvcTemplate;
    let mut spec = restore_with(
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        }),
        None,
    );
    // The operator creates this PVC, so it must be told the size.
    spec.target = RestoreTarget::Pvc(PvcTemplate {
        name: "restored".into(),
        storage_class_name: None,
        capacity: None,
        access_modes: vec![],
    });
    assert!(matches!(
        validate_restore(&spec),
        Err(ValidationError::MissingRequiredField { field }) if field.contains("capacity")
    ));
    spec.target = RestoreTarget::Pvc(PvcTemplate {
        name: "restored".into(),
        storage_class_name: None,
        capacity: Some("10Gi".into()),
        access_modes: vec![],
    });
    assert!(validate_restore(&spec).is_ok());
}

#[test]
fn restore_options_parallel_must_be_at_least_one() {
    // M2 flag sweep: options.parallel is a count knob (require_min, same shared
    // helper as RepositoryReplication.spec.sync.parallel) — 0 would otherwise
    // silently reach kopia's argv as `--parallel 0`.
    use crate::common::ObjectRef;
    use crate::restore::RestoreOptions;
    let mut spec = restore_with(
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        }),
        None,
    );
    spec.options = Some(RestoreOptions {
        parallel: Some(0),
        ..Default::default()
    });
    let errs = validate_restore_spec(&spec);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::InvalidFieldValue { field, .. } if field.contains("parallel"))),
        "expected an InvalidFieldValue for options.parallel, got {errs:?}"
    );

    spec.options = Some(RestoreOptions {
        parallel: Some(4),
        ..Default::default()
    });
    assert!(validate_restore_spec(&spec).is_empty());
}

#[test]
fn restore_as_of_must_be_rfc3339_and_message_says_how_to_fix() {
    use crate::restore::FromPolicy;
    let spec = restore_with(
        RestoreSource::FromPolicy(FromPolicy {
            name: "pg".into(),
            namespace: None,
            as_of: Some("yesterday".into()),
            offset: 0,
        }),
        None,
    );
    let err = validate_restore(&spec).unwrap_err();
    assert!(matches!(err, ValidationError::InvalidFieldValue { .. }));
    // The message a human acts on: names the field, the bad value, and the fix.
    let msg = err.to_string();
    assert!(msg.contains("restore.source.fromPolicy.asOf"), "{msg}");
    assert!(msg.contains("yesterday"), "{msg}");
    assert!(msg.contains("RFC3339"), "{msg}");
    assert!(msg.contains("2026-05-01T00:00:00Z"), "{msg}");

    // A valid RFC3339 instant (with offset) is accepted.
    let ok = restore_with(
        RestoreSource::FromPolicy(FromPolicy {
            name: "pg".into(),
            namespace: None,
            as_of: Some("2026-05-01T00:00:00+02:00".into()),
            offset: 1,
        }),
        None,
    );
    assert!(validate_restore(&ok).is_ok());
}

#[test]
fn restore_identity_snapshot_id_excludes_as_of_and_offset() {
    use crate::restore::IdentitySource;
    let base = IdentitySource {
        username: "u".into(),
        hostname: "h".into(),
        source_path: None,
        snapshot_id: Some("k1f1ec0a8".into()),
        as_of: None,
        offset: None,
    };
    let with_as_of = restore_with(
        RestoreSource::Identity(IdentitySource {
            as_of: Some("2026-05-01T00:00:00Z".into()),
            ..base.clone()
        }),
        Some(repo_ref(RepositoryKind::Repository, None)),
    );
    assert!(matches!(
        validate_restore(&with_as_of),
        Err(ValidationError::MutuallyExclusive { .. })
    ));
    let with_offset = restore_with(
        RestoreSource::Identity(IdentitySource {
            offset: Some(1),
            ..base.clone()
        }),
        Some(repo_ref(RepositoryKind::Repository, None)),
    );
    assert!(matches!(
        validate_restore(&with_offset),
        Err(ValidationError::MutuallyExclusive { .. })
    ));
    // An explicit offset: 0 is the "latest" default — not a conflict.
    let with_zero = restore_with(
        RestoreSource::Identity(IdentitySource {
            offset: Some(0),
            ..base
        }),
        Some(repo_ref(RepositoryKind::Repository, None)),
    );
    assert!(validate_restore(&with_zero).is_ok());
}

#[test]
fn restore_wait_timeout_must_parse_as_go_duration() {
    use crate::common::ObjectRef;
    use crate::restore::RestorePolicy;
    let mut spec = restore_with(
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        }),
        None,
    );
    spec.policy = Some(RestorePolicy {
        on_missing_snapshot: None,
        wait_timeout: Some("soon".into()),
    });
    let err = validate_restore(&spec).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("restore.policy.waitTimeout"), "{msg}");
    assert!(msg.contains("5m"), "{msg}");

    spec.policy = Some(RestorePolicy {
        on_missing_snapshot: None,
        wait_timeout: Some("5m".into()),
    });
    assert!(validate_restore(&spec).is_ok());
}

#[test]
fn hooks_are_validated_at_admission_with_actionable_messages() {
    use crate::common::PodSelector;
    use crate::snapshot_policy::{Hooks, HttpRequestHook, WorkloadExecHook};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
    let base: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: data } } ]\n",
    );
    let selector = PodSelector {
        pod_selector: LabelSelector::default(),
        container: None,
    };

    // workloadExec with no command → missing required field, with the path.
    let mut spec = base.clone();
    spec.hooks = Some(Hooks {
        before_snapshot: vec![Hook::WorkloadExec(WorkloadExecHook {
            selector: selector.clone(),
            command: vec![],
            timeout: None,
            continue_on_failure: false,
        })],
        after_snapshot: vec![],
    });
    let errs = validate_backup_config(&spec);
    assert!(
        errs.iter().any(|e| e
            .to_string()
            .contains("spec.hooks.beforeSnapshot[0].workloadExec.command")),
        "{errs:?}"
    );

    // httpRequest: relative URL and an unparseable timeout, both rejected
    // with the fix in the message.
    let mut spec = base.clone();
    spec.hooks = Some(Hooks {
        before_snapshot: vec![],
        after_snapshot: vec![Hook::HttpRequest(HttpRequestHook {
            url: "notifier.tools/fire".into(),
            method: Some("FETCH".into()),
            body: None,
            headers: Vec::new(),
            timeout: Some("soon".into()),
            continue_on_failure: false,
        })],
    });
    let errs = validate_backup_config(&spec);
    let all = errs
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("http://"), "{all}");

    // A well-formed hook set passes (lowercase method is normalized).
    let mut spec = base;
    spec.hooks = Some(Hooks {
        before_snapshot: vec![Hook::WorkloadExec(WorkloadExecHook {
            selector,
            command: vec!["sh".into(), "-c".into(), "sync".into()],
            timeout: Some("2m".into()),
            continue_on_failure: false,
        })],
        after_snapshot: vec![Hook::HttpRequest(HttpRequestHook {
            url: "https://notifier.tools.svc/fire".into(),
            method: Some("post".into()),
            body: Some("done".into()),
            headers: Vec::new(),
            timeout: Some("30s".into()),
            continue_on_failure: true,
        })],
    });
    assert!(validate_backup_config(&spec).is_empty());
}

/// Build a `SnapshotPolicySpec` whose single `afterSnapshot` hook is an
/// `httpRequest` with the given URL and headers — the fixture for the header
/// admission checks below.
fn http_hook_spec(url: &str, headers: Vec<HttpHeader>) -> SnapshotPolicySpec {
    use crate::snapshot_policy::{Hooks, HttpRequestHook};
    let mut spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: data } } ]\n",
    );
    spec.hooks = Some(Hooks {
        before_snapshot: vec![],
        after_snapshot: vec![Hook::HttpRequest(HttpRequestHook {
            url: url.into(),
            method: None,
            body: None,
            headers,
            timeout: None,
            continue_on_failure: false,
        })],
    });
    spec
}

#[test]
fn http_hook_headers_validate() {
    let hdr = |n: &str, v: &str| HttpHeader {
        name: n.into(),
        value: v.into(),
    };

    let ok = http_hook_spec(
        "https://example/notify",
        vec![hdr("Content-Type", "application/json")],
    );
    assert!(validate_backup_config(&ok).is_empty());

    let bad_name = http_hook_spec("https://example/notify", vec![hdr("Bad Header", "x")]); // space = not a token
    let errs = validate_backup_config(&bad_name);
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("headers[0].name")),
        "{errs:?}"
    );

    let bad_value = http_hook_spec(
        "https://example/notify",
        vec![hdr("X-Api-Key", "line1\nline2")],
    ); // CR/LF injection
    assert!(!validate_backup_config(&bad_value).is_empty());

    let dup = http_hook_spec(
        "https://example/notify",
        vec![hdr("X-K", "a"), hdr("x-k", "b")],
    ); // case-insensitive dup
    assert!(!validate_backup_config(&dup).is_empty());

    // Allowed edges — pin the mirror guarantee's boundaries so a later tightening
    // of the validator can't silently start rejecting values `http` accepts.

    // (a) A HTAB inside a value is valid field-content, not a control char.
    let tab_value = http_hook_spec("https://example/notify", vec![hdr("X-Tab", "a\tb")]);
    assert!(
        validate_backup_config(&tab_value).is_empty(),
        "a tab in a header value must pass: {:?}",
        validate_backup_config(&tab_value)
    );

    // (b) Non-ASCII UTF-8 bytes are all >= 0x20 and never DEL, so they pass —
    // `HeaderValue::from_str` accepts them too.
    let utf8_value = http_hook_spec("https://example/notify", vec![hdr("X-Utf8", "naïve-ütf8")]);
    assert!(
        validate_backup_config(&utf8_value).is_empty(),
        "a non-ASCII UTF-8 header value must pass: {:?}",
        validate_backup_config(&utf8_value)
    );

    // (c) An empty header name is not a token and must be rejected.
    let empty_name = http_hook_spec("https://example/notify", vec![hdr("", "x")]);
    let errs = validate_backup_config(&empty_name);
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("headers[0].name")),
        "an empty header name must be rejected: {errs:?}"
    );

    // (d) The header-name length cap mirrors `http`'s MAX_HEADER_NAME_LEN exactly:
    // 65535 all-token bytes pass; 65536 are rejected at admission (not at runtime).
    let max_name = "x".repeat(65_535);
    let at_cap = http_hook_spec("https://example/notify", vec![hdr(&max_name, "v")]);
    assert!(
        validate_backup_config(&at_cap).is_empty(),
        "a 65535-byte header name must pass: {:?}",
        validate_backup_config(&at_cap)
    );
    let over_name = "x".repeat(65_536);
    let over_cap = http_hook_spec("https://example/notify", vec![hdr(&over_name, "v")]);
    let errs = validate_backup_config(&over_cap);
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("headers[0].name")),
        "a 65536-byte header name must be rejected: {errs:?}"
    );
}

#[test]
fn http_hook_authorization_header_conflicts_with_url_userinfo() {
    let hdr = |n: &str, v: &str| HttpHeader {
        name: n.into(),
        value: v.into(),
    };

    let both = http_hook_spec(
        "https://user:pass@example/notify",
        vec![hdr("Authorization", "Bearer t")],
    );
    let errs = validate_backup_config(&both);
    assert!(
        errs.iter()
            .any(|e| e.to_string().to_lowercase().contains("authorization")),
        "{errs:?}"
    );

    // Either auth source alone is fine.
    assert!(
        validate_backup_config(&http_hook_spec("https://user:pass@example/notify", vec![]))
            .is_empty()
    );
    assert!(
        validate_backup_config(&http_hook_spec(
            "https://example/notify",
            vec![hdr("Authorization", "Bearer t")]
        ))
        .is_empty()
    );
}

#[test]
fn volume_snapshot_class_with_nfs_source_is_rejected() {
    // An NFS source can't be CSI-snapshotted, so an explicit volumeSnapshotClassName
    // alongside it is a config mistake — rejected with an actionable message.
    let spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
             volumeSnapshotClassName: csi-class\n\
             sources: [ { nfs: { server: nas.lan, path: /export/data } } ]\n",
    );
    let errs = validate_backup_config(&spec);
    let msg = errs
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(msg.contains("spec.volumeSnapshotClassName"), "{msg}");
    assert!(msg.contains("NFS"), "{msg}");

    // A PVC source with a class is fine; an NFS source WITHOUT a class is fine.
    let pvc: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
             volumeSnapshotClassName: csi-class\n\
             sources: [ { pvc: { name: data } } ]\n",
    );
    assert!(validate_backup_config(&pvc).is_empty());
    let nfs: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
             sources: [ { nfs: { server: nas.lan, path: /export/data } } ]\n",
    );
    assert!(validate_backup_config(&nfs).is_empty());
}

// --- pvcConsumer is backup-source-only: rejected on Restore AND Maintenance ---

#[test]
fn pvc_consumer_is_forbidden_on_non_backup_kinds() {
    use crate::common::{InheritSecurityContextFrom, MoverSpec, PvcConsumerInherit};

    // The shared guard powers both validate_restore and validate_maintenance.
    let with_consumer = MoverSpec {
        inherit_security_context_from: Some(InheritSecurityContextFrom::PvcConsumer(
            PvcConsumerInherit::default(),
        )),
        ..Default::default()
    };
    let err = forbid_pvc_consumer(&with_consumer, "maintenance", "Use X.").unwrap_err();
    match err {
        ValidationError::InvalidFieldValue { field, reason } => {
            assert_eq!(
                field,
                "maintenance.mover.inheritSecurityContextFrom.pvcConsumer"
            );
            assert!(reason.contains("backup source"), "{reason}");
        }
        other => panic!("expected InvalidFieldValue, got {other:?}"),
    }

    // workloadSelector and explicit contexts are fine on non-backup kinds.
    let selector_ok = MoverSpec {
        inherit_security_context_from: Some(InheritSecurityContextFrom::WorkloadSelector(
            crate::common::PodSelector {
                pod_selector: Default::default(),
                container: None,
            },
        )),
        ..Default::default()
    };
    assert!(forbid_pvc_consumer(&selector_ok, "restore", "x").is_ok());
    assert!(forbid_pvc_consumer(&MoverSpec::default(), "maintenance", "x").is_ok());
}

// --- the `snapshot` inherit variant is restore-only (variant × kind matrix) ---

/// Helper: a `MoverSpec` carrying one `inheritSecurityContextFrom` variant.
fn mover_with_inherit(i: crate::common::InheritSecurityContextFrom) -> crate::common::MoverSpec {
    crate::common::MoverSpec {
        inherit_security_context_from: Some(i),
        ..Default::default()
    }
}

#[test]
fn forbid_snapshot_inherit_rejects_only_the_snapshot_variant() {
    use crate::common::{
        InheritSecurityContextFrom, MoverSpec, PodSelector, PvcConsumerInherit, SnapshotInherit,
    };

    let err = forbid_snapshot_inherit(
        &mover_with_inherit(InheritSecurityContextFrom::Snapshot(SnapshotInherit {})),
        "snapshotPolicy",
        "restore-only; use pvcConsumer/workloadSelector.",
    )
    .unwrap_err();
    match err {
        ValidationError::InvalidFieldValue { field, reason } => {
            assert_eq!(
                field,
                "snapshotPolicy.mover.inheritSecurityContextFrom.snapshot"
            );
            assert!(reason.contains("restore-only"), "{reason}");
        }
        other => panic!("expected InvalidFieldValue, got {other:?}"),
    }

    // Every other shape passes through untouched.
    for ok in [
        mover_with_inherit(InheritSecurityContextFrom::WorkloadSelector(PodSelector {
            pod_selector: Default::default(),
            container: None,
        })),
        mover_with_inherit(InheritSecurityContextFrom::PvcConsumer(
            PvcConsumerInherit::default(),
        )),
        MoverSpec::default(),
    ] {
        assert!(forbid_snapshot_inherit(&ok, "maintenance", "x").is_ok());
    }
}

#[test]
fn snapshot_inherit_is_forbidden_on_snapshot_policy() {
    use crate::common::{InheritSecurityContextFrom, SnapshotInherit};
    use crate::snapshot_policy::SnapshotPolicySpec;

    let mut spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n",
    );
    assert!(validate_backup_config(&spec).is_empty(), "baseline valid");
    spec.mover = Some(mover_with_inherit(InheritSecurityContextFrom::Snapshot(
        SnapshotInherit {},
    )));
    let errs = validate_backup_config(&spec);
    let msg = errs
        .iter()
        .find(|e| {
            matches!(e, ValidationError::InvalidFieldValue { field, .. }
                if field == "snapshotPolicy.mover.inheritSecurityContextFrom.snapshot")
        })
        .map(|e| e.to_string())
        .unwrap_or_else(|| panic!("expected the snapshot-inherit rejection, got: {errs:?}"));
    // What/why/fix: a backup reads the LIVE workload; `snapshot` is restore-only.
    assert!(msg.contains("live workload"), "{msg}");
    assert!(msg.contains("restore-only"), "{msg}");
}

#[test]
fn snapshot_inherit_is_forbidden_on_maintenance() {
    use crate::common::{InheritSecurityContextFrom, SnapshotInherit};
    use crate::maintenance::{MaintenanceSpec, Ownership};

    let mut spec = MaintenanceSpec {
        repository: repo_ref(RepositoryKind::Repository, None),
        schedule: crate::maintenance::default_maintenance_schedule(),
        ownership: Ownership {
            owner: "kopiur/prod/nas".into(),
            owner_aliases: Vec::new(),
            takeover_policy: Default::default(),
        },
        mover: None,
        failure_policy: None,
        credential_projection: None,
    };
    assert!(validate_maintenance(&spec).is_empty(), "baseline valid");
    spec.mover = Some(mover_with_inherit(InheritSecurityContextFrom::Snapshot(
        SnapshotInherit {},
    )));
    let errs = validate_maintenance(&spec);
    let named = errs.iter().any(|e| {
        matches!(e, ValidationError::InvalidFieldValue { field, .. }
            if field == "maintenance.mover.inheritSecurityContextFrom.snapshot")
    });
    assert!(
        named,
        "expected the snapshot-inherit rejection, got: {errs:?}"
    );
}

#[test]
fn snapshot_inherit_on_replication_is_covered_by_the_whole_field_rejection() {
    use crate::common::{InheritSecurityContextFrom, SnapshotInherit};

    // RepositoryReplication rejects `inheritSecurityContextFrom` ENTIRELY
    // (`forbid_inherit`), so the new variant is rejected without a per-variant rule
    // — verified here so the coverage cannot silently regress if that ever changes.
    let err = super::forbid_inherit(
        &mover_with_inherit(InheritSecurityContextFrom::Snapshot(SnapshotInherit {})),
        "RepositoryReplication spec",
        "is not honored by a replication mover",
    )
    .expect_err("the snapshot variant must be caught by the whole-field rejection");
    assert!(
        err.to_string().contains("inheritSecurityContextFrom"),
        "{err}"
    );
}

#[test]
fn snapshot_inherit_is_valid_on_restore_with_every_source() {
    use crate::restore::RestoreSpec;

    // All admission-time source shapes accept the variant: `snapshotRef` reads the
    // CR directly; `fromPolicy`/`identity` (with or without a pinned snapshotID)
    // resolve recorded meta via the controller's CR-catalog search.
    let sources = [
        "source: { snapshotRef: { name: b } }\nrepository: { kind: Repository, name: r }\n",
        "source: { fromPolicy: { name: pg } }\nrepository: { kind: Repository, name: r }\n",
        "source: { identity: { username: u, hostname: h } }\n\
         repository: { kind: Repository, name: r }\n",
        "source: { identity: { username: u, hostname: h, snapshotID: k1 } }\n\
         repository: { kind: Repository, name: r }\n",
    ];
    for src in sources {
        let yaml = format!(
            "{src}target: {{ pvcRef: {{ name: restored }} }}\n\
             mover: {{ inheritSecurityContextFrom: {{ snapshot: {{}} }} }}\n"
        );
        let spec: RestoreSpec = crate::testutil::from_yaml(&yaml);
        assert!(
            validate_restore(&spec).is_ok(),
            "snapshot inherit must be valid with source: {src}"
        );
        assert!(validate_restore_spec(&spec).is_empty(), "{src}");
    }
}

#[test]
fn snapshot_inherit_is_allowed_with_populator_but_live_pod_variants_are_not() {
    use crate::restore::RestoreSpec;

    // `snapshot` needs no live pod at provision time — the recorded identity is
    // resolved in the controller before the Job — so it is the ONE inherit mode
    // valid with `target.populator` (deliberate carve-out).
    let ok: RestoreSpec = crate::testutil::from_yaml(
        "source: { fromPolicy: { name: pg } }\n\
         target: { populator: {} }\n\
         mover: { inheritSecurityContextFrom: { snapshot: {} } }\n",
    );
    assert!(validate_restore(&ok).is_ok());

    // The live-pod variant keeps its populator rejection.
    let selector: RestoreSpec = crate::testutil::from_yaml(
        "source: { fromPolicy: { name: pg } }\n\
         target: { populator: {} }\n\
         mover: { inheritSecurityContextFrom: { workloadSelector: { podSelector: { \
         matchLabels: { app: pg } } } } }\n",
    );
    let err = validate_restore(&selector).unwrap_err();
    assert!(
        matches!(&err, ValidationError::InvalidFieldValue { field, .. }
            if field == "restore.mover.inheritSecurityContextFrom"),
        "got {err:?}"
    );
}

// --- validate_mover: inheritSecurityContextFrom COMBINES with explicit (container / pod) ---

/// `inheritSecurityContextFrom` alongside an explicit `securityContext`/`podSecurityContext`
/// used to be rejected as mutually exclusive. They are adjacent merge layers
/// (`inherited ⊂ explicit`), so the pair is now accepted: the explicit context overrides the
/// inherited one field-wise, fills what the workload does not pin, and stands in alone when
/// inheritance cannot resolve a pod.
///
/// The old rationale — "so the privileged-mover gate runs on exactly one source" — was never
/// true: the gate has always evaluated the merged hardened+moverDefaults+recipe product, which
/// `enforce_security_context_invariants` normalizes first.
#[test]
fn mover_inherit_combines_with_explicit_contexts() {
    use crate::common::ObjectRef;
    use crate::common::{InheritSecurityContextFrom, MoverSpec, PodSelector};
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;

    let inherit = || {
        Some(InheritSecurityContextFrom::WorkloadSelector(PodSelector {
            pod_selector: LabelSelector::default(),
            container: None,
        }))
    };

    // inherit + container securityContext → accepted (explicit overrides the inherited UID).
    let with_container = MoverSpec {
        security_context: Some(SecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        }),
        inherit_security_context_from: inherit(),
        ..Default::default()
    };
    assert!(validate_mover(&with_container, "Restore mover").is_ok());

    // inherit + POD securityContext → accepted (e.g. force an fsGroup, inherit the rest).
    let with_pod = MoverSpec {
        pod_security_context: Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
        inherit_security_context_from: inherit(),
        ..Default::default()
    };
    assert!(validate_mover(&with_pod, "Restore mover").is_ok());

    // …and through the Restore validator, which is where users actually hit it.
    let mut spec = restore_with(
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        }),
        None,
    );
    spec.mover = Some(with_container);
    assert!(validate_restore(&spec).is_ok());

    // inherit alone, and explicit container+pod together, remain fine.
    let inherit_only = MoverSpec {
        inherit_security_context_from: inherit(),
        ..Default::default()
    };
    assert!(validate_mover(&inherit_only, "Restore mover").is_ok());
    let explicit_both = MoverSpec {
        security_context: Some(SecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        }),
        pod_security_context: Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(validate_mover(&explicit_both, "Restore mover").is_ok());
}

// --- validate_cron ---

#[test]
fn valid_cron_expressions_pass() {
    for expr in ["0 2 * * *", "*/15 * * * *", "0 0 1 1 *", "0 */6 * * *"] {
        assert!(validate_cron(expr).is_ok(), "{expr} should be valid");
    }
}

#[test]
fn jenkins_h_cron_passes_via_placeholder() {
    // H is substituted to 0 for shape-validation; real spread is in jitter.
    assert!(validate_cron("H 2 * * *").is_ok());
    assert!(validate_cron("H H * * *").is_ok());
}

#[test]
fn malformed_cron_is_rejected() {
    for expr in ["not a cron", "99 99 99 99 99", ""] {
        assert!(
            matches!(
                validate_cron(expr),
                Err(ValidationError::InvalidCron { .. })
            ),
            "{expr} should be rejected"
        );
    }
}

#[test]
fn six_and_seven_field_crons_stay_rejected() {
    // The accepted grammar is standard 5-field cron ONLY. croner 3's default
    // parser accepts optional seconds (6 fields) and year (7 fields); kopiur
    // pins both off (jitter::cron_parser) — widening the grammar would bypass
    // substitute_h's positional field math and change scheduling semantics.
    for expr in ["*/30 * * * * *", "0 0 2 * * 1", "0 0 2 * * 1 2030"] {
        assert!(
            matches!(
                validate_cron(expr),
                Err(ValidationError::InvalidCron { .. })
            ),
            "{expr} (6/7-field) should be rejected"
        );
    }
}

// --- validate_repository_no_inline_retention ---

#[test]
fn repository_inline_retention_hook_passes_today() {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::common::{Encryption, SecretKeyRef};
    let spec = RepositorySpec {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: "s".into(),
                namespace: None,
                key: None,
            },
        },
        create: None,
        bootstrap: None,
        mover_defaults: None,
        schedule_defaults: None,
        catalog: None,
        identity_defaults: None,
        server: None,
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
        parameters: None,
        deletion_protection: None,
    };
    assert!(validate_repository_no_inline_retention(&spec).is_ok());
}

// --- aggregate validators ---

#[test]
fn backup_config_aggregate_collects_multiple_errors() {
    let spec = SnapshotPolicySpec {
        repository: repo_ref(RepositoryKind::ClusterRepository, Some("forbidden")),
        identity: Some(Identity::default()),
        sources: vec![], // missing required
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
    };
    let errs = validate_backup_config(&spec);
    // Both: ClusterRepo namespace forbidden + missing sources.
    assert_eq!(errs.len(), 2);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::ClusterRepoNamespaceForbidden { .. }))
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::MissingRequiredField { .. }))
    );
}

#[test]
fn backup_config_valid_spec_has_no_errors() {
    use crate::snapshot_policy::{PvcSource, Source};
    let spec = SnapshotPolicySpec {
        repository: repo_ref(RepositoryKind::Repository, Some("backups")),
        identity: None,
        sources: vec![Source {
            pvc: Some(PvcSource {
                name: "data".into(),
            }),
            pvc_selector: None,
            nfs: None,
            source_path_override: None,
            source_path_strategy: None,
            ..Default::default()
        }],
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
    };
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn backup_aggregate_rejects_discovered_delete() {
    let spec = SnapshotSpec {
        policy_ref: None,
        tags: None,
        failure_policy: None,
        description: None,
        deletion_policy: Some(DeletionPolicy::Delete),
        on_schedule_delete: None,
        pin: false,
    };
    let errs = validate_backup(&spec, Origin::Discovered);
    assert_eq!(errs.len(), 1);
    assert!(matches!(
        errs[0],
        ValidationError::DiscoveredMustRetain { .. }
    ));
}

// --- validate_snapshot_tags ---

fn tags_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn snapshot_tags_clean_pass_and_absent_pass() {
    assert!(validate_snapshot_tags(None).is_empty());
    let tags = tags_of(&[("reason", "pre-upgrade"), ("team", "billing")]);
    assert!(validate_snapshot_tags(Some(&tags)).is_empty());
}

#[test]
fn snapshot_tags_reject_empty_key() {
    let tags = tags_of(&[("", "v")]);
    let errs = validate_snapshot_tags(Some(&tags));
    assert_eq!(errs.len(), 1);
    let msg = errs[0].to_string();
    assert!(msg.contains("spec.tags"), "{msg}");
    assert!(msg.contains("non-empty"), "{msg}");
}

#[test]
fn snapshot_tags_reject_colon_keys_citing_the_first_colon_split() {
    let tags = tags_of(&[("env:prod", "v")]);
    let errs = validate_snapshot_tags(Some(&tags));
    assert_eq!(errs.len(), 1);
    let msg = errs[0].to_string();
    // What (the key), why (kopia's first-colon split + the duplicate-key create
    // failure it can trip), fix (colon-free key).
    assert!(msg.contains("env:prod"), "{msg}");
    assert!(msg.contains("first colon"), "{msg}");
    assert!(msg.contains("duplicate"), "{msg}");
    assert!(msg.contains("colon-free"), "{msg}");
}

#[test]
fn snapshot_tags_reject_reserved_kopiur_prefix() {
    for key in ["kopiur", "kopiur-meta", "kopiurfoo"] {
        let tags = tags_of(&[(key, "v")]);
        let errs = validate_snapshot_tags(Some(&tags));
        assert_eq!(errs.len(), 1, "{key} must be rejected");
        let msg = errs[0].to_string();
        assert!(msg.contains("reserved"), "{msg}");
        assert!(msg.contains(key), "{msg}");
    }
}

#[test]
fn snapshot_tags_reject_oversize_key_and_value() {
    let long_key = "k".repeat(MAX_SNAPSHOT_TAG_KEY_LEN + 1);
    let errs = validate_snapshot_tags(Some(&tags_of(&[(long_key.as_str(), "v")])));
    assert_eq!(errs.len(), 1);
    assert!(errs[0].to_string().contains("63"), "{}", errs[0]);

    let long_value = "v".repeat(MAX_SNAPSHOT_TAG_VALUE_LEN + 1);
    let errs = validate_snapshot_tags(Some(&tags_of(&[("k", long_value.as_str())])));
    assert_eq!(errs.len(), 1);
    assert!(errs[0].to_string().contains("256"), "{}", errs[0]);

    // Exactly at the bounds is fine.
    let max_key = "k".repeat(MAX_SNAPSHOT_TAG_KEY_LEN);
    let max_value = "v".repeat(MAX_SNAPSHOT_TAG_VALUE_LEN);
    assert!(
        validate_snapshot_tags(Some(&tags_of(&[(max_key.as_str(), max_value.as_str())])))
            .is_empty()
    );
}

#[test]
fn snapshot_tags_bound_the_count() {
    let pairs: Vec<(String, String)> = (0..=MAX_SNAPSHOT_TAGS)
        .map(|i| (format!("k{i:02}"), "v".to_string()))
        .collect();
    let tags: BTreeMap<String, String> = pairs.into_iter().collect();
    let errs = validate_snapshot_tags(Some(&tags));
    assert_eq!(errs.len(), 1);
    assert!(errs[0].to_string().contains("10"), "{}", errs[0]);

    let ok: BTreeMap<String, String> = (0..MAX_SNAPSHOT_TAGS)
        .map(|i| (format!("k{i:02}"), "v".to_string()))
        .collect();
    assert!(validate_snapshot_tags(Some(&ok)).is_empty());
}

#[test]
fn snapshot_tags_accumulate_every_problem() {
    let tags = tags_of(&[("a:b", "v"), ("kopiur-x", "v")]);
    let errs = validate_snapshot_tags(Some(&tags));
    assert_eq!(errs.len(), 2, "{errs:?}");
}

#[test]
fn backup_aggregate_rejects_reserved_tags() {
    let spec = SnapshotSpec {
        policy_ref: None,
        tags: Some(tags_of(&[("kopiur-meta", "{}")])),
        failure_policy: None,
        description: None,
        deletion_policy: None,
        on_schedule_delete: None,
        pin: false,
    };
    let errs = validate_backup(&spec, Origin::Manual);
    assert!(
        errs.iter().any(|e| e.to_string().contains("reserved")),
        "{errs:?}"
    );
}

// --- validate_backup_on_schedule_delete ---

#[test]
fn discovered_and_adopted_on_schedule_delete_is_rejected_for_either_variant() {
    for origin in [Origin::Discovered, Origin::Adopted] {
        for v in [ScheduleDeletePolicy::Retain, ScheduleDeletePolicy::Delete] {
            let err = validate_backup_on_schedule_delete(origin, Some(v)).unwrap_err();
            match &err {
                ValidationError::DiscoveredCannotSetOnScheduleDelete { origin: o, got } => {
                    assert_eq!(*o, origin.label_value());
                    assert_eq!(got, &format!("{v:?}"));
                }
                other => panic!("expected DiscoveredCannotSetOnScheduleDelete, got {other:?}"),
            }
            // The message names the origin, the field, and the fix.
            let msg = err.to_string();
            assert!(msg.contains(origin.label_value()), "{msg}");
            assert!(msg.contains("onScheduleDelete"), "{msg}");
            assert!(msg.contains("Remove spec.onScheduleDelete"), "{msg}");
        }
        // Absent is fine.
        assert!(validate_backup_on_schedule_delete(origin, None).is_ok());
    }
}

#[test]
fn scheduled_and_manual_accept_on_schedule_delete() {
    for origin in [Origin::Scheduled, Origin::Manual] {
        for v in [
            None,
            Some(ScheduleDeletePolicy::Retain),
            Some(ScheduleDeletePolicy::Delete),
        ] {
            assert!(
                validate_backup_on_schedule_delete(origin, v).is_ok(),
                "{origin:?} + {v:?} should be accepted"
            );
        }
    }
}

#[test]
fn backup_aggregate_rejects_discovered_on_schedule_delete() {
    let spec = SnapshotSpec {
        policy_ref: None,
        tags: None,
        failure_policy: None,
        description: None,
        deletion_policy: None,
        on_schedule_delete: Some(ScheduleDeletePolicy::Retain),
        pin: false,
    };
    let errs = validate_backup(&spec, Origin::Discovered);
    assert!(errs.iter().any(|e| matches!(
        e,
        ValidationError::DiscoveredCannotSetOnScheduleDelete { .. }
    )));
}

#[test]
fn backup_schedule_aggregate_rejects_bad_cron() {
    use crate::common::PolicyRef;
    use crate::snapshot_schedule::ScheduleSpec;
    let spec = SnapshotScheduleSpec {
        policy_ref: Some(PolicyRef {
            name: "c".into(),
            namespace: None,
        }),
        policy_selector: None,
        schedule: ScheduleSpec {
            cron: "totally bad".into(),
            jitter: None,
            timezone: None,
            run_on_create: false,
            suspend: false,
            concurrency_policy: Default::default(),
            starting_deadline_seconds: None,
        },
        failed_jobs_history_limit: None,
        deletion: None,
    };
    let errs = validate_backup_schedule(&spec);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::InvalidCron { .. }))
    );
}

#[test]
fn backup_schedule_aggregate_rejects_bad_timezone() {
    use crate::common::PolicyRef;
    use crate::snapshot_schedule::ScheduleSpec;
    let spec = SnapshotScheduleSpec {
        policy_ref: Some(PolicyRef {
            name: "c".into(),
            namespace: None,
        }),
        policy_selector: None,
        schedule: ScheduleSpec {
            cron: "0 2 * * *".into(),
            jitter: None,
            timezone: Some("America/Chicgo".into()), // typo'd IANA name
            run_on_create: false,
            suspend: false,
            concurrency_policy: Default::default(),
            starting_deadline_seconds: None,
        },
        failed_jobs_history_limit: None,
        deletion: None,
    };
    let errs = validate_backup_schedule(&spec);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::InvalidTimezone { .. })),
        "a typo'd timezone must be rejected at admission: {errs:?}"
    );
}

#[test]
fn backup_schedule_aggregate_rejects_bad_jitter() {
    use crate::common::PolicyRef;
    use crate::snapshot_schedule::ScheduleSpec;
    let mk = |jitter: &str| SnapshotScheduleSpec {
        policy_ref: Some(PolicyRef {
            name: "c".into(),
            namespace: None,
        }),
        policy_selector: None,
        schedule: ScheduleSpec {
            cron: "0 2 * * *".into(),
            jitter: Some(jitter.into()),
            timezone: None,
            run_on_create: false,
            suspend: false,
            concurrency_policy: Default::default(),
            starting_deadline_seconds: None,
        },
        failed_jobs_history_limit: None,
        deletion: None,
    };
    // Unparseable / overflowing jitter is rejected at admission rather than silently
    // degrading to no-jitter at reconcile.
    for bad in ["every-hour", "9999999999999999h"] {
        let errs = validate_backup_schedule(&mk(bad));
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::InvalidFieldValue { field, .. } if field == "spec.schedule.jitter"
            )),
            "jitter {bad:?} must be rejected: {errs:?}"
        );
    }
    // A valid jitter is accepted.
    assert!(validate_backup_schedule(&mk("30m")).is_empty());
}

// --- §10 policyRef XOR policySelector ---

#[test]
fn schedule_requires_exactly_one_policy_target() {
    use crate::common::PolicyRef;
    use crate::snapshot_schedule::ScheduleSpec;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
    let base_schedule = || ScheduleSpec {
        cron: "0 2 * * *".into(),
        jitter: None,
        timezone: None,
        run_on_create: false,
        suspend: false,
        concurrency_policy: Default::default(),
        starting_deadline_seconds: None,
    };
    let pref = || {
        Some(PolicyRef {
            name: "pg".into(),
            namespace: None,
        })
    };
    let sel = || Some(LabelSelector::default());

    // Neither → MissingRequiredField.
    let neither = SnapshotScheduleSpec {
        policy_ref: None,
        policy_selector: None,
        schedule: base_schedule(),
        failed_jobs_history_limit: None,
        deletion: None,
    };
    assert!(matches!(
        validate_schedule_policy_target(&neither),
        Err(ValidationError::MissingRequiredField { .. })
    ));

    // Both → MutuallyExclusive.
    let both = SnapshotScheduleSpec {
        policy_ref: pref(),
        policy_selector: sel(),
        schedule: base_schedule(),
        failed_jobs_history_limit: None,
        deletion: None,
    };
    assert!(matches!(
        validate_schedule_policy_target(&both),
        Err(ValidationError::MutuallyExclusive { .. })
    ));

    // Exactly one (either form) → ok.
    let only_ref = SnapshotScheduleSpec {
        policy_ref: pref(),
        policy_selector: None,
        schedule: base_schedule(),
        failed_jobs_history_limit: None,
        deletion: None,
    };
    let only_sel = SnapshotScheduleSpec {
        policy_ref: None,
        policy_selector: sel(),
        schedule: base_schedule(),
        failed_jobs_history_limit: None,
        deletion: None,
    };
    assert!(validate_schedule_policy_target(&only_ref).is_ok());
    assert!(validate_schedule_policy_target(&only_sel).is_ok());
    // The aggregate validator surfaces the XOR problem too.
    assert!(
        validate_backup_schedule(&neither)
            .iter()
            .any(|e| matches!(e, ValidationError::MissingRequiredField { .. }))
    );
}

// --- validate_repository_maintenance / validate_repository ---

fn repo_spec_with_maintenance(m: Option<RepositoryMaintenanceSpec>) -> RepositorySpec {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::common::{Encryption, SecretKeyRef};
    RepositorySpec {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: "s".into(),
                namespace: None,
                key: None,
            },
        },
        create: None,
        bootstrap: None,
        mover_defaults: None,
        schedule_defaults: None,
        catalog: None,
        identity_defaults: None,
        server: None,
        maintenance: m,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
        parameters: None,
        deletion_protection: None,
    }
}

#[test]
fn repository_default_managed_maintenance_is_valid() {
    // Absent `maintenance` (default-on) and an empty block both pass.
    assert!(validate_repository(&repo_spec_with_maintenance(None)).is_empty());
    assert!(
        validate_repository(&repo_spec_with_maintenance(Some(
            RepositoryMaintenanceSpec::default()
        )))
        .is_empty()
    );
}

#[test]
fn repository_maintenance_namespace_rejected_on_namespaced_repo() {
    let m = RepositoryMaintenanceSpec {
        namespace: Some("kopia-system".into()),
        ..Default::default()
    };
    let errs = validate_repository(&repo_spec_with_maintenance(Some(m)));
    assert_eq!(
        errs,
        vec![ValidationError::MaintenanceNamespaceOnNamespacedRepo {
            namespace: "kopia-system".into()
        }]
    );
}

#[test]
fn repository_maintenance_namespace_allowed_on_cluster_repo() {
    let m = RepositoryMaintenanceSpec {
        namespace: Some("kopia-system".into()),
        ..Default::default()
    };
    // cluster_scoped = true: the namespace field is the placement selector.
    assert!(validate_repository_maintenance(&m, true).is_empty());
}

#[test]
fn repository_maintenance_bad_override_cron_is_rejected() {
    use crate::common::CronSpec;
    use crate::maintenance::MaintenanceSchedule;
    let m = RepositoryMaintenanceSpec {
        schedule: Some(MaintenanceSchedule {
            quick: CronSpec {
                cron: "totally bad".into(),
                jitter: None,
                timezone: None,
            },
            full: CronSpec {
                cron: "0 3 * * *".into(),
                jitter: None,
                timezone: None,
            },
            timezone: None,
        }),
        ..Default::default()
    };
    let errs = validate_repository_maintenance(&m, false);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::InvalidCron { .. }))
    );
}

#[test]
fn cluster_repository_rejects_all_false() {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::common::{Encryption, SecretKeyRef};
    let spec = ClusterRepositorySpec {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/r".into(),
            volume: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: "s".into(),
                namespace: Some("kopia-system".into()),
                key: None,
            },
        },
        create: None,
        bootstrap: None,
        mover_defaults: None,
        schedule_defaults: None,
        catalog: None,
        server: None,
        allowed_namespaces: AllowedNamespaces::All(false),
        identity_defaults: None,
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
        parameters: None,
        deletion_protection: None,
        credential_projection: None,
    };
    assert!(!validate_cluster_repository(&spec).is_empty());
}

#[test]
fn cluster_repository_rejects_bad_identity_expr() {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::common::IdentityDefaults;
    use crate::common::{Encryption, SecretKeyRef};
    let spec = ClusterRepositorySpec {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/r".into(),
            volume: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: "s".into(),
                namespace: Some("kopia-system".into()),
                key: None,
            },
        },
        create: None,
        bootstrap: None,
        mover_defaults: None,
        schedule_defaults: None,
        catalog: None,
        server: None,
        allowed_namespaces: AllowedNamespaces::All(true),
        // `namspace` is an out-of-scope typo → rejected at admission (ADR-0004 §5).
        identity_defaults: Some(IdentityDefaults {
            cluster: None,
            hostname_expr: Some("namspace".into()),
            username_expr: None,
        }),
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
        parameters: None,
        deletion_protection: None,
        credential_projection: None,
    };
    let errs = validate_cluster_repository(&spec);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::IdentityExprEval { .. })),
        "expected IdentityExprEval, got {errs:?}"
    );
}

// --- create-time immutability (ADR-0005 §7) -----------------------------

fn repo_spec_create(
    enc_secret: &str,
    splitter: Option<&str>,
    hash: Option<&str>,
    create_enc: Option<&str>,
) -> RepositorySpec {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::common::{CreateBehavior, Encryption, SecretKeyRef};
    RepositorySpec {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: enc_secret.into(),
                namespace: None,
                key: None,
            },
        },
        create: Some(CreateBehavior {
            enabled: true,
            encryption: create_enc.map(String::from),
            splitter: splitter.map(String::from),
            hash: hash.map(String::from),
            ecc: None,
        }),
        bootstrap: None,
        mover_defaults: None,
        schedule_defaults: None,
        catalog: None,
        identity_defaults: None,
        server: None,
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
        parameters: None,
        deletion_protection: None,
    }
}

#[test]
fn repository_immutability_accepts_unchanged_fields() {
    let old = repo_spec_create("pw", Some("FIXED-4M"), Some("BLAKE2B-256"), Some("AES256"));
    let new = old.clone();
    assert!(validate_repository_immutability(&old, &new).is_empty());
}

#[test]
fn repository_immutability_allows_changed_password_secret_ref() {
    // Renaming/repointing the password Secret is NOT an immutable change: kopia
    // fixes only the resolved password value, never the Secret reference, so a
    // rename with identical content must pass admission (regression: a GitOps
    // Secret rename used to wedge the whole Kustomization).
    let old = repo_spec_create("kopia-creds", None, None, None);
    let new = repo_spec_create("kopia-creds-renamed", None, None, None);
    assert!(
        validate_repository_immutability(&old, &new).is_empty(),
        "changing only the password Secret ref must be allowed"
    );
}

#[test]
fn cluster_repository_immutability_allows_changed_password_secret_ref() {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::common::{CreateBehavior, Encryption, SecretKeyRef};
    let mk = |secret: &str| ClusterRepositorySpec {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/r".into(),
            volume: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: secret.into(),
                namespace: Some("kopia-system".into()),
                key: None,
            },
        },
        create: Some(CreateBehavior {
            enabled: true,
            encryption: None,
            splitter: Some("FIXED-4M".into()),
            hash: None,
            ecc: None,
        }),
        bootstrap: None,
        mover_defaults: None,
        schedule_defaults: None,
        catalog: None,
        server: None,
        allowed_namespaces: AllowedNamespaces::All(true),
        identity_defaults: None,
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
        parameters: None,
        deletion_protection: None,
        credential_projection: None,
    };
    assert!(
        validate_cluster_repository_immutability(&mk("creds"), &mk("creds-renamed")).is_empty(),
        "changing only the password Secret ref must be allowed"
    );
}

#[test]
fn repository_immutability_rejects_changed_ecc() {
    use crate::common::Ecc;
    let mut old = repo_spec_create("pw", None, None, None);
    let mut new = old.clone();
    if let Some(c) = old.create.as_mut() {
        c.ecc = Some(Ecc {
            algorithm: Some("REED-SOLOMON-CRC32".into()),
            overhead_percent: Some(2),
        });
    }
    if let Some(c) = new.create.as_mut() {
        c.ecc = Some(Ecc {
            algorithm: Some("REED-SOLOMON-CRC32".into()),
            overhead_percent: Some(5), // changed overhead → immutable
        });
    }
    let errs = validate_repository_immutability(&old, &new);
    assert!(errs.contains(&ValidationError::Immutable {
        field: "create.ecc".to_string()
    }));
    // Unchanged ECC → no error.
    assert!(validate_repository_immutability(&old, &old.clone()).is_empty());
}

#[test]
fn repository_immutability_rejects_changed_splitter_hash_and_create_encryption() {
    let old = repo_spec_create("pw", Some("FIXED-4M"), Some("BLAKE2B-256"), Some("AES256"));
    let new = repo_spec_create("pw", Some("DYNAMIC"), Some("HMAC-SHA256"), Some("CHACHA20"));
    let errs = validate_repository_immutability(&old, &new);
    assert!(errs.contains(&ValidationError::Immutable {
        field: "create.splitter".to_string()
    }));
    assert!(errs.contains(&ValidationError::Immutable {
        field: "create.hash".to_string()
    }));
    assert!(errs.contains(&ValidationError::Immutable {
        field: "create.encryption".to_string()
    }));
    // Unchanged encryption secret → no `encryption` immutable error.
    assert!(!errs.contains(&ValidationError::Immutable {
        field: "encryption".to_string()
    }));
}

#[test]
fn repository_immutability_tolerates_absent_create_on_both_sides() {
    // create absent ⇒ no algos pinned; unchanged ⇒ no immutable errors.
    let mut old = repo_spec_create("pw", None, None, None);
    old.create = None;
    let new = old.clone();
    assert!(validate_repository_immutability(&old, &new).is_empty());
}

#[test]
fn cluster_repository_immutability_rejects_changed_splitter() {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::common::{CreateBehavior, Encryption, SecretKeyRef};
    let mk = |splitter: &str| ClusterRepositorySpec {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/r".into(),
            volume: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: "s".into(),
                namespace: Some("kopia-system".into()),
                key: None,
            },
        },
        create: Some(CreateBehavior {
            enabled: true,
            encryption: None,
            splitter: Some(splitter.into()),
            hash: None,
            ecc: None,
        }),
        bootstrap: None,
        mover_defaults: None,
        schedule_defaults: None,
        catalog: None,
        server: None,
        allowed_namespaces: AllowedNamespaces::All(true),
        identity_defaults: None,
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
        parameters: None,
        deletion_protection: None,
        credential_projection: None,
    };
    let old = mk("FIXED-4M");
    let same = mk("FIXED-4M");
    assert!(validate_cluster_repository_immutability(&old, &same).is_empty());
    let changed = mk("DYNAMIC");
    assert!(
        validate_cluster_repository_immutability(&old, &changed).contains(
            &ValidationError::Immutable {
                field: "create.splitter".to_string()
            }
        )
    );
}

// --- identity-collision detection (ADR-0005 §6) -------------------------

#[test]
fn identity_collision_same_repo_same_identity_is_detected() {
    let existing = vec![ExistingIdentity {
        identity: "pg@billing:/pvc/data".into(),
        repo_key: "ClusterRepository/shared".into(),
        name: "billing/pg-a".into(),
    }];
    assert_eq!(
        detect_identity_collision(
            "pg@billing:/pvc/data",
            "ClusterRepository/shared",
            "billing/pg-b",
            &existing
        ),
        Some("billing/pg-a".to_string())
    );
}

#[test]
fn identity_collision_different_repo_is_allowed() {
    let existing = vec![ExistingIdentity {
        identity: "pg@billing:/pvc/data".into(),
        repo_key: "ClusterRepository/shared".into(),
        name: "billing/pg-a".into(),
    }];
    // Same identity but a different repository → no collision (separate history).
    assert_eq!(
        detect_identity_collision(
            "pg@billing:/pvc/data",
            "Repository/billing/nas",
            "billing/pg-b",
            &existing
        ),
        None
    );
}

#[test]
fn identity_collision_skips_self() {
    let existing = vec![ExistingIdentity {
        identity: "pg@billing:/pvc/data".into(),
        repo_key: "ClusterRepository/shared".into(),
        name: "billing/pg-a".into(),
    }];
    // A re-apply of the same object (same name) must not collide with itself.
    assert_eq!(
        detect_identity_collision(
            "pg@billing:/pvc/data",
            "ClusterRepository/shared",
            "billing/pg-a",
            &existing
        ),
        None
    );
}

#[test]
fn identity_collision_different_identity_is_allowed() {
    let existing = vec![ExistingIdentity {
        identity: "pg@billing:/pvc/data".into(),
        repo_key: "ClusterRepository/shared".into(),
        name: "billing/pg-a".into(),
    }];
    assert_eq!(
        detect_identity_collision(
            "redis@billing:/pvc/cache",
            "ClusterRepository/shared",
            "billing/redis",
            &existing
        ),
        None
    );
}

// --- §13(d) RepositoryReplication ---

fn replication_spec(
    source: RepositoryRef,
    dest: crate::backend::Backend,
    cron: &str,
) -> RepositoryReplicationSpec {
    use crate::common::CronSpec;
    RepositoryReplicationSpec {
        source_ref: source,
        destination: dest,
        schedule: CronSpec {
            cron: cron.into(),
            jitter: None,
            timezone: None,
        },
        mover: None,
        suspend: false,
        sync: None,
    }
}

#[test]
fn replication_valid_spec_has_no_errors() {
    use crate::backend::{Backend, S3Backend};
    let spec = replication_spec(
        repo_ref(RepositoryKind::Repository, None),
        Backend::S3(S3Backend {
            bucket: "mirror".into(),
            prefix: None,
            endpoint: None,
            region: None,
            auth: None,
            tls: None,
        }),
        "0 5 * * *",
    );
    assert!(validate_repository_replication(&spec).is_empty());
}

#[test]
fn replication_rejects_bad_cron_and_bad_clusterrepo_ref() {
    use crate::backend::{Backend, S3Backend};
    // A ClusterRepository sourceRef with a namespace + a bad cron → two errors.
    let spec = replication_spec(
        repo_ref(RepositoryKind::ClusterRepository, Some("oops")),
        Backend::S3(S3Backend {
            bucket: "mirror".into(),
            prefix: None,
            endpoint: None,
            region: None,
            auth: None,
            tls: None,
        }),
        "not a cron",
    );
    let errs = validate_repository_replication(&spec);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::ClusterRepoNamespaceForbidden { .. }))
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::InvalidCron { .. }))
    );
}

#[test]
fn replication_rejects_invalid_destination_backend_content() {
    use crate::backend::{Backend, FilesystemBackend, NfsVolume, RepoVolume};
    // A filesystem destination with a relative NFS repo path is invalid content.
    let spec = replication_spec(
        repo_ref(RepositoryKind::Repository, None),
        Backend::Filesystem(FilesystemBackend {
            path: "/mirror".into(),
            volume: Some(RepoVolume::Nfs(NfsVolume {
                server: "nas".into(),
                path: "relative/path".into(),
            })),
        }),
        "0 5 * * *",
    );
    let errs = validate_repository_replication(&spec);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::InvalidFieldValue { .. }))
    );
}

#[test]
fn replication_sync_all_zero_is_valid() {
    // #216: a `sync` block is optional and every field individually optional;
    // a fully-populated but in-range block must not error.
    use crate::backend::{Backend, S3Backend};
    use crate::repository_replication::SyncOptions;
    let mut spec = replication_spec(
        repo_ref(RepositoryKind::Repository, None),
        Backend::S3(S3Backend {
            bucket: "mirror".into(),
            prefix: None,
            endpoint: None,
            region: None,
            auth: None,
            tls: None,
        }),
        "0 5 * * *",
    );
    spec.sync = Some(SyncOptions {
        parallel: Some(4),
        delete_extra: true,
        must_exist: Some(false),
        times: Some(true),
        update: Some(false),
        max_download_speed_bytes_per_second: Some(1_000_000),
        max_upload_speed_bytes_per_second: Some(500_000),
    });
    assert!(validate_repository_replication(&spec).is_empty());
}

#[test]
fn replication_sync_rejects_zero_parallel_and_zero_speeds() {
    // #216: `parallel`/the speed caps must be >= 1 — 0 is meaningless for a copy
    // parallelism or a throughput cap, and would otherwise silently reach kopia's
    // argv as `--parallel 0` etc.
    use crate::backend::{Backend, S3Backend};
    use crate::repository_replication::SyncOptions;
    let mut spec = replication_spec(
        repo_ref(RepositoryKind::Repository, None),
        Backend::S3(S3Backend {
            bucket: "mirror".into(),
            prefix: None,
            endpoint: None,
            region: None,
            auth: None,
            tls: None,
        }),
        "0 5 * * *",
    );
    spec.sync = Some(SyncOptions {
        parallel: Some(0),
        max_download_speed_bytes_per_second: Some(0),
        max_upload_speed_bytes_per_second: Some(0),
        ..Default::default()
    });
    let errs = validate_repository_replication(&spec);
    let invalid_count = errs
        .iter()
        .filter(|e| matches!(e, ValidationError::InvalidFieldValue { .. }))
        .count();
    assert_eq!(
        invalid_count, 3,
        "parallel + both speed caps must each be rejected independently, got {errs:?}"
    );
}

#[test]
fn replication_destination_differs_decision() {
    use crate::backend::{Backend, FilesystemBackend, S3Backend};
    let fs_a = Backend::Filesystem(FilesystemBackend {
        path: "/a".into(),
        volume: None,
    });
    let fs_a2 = Backend::Filesystem(FilesystemBackend {
        path: "/a".into(),
        volume: None,
    });
    let fs_b = Backend::Filesystem(FilesystemBackend {
        path: "/b".into(),
        volume: None,
    });
    // Same path → same target (self-replication).
    assert!(!replication_destination_differs(&fs_a, &fs_a2));
    // Different path → differ.
    assert!(replication_destination_differs(&fs_a, &fs_b));
    // S3 differs from filesystem.
    let s3 = Backend::S3(S3Backend {
        bucket: "b".into(),
        prefix: None,
        endpoint: None,
        region: None,
        auth: None,
        tls: None,
    });
    assert!(replication_destination_differs(&fs_a, &s3));
    // Same S3 bucket+prefix (and same endpoint/region) → same target.
    let s3b = Backend::S3(S3Backend {
        bucket: "b".into(),
        prefix: None,
        endpoint: None,
        region: None,
        auth: None,
        tls: None,
    });
    assert!(!replication_destination_differs(&s3, &s3b));
}

#[test]
fn replication_destination_differs_by_s3_endpoint_and_region() {
    // Issue #248: two DISTINCT S3 providers that happen to share a bucket name
    // (`kopiur`, no prefix) must be treated as different targets — keying on
    // bucket+prefix alone wrongly collapsed them to one and rejected the
    // replication as a self-replication.
    use crate::backend::{Backend, S3Backend};
    let s3 = |endpoint: Option<&str>, region: Option<&str>| {
        Backend::S3(S3Backend {
            bucket: "kopiur".into(),
            prefix: None,
            endpoint: endpoint.map(Into::into),
            region: region.map(Into::into),
            auth: None,
            tls: None,
        })
    };
    let nas = s3(Some("nas.example:3000"), None);
    let e2 = s3(Some("t3u7.fra3.idrivee2-58.com"), Some("eu-central-2"));
    // Different endpoints → distinct targets (the exact reported case).
    assert!(replication_destination_differs(&nas, &e2));
    // Same bucket, differ only by region → still distinct targets.
    let aws_us = s3(None, Some("us-east-1"));
    let aws_eu = s3(None, Some("eu-west-1"));
    assert!(replication_destination_differs(&aws_us, &aws_eu));
    // Fully identical endpoint+region+bucket+prefix → same target (a real
    // self-replication is still correctly rejected).
    assert!(!replication_destination_differs(
        &nas,
        &s3(Some("nas.example:3000"), None)
    ));
}

#[test]
fn replication_destination_differs_by_azure_account_and_fs_volume() {
    // Same class of bug as #248 on the other backends that carry an
    // above-the-container/path discriminator.
    use crate::backend::{
        AzureBackend, Backend, FilesystemBackend, NfsVolume, PvcVolume, RepoVolume,
    };
    let azure = |account: Option<&str>| {
        Backend::Azure(AzureBackend {
            container: "kopiur".into(),
            prefix: None,
            storage_account: account.map(Into::into),
            auth: None,
        })
    };
    // Same container name under two different storage accounts → distinct.
    assert!(replication_destination_differs(
        &azure(Some("acctprod")),
        &azure(Some("acctdr")),
    ));

    // Filesystem: the same in-pod mount path (`/repo`, the common default)
    // backed by two different volumes → distinct targets.
    let fs = |vol: Option<RepoVolume>| {
        Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: vol,
        })
    };
    let pvc_a = fs(Some(RepoVolume::Pvc(PvcVolume {
        name: "repo-a".into(),
    })));
    let pvc_b = fs(Some(RepoVolume::Pvc(PvcVolume {
        name: "repo-b".into(),
    })));
    assert!(replication_destination_differs(&pvc_a, &pvc_b));
    let nfs = fs(Some(RepoVolume::Nfs(NfsVolume {
        server: "nas.lan".into(),
        path: "/export/kopia".into(),
    })));
    assert!(replication_destination_differs(&pvc_a, &nfs));
    // Same path + same PVC → same target.
    assert!(!replication_destination_differs(
        &pvc_a,
        &fs(Some(RepoVolume::Pvc(PvcVolume {
            name: "repo-a".into()
        }))),
    ));
}

// --- validate_catalog_bounds ---

fn catalog(v: serde_json::Value) -> crate::common::CatalogBounds {
    serde_json::from_value(v).expect("CatalogBounds parses")
}

#[test]
fn catalog_bounds_valid_passes_both_scopes() {
    let c = catalog(serde_json::json!({
        "retain": { "perIdentity": 100, "maxAgeDays": 90 },
        "refreshInterval": "5m",
    }));
    assert!(validate_catalog_bounds(&c, false).is_empty());
    assert!(validate_catalog_bounds(&c, true).is_empty());
    // perIdentity: 0 = "materialize nothing" is a legal opt-out.
    let off = catalog(serde_json::json!({ "retain": { "perIdentity": 0 } }));
    assert!(validate_catalog_bounds(&off, false).is_empty());
}

#[test]
fn catalog_refresh_interval_must_parse_and_message_says_how_to_fix() {
    let c = catalog(serde_json::json!({ "refreshInterval": "every-hour" }));
    let errs = validate_catalog_bounds(&c, false);
    assert_eq!(errs.len(), 1);
    let msg = errs[0].to_string();
    // What failed, why, how to fix — a human acts on this text.
    assert!(msg.contains("catalog.refreshInterval"), "{msg}");
    assert!(msg.contains("every-hour"), "{msg}");
    assert!(msg.contains("30s, 5m, or 1h"), "{msg}");
    assert!(msg.contains("default (1h)"), "{msg}");
}

#[test]
fn catalog_refresh_interval_floor_is_enforced() {
    let c = catalog(serde_json::json!({ "refreshInterval": "5s" }));
    let errs = validate_catalog_bounds(&c, false);
    assert_eq!(errs.len(), 1);
    let msg = errs[0].to_string();
    assert!(msg.contains("30s minimum"), "{msg}");
    // Exactly the floor is fine.
    let ok = catalog(serde_json::json!({ "refreshInterval": "30s" }));
    assert!(validate_catalog_bounds(&ok, false).is_empty());
}

#[test]
fn catalog_retain_bounds_must_be_enforceable() {
    let c = catalog(serde_json::json!({
        "retain": { "perIdentity": -3, "maxAgeDays": 0 },
    }));
    let errs = validate_catalog_bounds(&c, true);
    assert_eq!(errs.len(), 2);
    let all = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>();
    assert!(
        all.iter().any(|m| m.contains("catalog.retain.perIdentity")),
        "{all:?}"
    );
    assert!(
        all.iter().any(|m| m.contains("catalog.retain.maxAgeDays")),
        "{all:?}"
    );
}

#[test]
fn catalog_fallback_namespace_is_cluster_repository_only() {
    let c = catalog(serde_json::json!({ "fallbackNamespace": "backups" }));
    // Allowed on the cluster-scoped kind…
    assert!(validate_catalog_bounds(&c, true).is_empty());
    // …rejected on a namespaced Repository, with the fix in the message.
    let errs = validate_catalog_bounds(&c, false);
    assert_eq!(errs.len(), 1);
    let msg = errs[0].to_string();
    assert!(msg.contains("catalog.fallbackNamespace"), "{msg}");
    assert!(msg.contains("ClusterRepository"), "{msg}");
    assert!(msg.contains("remove the field"), "{msg}");
}

#[test]
fn repository_validators_route_catalog_bounds() {
    // The aggregate validators must actually call validate_catalog_bounds with
    // the right scope, or the webhook silently admits what the docs forbid.
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
catalog:
  fallbackNamespace: backups
"#,
    );
    let errs = validate_repository(&repo);
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("catalog.fallbackNamespace")),
        "{errs:?}"
    );

    let crepo: ClusterRepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
    namespace: kopiur-system
allowedNamespaces:
  all: true
catalog:
  fallbackNamespace: backups
  refreshInterval: bogus
"#,
    );
    let errs = validate_cluster_repository(&crepo);
    // fallbackNamespace is legal here; the bad interval is not.
    assert!(
        !errs
            .iter()
            .any(|e| e.to_string().contains("catalog.fallbackNamespace")),
        "{errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("catalog.refreshInterval")),
        "{errs:?}"
    );
}

// --- catalog.foreignSnapshots × identityDefaults.cluster ---

#[test]
fn foreign_snapshots_fallback_requires_fallback_namespace() {
    // (b) Fallback with no fallbackNamespace ⇒ rejected.
    let c = catalog(serde_json::json!({ "foreignSnapshots": "Fallback" }));
    let errs = validate_catalog_bounds(&c, true);
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("catalog.foreignSnapshots")
                && e.to_string().contains("fallbackNamespace")),
        "{errs:?}"
    );

    // Fallback + fallbackNamespace set ⇒ accepted by validate_catalog_bounds
    // (the cluster-identity coupling is a separate rule, tested below).
    let c = catalog(serde_json::json!({
        "foreignSnapshots": "Fallback",
        "fallbackNamespace": "backups",
    }));
    assert!(validate_catalog_bounds(&c, true).is_empty());

    // Ignore never needs a fallbackNamespace.
    let c = catalog(serde_json::json!({ "foreignSnapshots": "Ignore" }));
    assert!(validate_catalog_bounds(&c, true).is_empty());
}

#[test]
fn foreign_snapshots_fallback_is_cluster_repository_only() {
    // (c) Fallback on a namespaced Repository (cluster_scoped: false) is
    // rejected, mirroring the fallbackNamespace-is-ClusterRepository-only
    // message (its-own-namespace wording).
    let c = catalog(serde_json::json!({
        "foreignSnapshots": "Fallback",
        "fallbackNamespace": "backups",
    }));
    assert!(validate_catalog_bounds(&c, true).is_empty());

    let errs = validate_catalog_bounds(&c, false);
    let msg = errs
        .iter()
        .map(|e| e.to_string())
        .find(|m| m.contains("catalog.foreignSnapshots"))
        .unwrap_or_else(|| panic!("expected a foreignSnapshots error: {errs:?}"));
    assert!(msg.contains("its own namespace"), "{msg}");
    assert!(msg.contains("Ignore or omit"), "{msg}");

    // Ignore is fine on a namespaced Repository.
    let c = catalog(serde_json::json!({ "foreignSnapshots": "Ignore" }));
    assert!(validate_catalog_bounds(&c, false).is_empty());
}

#[test]
fn foreign_snapshots_unknown_variant_is_rejected() {
    let value: serde_json::Value = serde_yaml::from_str("foreignSnapshots: Delete\n").unwrap();
    assert!(
        serde_json::from_value::<crate::common::CatalogBounds>(value).is_err(),
        "an unknown foreignSnapshots variant must be rejected"
    );
}

#[test]
fn foreign_snapshots_requires_a_cluster_identity_on_both_kinds() {
    // (a) foreignSnapshots set + no cluster identity ⇒ rejected on BOTH kinds,
    // with a kind-neutral message (never claims a field doesn't exist).
    let errs = validate_foreign_snapshots_cluster_coupling(
        Some(&catalog(
            serde_json::json!({ "foreignSnapshots": "Ignore" }),
        )),
        None,
    );
    assert_eq!(errs.len(), 1);
    assert!(matches!(
        errs[0],
        ValidationError::ForeignSnapshotsRequiresCluster
    ));
    let msg = errs[0].to_string();
    assert!(
        msg.contains("requires a cluster identity (`identityDefaults.cluster`)"),
        "{msg}"
    );

    // Empty-string cluster behaves like unset (matches classify_hostname's own rule).
    let errs = validate_foreign_snapshots_cluster_coupling(
        Some(&catalog(
            serde_json::json!({ "foreignSnapshots": "Ignore" }),
        )),
        Some(""),
    );
    assert_eq!(errs.len(), 1);

    // A real cluster identity clears rule (a) (and there's no fallbackNamespace, so (d) is inert).
    let errs = validate_foreign_snapshots_cluster_coupling(
        Some(&catalog(
            serde_json::json!({ "foreignSnapshots": "Ignore" }),
        )),
        Some("east"),
    );
    assert!(errs.is_empty(), "{errs:?}");

    // No catalog at all ⇒ nothing to validate.
    assert!(validate_foreign_snapshots_cluster_coupling(None, None).is_empty());
    assert!(validate_foreign_snapshots_cluster_coupling(None, Some("east")).is_empty());
}

#[test]
fn foreign_snapshots_choice_required_when_cluster_and_fallback_namespace_both_set() {
    // (d) cluster + fallbackNamespace set but foreignSnapshots ABSENT ⇒ rejected.
    let c = catalog(serde_json::json!({ "fallbackNamespace": "backups" }));
    let errs = validate_foreign_snapshots_cluster_coupling(Some(&c), Some("east"));
    assert_eq!(errs.len(), 1);
    assert!(matches!(
        errs[0],
        ValidationError::ForeignSnapshotsChoiceRequired
    ));

    // Explicit Ignore accepted.
    let c = catalog(serde_json::json!({
        "fallbackNamespace": "backups",
        "foreignSnapshots": "Ignore",
    }));
    assert!(validate_foreign_snapshots_cluster_coupling(Some(&c), Some("east")).is_empty());

    // Explicit Fallback accepted.
    let c = catalog(serde_json::json!({
        "fallbackNamespace": "backups",
        "foreignSnapshots": "Fallback",
    }));
    assert!(validate_foreign_snapshots_cluster_coupling(Some(&c), Some("east")).is_empty());

    // No fallbackNamespace ⇒ rule (d) does not apply even with a cluster identity.
    let c = catalog(serde_json::json!({}));
    assert!(validate_foreign_snapshots_cluster_coupling(Some(&c), Some("east")).is_empty());
}

#[test]
fn repository_validators_route_foreign_snapshots_cluster_coupling() {
    // The aggregate validators must actually call
    // validate_foreign_snapshots_cluster_coupling, or the webhook silently
    // admits what the docs forbid.

    // A namespaced Repository with no identityDefaults set — any
    // foreignSnapshots is rejected via the generic, kind-neutral message.
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
catalog:
  foreignSnapshots: Ignore
"#,
    );
    let errs = validate_repository(&repo);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::ForeignSnapshotsRequiresCluster)),
        "{errs:?}"
    );

    // M5: a namespaced Repository WITH identityDefaults.cluster set — Ignore is
    // now legal, exactly like a ClusterRepository with a cluster identity.
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
identityDefaults:
  cluster: east
catalog:
  foreignSnapshots: Ignore
"#,
    );
    let errs = validate_repository(&repo);
    assert!(
        !errs
            .iter()
            .any(|e| matches!(e, ValidationError::ForeignSnapshotsRequiresCluster)),
        "{errs:?}"
    );

    // A ClusterRepository with no identityDefaults.cluster: same rejection.
    let crepo: ClusterRepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
    namespace: kopiur-system
allowedNamespaces:
  all: true
catalog:
  foreignSnapshots: Ignore
"#,
    );
    let errs = validate_cluster_repository(&crepo);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::ForeignSnapshotsRequiresCluster)),
        "{errs:?}"
    );

    // With identityDefaults.cluster set, foreignSnapshots is legal.
    let crepo: ClusterRepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
    namespace: kopiur-system
allowedNamespaces:
  all: true
identityDefaults:
  cluster: east
catalog:
  foreignSnapshots: Ignore
"#,
    );
    let errs = validate_cluster_repository(&crepo);
    assert!(
        !errs
            .iter()
            .any(|e| matches!(e, ValidationError::ForeignSnapshotsRequiresCluster)),
        "{errs:?}"
    );

    // Cluster + fallbackNamespace set but foreignSnapshots absent ⇒ (d) fires
    // through the aggregate validator too.
    let crepo: ClusterRepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
    namespace: kopiur-system
allowedNamespaces:
  all: true
identityDefaults:
  cluster: east
catalog:
  fallbackNamespace: backups
"#,
    );
    let errs = validate_cluster_repository(&crepo);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::ForeignSnapshotsChoiceRequired)),
        "{errs:?}"
    );

    // Fallback on a namespaced Repository: rejected both by rule (a) (no
    // cluster identity at all) AND rule (c) (Fallback is ClusterRepository-only).
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
catalog:
  foreignSnapshots: Fallback
  fallbackNamespace: backups
"#,
    );
    let errs = validate_repository(&repo);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::ForeignSnapshotsRequiresCluster)),
        "{errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("its own namespace")),
        "{errs:?}"
    );

    // M5: Fallback on a namespaced Repository WITH a cluster identity set — rule
    // (a) no longer fires (there IS a cluster identity now), but rule (c)
    // (Fallback is ClusterRepository-only) still rejects it. Unchanged by M5.
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
identityDefaults:
  cluster: east
catalog:
  foreignSnapshots: Fallback
  fallbackNamespace: backups
"#,
    );
    let errs = validate_repository(&repo);
    assert!(
        !errs
            .iter()
            .any(|e| matches!(e, ValidationError::ForeignSnapshotsRequiresCluster)),
        "rule (a) must not fire once a cluster identity is set: {errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("its own namespace")),
        "rule (c) must still reject Fallback on a namespaced Repository: {errs:?}"
    );
}

// --- scheduleDefaults.timezone (GitHub #174 item 3) ---

#[test]
fn repository_rejects_bad_schedule_defaults_timezone() {
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
scheduleDefaults:
  timezone: America/Chicgo
"#,
    );
    let errs = validate_repository(&repo);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::InvalidTimezone { .. })),
        "a typo'd scheduleDefaults.timezone must be rejected at admission: {errs:?}"
    );
}

#[test]
fn repository_accepts_valid_schedule_defaults_timezone() {
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
scheduleDefaults:
  timezone: America/New_York
"#,
    );
    assert!(validate_repository(&repo).is_empty());
}

#[test]
fn cluster_repository_rejects_bad_schedule_defaults_timezone() {
    let crepo: ClusterRepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
    namespace: kopiur-system
allowedNamespaces:
  all: true
scheduleDefaults:
  timezone: America/Chicgo
"#,
    );
    let errs = validate_cluster_repository(&crepo);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::InvalidTimezone { .. })),
        "a typo'd scheduleDefaults.timezone must be rejected at admission: {errs:?}"
    );
}

#[test]
fn cluster_repository_accepts_valid_schedule_defaults_timezone() {
    let crepo: ClusterRepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
    namespace: kopiur-system
allowedNamespaces:
  all: true
scheduleDefaults:
  timezone: America/New_York
"#,
    );
    assert!(validate_cluster_repository(&crepo).is_empty());
}

// --- repository_warnings (inline-NFS fsGroup footgun) ---

#[test]
fn nfs_repo_without_write_identity_warns_about_fsgroup() {
    // An inline-NFS filesystem repo that relies only on the default/fsGroup
    // identity gets the actionable warning (it would fail at runtime: fsGroup
    // is a no-op on NFS).
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
    volume:
      nfs:
        server: nas.lan
        path: /export/kopia
encryption:
  passwordSecretRef:
    name: creds
moverDefaults:
  podSecurityContext:
    fsGroup: 3001
"#,
    );
    let warns = repository_warnings(&repo.backend, repo.mover_defaults.as_ref());
    assert_eq!(warns, vec![NFS_FSGROUP_WARNING.to_string()]);
}

#[test]
fn nfs_repo_with_supplemental_groups_does_not_warn() {
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
    volume:
      nfs:
        server: nas.lan
        path: /export/kopia
encryption:
  passwordSecretRef:
    name: creds
moverDefaults:
  podSecurityContext:
    supplementalGroups: [3001]
"#,
    );
    assert!(repository_warnings(&repo.backend, repo.mover_defaults.as_ref()).is_empty());
}

#[test]
fn nfs_repo_with_run_as_user_does_not_warn() {
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
    volume:
      nfs:
        server: nas.lan
        path: /export/kopia
encryption:
  passwordSecretRef:
    name: creds
moverDefaults:
  securityContext:
    runAsUser: 3001
"#,
    );
    assert!(repository_warnings(&repo.backend, repo.mover_defaults.as_ref()).is_empty());
}

#[test]
fn pvc_filesystem_and_object_store_repos_never_warn() {
    // The warning is NFS-specific: a PVC-backed filesystem repo honors fsGroup
    // (block CSI), and object stores have no filesystem permission surface.
    let pvc: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
    volume:
      pvc:
        name: repo-rwx
encryption:
  passwordSecretRef:
    name: creds
"#,
    );
    assert!(repository_warnings(&pvc.backend, pvc.mover_defaults.as_ref()).is_empty());

    let s3: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  s3:
    bucket: b
    endpoint: https://minio
encryption:
  passwordSecretRef:
    name: creds
"#,
    );
    assert!(repository_warnings(&s3.backend, s3.mover_defaults.as_ref()).is_empty());
}

#[test]
fn cluster_nfs_repo_shares_the_same_warning() {
    // ClusterRepository routes through the same helper (backend + moverDefaults).
    let crepo: ClusterRepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
    volume:
      nfs:
        server: nas.lan
        path: /export/kopia
encryption:
  passwordSecretRef:
    name: creds
    namespace: kopiur-system
allowedNamespaces:
  all: true
"#,
    );
    let warns = repository_warnings(&crepo.backend, crepo.mover_defaults.as_ref());
    assert_eq!(warns, vec![NFS_FSGROUP_WARNING.to_string()]);
}

// --- validate_server (spec.server) ---

use crate::server::{InsecureAuth, ServerAuth, ServerService, ServerSpec, ServiceType};

#[test]
fn server_insecure_without_ack_is_rejected() {
    let server = ServerSpec {
        auth: Some(ServerAuth::Insecure(InsecureAuth {
            acknowledge_insecure: false,
        })),
        ..Default::default()
    };
    assert_eq!(
        validate_server(&server, RepositoryMode::ReadWrite),
        vec![ValidationError::InsecureServerNotAcknowledged]
    );
}

#[test]
fn server_insecure_with_ack_is_ok() {
    let server = ServerSpec {
        auth: Some(ServerAuth::Insecure(InsecureAuth {
            acknowledge_insecure: true,
        })),
        ..Default::default()
    };
    assert!(validate_server(&server, RepositoryMode::ReadWrite).is_empty());
}

#[test]
fn server_generate_and_default_are_ok() {
    assert!(validate_server(&ServerSpec::default(), RepositoryMode::ReadWrite).is_empty());
    let server = ServerSpec {
        auth: Some(ServerAuth::Generate(Default::default())),
        service: Some(ServerService {
            r#type: ServiceType::NodePort,
            port: Some(30515),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(validate_server(&server, RepositoryMode::ReadWrite).is_empty());
}

#[test]
fn server_port_zero_is_rejected() {
    let server = ServerSpec {
        service: Some(ServerService {
            port: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        validate_server(&server, RepositoryMode::ReadWrite),
        vec![ValidationError::InvalidServerPort { port: 0 }]
    );
}

#[test]
fn server_read_only_false_on_read_only_repo_is_rejected() {
    // Contradictory: a ReadOnly repository cannot serve a writable UI.
    let server = ServerSpec {
        read_only: Some(false),
        ..Default::default()
    };
    let errs = validate_server(&server, RepositoryMode::ReadOnly);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(matches!(
        &errs[0],
        ValidationError::InvalidFieldValue { field, .. } if field == "server.readOnly"
    ));
    // The same field on a ReadWrite repo (the opt-in) is fine, as is omitting it.
    assert!(validate_server(&server, RepositoryMode::ReadWrite).is_empty());
    let on = ServerSpec {
        read_only: Some(true),
        ..Default::default()
    };
    assert!(validate_server(&on, RepositoryMode::ReadOnly).is_empty());
    assert!(validate_server(&ServerSpec::default(), RepositoryMode::ReadOnly).is_empty());
}

#[test]
fn cluster_repository_server_requires_namespace() {
    // A blank `server.namespace` on a ClusterRepository is rejected (cluster-scoped
    // resources have no implicit namespace). Built the cluster's way (YAML → typed)
    // so the test is robust to unrelated spec fields.
    let spec: ClusterRepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /r
encryption:
  passwordSecretRef:
    name: s
    namespace: kopia-system
allowedNamespaces:
  all: true
server:
  namespace: "  "
"#,
    );
    let errs = validate_cluster_repository(&spec);
    assert!(errs.contains(&ValidationError::ServerNamespaceRequired));
}

// --- resource requests <= limits (kube_quantity comparison) ---

mod resource_invariants {
    use super::*;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

    fn resources(reqs: &[(&str, &str)], lims: &[(&str, &str)]) -> ResourceRequirements {
        let map = |kv: &[(&str, &str)]| {
            let m: BTreeMap<String, Quantity> = kv
                .iter()
                .map(|(k, v)| (k.to_string(), Quantity(v.to_string())))
                .collect();
            if m.is_empty() { None } else { Some(m) }
        };
        ResourceRequirements {
            requests: map(reqs),
            limits: map(lims),
            claims: None,
        }
    }

    #[test]
    fn request_exceeding_limit_is_rejected_across_units() {
        // 1Gi request vs 512Mi limit — the comparison must span binary suffixes.
        let r = resources(&[("memory", "1Gi")], &[("memory", "512Mi")]);
        let err = validate_resources(&r, "SnapshotPolicy mover").unwrap_err();
        match err {
            ValidationError::InvalidFieldValue { field, reason } => {
                assert!(field.contains("resources.requests.memory"), "{field}");
                assert!(reason.contains("exceeds limit"), "{reason}");
            }
            other => panic!("expected InvalidFieldValue, got {other:?}"),
        }
    }

    #[test]
    fn cpu_millicpu_vs_whole_is_compared_correctly() {
        // 2 (cores) request vs 500m limit → request exceeds.
        assert!(validate_resources(&resources(&[("cpu", "2")], &[("cpu", "500m")]), "m").is_err());
        // 250m request vs 1 limit → fine.
        assert!(validate_resources(&resources(&[("cpu", "250m")], &[("cpu", "1")]), "m").is_ok());
    }

    #[test]
    fn request_within_limit_is_ok() {
        let r = resources(&[("memory", "256Mi")], &[("memory", "512Mi")]);
        assert!(validate_resources(&r, "m").is_ok());
    }

    #[test]
    fn missing_limit_for_a_request_is_not_flagged() {
        // requests without a matching limit is valid (the limit is "unbounded").
        let r = resources(&[("memory", "1Gi")], &[("cpu", "1")]);
        assert!(validate_resources(&r, "m").is_ok());
    }

    #[test]
    fn unparseable_quantity_is_skipped_never_a_false_reject() {
        // Best-effort: a garbage quantity must not cause a (wrong) rejection.
        let r = resources(&[("memory", "not-a-quantity")], &[("memory", "512Mi")]);
        assert!(validate_resources(&r, "m").is_ok());
    }
}

// --- failurePolicy positivity ---

#[test]
fn failure_policy_rejects_non_positive_and_negative_fields() {
    let bad_deadline = FailurePolicy {
        active_deadline_seconds: Some(0),
        ..Default::default()
    };
    assert!(validate_failure_policy(&bad_deadline, "Snapshot").is_err());

    let bad_grace = FailurePolicy {
        pod_startup_deadline_seconds: Some(-5),
        ..Default::default()
    };
    assert!(validate_failure_policy(&bad_grace, "Snapshot").is_err());

    let bad_backoff = FailurePolicy {
        backoff_limit: Some(-1),
        ..Default::default()
    };
    assert!(validate_failure_policy(&bad_backoff, "Snapshot").is_err());

    let good = FailurePolicy {
        backoff_limit: Some(2),
        active_deadline_seconds: Some(7200),
        pod_startup_deadline_seconds: Some(300),
    };
    assert!(validate_failure_policy(&good, "Snapshot").is_ok());
    // backoffLimit: 0 (no retries) is valid.
    assert!(
        validate_failure_policy(
            &FailurePolicy {
                backoff_limit: Some(0),
                ..Default::default()
            },
            "Snapshot"
        )
        .is_ok()
    );
}

#[test]
fn repository_health_rejects_negative_threshold_but_allows_zero() {
    // Negative is nonsensical → rejected with an actionable message.
    let bad = RepositoryHealthSpec {
        index_blob_warn_threshold: Some(-1),
        ..Default::default()
    };
    let err = validate_repository_health(Some(&bad), "Repository").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("indexBlobWarnThreshold"),
        "names the field: {msg}"
    );
    assert!(msg.contains(">= 0"), "explains the constraint: {msg}");

    // 0 (disable sentinel) and a positive threshold are valid; absent is valid.
    assert!(
        validate_repository_health(
            Some(&RepositoryHealthSpec {
                index_blob_warn_threshold: Some(0),
                ..Default::default()
            }),
            "Repository"
        )
        .is_ok()
    );
    assert!(
        validate_repository_health(
            Some(&RepositoryHealthSpec {
                index_blob_warn_threshold: Some(2000),
                ..Default::default()
            }),
            "ClusterRepository"
        )
        .is_ok()
    );
    assert!(validate_repository_health(None, "Repository").is_ok());
}

#[test]
fn repository_health_probe_interval_and_threshold_are_validated() {
    use crate::repository::RepositoryHealthProbeSpec;

    // Unparseable interval → rejected, names the field.
    let bad = RepositoryHealthSpec {
        probe: Some(RepositoryHealthProbeSpec {
            interval: Some("every-hour".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let err = validate_repository_health(Some(&bad), "Repository").unwrap_err();
    assert!(err.to_string().contains("health.probe.interval"), "{err}");

    // Below the 30s floor → rejected.
    let too_fast = RepositoryHealthSpec {
        probe: Some(RepositoryHealthProbeSpec {
            interval: Some("5s".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let err = validate_repository_health(Some(&too_fast), "ClusterRepository").unwrap_err();
    assert!(err.to_string().contains("30s minimum"), "{err}");

    // failureThreshold < 1 → rejected.
    let bad_threshold = RepositoryHealthSpec {
        probe: Some(RepositoryHealthProbeSpec {
            failure_threshold: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };
    let err = validate_repository_health(Some(&bad_threshold), "Repository").unwrap_err();
    assert!(err.to_string().contains("failureThreshold"), "{err}");

    // Valid probe (or omitted interval/threshold) is accepted.
    let ok = RepositoryHealthSpec {
        probe: Some(RepositoryHealthProbeSpec {
            enabled: Some(true),
            interval: Some("30s".to_string()),
            failure_threshold: Some(3),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(validate_repository_health(Some(&ok), "Repository").is_ok());
}

// --- retention keeps-nothing data-loss guard ---

#[test]
fn retention_keeps_nothing_detects_empty_and_all_zero() {
    assert!(retention_keeps_nothing(&Retention::default()));
    assert!(retention_keeps_nothing(&Retention {
        keep_latest: Some(0),
        keep_daily: Some(0),
        ..Default::default()
    }));
    assert!(!retention_keeps_nothing(&Retention {
        keep_latest: Some(1),
        ..Default::default()
    }));
    assert!(!retention_keeps_nothing(&Retention {
        keep_daily: Some(7),
        ..Default::default()
    }));
}

#[test]
fn backup_config_rejects_keeps_nothing_retention_but_not_absent() {
    // Some(empty retention) → rejected (would prune everything).
    let mut spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: data } } ]\n\
             retention: {}\n",
    );
    let errs = validate_backup_config(&spec);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::InvalidFieldValue { field, .. } if field == "spec.retention"
        )),
        "an empty retention must be rejected as data-loss: {errs:?}"
    );

    // retention: None → NOT flagged (means "don't prune").
    spec.retention = None;
    let errs = validate_backup_config(&spec);
    assert!(
        !errs.iter().any(|e| matches!(
            e,
            ValidationError::InvalidFieldValue { field, .. } if field == "spec.retention"
        )),
        "absent retention is the safe no-prune case and must not be flagged: {errs:?}"
    );
}

// --- staging validation ---

#[test]
fn backup_config_validates_staging_timeout() {
    // Valid Go-duration timeouts are accepted — including "0" (wait indefinitely).
    for t in ["10m", "1h", "0", "0s"] {
        let ok: SnapshotPolicySpec = crate::testutil::from_yaml(&format!(
            "repository: {{ kind: Repository, name: r }}\n\
             sources: [ {{ pvc: {{ name: data }} }} ]\n\
             staging: {{ timeout: {t:?} }}\n"
        ));
        assert!(
            validate_backup_config(&ok).is_empty(),
            "timeout {t:?} must be accepted: {:?}",
            validate_backup_config(&ok)
        );
    }

    // Absent staging / absent timeout are both fine (runtime default applies).
    let absent: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         staging: {}\n",
    );
    assert!(validate_backup_config(&absent).is_empty());

    // Unparseable timeout → rejected, names the field, message says what/why/fix.
    let bad: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         staging: { timeout: every-hour }\n",
    );
    let errs = validate_backup_config(&bad);
    let err = errs
        .iter()
        .find(|e| matches!(
            e, ValidationError::InvalidFieldValue { field, .. } if field == "spec.staging.timeout"
        ))
        .unwrap_or_else(|| panic!("expected spec.staging.timeout rejection, got {errs:?}"));
    let msg = err.to_string();
    assert!(msg.contains("Go-style duration"), "{msg}");
    assert!(msg.contains("default (10m)"), "{msg}");
}

// --- preflight validation ---

#[test]
fn backup_config_validates_preflight() {
    // Valid preflight is accepted.
    let ok: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         preflight:\n  timeout: 10m\n  checks:\n\
         \x20   - { name: a, expr: \"repository.ready\" }\n\
         \x20   - { name: b, expr: \"maintenance.hasRun\" }\n",
    );
    assert!(
        validate_backup_config(&ok).is_empty(),
        "{:?}",
        validate_backup_config(&ok)
    );

    // Bad timeout → rejected, names the field.
    let bad_to: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         preflight:\n  timeout: every-hour\n  checks: [ { name: a, expr: \"repository.ready\" } ]\n",
    );
    assert!(
        validate_backup_config(&bad_to).iter().any(|e| matches!(
            e, ValidationError::InvalidFieldValue { field, .. } if field == "spec.preflight.timeout"
        )),
        "{:?}",
        validate_backup_config(&bad_to)
    );

    // Duplicate check name → rejected.
    let dup: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         preflight:\n  checks:\n\
         \x20   - { name: same, expr: \"repository.ready\" }\n\
         \x20   - { name: same, expr: \"maintenance.hasRun\" }\n",
    );
    assert!(
        validate_backup_config(&dup).iter().any(|e| matches!(
            e, ValidationError::InvalidFieldValue { field, .. } if field.starts_with("spec.preflight.checks[")
        )),
        "{:?}",
        validate_backup_config(&dup)
    );

    // Bad expr (out-of-scope variable) → rejected.
    let bad_expr: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         preflight:\n  checks: [ { name: a, expr: \"bogus > 0\" } ]\n",
    );
    assert!(
        validate_backup_config(&bad_expr)
            .iter()
            .any(|e| matches!(e, ValidationError::PreflightExprEval { .. })),
        "{:?}",
        validate_backup_config(&bad_expr)
    );
}

#[test]
fn backup_config_validates_verification() {
    // New nested-quick shape is accepted.
    let ok: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         verification:\n  quick:\n    schedule: { cron: \"0 4 * * *\", jitter: 30m }\n",
    );
    assert!(
        validate_backup_config(&ok).is_empty(),
        "{:?}",
        validate_backup_config(&ok)
    );

    // GitHub #174: a re-applied OLD flat shape (`quick: { cron: ... }`) decodes with
    // schedule: None, and must be rejected with an actionable pointer to the move —
    // NOT a cryptic structural error. Assert the field AND the message text.
    let old_shape: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         verification:\n  quick: { cron: \"0 4 * * *\", jitter: 30m }\n",
    );
    let errs = validate_backup_config(&old_shape);
    let quick_err = errs
        .iter()
        .find_map(|e| match e {
            ValidationError::InvalidFieldValue { field, reason }
                if field == "spec.verification.quick.schedule" =>
            {
                Some(reason)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a quick.schedule rejection, got: {errs:?}"));
    assert!(
        quick_err.contains("verification.quick.schedule.cron")
            && quick_err.contains("Move your cron/jitter/timezone fields under `schedule:`"),
        "message must name the move actionably, got: {quick_err:?}"
    );

    // A bad cron under the new schedule is still rejected (the schedule is validated).
    let bad_cron: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         verification:\n  quick:\n    schedule: { cron: \"not a cron\" }\n",
    );
    assert!(
        validate_backup_config(&bad_cron)
            .iter()
            .any(|e| matches!(e, ValidationError::InvalidCron { .. })),
        "{:?}",
        validate_backup_config(&bad_cron)
    );

    // Quick entirely absent ⇒ no verification error (quick is opt-in).
    let no_quick: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         verification:\n  deep:\n    schedule: { cron: \"0 5 * * 0\" }\n",
    );
    assert!(
        validate_backup_config(&no_quick).is_empty(),
        "{:?}",
        validate_backup_config(&no_quick)
    );
}

#[test]
fn backup_config_validates_verification_tuning_knobs() {
    // Zero is rejected for the count knobs (>= 1) on both tiers.
    for (yaml, field) in [
        (
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: data } } ]\n\
             verification:\n  quick:\n    schedule: { cron: \"0 4 * * *\" }\n    parallel: 0\n",
            "SnapshotPolicy spec.verification.quick.parallel",
        ),
        (
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: data } } ]\n\
             verification:\n  quick:\n    schedule: { cron: \"0 4 * * *\" }\n    fileParallelism: 0\n",
            "SnapshotPolicy spec.verification.quick.fileParallelism",
        ),
        (
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: data } } ]\n\
             verification:\n  quick:\n    schedule: { cron: \"0 4 * * *\" }\n    fileQueueLength: 0\n",
            "SnapshotPolicy spec.verification.quick.fileQueueLength",
        ),
        (
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: data } } ]\n\
             verification:\n  deep:\n    schedule: { cron: \"0 5 * * 0\" }\n    parallel: 0\n",
            "SnapshotPolicy spec.verification.deep.parallel",
        ),
    ] {
        let spec: SnapshotPolicySpec = crate::testutil::from_yaml(yaml);
        let errs = validate_backup_config(&spec);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::InvalidFieldValue { field: f, .. } if f == field
            )),
            "expected a rejection of {field}, got: {errs:?}"
        );
    }

    // maxErrors: 0 is kopia's own default ("stop at first error") — unconstrained,
    // never rejected.
    let max_errors_zero: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         verification:\n  quick:\n    schedule: { cron: \"0 4 * * *\" }\n    maxErrors: 0\n",
    );
    assert!(
        validate_backup_config(&max_errors_zero).is_empty(),
        "{:?}",
        validate_backup_config(&max_errors_zero)
    );

    // Positive values on all knobs are accepted.
    let ok: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         verification:\n  \
           quick:\n    schedule: { cron: \"0 4 * * *\" }\n    parallel: 2\n    \
             fileParallelism: 4\n    fileQueueLength: 100\n    maxErrors: 1\n  \
           deep:\n    schedule: { cron: \"0 5 * * 0\" }\n    parallel: 2\n",
    );
    assert!(
        validate_backup_config(&ok).is_empty(),
        "{:?}",
        validate_backup_config(&ok)
    );
}

#[test]
fn backup_config_rejects_zero_upload_limit_mb() {
    // M4 flag sweep (issue #216 category sweep): `upload.limitMb` is a count
    // knob (require_min, same shared validator as the M2/M3 sweeps).
    let zero: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         upload:\n  limitMb: 0\n",
    );
    let errs = validate_backup_config(&zero);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::InvalidFieldValue { field, .. }
                if field == "SnapshotPolicy spec.upload.limitMb"
        )),
        "expected a limitMb rejection, got: {errs:?}"
    );

    let ok: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         upload:\n  limitMb: 100\n",
    );
    assert!(
        validate_backup_config(&ok).is_empty(),
        "{:?}",
        validate_backup_config(&ok)
    );

    // Absent ⇒ no error (limitMb is opt-in).
    let absent: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n",
    );
    assert!(validate_backup_config(&absent).is_empty());
}

// --- identity shape validation ---

#[test]
fn identity_component_accepts_normal_values() {
    for v in [
        "postgres-data",
        "billing",
        "billing-postgres-data",
        "team.prod",
        "my_app",
        "a",
        "café", // unicode letters pass — shape-only, not a character class
    ] {
        assert!(
            validate_identity_component("f", v).is_ok(),
            "{v:?} should be accepted"
        );
    }
}

#[test]
fn identity_component_rejects_kopia_delimiters_and_blanks() {
    // The exact misparse/un-findability cases kopia's first-@/first-: parser hits.
    for (v, why) in [
        ("", "empty"),
        ("a@b", "@ delimiter"),
        ("ho:st", ": delimiter"),
        ("has space", "whitespace"),
        ("tab\there", "tab"),
        ("line\nbreak", "newline"),
        ("nul\0byte", "control char"),
    ] {
        let err = validate_identity_component("spec.identity.username", v).unwrap_err();
        assert!(
            matches!(err, ValidationError::IdentityComponentInvalid { .. }),
            "{v:?} ({why}) should be rejected, got {err:?}"
        );
    }
    // Over the length cap.
    let long = "a".repeat(IDENTITY_MAX_LEN + 1);
    assert!(validate_identity_component("f", &long).is_err());
}

#[test]
fn source_path_is_lenient_but_rejects_empty_and_control() {
    // Spaces and ':' are fine in a path (only the first ':' is kopia's delimiter).
    assert!(validate_source_path("f", "/pvc/data").is_ok());
    assert!(validate_source_path("f", "/mnt/My Files").is_ok());
    assert!(validate_source_path("f", "/data:extra").is_ok());
    // Empty-when-set and control chars are not.
    assert!(validate_source_path("f", "").is_err());
    assert!(validate_source_path("f", "/data\nx").is_err());
}

// --- validate_cluster_name (identityDefaults.cluster, M1) ---

#[test]
fn cluster_name_accepts_valid_labels() {
    for v in [
        "east",
        "east-prod",
        "a",
        "a1",
        &"a".repeat(CLUSTER_NAME_MAX_LEN),
    ] {
        assert!(validate_cluster_name(v).is_ok(), "{v:?} should be accepted");
    }
    assert_eq!(CLUSTER_NAME_MAX_LEN, 32, "table below assumes this bound");
}

#[test]
fn cluster_name_rejects_over_length() {
    let too_long = "a".repeat(CLUSTER_NAME_MAX_LEN + 1);
    let err = validate_cluster_name(&too_long).unwrap_err();
    assert!(
        matches!(err, ValidationError::ClusterNameInvalid { .. }),
        "{err:?}"
    );
}

#[test]
fn cluster_name_rejects_empty() {
    let err = validate_cluster_name("").unwrap_err();
    assert!(matches!(err, ValidationError::ClusterNameInvalid { .. }));
}

#[test]
fn cluster_name_rejects_uppercase() {
    let err = validate_cluster_name("East").unwrap_err();
    assert!(matches!(err, ValidationError::ClusterNameInvalid { .. }));
}

#[test]
fn cluster_name_rejects_leading_and_trailing_dash() {
    for v in ["-east", "east-"] {
        let err = validate_cluster_name(v).unwrap_err();
        assert!(
            matches!(err, ValidationError::ClusterNameInvalid { .. }),
            "{v:?} should be rejected"
        );
    }
}

#[test]
fn cluster_name_rejects_dot_with_delimiter_message() {
    // The "no dots" message must explain the FIRST-dot-is-the-delimiter rule,
    // since that's what makes a dotted cluster name dangerous (classify_hostname
    // would silently disagree with intent rather than error).
    let err = validate_cluster_name("ea.st").unwrap_err();
    match &err {
        ValidationError::ClusterNameInvalid { reason, .. } => {
            assert!(
                reason.contains("delimiter"),
                "message should explain the namespace/cluster delimiter rule: {reason}"
            );
        }
        other => panic!("expected ClusterNameInvalid, got {other:?}"),
    }
}

#[test]
fn cluster_repository_rejects_bad_cluster_name() {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::common::IdentityDefaults;
    use crate::common::{Encryption, SecretKeyRef};
    let spec = ClusterRepositorySpec {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/r".into(),
            volume: None,
        }),
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: "s".into(),
                namespace: Some("kopia-system".into()),
                key: None,
            },
        },
        create: None,
        bootstrap: None,
        mover_defaults: None,
        schedule_defaults: None,
        catalog: None,
        server: None,
        allowed_namespaces: AllowedNamespaces::All(true),
        identity_defaults: Some(IdentityDefaults {
            cluster: Some("East".into()), // uppercase — invalid RFC 1123 label
            hostname_expr: None,
            username_expr: None,
        }),
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
        parameters: None,
        deletion_protection: None,
        credential_projection: None,
    };
    let errs = validate_cluster_repository(&spec);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::ClusterNameInvalid { .. })),
        "{errs:?}"
    );
}

#[test]
fn repository_rejects_bad_cluster_name() {
    // M5: `RepositorySpec.identityDefaults.cluster` is validated exactly like
    // `ClusterRepositorySpec`'s field of the same name.
    let repo: RepositorySpec = crate::testutil::from_yaml(
        r#"
backend:
  filesystem:
    path: /repo
encryption:
  passwordSecretRef:
    name: creds
identityDefaults:
  cluster: East
"#,
    );
    let errs = validate_repository(&repo);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::ClusterNameInvalid { .. })),
        "{errs:?}"
    );
}

#[test]
fn repository_rejects_bad_identity_expr() {
    // M5: `RepositorySpec.identityDefaults.{hostnameExpr,usernameExpr}` are
    // validated exactly like `ClusterRepositorySpec`'s fields of the same name.
    // `namspace` is an out-of-scope typo → rejected at admission (ADR-0004 §5),
    // mirroring `cluster_repository_rejects_bad_identity_expr`.
    let repo: RepositorySpec = crate::testutil::from_yaml(
        "backend: { filesystem: { path: /repo } }\n\
         encryption: { passwordSecretRef: { name: creds } }\n\
         identityDefaults:\n  hostnameExpr: namspace\n",
    );
    let errs = validate_repository(&repo);
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::IdentityExprEval { .. })),
        "{errs:?}"
    );
}

#[test]
fn backup_config_rejects_bad_identity_override_and_path() {
    let mut spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: data } } ]\n",
    );
    spec.identity = Some(Identity {
        username: Some("bad@user".into()),
        hostname: Some("ok-host".into()),
    });
    spec.sources[0].source_path_override = Some("/data\nx".into());
    let errs = validate_backup_config(&spec);
    assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::IdentityComponentInvalid { field, .. } if field == "spec.identity.username")),
            "{errs:?}"
        );
    assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::IdentitySourcePathInvalid { field, .. } if field == "spec.sources[0].sourcePathOverride")),
            "{errs:?}"
        );
}

// --- fork-on-edit detectors ---

#[test]
fn identity_fork_only_when_history_change_and_unacked() {
    assert!(detect_identity_fork("pg@a", "pg@b", true, false).is_some());
    assert!(detect_identity_fork("pg@a", "pg@b", false, false).is_none()); // no history
    assert!(detect_identity_fork("pg@a", "pg@b", true, true).is_none()); // acked
    assert!(detect_identity_fork("pg@a", "pg@a", true, false).is_none()); // no change
}

#[test]
fn source_path_fork_matches_by_pvc_name() {
    let mk = |path_override: Option<&str>| -> SnapshotPolicySpec {
        let mut s: SnapshotPolicySpec = crate::testutil::from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: data } } ]\n",
        );
        s.sources[0].source_path_override = path_override.map(String::from);
        s
    };
    // Default /pvc/data → explicit /data on the SAME pvc, with history, no ack → fork.
    let old = mk(None);
    let new = mk(Some("/data"));
    assert!(detect_source_path_fork(&old, &new, true, false).is_some());
    // Acked → allowed.
    assert!(detect_source_path_fork(&old, &new, true, true).is_none());
    // No history → allowed.
    assert!(detect_source_path_fork(&old, &new, false, false).is_none());
    // Same effective path (both default) → allowed.
    assert!(detect_source_path_fork(&mk(None), &mk(None), true, false).is_none());
}

// --- repository identityDefaults edit guard ---

#[test]
fn repository_identity_change_decision_table() {
    use crate::common::IdentityDefaults;

    let east = IdentityDefaults {
        cluster: Some("east".into()),
        hostname_expr: None,
        username_expr: None,
    };
    let west = IdentityDefaults {
        cluster: Some("west".into()),
        hostname_expr: None,
        username_expr: None,
    };
    let consumers = vec!["billing/pg".to_string()];

    // No change ⇒ None, regardless of consumers/ack.
    assert!(
        detect_repository_identity_change(Some(&east), Some(&east), false, &consumers).is_none()
    );
    assert!(detect_repository_identity_change(None, None, false, &consumers).is_none());

    // Change + no consumers with history ⇒ None (nothing to orphan).
    assert!(detect_repository_identity_change(Some(&east), Some(&west), false, &[]).is_none());

    // Change + consumers + not acked ⇒ Some, naming the consumer.
    let err = detect_repository_identity_change(Some(&east), Some(&west), false, &consumers);
    assert!(
        matches!(&err, Some(ValidationError::RepositoryIdentityWouldFork { consumers: c }) if c == &consumers),
        "{err:?}"
    );

    // Acked ⇒ None even with consumers.
    assert!(
        detect_repository_identity_change(Some(&east), Some(&west), true, &consumers).is_none()
    );

    // Going from `None` (no identityDefaults at all) to `Some` is a change too.
    assert!(detect_repository_identity_change(None, Some(&west), false, &consumers).is_some());

    // Each of cluster/hostnameExpr/usernameExpr individually triggers.
    let base = IdentityDefaults {
        cluster: Some("east".into()),
        hostname_expr: Some("namespace".into()),
        username_expr: Some("'svc'".into()),
    };
    let cluster_changed = IdentityDefaults {
        cluster: Some("west".into()),
        ..base.clone()
    };
    let hostname_changed = IdentityDefaults {
        hostname_expr: Some("namespace + '.' + cluster".into()),
        ..base.clone()
    };
    let username_changed = IdentityDefaults {
        username_expr: Some("'other'".into()),
        ..base.clone()
    };
    assert!(
        detect_repository_identity_change(Some(&base), Some(&cluster_changed), false, &consumers)
            .is_some()
    );
    assert!(
        detect_repository_identity_change(Some(&base), Some(&hostname_changed), false, &consumers)
            .is_some()
    );
    assert!(
        detect_repository_identity_change(Some(&base), Some(&username_changed), false, &consumers)
            .is_some()
    );
}

// --- validate_access_modes (shared: staging + restore target) ---

#[test]
fn access_modes_accept_canonical_unique_lists() {
    use crate::common::PvcAccessMode as M;
    assert!(validate_access_modes("f", &[]).is_empty());
    assert!(validate_access_modes("f", &[M::ReadOnlyMany]).is_empty());
    assert!(validate_access_modes("f", &[M::ReadWriteOnce, M::ReadWriteMany]).is_empty());
    // RWOP alone is exactly what the apiserver allows.
    assert!(validate_access_modes("f", &[M::ReadWriteOncePod]).is_empty());
}

#[test]
fn access_modes_reject_unknown_with_the_value_and_valid_set_quoted() {
    use crate::common::PvcAccessMode as M;
    let errs = validate_access_modes(
        "spec.staging.accessModes",
        &[M::Unknown("ReadWriteOnze".into())],
    );
    assert_eq!(errs.len(), 1, "{errs:?}");
    let msg = errs[0].to_string();
    assert!(msg.contains("spec.staging.accessModes[0]"), "{msg}");
    assert!(
        msg.contains("ReadWriteOnze"),
        "the bad value must be quoted: {msg}"
    );
    assert!(
        msg.contains("ReadWriteOnce") && msg.contains("ReadWriteOncePod"),
        "the valid set must be listed: {msg}"
    );
}

#[test]
fn access_modes_reject_duplicates_and_rwop_combinations() {
    use crate::common::PvcAccessMode as M;
    let errs = validate_access_modes("f", &[M::ReadWriteOnce, M::ReadWriteOnce]);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].to_string().contains("duplicate"), "{errs:?}");

    // The apiserver invariant: RWOP must be the sole mode — caught at admission
    // instead of wedging the first run in a PVC-create retry loop.
    let errs = validate_access_modes("f", &[M::ReadWriteOncePod, M::ReadOnlyMany]);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].to_string().contains("ReadWriteOncePod"), "{errs:?}");
}

// --- validate_staging: staged-PVC overrides need a staged PVC to act on ---

#[test]
fn staging_overrides_accepted_for_snapshot_and_clone_pvc_sources() {
    for method in ["Snapshot", "Clone"] {
        let spec: SnapshotPolicySpec = crate::testutil::from_yaml(&format!(
            "repository: {{ kind: Repository, name: r }}\n\
             copyMethod: {method}\n\
             sources: [ {{ pvc: {{ name: data }} }} ]\n\
             staging: {{ storageClassName: cephfs-shallow, accessModes: [ReadOnlyMany] }}\n"
        ));
        assert!(
            validate_backup_config(&spec).is_empty(),
            "{method} + overrides must be valid"
        );
    }
}

#[test]
fn staging_overrides_rejected_for_direct_copy_method() {
    let spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         copyMethod: Direct\n\
         sources: [ { pvc: { name: data } } ]\n\
         staging: { storageClassName: cephfs-shallow }\n",
    );
    let errs = validate_backup_config(&spec);
    let msg = errs
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(msg.contains("spec.staging.storageClassName"), "{msg}");
    assert!(msg.contains("Direct"), "{msg}");
    assert!(
        msg.contains("Snapshot/Clone"),
        "the fix must be named: {msg}"
    );

    // Direct + a bare staging.timeout stays admitted (pre-existing leniency:
    // tightening it would reject persisted objects on re-apply).
    let spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         copyMethod: Direct\n\
         sources: [ { pvc: { name: data } } ]\n\
         staging: { timeout: 5m }\n",
    );
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn staging_overrides_rejected_for_nfs_and_pvc_selector_sources() {
    // NFS is read directly, never staged.
    let spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { nfs: { server: nas.lan, path: /export/data } } ]\n\
         staging: { accessModes: [ReadOnlyMany] }\n",
    );
    let errs = validate_backup_config(&spec);
    let msg = errs
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(msg.contains("spec.staging.accessModes"), "{msg}");
    assert!(msg.contains("NFS"), "{msg}");

    // pvcSelector expansion skips staging, so an override would be silently inert.
    let spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvcSelector: { labelSelector: { matchLabels: { app: pg } } } } ]\n\
         staging: { storageClassName: cephfs-shallow }\n",
    );
    let errs = validate_backup_config(&spec);
    let msg = errs
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(msg.contains("pvcSelector"), "{msg}");
}

#[test]
fn staging_unknown_access_mode_rejected_via_backup_config() {
    // The legacy-decode `Unknown` value is rejected by the SHARED validator, so the
    // webhook (admission) and controller (defensive) both refuse it identically.
    let spec: SnapshotPolicySpec = crate::testutil::from_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { pvc: { name: data } } ]\n\
         staging: { accessModes: [ReadWriteOnze] }\n",
    );
    let errs = validate_backup_config(&spec);
    let msg = errs
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(msg.contains("ReadWriteOnze"), "{msg}");
}

#[test]
fn restore_pvc_target_rejects_unknown_and_rwop_combo_access_modes() {
    use crate::common::{ObjectRef, PvcAccessMode as M};
    use crate::restore::PvcTemplate;
    let mut spec = restore_with(
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        }),
        None,
    );
    // A legacy stored (or force-applied) bogus mode: rejected per-CR with the
    // value quoted — this is the graceful path for pre-enforcement data.
    spec.target = RestoreTarget::Pvc(PvcTemplate {
        name: "restored".into(),
        storage_class_name: None,
        capacity: Some("10Gi".into()),
        access_modes: vec![M::Unknown("ReadWriteOnze".into())],
    });
    let err = validate_restore(&spec).unwrap_err();
    assert!(err.to_string().contains("ReadWriteOnze"), "{err}");
    assert!(
        err.to_string().contains("restore.target.pvc.accessModes"),
        "{err}"
    );

    // RWOP combined with another mode: the apiserver would reject the PVC.
    spec.target = RestoreTarget::Pvc(PvcTemplate {
        name: "restored".into(),
        storage_class_name: None,
        capacity: Some("10Gi".into()),
        access_modes: vec![M::ReadWriteOncePod, M::ReadWriteOnce],
    });
    let err = validate_restore(&spec).unwrap_err();
    assert!(err.to_string().contains("ReadWriteOncePod"), "{err}");

    // A canonical single mode passes.
    spec.target = RestoreTarget::Pvc(PvcTemplate {
        name: "restored".into(),
        storage_class_name: None,
        capacity: Some("10Gi".into()),
        access_modes: vec![M::ReadOnlyMany],
    });
    assert!(validate_restore(&spec).is_ok());
}

/// M6: each `ownership.ownerAliases` entry gets the kopia identity shape rule
/// (it becomes an identity hostname via `kopia_lease_identity`); `owner`
/// itself is deliberately NOT tightened (stored pre-M6 CRs may carry strings
/// the lease sanitizer already handles — a new rejection would hard-stop a
/// working Maintenance on its next defensive re-validation).
#[test]
fn maintenance_owner_aliases_are_shape_validated_but_owner_is_not_tightened() {
    use crate::maintenance::{MaintenanceSpec, Ownership};

    let spec = |owner: &str, aliases: Vec<String>| MaintenanceSpec {
        repository: repo_ref(RepositoryKind::Repository, None),
        schedule: crate::maintenance::default_maintenance_schedule(),
        ownership: Ownership {
            owner: owner.into(),
            owner_aliases: aliases,
            takeover_policy: Default::default(),
        },
        mover: None,
        failure_policy: None,
        credential_projection: None,
    };

    // Well-formed aliases (lease strings) pass.
    assert!(
        validate_maintenance(&spec(
            "kopiur/east/prod/nas",
            vec!["kopiur/prod/nas".into()]
        ))
        .is_empty()
    );

    // An alias with an identity delimiter is rejected, naming the entry.
    let errs = validate_maintenance(&spec(
        "kopiur/east/prod/nas",
        vec!["kopiur/prod/nas".into(), "bad@alias".into()],
    ));
    assert_eq!(errs.len(), 1, "{errs:?}");
    let msg = errs[0].to_string();
    assert!(msg.contains("ownerAliases[1]"), "{msg}");
    assert!(msg.contains('@'), "{msg}");

    // A legacy hand-authored owner with an '@' is still accepted (no stored-CR
    // regression) — the lease sanitizer collapses it safely at run time.
    assert!(validate_maintenance(&spec("admin@legacy-host", Vec::new())).is_empty());
}

// --- schedule_cr_growth_warning (issue #249) ---

#[test]
fn hourly_or_slower_schedules_get_no_growth_warning() {
    // Hourly, jittered-hourly (H is minute-of-window, not cadence), and daily are
    // all within the "one CR per hour or less" comfort zone → no warning.
    for cron in ["0 * * * *", "H * * * *", "0 2 * * *", "0 0 * * 0"] {
        assert_eq!(
            schedule_cr_growth_warning(cron),
            None,
            "{cron} should not warn"
        );
    }
}

#[test]
fn sub_hourly_schedules_warn_with_the_fires_per_hour_count() {
    let ten = schedule_cr_growth_warning("*/10 * * * *").expect("*/10 warns");
    assert!(ten.contains("6×/hour"), "{ten}");
    assert!(ten.contains("docs/backups.md"), "{ten}");
    let five = schedule_cr_growth_warning("*/5 * * * *").expect("*/5 warns");
    assert!(five.contains("12×/hour"), "{five}");
    let twice = schedule_cr_growth_warning("0,30 * * * *").expect("0,30 warns");
    assert!(twice.contains("2×/hour"), "{twice}");
}

#[test]
fn day_constrained_sub_hourly_schedule_still_warns() {
    // The window anchors on the cron's FIRST fire, so a Sunday-only every-10-min
    // schedule is measured during an active hour (a fixed Monday hour would miss it).
    let w = schedule_cr_growth_warning("*/10 * * * 0").expect("sunday */10 warns");
    assert!(w.contains("6×/hour"), "{w}");
}

/// The `merged` snippet in `deploy/examples/18-inherit-security-context.yaml` — inherit +
/// explicit override — must deserialize into the real typed spec AND pass validation. Guards
/// the example the docs page teaches from rotting (and from the field names being wrong:
/// these types do not `deny_unknown_fields`, so a typo'd optional key is silently dropped).
#[test]
fn example_18_merged_policy_is_valid() {
    let file = include_str!("../../../../deploy/examples/18-inherit-security-context.yaml");
    // Pick the document by name — the file's last document is a commented-out Restore, so
    // "take the last chunk" would silently test nothing.
    let doc = file
        .split("\n---\n")
        .find(|d| d.contains("name: app-data-merged"))
        .expect("the `merged` policy document must exist in example 18");
    // YAML -> serde_json::Value -> typed, the way the cluster does it (CLAUDE.md §5):
    // serde_yaml 0.9 mis-encodes externally-tagged enums when deserialized directly.
    let policy: crate::snapshot_policy::SnapshotPolicy = crate::testutil::from_yaml(doc);

    let mover = policy.spec.mover.as_ref().expect("mover set");
    assert!(
        mover.inherit_security_context_from.is_some(),
        "inheritSecurityContextFrom must survive deserialization (not a dropped unknown key)"
    );
    assert_eq!(
        mover.security_context.as_ref().and_then(|s| s.run_as_user),
        Some(1000),
        "the explicit override must survive deserialization"
    );
    assert_eq!(
        mover
            .pod_security_context
            .as_ref()
            .and_then(|p| p.supplemental_groups.clone()),
        Some(vec![3001]),
    );
    // And the pair the docs teach must actually be accepted.
    validate_mover(mover, "SnapshotPolicy mover").expect("inherit + explicit must be accepted");
}

/// `RepositoryReplication.spec.mover.inheritSecurityContextFrom` used to be accepted at
/// admission and then silently dropped at reconcile: `repository_replication.rs` passes the
/// explicit contexts straight to `resolve_mover` and never resolves inheritance. The manifest
/// said the mover ran as the workload; it ran as something else, and nothing said otherwise.
///
/// A replication mover copies blobs repo→repo and never reads a workload's files, so there is
/// no workload identity to take. Reject it rather than ignore it.
#[test]
fn replication_mover_rejects_inherit_instead_of_silently_dropping_it() {
    use crate::common::{InheritSecurityContextFrom, MoverSpec, PodSelector, PvcConsumerInherit};
    use k8s_openapi::api::core::v1::SecurityContext;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;

    let with_inherit = |i: InheritSecurityContextFrom| MoverSpec {
        inherit_security_context_from: Some(i),
        ..Default::default()
    };

    // Both variants are equally unhonorable here.
    for inherit in [
        InheritSecurityContextFrom::PvcConsumer(PvcConsumerInherit::default()),
        InheritSecurityContextFrom::WorkloadSelector(PodSelector {
            pod_selector: LabelSelector::default(),
            container: None,
        }),
    ] {
        let err = super::forbid_inherit(
            &with_inherit(inherit),
            "RepositoryReplication spec",
            "is not honored by a replication mover",
        )
        .expect_err("inherit on a replication mover must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("inheritSecurityContextFrom"),
            "the message must name the offending field, got: {msg}"
        );
    }

    // An explicit context — the actual remedy — is still fine.
    let explicit = MoverSpec {
        security_context: Some(SecurityContext {
            run_as_user: Some(3001),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(super::forbid_inherit(&explicit, "RepositoryReplication spec", "x").is_ok());
    assert!(validate_mover(&explicit, "RepositoryReplication mover").is_ok());
}

/// The same rejection, through the REAL entry point users hit. `validate_repository_replication`
/// is called by both the webhook (`handlers.rs`) and the controller
/// (`repository_replication.rs`), so this covers webhook-disabled installs too.
#[test]
fn replication_validator_surfaces_the_inherit_rejection() {
    use crate::backend::{Backend, S3Backend};
    use crate::common::{InheritSecurityContextFrom, MoverSpec, PvcConsumerInherit};

    let mut spec = replication_spec(
        repo_ref(RepositoryKind::Repository, None),
        Backend::S3(S3Backend {
            bucket: "mirror".into(),
            prefix: None,
            endpoint: None,
            region: None,
            auth: None,
            tls: None,
        }),
        "0 5 * * *",
    );
    // Sanity: this spec is otherwise valid, so any error below is the one under test.
    assert!(validate_repository_replication(&spec).is_empty());

    spec.mover = Some(MoverSpec {
        inherit_security_context_from: Some(InheritSecurityContextFrom::PvcConsumer(
            PvcConsumerInherit::default(),
        )),
        ..Default::default()
    });
    let errs = validate_repository_replication(&spec);
    let named = errs.iter().any(|e| {
        matches!(e, ValidationError::InvalidFieldValue { field, .. }
            if field == "RepositoryReplication spec.mover.inheritSecurityContextFrom")
    });
    assert!(
        named,
        "expected the inherit field to be rejected, got: {errs:?}"
    );
    // The message must say WHY and WHAT TO DO, not just "invalid".
    let msg = errs
        .iter()
        .find(|e| {
            matches!(e, ValidationError::InvalidFieldValue { field, .. }
            if field.ends_with("inheritSecurityContextFrom"))
        })
        .map(|e| e.to_string())
        .unwrap();
    assert!(
        msg.contains("never reads a workload's files") && msg.contains("mover.securityContext"),
        "the message must explain the why and name the remedy, got: {msg}"
    );
}

/// The e2e `replication_with_inherit` fixture (crates/e2e/tests/webhook.rs) must be rejected
/// for EXACTLY ONE reason: the inherit field. A fixture that is invalid some other way would
/// still be DENIED at admission, and the e2e test would pass while proving nothing.
#[test]
fn e2e_replication_inherit_fixture_is_rejected_only_for_inherit() {
    let v: serde_json::Value = crate::testutil::from_yaml(
        r#"
sourceRef: { kind: Repository, name: any }
destination: { s3: { bucket: mirror, region: us-east-1 } }
schedule: { cron: "0 5 * * *" }
mover:
  inheritSecurityContextFrom:
    pvcConsumer: {}
"#,
    );
    let spec: crate::repository_replication::RepositoryReplicationSpec =
        serde_json::from_value(v).expect("the e2e fixture shape deserializes");
    let errs = validate_repository_replication(&spec);
    assert_eq!(
        errs.len(),
        1,
        "the fixture must be otherwise valid so the e2e denial is attributable, got: {errs:?}"
    );
    assert!(matches!(
        &errs[0],
        ValidationError::InvalidFieldValue { field, .. }
            if field == "RepositoryReplication spec.mover.inheritSecurityContextFrom"
    ));
}

/// The `mover` shapes the new e2e scenarios build (crates/e2e/tests/security_context_compat.rs)
/// must deserialize into the typed spec. The e2e harness's `cr()` helper panics at *runtime* on
/// a bad shape, so a typo there fails inside a kind cluster minutes later, if at all — and a
/// silently-dropped optional key (no `deny_unknown_fields`) would not fail at all. Pin them here.
#[test]
fn e2e_inherit_scenario_mover_shapes_deserialize() {
    use crate::common::{InheritSecurityContextFrom, MoverSpec};

    // Scenario (c)/(d)/(e): pvcConsumer, alone and with an explicit override.
    let cases: &[(&str, &str)] = &[
        (
            "pvcConsumer alone",
            r#"{ "inheritSecurityContextFrom": { "pvcConsumer": {} } }"#,
        ),
        (
            "pvcConsumer + explicit override",
            r#"{ "inheritSecurityContextFrom": { "pvcConsumer": {} },
                 "securityContext": { "runAsUser": 1000 } }"#,
        ),
    ];
    for (what, json) in cases {
        let mover: MoverSpec = serde_json::from_str(json).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert!(
            matches!(
                mover.inherit_security_context_from,
                Some(InheritSecurityContextFrom::PvcConsumer(_))
            ),
            "{what}: pvcConsumer must survive deserialization, not be dropped as unknown"
        );
        validate_mover(&mover, "SnapshotPolicy mover")
            .unwrap_or_else(|e| panic!("{what}: must be accepted, got {e}"));
    }

    // The override case must actually carry the explicit UID (the thing the e2e asserts wins).
    let mover: MoverSpec = serde_json::from_str(cases[1].1).unwrap();
    assert_eq!(
        mover.security_context.and_then(|s| s.run_as_user),
        Some(1000)
    );

    // Scenario (c)'s UID-less workload container context.
    let sc: k8s_openapi::api::core::v1::SecurityContext =
        serde_json::from_str(r#"{ "allowPrivilegeEscalation": false }"#).unwrap();
    assert_eq!(
        crate::common::effective_run_as_user(Some(&sc), None),
        None,
        "the e2e's UID-less workload must genuinely pin no UID, or scenario (c) proves nothing"
    );
}

// --- #254: source readOnly ------------------------------------------------

/// Parse a SnapshotPolicy spec the way the apiserver would (YAML → Value → typed),
/// per the repo's testutil rule — never `serde_yaml` straight into a typed value.
fn policy_yaml(body: &str) -> SnapshotPolicySpec {
    crate::testutil::from_yaml(body)
}

#[test]
fn source_read_only_defaults_to_true_and_accepts_an_explicit_false() {
    use crate::snapshot_policy::source_read_only;
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: data } } ]\n",
    );
    assert!(
        source_read_only(&spec.sources[0]),
        "an unset readOnly must resolve to the CRD's advertised default (true)"
    );
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\n\
         copyMethod: Snapshot\n\
         sources: [ { pvc: { name: data }, readOnly: false } ]\n",
    );
    assert!(!source_read_only(&spec.sources[0]));
    // Staged copyMethods need no acknowledgement: the fsGroup walk lands on the
    // throwaway staged PVC, never on the workload's volume.
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn writable_nfs_source_is_rejected_because_fsgroup_never_applies_to_it() {
    // readOnly: false exists for exactly one reason — making fsGroup apply — and the
    // kubelet does not apply fsGroup to in-tree NFS volumes at all. Allowing it would
    // ship a knob that cannot do the only thing it is for, while making the export
    // writable to the mover.
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { nfs: { server: nas.lan, path: /export/media }, readOnly: false } ]\n",
    );
    let errs = validate_backup_config(&spec);
    let msg = format!("{errs:?}");
    assert!(!errs.is_empty(), "a writable nfs source must be rejected");
    assert!(msg.contains("fsGroup"), "must say WHY: {msg}");
    // ...and point at what does work on NFS.
    assert!(
        msg.contains("supplementalGroups") || msg.contains("runAsUser"),
        "{msg}"
    );
    // A read-only nfs source stays valid — this rule must not break existing configs.
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\n\
         sources: [ { nfs: { server: nas.lan, path: /export/media } } ]\n",
    );
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn direct_plus_writable_source_needs_an_acknowledgement() {
    // The hazard: Direct mounts the LIVE volume, so the kubelet's recursive fsGroup
    // chgrp rewrites production data — and the mover ships fsGroup 65532 by default,
    // so one bool would silently re-group a running app's files.
    let direct = |ack: &str| {
        policy_yaml(&format!(
            "repository: {{ kind: Repository, name: r }}\n\
             copyMethod: Direct\n\
             sources: [ {{ pvc: {{ name: data }}, readOnly: false{ack} }} ]\n"
        ))
    };
    let errs = validate_backup_config(&direct(""));
    let msg = format!("{errs:?}");
    assert!(!errs.is_empty(), "Direct + readOnly: false must be gated");
    assert!(
        msg.contains("acknowledgeLiveMutation"),
        "must name the way through: {msg}"
    );
    assert!(
        msg.contains("Snapshot/Clone"),
        "must offer the safe alternative: {msg}"
    );

    // Acknowledged → allowed. The user has said the words.
    assert!(validate_backup_config(&direct(", acknowledgeLiveMutation: true")).is_empty());
    // Explicitly declined is not an acknowledgement.
    assert!(!validate_backup_config(&direct(", acknowledgeLiveMutation: false")).is_empty());

    // Direct + read-only (the default) is untouched — this gate must not tax the
    // overwhelmingly common config.
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\n\
         copyMethod: Direct\nsources: [ { pvc: { name: data } } ]\n",
    );
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn a_writable_nfs_source_is_rejected_once_not_told_to_acknowledge_it() {
    // `Direct` + writable + nfs used to yield TWO errors: the (correct) nfs rejection,
    // and advice to set acknowledgeLiveMutation — which could never make it valid, since
    // the nfs rule rejects regardless. The kubelet does not apply fsGroup to in-tree NFS
    // at all, so "the kubelet will rewrite your live volume" is false there to begin with.
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\n\
         copyMethod: Direct\n\
         sources: [ { nfs: { server: nas.lan, path: /export/media }, readOnly: false } ]\n",
    );
    let errs = validate_backup_config(&spec);
    let msg = format!("{errs:?}");
    assert_eq!(errs.len(), 1, "one mistake, one error: {errs:#?}");
    assert!(
        msg.contains("fsGroup"),
        "and it must be the nfs rejection: {msg}"
    );
    assert!(
        !msg.contains("acknowledgeLiveMutation"),
        "must not advise an acknowledgement that cannot help: {msg}"
    );
}

#[test]
fn a_stale_acknowledgement_is_ignored_rather_than_rejected() {
    // Deliberate deviation from the reject-don't-silently-ignore precedent: rejecting a
    // no-longer-needed ack would make switching copyMethod between Direct and
    // Snapshot/Clone a two-step edit in BOTH directions. An ack is never harmful to carry.
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\n\
         copyMethod: Snapshot\n\
         sources: [ { pvc: { name: data }, acknowledgeLiveMutation: true } ]\n",
    );
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn writable_source_conflicts_with_a_read_only_many_staged_pvc() {
    // A ReadOnlyMany staged PVC cannot be mounted read-write: without this the kubelet
    // fails the mount at backup time with an opaque error, long after admission.
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\n\
         copyMethod: Snapshot\n\
         staging: { accessModes: [ReadOnlyMany] }\n\
         sources: [ { pvc: { name: data }, readOnly: false } ]\n",
    );
    let errs = validate_backup_config(&spec);
    let msg = format!("{errs:?}");
    assert!(
        !errs.is_empty(),
        "ReadOnlyMany + readOnly: false must be rejected"
    );
    assert!(msg.contains("ReadOnlyMany"), "{msg}");
    // The same conflict exists for a read-only staged CLASS, which admission cannot see.
    assert!(
        msg.contains("backingSnapshot"),
        "must warn about the invisible twin: {msg}"
    );

    // ReadOnlyMany + the read-only default is the documented pairing — still valid.
    let spec = policy_yaml(
        "repository: { kind: Repository, name: r }\n\
         copyMethod: Snapshot\n\
         staging: { accessModes: [ReadOnlyMany] }\n\
         sources: [ { pvc: { name: data } } ]\n",
    );
    assert!(validate_backup_config(&spec).is_empty());
}

// --- #258: spec.parameters.epoch ------------------------------------------

fn repo_yaml(body: &str) -> RepositorySpec {
    crate::testutil::from_yaml(body)
}

const REPO_BASE: &str = "backend: { filesystem: { path: /repo } }\n\
                         encryption: { passwordSecretRef: { name: s, key: KOPIA_PASSWORD } }\n";

#[test]
fn epoch_parameters_are_optional_and_accept_go_durations() {
    // The inert case: no parameters block at all changes nothing.
    assert!(validate_repository(&repo_yaml(REPO_BASE)).is_empty());
    // The reporter's fix.
    let spec = repo_yaml(&format!(
        "{REPO_BASE}parameters:\n  epoch:\n    minDuration: 6h\n    refreshFrequency: 20m\n    \
         advanceOnCount: 20\n    advanceOnSizeMiB: 10\n    checkpointFrequency: 7\n    \
         deleteParallelism: 4\n"
    ));
    assert!(validate_repository(&spec).is_empty());
}

#[test]
fn an_unparseable_epoch_duration_is_rejected_at_admission() {
    // These are the first CRD durations that reach a kopia CLI, so the webhook's promise —
    // a value it admits never fails at reconcile time — has teeth here.
    let spec = repo_yaml(&format!(
        "{REPO_BASE}parameters:\n  epoch:\n    minDuration: every-6-hours\n"
    ));
    let errs = validate_repository(&spec);
    let msg = format!("{errs:?}");
    assert!(!errs.is_empty());
    assert!(msg.contains("minDuration"), "{msg}");
    assert!(
        msg.contains("6h") || msg.contains("Go-style"),
        "must show the grammar: {msg}"
    );
}

#[test]
fn an_epoch_duration_beyond_kopias_range_is_rejected() {
    // parse_go_duration accepts a bare seconds count of any size, but kopia stores these
    // as a Go time.Duration — an i64 NANOSECOND count, max ~292 years. Left unbounded,
    // `as_nanos() as i64` in the drift comparator wraps to a NEGATIVE number
    // (999999999999999999s -> -6930898828444486144), which would report drift against
    // every observation and re-apply set-parameters on every bootstrap forever.
    let spec = repo_yaml(&format!(
        "{REPO_BASE}parameters:\n  epoch:\n    minDuration: \"999999999999999999\"\n"
    ));
    let errs = validate_repository(&spec);
    let msg = format!("{errs:?}");
    assert!(
        !errs.is_empty(),
        "a duration beyond i64 nanoseconds must be rejected"
    );
    assert!(
        msg.contains("292 years") || msg.contains("too large"),
        "{msg}"
    );

    // The realistic values stay valid — the bound must not tax anyone.
    let spec = repo_yaml(&format!(
        "{REPO_BASE}parameters:\n  epoch:\n    minDuration: 6h\n    refreshFrequency: 20m\n"
    ));
    assert!(validate_repository(&spec).is_empty());
}

#[test]
fn non_positive_epoch_counts_are_rejected() {
    for field in [
        "advanceOnCount",
        "advanceOnSizeMiB",
        "checkpointFrequency",
        "deleteParallelism",
    ] {
        let spec = repo_yaml(&format!(
            "{REPO_BASE}parameters:\n  epoch:\n    {field}: 0\n"
        ));
        let errs = validate_repository(&spec);
        assert!(!errs.is_empty(), "{field}: 0 must be rejected");
        assert!(format!("{errs:?}").contains(field));
    }
}

#[test]
fn a_read_only_repository_cannot_declare_epoch_parameters() {
    // `kopia repository set-parameters` rewrites the repository-global format blob and
    // HARD-ERRORS on a read-only connection (`storage is read-only`). Silently ignoring
    // the block would leave the user watching a setting that can never land; reject it,
    // the way an NFS source + volumeSnapshotClassName is rejected.
    let spec = repo_yaml(&format!(
        "{REPO_BASE}mode: ReadOnly\nparameters:\n  epoch:\n    minDuration: 6h\n"
    ));
    let errs = validate_repository(&spec);
    let msg = format!("{errs:?}");
    assert!(!errs.is_empty(), "ReadOnly + parameters must be rejected");
    assert!(msg.contains("ReadOnly"), "{msg}");
    assert!(msg.contains("set-parameters"), "must say WHY: {msg}");

    // ReadOnly WITHOUT parameters stays valid — this must not tax a plain consumer repo.
    let spec = repo_yaml(&format!("{REPO_BASE}mode: ReadOnly\n"));
    assert!(validate_repository(&spec).is_empty());
}

#[test]
fn cluster_repository_gets_the_identical_parameters_rules() {
    // The two kinds have fully duplicated reconcilers, so a rule that lands on only one of
    // them is the classic way half this API surface ships as a silent no-op.
    let base = "backend: { filesystem: { path: /repo } }\n\
                encryption: { passwordSecretRef: { name: s, namespace: kopiur-system, key: KOPIA_PASSWORD } }\n\
                allowedNamespaces: { all: true }\n";
    let spec: ClusterRepositorySpec = crate::testutil::from_yaml(&format!(
        "{base}parameters:\n  epoch:\n    minDuration: nonsense\n"
    ));
    let errs = validate_cluster_repository(&spec);
    assert!(
        format!("{errs:?}").contains("minDuration"),
        "ClusterRepository must validate spec.parameters too: {errs:?}"
    );

    let spec: ClusterRepositorySpec = crate::testutil::from_yaml(&format!(
        "{base}mode: ReadOnly\nparameters:\n  epoch:\n    minDuration: 6h\n"
    ));
    assert!(
        !validate_cluster_repository(&spec).is_empty(),
        "a ReadOnly ClusterRepository must reject parameters as well"
    );

    let spec: ClusterRepositorySpec = crate::testutil::from_yaml(&format!(
        "{base}parameters:\n  epoch:\n    minDuration: 6h\n"
    ));
    assert!(validate_cluster_repository(&spec).is_empty());
}

// --- #332: object-lock blob retention -------------------------------------------------

/// An object-lock-capable base. `REPO_BASE` is `filesystem`, which blob retention rejects.
const S3_REPO_BASE: &str = "backend: { s3: { bucket: b } }\n\
                            encryption: { passwordSecretRef: { name: s, key: KOPIA_PASSWORD } }\n";

#[test]
fn blob_retention_accepts_both_modes_and_disable() {
    // The inert case: no blobRetention at all changes nothing about the repository.
    assert!(validate_repository(&repo_yaml(S3_REPO_BASE)).is_empty());

    for mode in ["governance", "compliance"] {
        let spec = repo_yaml(&format!(
            "{S3_REPO_BASE}parameters:\n  blobRetention:\n    {mode}:\n      period: 720h\n"
        ));
        assert!(
            validate_repository(&spec).is_empty(),
            "{mode}/720h must be accepted"
        );
    }

    // Disabling carries no period at all — the type has nowhere to put one.
    let spec = repo_yaml(&format!(
        "{S3_REPO_BASE}parameters:\n  blobRetention:\n    disabled: true\n"
    ));
    assert!(validate_repository(&spec).is_empty());

    // blobRetention alongside epoch, and blobRetention with NO epoch — the second is the
    // regression guard: validate_repository_parameters used to early-return when `epoch`
    // was absent, which would have made every rule below dead code.
    let spec = repo_yaml(&format!(
        "{S3_REPO_BASE}parameters:\n  epoch:\n    minDuration: 6h\n  \
         blobRetention:\n    governance:\n      period: 720h\n"
    ));
    assert!(validate_repository(&spec).is_empty());
    let spec = repo_yaml(&format!(
        "{S3_REPO_BASE}parameters:\n  blobRetention:\n    governance:\n      period: 1h\n"
    ));
    assert!(
        !validate_repository(&spec).is_empty(),
        "blobRetention must be validated even when spec.parameters.epoch is absent"
    );
}

#[test]
fn a_retention_period_below_kopias_one_day_floor_is_rejected() {
    // kopia's own Validate(): "invalid retention-period, the minimum required is 1-day and
    // there is no maximum limit". Catching it here turns a hard set-parameters failure on
    // every reconcile into one admission error.
    // Quoted, because YAML parses a bare `3600` as an integer and `period` is a string —
    // that shape is rejected by the CRD's own `type: string` before it ever reaches here.
    for period in ["1h", "23h", "60m", "\"3600\""] {
        let spec = repo_yaml(&format!(
            "{S3_REPO_BASE}parameters:\n  blobRetention:\n    governance:\n      period: {period}\n"
        ));
        let errs = validate_repository(&spec);
        assert!(
            !errs.is_empty(),
            "{period} is under 24h and must be rejected"
        );
        assert!(
            format!("{errs:?}").contains("1-day"),
            "must quote kopia's own wording so the two are searchable together: {errs:?}"
        );
    }
    // Exactly the floor is allowed.
    let spec = repo_yaml(&format!(
        "{S3_REPO_BASE}parameters:\n  blobRetention:\n    governance:\n      period: 24h\n"
    ));
    assert!(validate_repository(&spec).is_empty(), "24h is the boundary");
}

#[test]
fn a_retention_period_in_days_is_rejected_with_the_hours_hint() {
    // kopia's CLI DOES accept `30d` (it extends Go's parser), but kopiur's duration grammar
    // is deliberately narrower — h/m/s only, one grammar across every CRD field. `30d` is
    // therefore the single most likely thing a user copying from kopia's docs will write,
    // so the message has to name the fix rather than just say "invalid".
    let spec = repo_yaml(&format!(
        "{S3_REPO_BASE}parameters:\n  blobRetention:\n    governance:\n      period: 30d\n"
    ));
    let errs = validate_repository(&spec);
    let msg = format!("{errs:?}");
    assert!(!errs.is_empty(), "30d must be rejected");
    assert!(
        msg.contains("720h"),
        "must name the replacement value: {msg}"
    );

    // And the same i64-nanosecond ceiling the epoch durations carry.
    let spec = repo_yaml(&format!(
        "{S3_REPO_BASE}parameters:\n  blobRetention:\n    governance:\n      \
         period: \"999999999999999999\"\n"
    ));
    let errs = validate_repository(&spec);
    assert!(
        !errs.is_empty(),
        "a period beyond i64 nanoseconds is rejected"
    );
    assert!(format!("{errs:?}").contains("292 years"), "{errs:?}");
}

#[test]
fn blob_retention_is_rejected_on_backends_without_object_lock() {
    // On an unsupported backend `set-parameters` does NOT no-op — it hard-fails with
    // `blob-retention: unsupported put-blob option`, and the bootstrap re-runs it every
    // reconcile. Rejecting at admission is what stops that becoming a permanent Warning loop.
    let supported = [
        ("s3", "backend: { s3: { bucket: b } }\n"),
        (
            "azure",
            "backend: { azure: { container: c, storageAccount: a } }\n",
        ),
        ("gcs", "backend: { gcs: { bucket: b } }\n"),
    ];
    let unsupported = [
        ("filesystem", "backend: { filesystem: { path: /repo } }\n"),
        ("b2", "backend: { b2: { bucket: b } }\n"),
        (
            "sftp",
            "backend: { sftp: { host: h, path: /p, username: u } }\n",
        ),
        ("webdav", "backend: { webDav: { url: 'https://w/dav' } }\n"),
        ("rclone", "backend: { rclone: { remotePath: 'r:/p' } }\n"),
        ("gdrive", "backend: { gdrive: { folderId: fid } }\n"),
    ];
    let enc = "encryption: { passwordSecretRef: { name: s, key: KOPIA_PASSWORD } }\n";
    let params = "parameters:\n  blobRetention:\n    governance:\n      period: 720h\n";

    for (name, backend) in supported {
        let spec = repo_yaml(&format!("{backend}{enc}{params}"));
        assert!(
            validate_repository(&spec).is_empty(),
            "{name} supports object lock and must be accepted"
        );
    }
    for (name, backend) in unsupported {
        let spec = repo_yaml(&format!("{backend}{enc}{params}"));
        let errs = validate_repository(&spec);
        assert!(
            !errs.is_empty(),
            "{name} cannot object-lock and must be rejected"
        );
        assert!(
            format!("{errs:?}").contains("unsupported put-blob option"),
            "the message must carry kopia's verbatim error so a user can grep for it: {errs:?}"
        );
        // ...but the same backend WITHOUT blobRetention stays perfectly valid.
        let spec = repo_yaml(&format!("{backend}{enc}"));
        assert!(
            validate_repository(&spec).is_empty(),
            "{name} without blobRetention must not be taxed"
        );
    }
}

#[test]
fn a_read_only_repository_cannot_declare_blob_retention() {
    let spec = repo_yaml(&format!(
        "{S3_REPO_BASE}mode: ReadOnly\nparameters:\n  blobRetention:\n    \
         governance:\n      period: 720h\n"
    ));
    let errs = validate_repository(&spec);
    let msg = format!("{errs:?}");
    assert!(
        !errs.is_empty(),
        "ReadOnly + blobRetention must be rejected"
    );
    assert!(msg.contains("set-parameters"), "must say WHY: {msg}");
}

#[test]
fn cluster_repository_gets_the_identical_blob_retention_rules() {
    // The two kinds have fully duplicated reconcilers; a rule landing on only one of them
    // is how half an API surface ships as a silent no-op.
    let base = "backend: { s3: { bucket: b } }\n\
                encryption: { passwordSecretRef: { name: s, namespace: kopiur-system, key: KOPIA_PASSWORD } }\n\
                allowedNamespaces: { all: true }\n";
    let spec: ClusterRepositorySpec = crate::testutil::from_yaml(&format!(
        "{base}parameters:\n  blobRetention:\n    governance:\n      period: 1h\n"
    ));
    assert!(
        format!("{:?}", validate_cluster_repository(&spec)).contains("1-day"),
        "ClusterRepository must enforce the retention floor too"
    );

    let fs_base = "backend: { filesystem: { path: /repo } }\n\
                   encryption: { passwordSecretRef: { name: s, namespace: kopiur-system, key: KOPIA_PASSWORD } }\n\
                   allowedNamespaces: { all: true }\n";
    let spec: ClusterRepositorySpec = crate::testutil::from_yaml(&format!(
        "{fs_base}parameters:\n  blobRetention:\n    governance:\n      period: 720h\n"
    ));
    assert!(
        !validate_cluster_repository(&spec).is_empty(),
        "the backend gate must apply to ClusterRepository as well"
    );

    let spec: ClusterRepositorySpec = crate::testutil::from_yaml(&format!(
        "{base}parameters:\n  blobRetention:\n    governance:\n      period: 720h\n"
    ));
    assert!(validate_cluster_repository(&spec).is_empty());
}
