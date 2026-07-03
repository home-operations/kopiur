use super::*;
use crate::cluster_repository::{AllowedNamespaces, ClusterRepositorySpec};
use crate::common::{
    DeletionPolicy, FailurePolicy, Identity, RepositoryKind, RepositoryMode, RepositoryRef,
    Retention,
};
use crate::maintenance::RepositoryMaintenanceSpec;
use crate::repository::{RepositoryHealthSpec, RepositorySpec};
use crate::repository_replication::RepositoryReplicationSpec;
use crate::restore::{RestoreSource, RestoreSpec, RestoreTarget};
use crate::snapshot::{Origin, SnapshotSpec};
use crate::snapshot_policy::{Hook, SnapshotPolicySpec, Source};
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
            timeout: Some("30s".into()),
            continue_on_failure: true,
        })],
    });
    assert!(validate_backup_config(&spec).is_empty());
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

// --- validate_mover: inheritSecurityContextFrom XOR explicit (container OR pod) ---

#[test]
fn mover_inherit_is_mutually_exclusive_with_both_explicit_contexts() {
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

    // inherit + container securityContext → rejected.
    let with_container = MoverSpec {
        security_context: Some(SecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        }),
        inherit_security_context_from: inherit(),
        ..Default::default()
    };
    assert!(matches!(
        validate_mover(&with_container, "Restore mover"),
        Err(ValidationError::MutuallyExclusive { .. })
    ));

    // inherit + POD securityContext → also rejected (inherit copies the pod context too).
    let with_pod = MoverSpec {
        pod_security_context: Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
        inherit_security_context_from: inherit(),
        ..Default::default()
    };
    assert!(matches!(
        validate_mover(&with_pod, "Restore mover"),
        Err(ValidationError::MutuallyExclusive { .. })
    ));

    // Surfaced through the Restore validator.
    let mut spec = restore_with(
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        }),
        None,
    );
    spec.mover = Some(with_container);
    assert!(matches!(
        validate_restore(&spec),
        Err(ValidationError::MutuallyExclusive { .. })
    ));

    // inherit alone, or explicit container+pod together (no inherit), are both fine.
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
        server: None,
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
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
        }],
        copy_method: Default::default(),
        volume_snapshot_class_name: None,
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
    };
    assert!(validate_backup_config(&spec).is_empty());
}

#[test]
fn backup_aggregate_rejects_discovered_delete() {
    let spec = SnapshotSpec {
        policy_ref: None,
        tags: None,
        failure_policy: None,
        deletion_policy: Some(DeletionPolicy::Delete),
        pin: false,
    };
    let errs = validate_backup(&spec, Origin::Discovered);
    assert_eq!(errs.len(), 1);
    assert!(matches!(
        errs[0],
        ValidationError::DiscoveredMustRetain { .. }
    ));
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
    };
    let only_sel = SnapshotScheduleSpec {
        policy_ref: None,
        policy_selector: sel(),
        schedule: base_schedule(),
        failed_jobs_history_limit: None,
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
        server: None,
        maintenance: m,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
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
        credential_projection: None,
    };
    assert!(!validate_cluster_repository(&spec).is_empty());
}

#[test]
fn cluster_repository_rejects_bad_identity_expr() {
    use crate::backend::{Backend, FilesystemBackend};
    use crate::cluster_repository::IdentityDefaults;
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
            hostname_expr: Some("namspace".into()),
            username_expr: None,
        }),
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
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
        server: None,
        maintenance: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        suspend: false,
        health: None,
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
        destination_encryption: None,
        schedule: CronSpec {
            cron: cron.into(),
            jitter: None,
            timezone: None,
        },
        mover: None,
        suspend: false,
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
    // Same S3 bucket+prefix → same target.
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
            enabled: true,
            interval: Some("every-hour".to_string()),
            failure_threshold: None,
        }),
        ..Default::default()
    };
    let err = validate_repository_health(Some(&bad), "Repository").unwrap_err();
    assert!(err.to_string().contains("health.probe.interval"), "{err}");

    // Below the 30s floor → rejected.
    let too_fast = RepositoryHealthSpec {
        probe: Some(RepositoryHealthProbeSpec {
            enabled: true,
            interval: Some("5s".to_string()),
            failure_threshold: None,
        }),
        ..Default::default()
    };
    let err = validate_repository_health(Some(&too_fast), "ClusterRepository").unwrap_err();
    assert!(err.to_string().contains("30s minimum"), "{err}");

    // failureThreshold < 1 → rejected.
    let bad_threshold = RepositoryHealthSpec {
        probe: Some(RepositoryHealthProbeSpec {
            enabled: true,
            interval: None,
            failure_threshold: Some(0),
        }),
        ..Default::default()
    };
    let err = validate_repository_health(Some(&bad_threshold), "Repository").unwrap_err();
    assert!(err.to_string().contains("failureThreshold"), "{err}");

    // Valid probe (or omitted interval/threshold) is accepted.
    let ok = RepositoryHealthSpec {
        probe: Some(RepositoryHealthProbeSpec {
            enabled: true,
            interval: Some("30s".to_string()),
            failure_threshold: Some(3),
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
