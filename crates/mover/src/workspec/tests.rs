use super::*;

fn sample_identity() -> ResolvedIdentity {
    ResolvedIdentity {
        username: "mydb".into(),
        hostname: "prod".into(),
        source_path: "/pvc/mydb".into(),
    }
}

fn sample_target() -> TargetRef {
    TargetRef {
        api_version: "kopiur.home-operations.com/v1alpha1".into(),
        kind: "Snapshot".into(),
        name: "mydb-20260601".into(),
        namespace: "prod".into(),
    }
}

fn roundtrip(spec: &MoverWorkSpec) -> MoverWorkSpec {
    let json = serde_json::to_string_pretty(spec).unwrap();
    serde_json::from_str(&json).unwrap()
}

#[test]
fn backup_roundtrip() {
    let mut tags = BTreeMap::new();
    tags.insert("app".into(), "mydb".into());
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Snapshot(SnapshotOp {
            source_path: "/data".into(),
            tags,
            policy: Default::default(),
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: sample_target(),
        hook_plan: HookPlanSummary {
            pre: vec!["fsfreeze".into()],
            post: vec!["fsunfreeze".into()],
        },
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "Snapshot");
}

#[test]
fn restore_roundtrip() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Restore(RestoreOp {
            snapshot_id: "abc123".into(),
            target_path: "/data".into(),
            anchor: SnapshotAnchor {
                source_path: "/pvc/db".into(),
                start_time: Some("2026-06-19T05:54:19Z".into()),
            },
            ignore_permission_errors: Some(true),
            write_files_atomically: Some(false),
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::S3 {
            bucket: "backups".into(),
            endpoint: Some("https://minio.local".into()),
            prefix: Some("kopiur/".into()),
            region: None,
            disable_tls: false,
            disable_tls_verification: false,
            ambient_credentials: false,
        },
        target_ref: TargetRef {
            kind: "Restore".into(),
            ..sample_target()
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions {
            progress_interval_secs: 10,
            operation_timeout_secs: Some(3600),
        },
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "Restore");
}

#[test]
fn snapshot_delete_roundtrip() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::SnapshotDelete(SnapshotDeleteOp {
            snapshot_id: "todelete".into(),
            anchor: SnapshotAnchor::default(),
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: sample_target(),
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "SnapshotDelete");
}

#[test]
fn bootstrap_repository_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::BootstrapRepository(BootstrapRepositoryOp {
            auto_create: true,
            scan_catalog: true,
            create_options: Default::default(),
            maintenance_owner: Some("kopiur@kopiur-ns-repo".into()),
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::S3 {
            bucket: "b".into(),
            endpoint: Some("minio:9000".into()),
            prefix: None,
            region: None,
            disable_tls: true,
            disable_tls_verification: false,
            ambient_credentials: false,
        },
        target_ref: TargetRef {
            kind: "Repository".into(),
            ..sample_target()
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "BootstrapRepository");
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    // Externally tagged: { "bootstrapRepository": { "autoCreate": true, ... } }.
    assert_eq!(v["operation"]["bootstrapRepository"]["autoCreate"], true);
    assert_eq!(v["operation"]["bootstrapRepository"]["scanCatalog"], true);
    // S3 disable-tls flows on the wire (camelCase, omitted when false).
    assert_eq!(v["repository"]["s3"]["disableTls"], true);
    assert!(
        v["repository"]["s3"]
            .get("disableTlsVerification")
            .is_none()
    );
}

#[test]
fn maintenance_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Maintenance(MaintenanceOp {
            mode: kopiur_kopia::MaintenanceMode::Full,
            owner: "kopiur/prod/nas-primary".into(),
            takeover_policy: kopiur_api::TakeoverPolicy::Force,
        }),
        identity: ResolvedIdentity {
            username: "kopiur-maintenance".into(),
            hostname: "prod".into(),
            source_path: String::new(),
        },
        repository: RepositoryConnect::S3 {
            bucket: "b".into(),
            endpoint: Some("minio:9000".into()),
            prefix: None,
            region: None,
            disable_tls: true,
            disable_tls_verification: false,
            ambient_credentials: false,
        },
        target_ref: TargetRef {
            kind: "Maintenance".into(),
            ..sample_target()
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "Maintenance");
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    // Externally tagged: { "maintenance": { "mode": "full", "owner": ... } }.
    assert_eq!(v["operation"]["maintenance"]["mode"], "full");
    assert_eq!(
        v["operation"]["maintenance"]["owner"],
        "kopiur/prod/nas-primary"
    );
    assert_eq!(v["operation"]["maintenance"]["takeoverPolicy"], "Force");
}

#[test]
fn browse_session_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::BrowseSession(BrowseSessionOp { ttl_seconds: 1800 }),
        identity: sample_identity(),
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: sample_target(),
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "BrowseSession");
    // Externally tagged on the wire: { "browseSession": { "ttlSeconds": 1800 } }.
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    assert_eq!(v["operation"]["browseSession"]["ttlSeconds"], 1800);
}

#[test]
fn browse_session_ttl_defaults_to_15_minutes() {
    // An omitted ttlSeconds deserializes to the 900s default — the wire
    // contract a CLI that sends `{"browseSession": {}}` relies on.
    let op: BrowseSessionOp = serde_json::from_str("{}").unwrap();
    assert_eq!(op.ttl_seconds, 900);
    assert_eq!(BrowseSessionOp::default().ttl_seconds, 900);
}

#[test]
fn externally_tagged_operation_shape() {
    // Assert the wire shape is externally tagged: { "snapshot": {...} }.
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Snapshot(SnapshotOp {
            source_path: "/data".into(),
            tags: BTreeMap::new(),
            policy: Default::default(),
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: sample_target(),
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    assert!(v["operation"]["snapshot"].is_object());
    assert!(v["operation"]["snapshot"]["sourcePath"].is_string());
    // Repository is externally tagged too.
    assert!(v["repository"]["filesystem"]["path"].is_string());
}

#[test]
fn defaults_fill_in_when_absent() {
    // A minimal spec: omit version, hookPlan, options entirely.
    let json = r#"{
        "operation": {"snapshotDelete": {"snapshotId": "x"}},
        "identity": {"username": "u", "hostname": "h", "sourcePath": "/p"},
        "repository": {"filesystem": {"path": "/repo"}},
        "targetRef": {"apiVersion": "kopiur.home-operations.com/v1alpha1", "kind": "Snapshot", "name": "n", "namespace": "ns"}
    }"#;
    let spec: MoverWorkSpec = serde_json::from_str(json).unwrap();
    assert_eq!(spec.version, 1);
    assert_eq!(spec.options.progress_interval_secs, 5);
    assert_eq!(spec.options.operation_timeout_secs, None);
    assert!(spec.hook_plan.pre.is_empty());
}

#[test]
fn connect_spec_conversion() {
    let fs = RepositoryConnect::Filesystem {
        path: "/repo".into(),
    };
    assert_eq!(
        fs.to_connect_spec(),
        kopiur_kopia::ConnectSpec::Filesystem {
            path: "/repo".into()
        }
    );
    let s3 = RepositoryConnect::S3 {
        bucket: "b".into(),
        endpoint: None,
        prefix: None,
        region: Some("r".into()),
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
    };
    assert_eq!(
        s3.to_connect_spec(),
        kopiur_kopia::ConnectSpec::S3 {
            bucket: "b".into(),
            endpoint: None,
            prefix: None,
            region: Some("r".into()),
            disable_tls: false,
            disable_tls_verification: false,
            ambient_credentials: false,
        }
    );
}

#[test]
fn object_store_backends_convert_and_roundtrip() {
    use kopiur_kopia::ConnectSpec;
    // One representative per non-trivial backend: assert both the wire
    // round-trip and the conversion to the kopia client spec.
    let cases: Vec<(RepositoryConnect, ConnectSpec)> = vec![
        (
            RepositoryConnect::Azure {
                container: "c".into(),
                storage_account: Some("acct".into()),
                prefix: None,
            },
            ConnectSpec::Azure {
                container: "c".into(),
                storage_account: Some("acct".into()),
                prefix: None,
            },
        ),
        (
            RepositoryConnect::Gcs {
                bucket: "b".into(),
                prefix: Some("p/".into()),
            },
            ConnectSpec::Gcs {
                bucket: "b".into(),
                prefix: Some("p/".into()),
                credentials_file: None,
            },
        ),
        (
            RepositoryConnect::B2 {
                bucket: "b".into(),
                prefix: None,
            },
            ConnectSpec::B2 {
                bucket: "b".into(),
                prefix: None,
            },
        ),
        (
            RepositoryConnect::Sftp {
                host: "h".into(),
                path: "/r".into(),
                port: Some(2222),
                username: Some("u".into()),
                keyfile: Some("/k".into()),
            },
            ConnectSpec::Sftp {
                host: "h".into(),
                path: "/r".into(),
                port: Some(2222),
                username: Some("u".into()),
                keyfile: Some("/k".into()),
                known_hosts: None,
            },
        ),
        (
            RepositoryConnect::WebDav {
                url: "https://dav".into(),
            },
            ConnectSpec::WebDav {
                url: "https://dav".into(),
            },
        ),
        (
            RepositoryConnect::Rclone {
                remote_path: "r:bucket".into(),
            },
            ConnectSpec::Rclone {
                remote_path: "r:bucket".into(),
                config_file: None,
            },
        ),
    ];
    for (wire, expected_spec) in cases {
        // Wire round-trip (externally tagged, camelCase).
        let json = serde_json::to_string(&wire).unwrap();
        let back: RepositoryConnect = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wire, "round-trip for {json}");
        // Conversion to the kopia client spec.
        assert_eq!(wire.to_connect_spec(), expected_spec);
    }
}

#[test]
fn restore_op_maps_options_and_defaults_absent() {
    // Options present → mapped onto the kopia client options.
    let op = RestoreOp {
        snapshot_id: "s".into(),
        target_path: "/data".into(),
        anchor: SnapshotAnchor::default(),
        ignore_permission_errors: Some(false),
        write_files_atomically: Some(true),
    };
    let opts = op.restore_options();
    assert_eq!(opts.ignore_permission_errors, Some(false));
    assert_eq!(opts.write_files_atomically, Some(true));

    // Older wire payload without the option/anchor fields still deserializes
    // (forward/backward compatible), mapping to kopia defaults (None/empty).
    let json = r#"{"snapshotId":"s","targetPath":"/data"}"#;
    let parsed: RestoreOp = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.ignore_permission_errors, None);
    assert_eq!(parsed.restore_options().write_files_atomically, None);
    assert!(parsed.anchor.is_empty());
}

#[test]
fn azure_wire_shape_is_external_camel_case() {
    let wire = RepositoryConnect::Azure {
        container: "c".into(),
        storage_account: Some("acct".into()),
        prefix: None,
    };
    let v: serde_json::Value = serde_json::to_value(&wire).unwrap();
    assert!(v["azure"]["container"].is_string());
    assert_eq!(v["azure"]["storageAccount"], "acct");
    // prefix omitted when None.
    assert!(v["azure"].get("prefix").is_none());
}

#[test]
fn s3_ambient_credentials_roundtrips_and_defaults_false() {
    // Workload identity travels the wire as `ambientCredentials: true` and
    // reaches the kopia spec.
    let wire = RepositoryConnect::S3 {
        bucket: "b".into(),
        endpoint: None,
        prefix: None,
        region: None,
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: true,
    };
    let v: serde_json::Value = serde_json::to_value(&wire).unwrap();
    assert_eq!(v["s3"]["ambientCredentials"], true);
    let back: RepositoryConnect = serde_json::from_value(v).unwrap();
    assert_eq!(back, wire);
    match back.to_connect_spec() {
        kopiur_kopia::ConnectSpec::S3 {
            ambient_credentials,
            ..
        } => assert!(ambient_credentials),
        other => panic!("expected S3, got {other:?}"),
    }

    // Back-compat: a work-spec ConfigMap written before the field existed
    // (no `ambientCredentials` key) still parses, defaulting to static keys.
    let legacy = serde_json::json!({ "s3": { "bucket": "b" } });
    let parsed: RepositoryConnect = serde_json::from_value(legacy).unwrap();
    match &parsed {
        RepositoryConnect::S3 {
            ambient_credentials,
            ..
        } => assert!(!ambient_credentials),
        other => panic!("expected S3, got {other:?}"),
    }
    // And `false` stays off the wire, so legacy movers can read new specs too.
    let v = serde_json::to_value(&parsed).unwrap();
    assert!(v["s3"].get("ambientCredentials").is_none());
}

// --- §13(b)/§13(f) policy-args mapping (api spec → work-spec → kopia args) ---

#[test]
fn policy_args_from_policy_maps_all_flattened_knobs() {
    use kopiur_api::snapshot_policy::{Compression, ErrorHandling, Files, Upload};
    let spec = kopiur_api::SnapshotPolicySpec {
        repository: kopiur_api::common::RepositoryRef {
            kind: Default::default(),
            name: "r".into(),
            namespace: None,
        },
        identity: None,
        sources: vec![],
        copy_method: Default::default(),
        volume_snapshot_class_name: None,
        group_by: None,
        retention: None,
        default_deletion_policy: None,
        compression: Some(Compression {
            compressor: Some("zstd".into()),
            never_compress: vec!["*.mp4".into()],
        }),
        files: Some(Files {
            ignore_rules: vec!["*.tmp".into(), "*/cache/*".into()],
            ignore_cache_dirs: true,
            ignore_identical_snapshots: false,
        }),
        extra_args: vec!["--one-file-system".into()],
        error_handling: Some(ErrorHandling {
            ignore_file_errors: true,
            ignore_dir_errors: false,
            ignore_unknown_types: true,
        }),
        upload: Some(Upload {
            max_parallel_snapshots: Some(4),
            max_parallel_file_reads: Some(8),
        }),
        verification: None,
        suspend: false,
        hooks: None,
        mover: None,
        credential_projection: None,
    };
    let p = PolicyArgsSpec::from_policy(&spec);
    assert_eq!(p.compression.as_deref(), Some("zstd"));
    assert_eq!(p.never_compress, vec!["*.mp4".to_string()]);
    assert_eq!(p.ignore, vec!["*.tmp".to_string(), "*/cache/*".to_string()]);
    assert_eq!(p.ignore_cache_dirs, Some(true));
    assert_eq!(p.ignore_file_errors, Some(true));
    // false bools don't emit a flag (leave kopia's default), so they map to None.
    assert_eq!(p.ignore_dir_errors, None);
    assert_eq!(p.ignore_unknown_types, Some(true));
    assert_eq!(p.max_parallel_snapshots, Some(4));
    assert_eq!(p.max_parallel_file_reads, Some(8));
    assert_eq!(p.extra_args, vec!["--one-file-system".to_string()]);
    assert!(!p.is_empty());

    // The kopia args builder emits the expected flags (end-to-end into argv).
    let args = p.to_kopia();
    assert_eq!(args.compression.as_deref(), Some("zstd"));
    assert_eq!(args.ignore_file_errors, Some(true));
    assert_eq!(args.max_parallel_snapshots, Some(4));
    // No per-policy splitter (ADR-0004 §4b).
    assert_eq!(args.splitter, None);
}

#[test]
fn policy_args_from_empty_policy_is_empty() {
    let spec = kopiur_api::SnapshotPolicySpec {
        repository: kopiur_api::common::RepositoryRef {
            kind: Default::default(),
            name: "r".into(),
            namespace: None,
        },
        identity: None,
        sources: vec![],
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
        suspend: false,
        hooks: None,
        mover: None,
        credential_projection: None,
    };
    assert!(PolicyArgsSpec::from_policy(&spec).is_empty());
}

// --- §13(e) throttle mapping ---

#[test]
fn throttle_from_mover_defaults_maps_and_empties() {
    use kopiur_api::common::{MoverDefaults, Throttle};
    let defaults = MoverDefaults {
        throttle: Some(Throttle {
            upload_bytes_per_second: Some(5_000_000),
            download_bytes_per_second: None,
            read_ops_per_second: Some(20),
            write_ops_per_second: None,
        }),
        ..Default::default()
    };
    let t = ThrottleSpec::from_mover_defaults(Some(&defaults));
    assert_eq!(t.upload_bytes_per_second, Some(5_000_000));
    assert_eq!(t.read_ops_per_second, Some(20));
    assert!(!t.is_empty());
    let args = t.to_kopia().args();
    assert!(
        args.windows(2)
            .any(|w| w == ["--upload-bytes-per-second", "5000000"])
    );

    // No throttle ⇒ empty (mover skips the call).
    assert!(ThrottleSpec::from_mover_defaults(None).is_empty());
    assert!(ThrottleSpec::from_mover_defaults(Some(&MoverDefaults::default())).is_empty());
}

// --- §13(a) create-options (ECC) mapping ---

#[test]
fn create_options_from_create_maps_ecc_and_algos() {
    use kopiur_api::common::{CreateBehavior, Ecc};
    let create = CreateBehavior {
        enabled: true,
        encryption: Some("AES256-GCM-HMAC-SHA256".into()),
        splitter: Some("DYNAMIC-4M-BUZHASH".into()),
        hash: Some("BLAKE2B-256".into()),
        ecc: Some(Ecc {
            algorithm: Some("REED-SOLOMON-CRC32".into()),
            overhead_percent: Some(2),
        }),
    };
    let c = CreateOptionsSpec::from_create(Some(&create));
    assert_eq!(c.encryption.as_deref(), Some("AES256-GCM-HMAC-SHA256"));
    assert_eq!(c.ecc.as_deref(), Some("REED-SOLOMON-CRC32"));
    assert_eq!(c.ecc_overhead_percent, Some(2));
    assert!(!c.is_empty());
    // Args reach kopia's `repository create` flags.
    let args = c.to_kopia().args();
    assert!(
        args.windows(2)
            .any(|w| w == ["--ecc", "REED-SOLOMON-CRC32"])
    );
    assert!(
        args.windows(2)
            .any(|w| w == ["--ecc-overhead-percent", "2"])
    );

    // Absent ⇒ empty.
    assert!(CreateOptionsSpec::from_create(None).is_empty());
}

// --- §13(c) snapshot-pin op ---

#[test]
fn snapshot_pin_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::SnapshotPin(SnapshotPinOp {
            snapshot_id: "k123".into(),
            pin: true,
            anchor: SnapshotAnchor {
                source_path: "/pvc/db".into(),
                start_time: Some("2026-06-19T05:54:19Z".into()),
            },
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: sample_target(),
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "SnapshotPin");
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    assert_eq!(v["operation"]["snapshotPin"]["snapshotId"], "k123");
    assert_eq!(v["operation"]["snapshotPin"]["pin"], true);
    // Anchors flow externally as camelCase under the op payload.
    assert_eq!(
        v["operation"]["snapshotPin"]["anchor"]["sourcePath"],
        "/pvc/db"
    );
    // An old work spec without the anchor still deserializes (defaulted).
    let legacy = r#"{"snapshotId":"k123","pin":false}"#;
    let parsed: SnapshotPinOp = serde_json::from_str(legacy).unwrap();
    assert!(parsed.anchor.is_empty());
}

// --- §4 verify op ---

#[test]
fn verify_quick_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Verify(VerifyOp {
            tier: VerifyTier::Quick(QuickVerify {
                verify_files_percent: Some(10),
                max_errors: Some(3),
                parallel: None,
            }),
            success_expr: Some("stats.files > 0 && stats.errors == 0".into()),
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::S3 {
            bucket: "b".into(),
            endpoint: None,
            prefix: None,
            region: None,
            disable_tls: false,
            disable_tls_verification: false,
            ambient_credentials: false,
        },
        target_ref: TargetRef {
            kind: "SnapshotPolicy".into(),
            ..sample_target()
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "Verify");
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    // Externally tagged tier: { "verify": { "tier": { "quick": {...} } } }.
    assert_eq!(
        v["operation"]["verify"]["tier"]["quick"]["verifyFilesPercent"],
        10
    );
    assert_eq!(
        v["operation"]["verify"]["successExpr"],
        "stats.files > 0 && stats.errors == 0"
    );
    // The quick tier maps to the kopia client VerifyOptions.
    if let Operation::Verify(op) = &spec.operation {
        assert_eq!(op.tier.kind_str(), "quick");
        if let VerifyTier::Quick(q) = &op.tier {
            let kopia = q.to_kopia();
            assert_eq!(kopia.verify_files_percent, Some(10));
            assert_eq!(kopia.max_errors, Some(3));
        } else {
            panic!("expected quick tier");
        }
    } else {
        panic!("expected verify op");
    }
}

#[test]
fn verify_deep_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Verify(VerifyOp {
            tier: VerifyTier::Deep(DeepVerify {
                scratch_path: "/scratch".into(),
                snapshot_id: Some("k99".into()),
            }),
            success_expr: None,
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: TargetRef {
            kind: "SnapshotPolicy".into(),
            ..sample_target()
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        v["operation"]["verify"]["tier"]["deep"]["scratchPath"],
        "/scratch"
    );
    assert_eq!(
        v["operation"]["verify"]["tier"]["deep"]["snapshotId"],
        "k99"
    );
    if let Operation::Verify(op) = &spec.operation {
        assert_eq!(op.tier.kind_str(), "deep");
    } else {
        panic!("expected verify op");
    }
}

// --- §13(d) replicate op ---

#[test]
fn replicate_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Replicate(ReplicateOp {
            destination: RepositoryConnect::S3 {
                bucket: "mirror".into(),
                endpoint: Some("https://offsite".into()),
                prefix: None,
                region: Some("us-east-1".into()),
                disable_tls: false,
                disable_tls_verification: false,
                ambient_credentials: false,
            },
            delete_extra: true,
        }),
        // The source repository the mover connects to.
        identity: ResolvedIdentity {
            username: "kopiur-replication".into(),
            hostname: "prod".into(),
            source_path: String::new(),
        },
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: TargetRef {
            kind: "RepositoryReplication".into(),
            ..sample_target()
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "Replicate");
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    // Externally tagged: { "replicate": { "destination": { "s3": {...} }, ... } }.
    assert_eq!(
        v["operation"]["replicate"]["destination"]["s3"]["bucket"],
        "mirror"
    );
    assert_eq!(v["operation"]["replicate"]["deleteExtra"], true);
    // The destination converts to the kopia client connect spec.
    if let Operation::Replicate(op) = &spec.operation {
        assert_eq!(
            op.destination.to_connect_spec(),
            kopiur_kopia::ConnectSpec::S3 {
                bucket: "mirror".into(),
                endpoint: Some("https://offsite".into()),
                prefix: None,
                region: Some("us-east-1".into()),
                disable_tls: false,
                disable_tls_verification: false,
                ambient_credentials: false,
            }
        );
    } else {
        panic!("expected replicate op");
    }
}
