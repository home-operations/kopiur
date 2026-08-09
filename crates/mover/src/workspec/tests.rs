use super::*;

fn sample_identity() -> ResolvedIdentity {
    ResolvedIdentity {
        username: "mydb".into(),
        hostname: "prod".into(),
        source_path: "/pvc/mydb".into(),
    }
}

#[test]
fn resolved_identity_source_spec_is_user_host_path() {
    // The `username@hostname:path` form is what a snapshot is recorded under and
    // what `snapshot verify --sources` matches against (issue #250), so it must
    // reassemble the three components in exactly that shape.
    assert_eq!(sample_identity().source_spec(), "mydb@prod:/pvc/mydb");
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
            fail_fast: None,
            upload_limit_mb: None,
            description: None,
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
fn snapshot_op_create_knobs_roundtrip_wire_shape_and_map_to_kopia() {
    // M4 flag sweep (issue #216 category sweep): failFast/uploadLimitMb/
    // description round-trip on the wire, serialize under their camelCase
    // names, and `create_options()` carries them into the kopia client's
    // `SnapshotCreateOptions` unchanged.
    let op = SnapshotOp {
        source_path: "/data".into(),
        tags: BTreeMap::new(),
        policy: Default::default(),
        fail_fast: Some(true),
        upload_limit_mb: Some(250),
        description: Some("pre-upgrade snapshot".into()),
    };
    let json = serde_json::to_value(&op).unwrap();
    assert_eq!(json["failFast"], true);
    assert_eq!(json["uploadLimitMb"], 250);
    assert_eq!(json["description"], "pre-upgrade snapshot");
    let reparsed: SnapshotOp = serde_json::from_value(json).unwrap();
    assert_eq!(reparsed, op);
    assert_eq!(
        op.create_options(),
        kopiur_kopia::SnapshotCreateOptions {
            fail_fast: Some(true),
            upload_limit_mb: Some(250),
            description: Some("pre-upgrade snapshot".into()),
        }
    );
}

#[test]
fn snapshot_op_old_wire_decodes_with_m4_fields_defaulted() {
    // A work-spec ConfigMap written before M4 (just sourcePath/tags/policy)
    // must still decode — `#[serde(default)]` on every new field is the
    // guard, and the all-None result reproduces the pre-M4 `snapshot create`
    // argv exactly (SnapshotCreateOptions::default()).
    let legacy = serde_json::json!({
        "sourcePath": "/data",
        "tags": {"app": "mydb"},
    });
    let op: SnapshotOp = serde_json::from_value(legacy).expect("legacy snapshot op decodes");
    assert_eq!(op.fail_fast, None);
    assert_eq!(op.upload_limit_mb, None);
    assert_eq!(op.description, None);
    assert_eq!(
        op.create_options(),
        kopiur_kopia::SnapshotCreateOptions::default()
    );
}

#[test]
fn restore_roundtrip() {
    let spec = MoverWorkSpec {
        version: 2,
        operation: Operation::Restore(RestoreOp {
            source: RestoreSelection::Snapshot("abc123".into()),
            target_path: "/data".into(),
            anchor: SnapshotAnchor {
                source_path: "/pvc/db".into(),
                start_time: Some("2026-06-19T05:54:19Z".into()),
                username: None,
                hostname: None,
            },
            ignore_permission_errors: Some(true),
            write_files_atomically: Some(false),
            parallel: Some(4),
            write_sparse_files: Some(true),
            skip_owners: Some(false),
            skip_permissions: Some(true),
            skip_times: Some(false),
            overwrite_files: Some(true),
            overwrite_directories: Some(false),
            overwrite_symlinks: Some(true),
            ignore_errors: Some(false),
            skip_existing: Some(true),
            delete_extra: true,
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
            ca_bundle_pem: None,
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
    // The externally-tagged source serializes under its own camelCase key.
    let v = serde_json::to_value(&spec).unwrap();
    assert_eq!(v["operation"]["restore"]["source"]["snapshot"], "abc123");
    // M2 flag sweep: every new leaf field reaches the wire, camelCased.
    let restore = &v["operation"]["restore"];
    assert_eq!(restore["parallel"], 4);
    assert_eq!(restore["writeSparseFiles"], true);
    assert_eq!(restore["skipOwners"], false);
    assert_eq!(restore["skipPermissions"], true);
    assert_eq!(restore["skipTimes"], false);
    assert_eq!(restore["overwriteFiles"], true);
    assert_eq!(restore["overwriteDirectories"], false);
    assert_eq!(restore["overwriteSymlinks"], true);
    assert_eq!(restore["ignoreErrors"], false);
    assert_eq!(restore["skipExisting"], true);
    assert_eq!(restore["deleteExtra"], true);
    // Controller-glue guard: every field reaches the kopia client's options —
    // no dormant plumbing (the M2 gap-sweep bug class).
    if let Operation::Restore(op) = &spec.operation {
        assert_eq!(
            op.restore_options(),
            kopiur_kopia::RestoreOptions {
                ignore_permission_errors: Some(true),
                write_files_atomically: Some(false),
                parallel: Some(4),
                write_sparse_files: Some(true),
                skip_owners: Some(false),
                skip_permissions: Some(true),
                skip_times: Some(false),
                overwrite_files: Some(true),
                overwrite_directories: Some(false),
                overwrite_symlinks: Some(true),
                ignore_errors: Some(false),
                skip_existing: Some(true),
                delete_extra: Some(true),
            }
        );
    } else {
        panic!("expected restore op");
    }
}

#[test]
fn restore_resolve_source_roundtrips_and_wire_shape() {
    // The in-Job (object-store) path: an unresolved selector instead of an id.
    let spec = MoverWorkSpec {
        version: 2,
        operation: Operation::Restore(RestoreOp {
            source: RestoreSelection::Resolve(RestoreSelector {
                username: "restore".into(),
                hostname: "prod".into(),
                source_path: Some("/pvc/db".into()),
                as_of: Some("2026-06-19T05:54:19Z".into()),
                offset: 0,
                on_missing: kopiur_api::restore::OnMissingSnapshot::Continue,
                wait_deadline: Some("2026-06-19T06:00:00Z".into()),
            }),
            target_path: "/data".into(),
            anchor: SnapshotAnchor::default(),
            ignore_permission_errors: None,
            write_files_atomically: None,
            parallel: None,
            write_sparse_files: None,
            skip_owners: None,
            skip_permissions: None,
            skip_times: None,
            overwrite_files: None,
            overwrite_directories: None,
            overwrite_symlinks: None,
            ignore_errors: None,
            skip_existing: None,
            delete_extra: false,
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::S3 {
            bucket: "backups".into(),
            endpoint: None,
            prefix: None,
            region: None,
            disable_tls: false,
            disable_tls_verification: false,
            ambient_credentials: false,
            ca_bundle_pem: None,
        },
        target_ref: TargetRef {
            kind: "Restore".into(),
            ..sample_target()
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    let v = serde_json::to_value(&spec).unwrap();
    let sel = &v["operation"]["restore"]["source"]["resolve"];
    assert_eq!(sel["username"], "restore");
    assert_eq!(sel["offset"], 0);
    assert_eq!(sel["onMissing"], "Continue");
    assert_eq!(sel["waitDeadline"], "2026-06-19T06:00:00Z");
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
fn snapshot_delete_legacy_wire_still_decodes_after_batch_variant_added() {
    // Wire-shape guard: adding the sibling `SnapshotDeleteBatch` Operation
    // variant must not perturb `SnapshotDelete`'s existing external tag/shape
    // — an in-flight `{name}-delete` Job spawned by an older controller
    // during an operator upgrade must still decode against the new mover
    // image.

    // The oldest wire shape: no anchor key at all (pre-anchor Jobs).
    let bare = serde_json::json!({
        "snapshotDelete": { "snapshotId": "todelete" }
    });
    let op: Operation = serde_json::from_value(bare).expect("bare SnapshotDelete decodes");
    assert_eq!(op.kind_str(), "SnapshotDelete");
    match op {
        Operation::SnapshotDelete(SnapshotDeleteOp {
            snapshot_id,
            anchor,
        }) => {
            assert_eq!(snapshot_id, "todelete");
            assert_eq!(anchor, SnapshotAnchor::default());
        }
        other => panic!("expected SnapshotDelete, got {other:?}"),
    }

    // Today's exact wire shape: snapshotId + a populated anchor.
    let current = serde_json::json!({
        "snapshotDelete": {
            "snapshotId": "todelete",
            "anchor": {
                "sourcePath": "/pvc/db",
                "startTime": "2026-06-19T05:54:19Z",
            }
        }
    });
    let op: Operation = serde_json::from_value(current).expect("current SnapshotDelete decodes");
    assert_eq!(op.kind_str(), "SnapshotDelete");
    match op {
        Operation::SnapshotDelete(SnapshotDeleteOp {
            snapshot_id,
            anchor,
        }) => {
            assert_eq!(snapshot_id, "todelete");
            assert_eq!(anchor.source_path, "/pvc/db");
            assert_eq!(anchor.start_time.as_deref(), Some("2026-06-19T05:54:19Z"));
        }
        other => panic!("expected SnapshotDelete, got {other:?}"),
    }
}

#[test]
fn snapshot_delete_batch_roundtrip() {
    let spec = MoverWorkSpec {
        version: 2,
        operation: Operation::SnapshotDeleteBatch(SnapshotDeleteBatchOp {
            items: vec![
                SnapshotDeleteItem {
                    snapshot_id: "a1".into(),
                    anchor: SnapshotAnchor {
                        source_path: "/pvc/db".into(),
                        start_time: Some("2026-06-19T05:54:19Z".into()),
                        username: Some("mydb".into()),
                        hostname: Some("prod".into()),
                    },
                },
                SnapshotDeleteItem {
                    snapshot_id: "a2".into(),
                    anchor: SnapshotAnchor::default(),
                },
            ],
        }),
        identity: sample_identity(),
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: TargetRef {
            kind: "SnapshotDeleteBatch".into(),
            ..sample_target()
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "SnapshotDeleteBatch");
    // Externally tagged: { "snapshotDeleteBatch": { "items": [...] } }, camelCase.
    let v = serde_json::to_value(&spec).unwrap();
    let items = &v["operation"]["snapshotDeleteBatch"]["items"];
    assert_eq!(items[0]["snapshotId"], "a1");
    assert_eq!(items[0]["anchor"]["startTime"], "2026-06-19T05:54:19Z");
    assert_eq!(items[0]["anchor"]["username"], "mydb");
    assert_eq!(items[1]["snapshotId"], "a2");
}

#[test]
fn snapshot_delete_batch_item_omits_empty_anchor() {
    // Wire-shape guard: an item with no anchor must elide the key entirely
    // (mirrors SnapshotDeleteOp's own idiom), not serialize `"anchor":{}`.
    let item = SnapshotDeleteItem {
        snapshot_id: "a1".into(),
        anchor: SnapshotAnchor::default(),
    };
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(v["snapshotId"], "a1");
    assert!(
        v.get("anchor").is_none(),
        "an empty anchor must be omitted from the wire, not emitted as {{}}"
    );
    // And an item WITH an anchor keeps it.
    let with_anchor = SnapshotDeleteItem {
        snapshot_id: "a2".into(),
        anchor: SnapshotAnchor {
            source_path: "/pvc/db".into(),
            ..Default::default()
        },
    };
    let v2 = serde_json::to_value(&with_anchor).unwrap();
    assert_eq!(v2["anchor"]["sourcePath"], "/pvc/db");
}

#[test]
fn bootstrap_repository_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::BootstrapRepository(BootstrapRepositoryOp {
            auto_create: true,
            scan_catalog: true,
            create_options: Default::default(),
            epoch_parameters: Default::default(),
            blob_retention: None,
            maintenance_owner: Some("kopiur@kopiur-ns-repo".into()),
            catalog_foreign_prefilter_cluster: Some("east".into()),
            restamp_policy: RestampPolicy::OwnFormatsOnly,
            maintenance_owner_aliases: vec!["kopiur@kopiur-ns-repo-legacy".into()],
            read_only: true,
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
            ca_bundle_pem: None,
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
    assert_eq!(
        v["operation"]["bootstrapRepository"]["catalogForeignPrefilterCluster"],
        "east"
    );
    assert_eq!(
        v["operation"]["bootstrapRepository"]["restampPolicy"],
        "ownFormatsOnly"
    );
    assert_eq!(
        v["operation"]["bootstrapRepository"]["maintenanceOwnerAliases"][0],
        "kopiur@kopiur-ns-repo-legacy"
    );
    assert_eq!(v["operation"]["bootstrapRepository"]["readOnly"], true);
    // S3 disable-tls flows on the wire (camelCase, omitted when false).
    assert_eq!(v["repository"]["s3"]["disableTls"], true);
    assert!(
        v["repository"]["s3"]
            .get("disableTlsVerification")
            .is_none()
    );
}

#[test]
fn bootstrap_repository_old_wire_json_without_the_prefilter_field_still_decodes() {
    // Pre-M4 work-spec ConfigMaps never carried this key at all — a controller
    // upgraded ahead of a mover (or vice versa) must not wedge.
    let old = r#"{"autoCreate":true,"scanCatalog":true}"#;
    let parsed: BootstrapRepositoryOp = serde_json::from_str(old).unwrap();
    assert!(parsed.auto_create);
    assert!(parsed.scan_catalog);
    assert!(parsed.maintenance_owner.is_none());
    assert!(parsed.catalog_foreign_prefilter_cluster.is_none());
    // M6 compat: pre-M6 work specs never carried these keys either — they must
    // decode to the pre-M6 behavior (AnyStale, no aliases, read-write connect).
    assert_eq!(parsed.restamp_policy, RestampPolicy::AnyStale);
    assert!(parsed.maintenance_owner_aliases.is_empty());
    assert!(!parsed.read_only);
    // #258 compat: a work spec written before spec.parameters existed carries no
    // epochParameters key, and must decode to "declare nothing" — NOT to a set of
    // defaults that would then be applied to the repository.
    assert!(parsed.epoch_parameters.is_empty());
}

#[test]
fn bootstrap_repository_new_wire_json_round_trips_to_old_shape_when_unset() {
    // New→old direction: when the M6 fields are at their defaults, they must
    // not appear on the wire at all, so an OLD mover (that has never heard of
    // them) still parses the JSON a NEW controller writes.
    let op = BootstrapRepositoryOp {
        auto_create: true,
        scan_catalog: true,
        create_options: Default::default(),
        epoch_parameters: Default::default(),
        blob_retention: None,
        maintenance_owner: None,
        catalog_foreign_prefilter_cluster: None,
        restamp_policy: RestampPolicy::AnyStale,
        maintenance_owner_aliases: Vec::new(),
        read_only: false,
    };
    let v = serde_json::to_value(&op).unwrap();
    assert!(v.get("maintenanceOwnerAliases").is_none());
    assert!(v.get("readOnly").is_none());
    // #258: same contract. A repository that declares no spec.parameters must put no
    // epochParameters key on the wire at all.
    assert!(v.get("epochParameters").is_none());
    // restampPolicy has no `skip_serializing_if` (it's not carried as absent
    // vs. present the way an Option/Vec is) — it always serializes, but at its
    // default value, which an old mover that has never heard of the key
    // would simply never read (it decodes what it recognizes and ignores the
    // rest), so this is still forward/backward compatible in practice.
    assert_eq!(v["restampPolicy"], "anyStale");
}

#[test]
fn maintenance_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Maintenance(MaintenanceOp {
            mode: kopiur_kopia::MaintenanceMode::Full,
            owner: "kopiur/prod/nas-primary".into(),
            owner_aliases: vec!["kopiur/prod-legacy/nas-primary".into()],
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
            ca_bundle_pem: None,
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
    assert_eq!(
        v["operation"]["maintenance"]["ownerAliases"][0],
        "kopiur/prod-legacy/nas-primary"
    );
    assert_eq!(v["operation"]["maintenance"]["takeoverPolicy"], "Force");
}

#[test]
fn maintenance_op_old_wire_json_without_owner_aliases_still_decodes() {
    // Pre-M6 work-spec ConfigMaps never carried `ownerAliases` at all.
    let old = r#"{"mode":"quick","owner":"kopiur/prod/nas-primary"}"#;
    let parsed: MaintenanceOp = serde_json::from_str(old).unwrap();
    assert_eq!(parsed.owner, "kopiur/prod/nas-primary");
    assert!(parsed.owner_aliases.is_empty());
    assert_eq!(parsed.takeover_policy, kopiur_api::TakeoverPolicy::Never);
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
            fail_fast: None,
            upload_limit_mb: None,
            description: None,
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
    assert_eq!(spec.version, 2);
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
        ca_bundle_pem: None,
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
            root_ca_pem: None,
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
                startup_timeout: Some("2m".into()),
            },
            ConnectSpec::Rclone {
                remote_path: "r:bucket".into(),
                config_file: None,
                startup_timeout: Some("2m".into()),
            },
        ),
        (
            RepositoryConnect::Gdrive {
                folder_id: "fid".into(),
            },
            ConnectSpec::Gdrive {
                folder_id: "fid".into(),
                credentials_file: None,
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
        source: RestoreSelection::Snapshot("s".into()),
        target_path: "/data".into(),
        anchor: SnapshotAnchor::default(),
        ignore_permission_errors: Some(false),
        write_files_atomically: Some(true),
        parallel: Some(2),
        write_sparse_files: None,
        skip_owners: None,
        skip_permissions: None,
        skip_times: Some(true),
        overwrite_files: None,
        overwrite_directories: None,
        overwrite_symlinks: None,
        ignore_errors: None,
        skip_existing: None,
        delete_extra: true,
    };
    let opts = op.restore_options();
    assert_eq!(opts.ignore_permission_errors, Some(false));
    assert_eq!(opts.write_files_atomically, Some(true));
    assert_eq!(opts.parallel, Some(2));
    assert_eq!(opts.skip_times, Some(true));
    // Regression guard: `enableFileDeletion`/`delete_extra: true` must map to
    // `Some(true)` — the exact bug this milestone fixes.
    assert_eq!(opts.delete_extra, Some(true));

    // A wire payload with just the source + path still deserializes (option/anchor
    // fields default), mapping to kopia defaults (None/empty).
    let json = r#"{"source":{"snapshot":"s"},"targetPath":"/data"}"#;
    let parsed: RestoreOp = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.ignore_permission_errors, None);
    assert_eq!(parsed.restore_options().write_files_atomically, None);
    assert!(parsed.anchor.is_empty());
    assert!(!parsed.delete_extra);
    assert_eq!(parsed.restore_options().delete_extra, None);
}

#[test]
fn restore_op_old_wire_decodes_with_m2_fields_defaulted() {
    // M2 flag sweep: a work-spec ConfigMap written before this milestone's
    // fields existed (just source/targetPath/anchor/the original two options)
    // must still decode — `#[serde(default)]` on every new field is the guard.
    let legacy = serde_json::json!({
        "source": { "snapshot": "s" },
        "targetPath": "/data",
        "ignorePermissionErrors": true,
        "writeFilesAtomically": false,
    });
    let op: RestoreOp = serde_json::from_value(legacy).expect("legacy restore op decodes");
    assert_eq!(op.ignore_permission_errors, Some(true));
    assert_eq!(op.write_files_atomically, Some(false));
    assert_eq!(op.parallel, None);
    assert_eq!(op.write_sparse_files, None);
    assert_eq!(op.skip_owners, None);
    assert_eq!(op.skip_permissions, None);
    assert_eq!(op.skip_times, None);
    assert_eq!(op.overwrite_files, None);
    assert_eq!(op.overwrite_directories, None);
    assert_eq!(op.overwrite_symlinks, None);
    assert_eq!(op.ignore_errors, None);
    assert_eq!(op.skip_existing, None);
    assert!(!op.delete_extra);
    // All-None/false new fields reproduce today's RestoreOptions exactly.
    assert_eq!(
        op.restore_options(),
        kopiur_kopia::RestoreOptions {
            ignore_permission_errors: Some(true),
            write_files_atomically: Some(false),
            ..Default::default()
        }
    );
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
        ca_bundle_pem: None,
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

#[test]
fn s3_ca_bundle_pem_roundtrips_and_reaches_the_kopia_connect_spec() {
    const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBfake\n-----END CERTIFICATE-----\n";
    // The resolved PEM travels the wire as `caBundlePem` (content, never a
    // ConfigMap reference — the mover has no ConfigMap access).
    let wire = RepositoryConnect::S3 {
        bucket: "b".into(),
        endpoint: Some("https://minio.internal".into()),
        prefix: None,
        region: None,
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
        ca_bundle_pem: Some(PEM.into()),
    };
    let v: serde_json::Value = serde_json::to_value(&wire).unwrap();
    assert_eq!(v["s3"]["caBundlePem"], PEM);
    let back: RepositoryConnect = serde_json::from_value(v).unwrap();
    assert_eq!(back, wire);
    // …and it reaches the kopia connect spec's `root_ca_pem`, the single
    // path to `--root-ca-pem-base64` for EVERY verb (kopia persists it in the
    // connection config at connect time).
    match back.to_connect_spec() {
        kopiur_kopia::ConnectSpec::S3 { root_ca_pem, .. } => {
            assert_eq!(root_ca_pem.as_deref(), Some(PEM));
        }
        other => panic!("expected S3, got {other:?}"),
    }

    // Back-compat: a work spec written before the field existed still parses
    // (defaults to None — no CA, exactly the old behavior)…
    let legacy = serde_json::json!({ "s3": { "bucket": "b" } });
    let parsed: RepositoryConnect = serde_json::from_value(legacy).unwrap();
    match &parsed {
        RepositoryConnect::S3 { ca_bundle_pem, .. } => assert_eq!(*ca_bundle_pem, None),
        other => panic!("expected S3, got {other:?}"),
    }
    // …and `None` stays OFF the wire, so legacy movers can read new specs too.
    let v = serde_json::to_value(&parsed).unwrap();
    assert!(v["s3"].get("caBundlePem").is_none());
}

// --- §13(b)/§13(f) policy-args mapping (api spec → work-spec → kopia args) ---

#[test]
fn policy_args_from_policy_maps_all_flattened_knobs() {
    use kopiur_api::snapshot_policy::{Compression, ErrorHandling, Files, Upload};
    let spec = kopiur_api::SnapshotPolicySpec {
        repository: Some(kopiur_api::common::RepositoryRef {
            kind: Default::default(),
            name: "r".into(),
            namespace: None,
        }),
        repositories: vec![],
        identity: None,
        sources: vec![],
        copy_method: Default::default(),
        volume_snapshot_class_name: None,
        staging: None,
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
            // M4 flag sweep: `failFast` is a `snapshot create` argv flag, not a
            // `policy set` knob — it must NOT leak into `PolicyArgsSpec` below.
            fail_fast: true,
        }),
        upload: Some(Upload {
            max_parallel_snapshots: Some(4),
            max_parallel_file_reads: Some(8),
            // Same non-leak guard as `fail_fast` above, for `limitMb`.
            limit_mb: Some(100),
        }),
        verification: None,
        preflight: None,
        suspend: false,
        hooks: None,
        mover: None,
        credential_projection: None,
        deletion: None,
        adoption: None,
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
    // M4 flag sweep: `errorHandling.failFast`/`upload.limitMb` are `snapshot
    // create` argv flags (they ride `SnapshotOp`, not `policy set`), so they
    // must not appear anywhere on the wire `PolicyArgsSpec` produces —
    // `PolicyArgsSpec` structurally has no such fields, and this proves it at
    // the JSON boundary too.
    let policy_json = serde_json::to_value(&p).unwrap();
    assert!(policy_json.get("failFast").is_none());
    assert!(policy_json.get("limitMb").is_none());

    // The kopia args builder emits the expected flags (end-to-end into argv).
    let args = p.to_kopia();
    assert_eq!(args.compression.as_deref(), Some("zstd"));
    assert_eq!(args.ignore_file_errors, Some(true));
    assert_eq!(args.max_parallel_snapshots, Some(4));
    // No per-policy splitter (ADR-0004 §4b).
    assert_eq!(args.splitter, None);
    // M0b: the create-time retention `keep_*` pin is NOT user-configurable and
    // never rides `PolicyArgsSpec` — `to_kopia()` must never populate it, even
    // when every other knob is set (the mover applies the pin directly,
    // separately, at the identity scope).
    assert_eq!(args.keep_latest, None);
    assert_eq!(args.keep_hourly, None);
    assert_eq!(args.keep_daily, None);
    assert_eq!(args.keep_weekly, None);
    assert_eq!(args.keep_monthly, None);
    assert_eq!(args.keep_annual, None);
}

/// A `SnapshotPolicySpec` with every optional policy surface left `None`/empty
/// except the one field the test overrides — shared by the two ignoreRules
/// glue tests below (and easy to spot the ONE field each varies).
fn empty_policy_spec() -> kopiur_api::SnapshotPolicySpec {
    kopiur_api::SnapshotPolicySpec {
        repository: Some(kopiur_api::common::RepositoryRef {
            kind: Default::default(),
            name: "r".into(),
            namespace: None,
        }),
        repositories: vec![],
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
    }
}

/// #351: the field was declared, schema-generated, documented at
/// `docs/reference/crds/snapshot-policy.md` — and `from_policy` never read it,
/// so the knob did nothing at all.
///
/// Only `true` is emitted. `false`/unset must leave the PATH-scoped policy
/// untouched, for two reasons: the mover pins `false` at the identity scope on
/// every run (so the guarantee holds without this), and emitting anything here
/// would make `is_empty()` permanently false and force a `kopia policy set` on
/// every otherwise-unconfigured policy.
#[test]
fn from_policy_only_raises_ignore_identical_snapshots_on_opt_in() {
    let mut spec = empty_policy_spec();

    spec.files = Some(kopiur_api::snapshot_policy::Files {
        ignore_rules: vec![],
        ignore_cache_dirs: false,
        ignore_identical_snapshots: true,
    });
    let p = PolicyArgsSpec::from_policy(&spec);
    assert_eq!(p.ignore_identical_snapshots, Some(true));
    assert!(!p.is_empty(), "an opt-in must reach kopia");
    assert_eq!(p.to_kopia().ignore_identical_snapshots, Some(true));

    // Opted out → absent, never `Some(false)`.
    spec.files = Some(kopiur_api::snapshot_policy::Files {
        ignore_rules: vec![],
        ignore_cache_dirs: false,
        ignore_identical_snapshots: false,
    });
    let p = PolicyArgsSpec::from_policy(&spec);
    assert_eq!(p.ignore_identical_snapshots, None);
    assert!(
        p.is_empty(),
        "an opted-out policy with nothing else set must still skip `policy set`"
    );

    // Absent `files:` block → likewise absent.
    spec.files = None;
    assert_eq!(
        PolicyArgsSpec::from_policy(&spec).ignore_identical_snapshots,
        None
    );
}

/// The load-bearing glue test (task PR4): the apiserver only server-side-defaults
/// NESTED fields when the parent object is present, so a `SnapshotPolicy` that
/// omits `files:` entirely NEVER gets `Files.ignoreRules`'s schema default
/// applied — the controller glue (`PolicyArgsSpec::from_policy`'s `None` arm)
/// must apply `kopiur_api::snapshot_policy::default_ignore_rules()` itself. This
/// is the seam a regression would silently skip: `files: None` must still reach
/// the mover with the 5-entry OS-artifact exclude set.
#[test]
fn policy_args_from_absent_files_block_gets_default_ignore_rules() {
    let spec = empty_policy_spec();
    assert!(spec.files.is_none());
    let p = PolicyArgsSpec::from_policy(&spec);
    assert_eq!(
        p.ignore,
        kopiur_api::snapshot_policy::default_ignore_rules()
    );
    // ignore_cache_dirs is untouched by the ignoreRules default — still None
    // (absent `files:` leaves kopia's own default for that knob).
    assert_eq!(p.ignore_cache_dirs, None);
    assert!(
        !p.is_empty(),
        "a non-empty `ignore` means the mover DOES run `kopia policy set`"
    );
}

/// The explicit opt-out (`files: { ignoreRules: [] }`) must NOT be overridden by
/// the glue's default — only the wholly-absent `files: None` case falls back to
/// `default_ignore_rules()`. A present-but-empty `Files` is honored verbatim.
#[test]
fn policy_args_from_explicit_opt_out_files_is_empty() {
    use kopiur_api::snapshot_policy::Files;
    let mut spec = empty_policy_spec();
    spec.files = Some(Files {
        ignore_rules: vec![],
        ignore_cache_dirs: false,
        ignore_identical_snapshots: false,
    });
    let p = PolicyArgsSpec::from_policy(&spec);
    assert!(
        p.ignore.is_empty(),
        "explicit ignoreRules: [] must opt fully out, got {:?}",
        p.ignore
    );
    assert!(p.is_empty());
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
                username: None,
                hostname: None,
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

// --- M0a: SnapshotAnchor identity fields (cross-identity delete/verify hazard) ---

#[test]
fn snapshot_anchor_without_identity_fields_still_deserializes() {
    // Work-spec JSON stamped before this fix carries no username/hostname —
    // must still decode, with the matchers then falling back to path-only.
    let legacy = serde_json::json!({
        "sourcePath": "/pvc/db",
        "startTime": "2026-06-19T05:54:19Z",
    });
    let anchor: SnapshotAnchor = serde_json::from_value(legacy).expect("legacy anchor decodes");
    assert_eq!(anchor.source_path, "/pvc/db");
    assert!(anchor.username.is_none());
    assert!(anchor.hostname.is_none());
    assert_eq!(anchor.identity_filter(), None);
    // `is_empty()` is unaffected by the new fields — it still only reflects
    // whether there's a path/time to anchor on at all.
    assert!(!anchor.is_empty());
}

#[test]
fn snapshot_anchor_identity_fields_roundtrip() {
    let anchor = SnapshotAnchor {
        source_path: "/pvc/db".into(),
        start_time: Some("2026-06-19T05:54:19Z".into()),
        username: Some("app".into()),
        hostname: Some("cluster-a".into()),
    };
    let v = serde_json::to_value(&anchor).unwrap();
    assert_eq!(v["sourcePath"], "/pvc/db");
    assert_eq!(v["username"], "app");
    assert_eq!(v["hostname"], "cluster-a");
    let back: SnapshotAnchor = serde_json::from_value(v).unwrap();
    assert_eq!(back, anchor);
    assert_eq!(anchor.identity_filter(), Some(("app", "cluster-a")));
}

#[test]
fn snapshot_anchor_identity_filter_requires_both_halves() {
    // A half-populated anchor (shouldn't happen from a real producer, but the
    // helper must not silently treat one known half as a match) yields no
    // filter — same as a fully-legacy anchor, so matching stays path-only.
    let half = SnapshotAnchor {
        source_path: "/pvc/db".into(),
        start_time: None,
        username: Some("app".into()),
        hostname: None,
    };
    assert_eq!(half.identity_filter(), None);
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
                file_parallelism: None,
                file_queue_length: None,
            }),
            success_expr: Some("stats.files > 0 && stats.errors == 0".into()),
            repository_key: None,
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
            ca_bundle_pem: None,
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
    // Single-repo golden byte: no repositoryKey key at all when None.
    assert!(
        v["operation"]["verify"].get("repositoryKey").is_none(),
        "repository_key: None must elide"
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
                parallel: None,
            }),
            success_expr: None,
            repository_key: Some("Repository/backups/nas".into()),
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
    // Multi-repo: the per-repo key rides the wire camelCased.
    assert_eq!(
        v["operation"]["verify"]["repositoryKey"],
        "Repository/backups/nas"
    );
    if let Operation::Verify(op) = &spec.operation {
        assert_eq!(op.tier.kind_str(), "deep");
    } else {
        panic!("expected verify op");
    }
}

// --- M3 (issue #216 category sweep): quick tuning knobs + deep restore parallelism ---

#[test]
fn quick_verify_tuning_knobs_roundtrip_and_map_to_kopia() {
    let q = QuickVerify {
        verify_files_percent: Some(10),
        max_errors: Some(1),
        parallel: Some(2),
        file_parallelism: Some(4),
        file_queue_length: Some(100),
    };
    let v: serde_json::Value = serde_json::to_value(&q).unwrap();
    assert_eq!(v["parallel"], 2);
    assert_eq!(v["fileParallelism"], 4);
    assert_eq!(v["fileQueueLength"], 100);
    let reparsed: QuickVerify = serde_json::from_value(v).unwrap();
    assert_eq!(reparsed, q);

    let kopia = q.to_kopia();
    assert_eq!(kopia.parallel, Some(2));
    assert_eq!(kopia.file_parallelism, Some(4));
    assert_eq!(kopia.file_queue_length, Some(100));
}

#[test]
fn quick_verify_old_wire_json_without_new_knobs_still_decodes() {
    // Pre-M3 work-spec ConfigMaps only ever carried these three keys. A mover
    // upgraded ahead of a controller still writing the old shape must not wedge.
    let old = r#"{"verifyFilesPercent":10,"maxErrors":3}"#;
    let parsed: QuickVerify = serde_json::from_str(old).unwrap();
    assert_eq!(parsed.verify_files_percent, Some(10));
    assert_eq!(parsed.max_errors, Some(3));
    assert!(parsed.parallel.is_none());
    assert!(parsed.file_parallelism.is_none());
    assert!(parsed.file_queue_length.is_none());
}

#[test]
fn deep_verify_parallel_roundtrips_and_old_wire_json_still_decodes() {
    let d = DeepVerify {
        scratch_path: "/scratch".into(),
        snapshot_id: Some("k1".into()),
        parallel: Some(2),
    };
    let v: serde_json::Value = serde_json::to_value(&d).unwrap();
    assert_eq!(v["parallel"], 2);
    let reparsed: DeepVerify = serde_json::from_value(v).unwrap();
    assert_eq!(reparsed, d);

    // Pre-M3 work-spec ConfigMaps had no `parallel` key.
    let old = r#"{"scratchPath":"/scratch","snapshotId":"k1"}"#;
    let parsed: DeepVerify = serde_json::from_str(old).unwrap();
    assert_eq!(parsed.scratch_path, "/scratch");
    assert!(parsed.parallel.is_none());
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
                ca_bundle_pem: None,
            },
            delete_extra: true,
            parallel: Some(8),
            must_exist: Some(false),
            times: Some(true),
            update: Some(false),
            max_download_speed_bytes_per_second: Some(1_000_000),
            max_upload_speed_bytes_per_second: Some(500_000),
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
    assert_eq!(v["operation"]["replicate"]["parallel"], 8);
    assert_eq!(v["operation"]["replicate"]["mustExist"], false);
    assert_eq!(v["operation"]["replicate"]["times"], true);
    assert_eq!(v["operation"]["replicate"]["update"], false);
    assert_eq!(
        v["operation"]["replicate"]["maxDownloadSpeedBytesPerSecond"],
        1_000_000
    );
    assert_eq!(
        v["operation"]["replicate"]["maxUploadSpeedBytesPerSecond"],
        500_000
    );
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
                root_ca_pem: None,
            }
        );
        // #216 controller-glue guard: every new op field reaches the kopia
        // client's SyncToOptions — no dormant plumbing.
        assert_eq!(
            op.sync_options(),
            kopiur_kopia::SyncToOptions {
                parallel: Some(8),
                delete_extra: true,
                must_exist: Some(false),
                times: Some(true),
                update: Some(false),
                max_download_speed_bytes_per_second: Some(1_000_000),
                max_upload_speed_bytes_per_second: Some(500_000),
            }
        );
    } else {
        panic!("expected replicate op");
    }
}

#[test]
fn replicate_op_old_wire_decodes_with_sync_fields_defaulted() {
    // #216: a work-spec ConfigMap written before `sync` tuning existed (just
    // `destination` + `deleteExtra`) must still decode — the mover pairs one
    // controller+mover image per Job, but old ConfigMaps can persist across a
    // rolling restart. `#[serde(default)]` on every new field is the guard.
    let legacy = serde_json::json!({
        "destination": { "filesystem": { "path": "/mirror" } },
        "deleteExtra": true,
    });
    let op: ReplicateOp = serde_json::from_value(legacy).expect("legacy replicate op decodes");
    assert!(op.delete_extra);
    assert_eq!(op.parallel, None);
    assert_eq!(op.must_exist, None);
    assert_eq!(op.times, None);
    assert_eq!(op.update, None);
    assert_eq!(op.max_download_speed_bytes_per_second, None);
    assert_eq!(op.max_upload_speed_bytes_per_second, None);
    // All-None sync fields reproduce today's SyncToOptions exactly.
    assert_eq!(
        op.sync_options(),
        kopiur_kopia::SyncToOptions {
            delete_extra: true,
            ..Default::default()
        }
    );
}

// --- snapshot replication op (SnapshotReplication CRD, M4) ---

fn sample_snapshot_replicate_op() -> SnapshotReplicateOp {
    SnapshotReplicateOp {
        destination: RepositoryConnect::S3 {
            bucket: "offsite".into(),
            endpoint: Some("https://minio.remote".into()),
            prefix: Some("copies/".into()),
            region: None,
            disable_tls: false,
            disable_tls_verification: false,
            ca_bundle_pem: None,
            ambient_credentials: false,
        },
        destination_repository: ReplicationRepositoryRef {
            kind: "ClusterRepository".into(),
            name: "offsite".into(),
            namespace: None,
            uid: "dest-uid-1".into(),
        },
        source_repository: ReplicationSourceRef {
            kind: "Repository".into(),
            name: "nas-primary".into(),
            namespace: Some("backups".into()),
        },
        include: vec![IdentityMatcherSpec {
            username: Some("my*".into()),
            hostname: None,
            source_path: None,
        }],
        exclude: vec![IdentityMatcherSpec {
            username: None,
            hostname: Some("staging".into()),
            source_path: None,
        }],
        latest_only: true,
        parallel: Some(4),
        policies: PolicyCopyModeSpec::Copy,
        pruning: PruningSpec::Retention(ReplicationRetentionSpec {
            keep_daily: Some(7),
            keep_weekly: Some(4),
            ..Default::default()
        }),
    }
}

#[test]
fn snapshot_replicate_roundtrip_and_wire_shape() {
    let spec = MoverWorkSpec {
        version: 2,
        operation: Operation::SnapshotReplicate(sample_snapshot_replicate_op()),
        identity: sample_identity(),
        repository: RepositoryConnect::Filesystem {
            path: "/repo".into(),
        },
        target_ref: TargetRef {
            api_version: "kopiur.home-operations.com/v1alpha1".into(),
            kind: "SnapshotReplication".into(),
            name: "offsite-mirror".into(),
            namespace: "backups".into(),
        },
        hook_plan: HookPlanSummary::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    assert_eq!(roundtrip(&spec), spec);
    assert_eq!(spec.operation.kind_str(), "SnapshotReplicate");

    // Externally tagged, camelCase wire shape — typed via the JSON path, the
    // same way the cluster parses it.
    let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
    let op = &v["operation"]["snapshotReplicate"];
    assert_eq!(op["destination"]["s3"]["bucket"], "offsite");
    assert_eq!(op["destinationRepository"]["kind"], "ClusterRepository");
    assert_eq!(op["destinationRepository"]["uid"], "dest-uid-1");
    assert!(op["destinationRepository"].get("namespace").is_none());
    assert_eq!(op["sourceRepository"]["name"], "nas-primary");
    assert_eq!(op["sourceRepository"]["namespace"], "backups");
    assert_eq!(op["include"][0]["username"], "my*");
    assert!(op["include"][0].get("hostname").is_none());
    assert_eq!(op["exclude"][0]["hostname"], "staging");
    assert_eq!(op["latestOnly"], true);
    assert_eq!(op["parallel"], 4);
    assert_eq!(op["policies"], "copy");
    // Pruning is externally tagged: { "retention": { "keepDaily": 7, ... } }.
    assert_eq!(op["pruning"]["retention"]["keepDaily"], 7);
    assert_eq!(op["pruning"]["retention"]["keepWeekly"], 4);
    assert!(op["pruning"]["retention"].get("keepLatest").is_none());
}

#[test]
fn snapshot_replicate_pruning_marker_variants_roundtrip() {
    // The empty marker sub-objects round-trip as `{ "none": {} }` /
    // `{ "mirrorSource": {} }` (external tagging; future knobs slot in).
    for (pruning, key) in [
        (PruningSpec::None(NoPruningSpec {}), "none"),
        (
            PruningSpec::MirrorSource(MirrorSourcePruningSpec {}),
            "mirrorSource",
        ),
    ] {
        let v = serde_json::to_value(&pruning).unwrap();
        assert_eq!(v[key], serde_json::json!({}), "{key}: {v}");
        let back: PruningSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, pruning);
    }
    assert!(PruningSpec::default().is_none());
}

#[test]
fn snapshot_replicate_old_wire_decodes_with_selection_and_pruning_defaulted() {
    // A minimal wire payload (just the repository blocks) must decode with
    // every optional knob at its default: include/exclude empty, full history,
    // policies none, pruning none — serde(default) is the guard.
    let legacy = serde_json::json!({
        "destination": { "filesystem": { "path": "/mirror" } },
        "destinationRepository": { "kind": "Repository", "name": "offsite", "uid": "u1" },
        "sourceRepository": { "kind": "Repository", "name": "nas" },
    });
    let op: SnapshotReplicateOp =
        serde_json::from_value(legacy).expect("minimal snapshot-replicate op decodes");
    assert!(op.include.is_empty());
    assert!(op.exclude.is_empty());
    assert!(!op.latest_only);
    assert_eq!(op.parallel, None);
    assert_eq!(op.policies, PolicyCopyModeSpec::None);
    assert!(op.pruning.is_none());
    assert_eq!(op.destination_repository.namespace, None);
    assert_eq!(op.source_repository.namespace, None);
}

#[test]
fn policy_copy_mode_maps_every_variant_to_the_kopia_mode() {
    // Exhaustive mapping — kopia's own default for --policies is TRUE, so the
    // None arm rendering an explicit --no-policies downstream depends on this
    // mapping never silently dropping a variant.
    use kopiur_kopia::MigratePolicies;
    assert_eq!(PolicyCopyModeSpec::None.to_kopia(), MigratePolicies::None);
    assert_eq!(PolicyCopyModeSpec::Copy.to_kopia(), MigratePolicies::Copy);
    assert_eq!(
        PolicyCopyModeSpec::CopyOverwrite.to_kopia(),
        MigratePolicies::CopyOverwrite
    );
    // Wire values are camelCase strings.
    assert_eq!(
        serde_json::to_value(PolicyCopyModeSpec::CopyOverwrite).unwrap(),
        "copyOverwrite"
    );
}

#[test]
fn replication_retention_maps_onto_the_api_gfs_policy_field_for_field() {
    let spec = ReplicationRetentionSpec {
        keep_latest: Some(1),
        keep_hourly: Some(2),
        keep_daily: Some(3),
        keep_weekly: Some(4),
        keep_monthly: Some(5),
        keep_annual: Some(6),
    };
    let r = spec.to_retention();
    assert_eq!(r.keep_latest, Some(1));
    assert_eq!(r.keep_hourly, Some(2));
    assert_eq!(r.keep_daily, Some(3));
    assert_eq!(r.keep_weekly, Some(4));
    assert_eq!(r.keep_monthly, Some(5));
    assert_eq!(r.keep_annual, Some(6));
}

// --- M6: maintenance-owner restamp policy (connect-to-existing self-heal) --

#[test]
fn restamp_target_skips_the_create_path_regardless_of_policy() {
    // On CREATE the owner is stamped unconditionally elsewhere; the connect
    // self-heal must never also fire, under either policy.
    for policy in [RestampPolicy::AnyStale, RestampPolicy::OwnFormatsOnly] {
        assert_eq!(
            maintenance_restamp_target(true, Some("kopiur@kopiur-prod"), policy, &[], "anything"),
            None,
            "{policy:?}"
        );
    }
}

#[test]
fn restamp_target_skips_without_a_configured_owner() {
    // No stable owner configured (e.g. maintenance disabled) → never stamp,
    // under either policy.
    for policy in [RestampPolicy::AnyStale, RestampPolicy::OwnFormatsOnly] {
        assert_eq!(
            maintenance_restamp_target(false, None, policy, &[], "ephemeral@pod-xyz"),
            None,
            "{policy:?}"
        );
    }
}

/// The `{AnyStale, OwnFormatsOnly} x {empty, ==desired, alias, foreign,
/// ephemeral}` decision table (M6). `foreign x OwnFormatsOnly -> None` is the
/// anti-ping-pong regression: two clusters cluster-qualifying the SAME
/// underlying repository must never see each other's owner as "stale" and
/// re-claim it on every bootstrap connect.
#[test]
fn restamp_target_decision_table() {
    let desired = "kopiur@kopiur-east-media-nas";
    let alias_owner = "kopiur@kopiur-media-nas"; // the pre-cluster lease's owner
    let aliases = vec![alias_owner.to_string()];
    let foreign = "someone-else@their-host";
    let ephemeral = "nonroot@rustfs-kopiur-bootstrap-5trlr"; // kopia's auto-assigned pod identity

    // current == desired: never restamp, either policy (nothing to heal).
    for policy in [RestampPolicy::AnyStale, RestampPolicy::OwnFormatsOnly] {
        assert_eq!(
            maintenance_restamp_target(false, Some(desired), policy, &aliases, desired),
            None,
            "==desired x {policy:?}"
        );
    }

    // current empty (never-run repo): both policies heal it.
    assert_eq!(
        maintenance_restamp_target(false, Some(desired), RestampPolicy::AnyStale, &aliases, ""),
        Some(desired),
        "empty x AnyStale"
    );
    assert_eq!(
        maintenance_restamp_target(
            false,
            Some(desired),
            RestampPolicy::OwnFormatsOnly,
            &aliases,
            ""
        ),
        Some(desired),
        "empty x OwnFormatsOnly"
    );

    // current is a recognized alias (the migration path): both policies heal it.
    assert_eq!(
        maintenance_restamp_target(
            false,
            Some(desired),
            RestampPolicy::AnyStale,
            &aliases,
            alias_owner
        ),
        Some(desired),
        "alias x AnyStale"
    );
    assert_eq!(
        maintenance_restamp_target(
            false,
            Some(desired),
            RestampPolicy::OwnFormatsOnly,
            &aliases,
            alias_owner
        ),
        Some(desired),
        "alias x OwnFormatsOnly"
    );

    // current is a genuinely foreign owner: AnyStale still clobbers it
    // (single-cluster behavior, unchanged); OwnFormatsOnly refuses — THE
    // anti-ping-pong regression test.
    assert_eq!(
        maintenance_restamp_target(
            false,
            Some(desired),
            RestampPolicy::AnyStale,
            &aliases,
            foreign
        ),
        Some(desired),
        "foreign x AnyStale"
    );
    assert_eq!(
        maintenance_restamp_target(
            false,
            Some(desired),
            RestampPolicy::OwnFormatsOnly,
            &aliases,
            foreign
        ),
        None,
        "foreign x OwnFormatsOnly (anti-ping-pong)"
    );

    // current is an ancient/ephemeral owner this operator never recognized:
    // same shape as "foreign" — AnyStale clobbers, OwnFormatsOnly refuses (a
    // one-time takeoverPolicy: Force is required to move it under this policy).
    assert_eq!(
        maintenance_restamp_target(
            false,
            Some(desired),
            RestampPolicy::AnyStale,
            &aliases,
            ephemeral
        ),
        Some(desired),
        "ephemeral x AnyStale"
    );
    assert_eq!(
        maintenance_restamp_target(
            false,
            Some(desired),
            RestampPolicy::OwnFormatsOnly,
            &aliases,
            ephemeral
        ),
        None,
        "ephemeral x OwnFormatsOnly"
    );
}

// --- #258: epoch parameter drift ------------------------------------------

/// kopia's observed defaults, straight from `crates/kopia/tests/fixtures/repository_status.json`.
fn observed_defaults() -> kopiur_kopia::model::EpochParameters {
    kopiur_kopia::model::EpochParameters {
        enabled: true,
        min_epoch_duration_ns: 86_400_000_000_000, // 24h
        epoch_refresh_frequency_ns: 1_200_000_000_000, // 20m
        cleanup_safety_margin_ns: 14_400_000_000_000, // 4h
        advance_on_count: 20,
        advance_on_total_size_bytes: 10_485_760, // 10 MiB
        checkpoint_frequency: 7,
        delete_parallelism: 4,
    }
}

#[test]
fn epoch_drift_is_none_when_nothing_is_declared() {
    // The inert case, and the one that matters most: a repository that never mentions
    // spec.parameters must never trigger set-parameters — which invalidates every other
    // kopia client's cached format blob.
    let desired = EpochParametersSpec::default();
    assert!(desired.is_empty());
    assert!(epoch_drift(&desired, Some(&observed_defaults())).is_none());
    assert!(epoch_drift(&desired, None).is_none());
}

#[test]
fn epoch_drift_is_none_when_the_declared_values_already_match() {
    // Converged: declaring kopia's current values must be a no-op, not a re-apply on
    // every single bootstrap.
    let desired = EpochParametersSpec {
        min_duration: Some("24h".into()),
        refresh_frequency: Some("20m".into()),
        advance_on_count: Some(20),
        advance_on_size_mb: Some(10),
        checkpoint_frequency: Some(7),
        delete_parallelism: Some(4),
    };
    assert!(
        epoch_drift(&desired, Some(&observed_defaults())).is_none(),
        "declaring exactly what the repository already reports is not drift"
    );
}

#[test]
fn epoch_drift_compares_durations_by_value_not_by_string() {
    // "24h", "1440m" and "86400s" are the same parameter. A string compare would report
    // drift forever and re-apply on every bootstrap — the fleet-wide reconnect churn this
    // whole drift check exists to avoid.
    for equivalent in ["24h", "1440m", "86400s", "86400"] {
        let desired = EpochParametersSpec {
            min_duration: Some(equivalent.into()),
            ..Default::default()
        };
        assert!(
            epoch_drift(&desired, Some(&observed_defaults())).is_none(),
            "{equivalent:?} is 24h and must not read as drift"
        );
    }
}

#[test]
fn epoch_drift_emits_only_the_parameters_that_actually_differ() {
    // The reporter's fix: 24h -> 6h, everything else left alone.
    let desired = EpochParametersSpec {
        min_duration: Some("6h".into()),
        refresh_frequency: Some("20m".into()), // already matches
        advance_on_count: Some(20),            // already matches
        ..Default::default()
    };
    let args = epoch_drift(&desired, Some(&observed_defaults())).expect("minDuration drifted");
    assert_eq!(args.epoch_min_duration.as_deref(), Some("6h"));
    assert_eq!(
        args.epoch_refresh_frequency, None,
        "a converged parameter must not be re-sent"
    );
    assert_eq!(args.epoch_advance_on_count, None);
    assert_eq!(args.args(), vec!["--epoch-min-duration", "6h"]);
}

#[test]
fn epoch_drift_converts_the_size_threshold_in_mebibytes() {
    // kopia reports BYTES; the flag is named `-mb` and means MiB. 10485760 == 10 MiB, so
    // declaring 10 is converged. Dividing by 1e6 would read 10485760 as ~10.49 and report
    // drift every time, re-applying forever.
    let converged = EpochParametersSpec {
        advance_on_size_mb: Some(10),
        ..Default::default()
    };
    assert!(epoch_drift(&converged, Some(&observed_defaults())).is_none());

    let changed = EpochParametersSpec {
        advance_on_size_mb: Some(32),
        ..Default::default()
    };
    let args = epoch_drift(&changed, Some(&observed_defaults())).expect("size drifted");
    assert_eq!(args.epoch_advance_on_size_mb, Some(32));
}

#[test]
fn epoch_drift_applies_everything_when_nothing_was_observed() {
    // Older kopia, or a status we could not read: apply what is declared rather than skip.
    // set-parameters is idempotent, so re-applying a converged value is harmless; silently
    // never applying a declared one is not.
    let desired = EpochParametersSpec {
        min_duration: Some("6h".into()),
        advance_on_count: Some(50),
        ..Default::default()
    };
    let args = epoch_drift(&desired, None).expect("no observation ⇒ apply the declaration");
    assert_eq!(args.epoch_min_duration.as_deref(), Some("6h"));
    assert_eq!(args.epoch_advance_on_count, Some(50));
}

#[test]
fn epoch_parameters_spec_renders_durations_for_kopias_cli() {
    // kopia REJECTS a bare number (`--epoch-min-duration=3600` → "missing unit"), while
    // kopiur's parse_go_duration accepts one. Rendering at the boundary is what stops the
    // webhook admitting a value the mover then dies on.
    let api = kopiur_api::repository::EpochParameters {
        min_duration: Some("3600".into()),
        refresh_frequency: Some("20m".into()),
        ..Default::default()
    };
    let spec = EpochParametersSpec::from_api(&api);
    assert_eq!(spec.min_duration.as_deref(), Some("1h"));
    assert_eq!(spec.refresh_frequency.as_deref(), Some("20m"));
}

#[test]
fn observed_epoch_renders_nanoseconds_back_to_go_durations() {
    // status.parameters.epoch must be directly comparable to spec.parameters.epoch, so the
    // mirror renders kopia's nanoseconds through the same grammar the spec is written in.
    let o = observed_epoch(&observed_defaults());
    assert!(o.enabled);
    assert_eq!(o.min_duration, "24h");
    assert_eq!(o.refresh_frequency, "20m");
    assert_eq!(o.cleanup_safety_margin, "4h");
    assert_eq!(o.advance_on_count, 20);
    assert_eq!(o.advance_on_size_mb, 10, "bytes → MiB");
    assert_eq!(o.checkpoint_frequency, 7);
    assert_eq!(o.delete_parallelism, 4);
}

// --- #332: object-lock blob retention -------------------------------------------------

fn retention_on(mode: &str, ns: i64) -> kopiur_kopia::model::BlobRetention {
    kopiur_kopia::model::BlobRetention {
        mode: mode.into(),
        period_ns: ns,
    }
}
/// What kopia reports for a repository with retention off: `{}` — both keys omitempty.
fn retention_off() -> kopiur_kopia::model::BlobRetention {
    kopiur_kopia::model::BlobRetention::default()
}
/// 720h == 30 days, in the nanoseconds kopia reports.
const NS_720H: i64 = 2_592_000_000_000_000;

fn want(mode: &str, period: Option<&str>) -> BlobRetentionSpec {
    BlobRetentionSpec {
        mode: mode.into(),
        period: period.map(str::to_string),
    }
}

#[test]
fn blob_retention_drift_is_none_when_nothing_is_declared() {
    // THE inert case. A repository that never mentions blobRetention must never have
    // `set-parameters` run against it — adding this feature has to be a no-op for every
    // existing repository, and `set-parameters` invalidates every client's cached format
    // blob. Checked against all three observation shapes.
    assert!(blob_retention_drift(None, None).is_none());
    assert!(blob_retention_drift(None, Some(&retention_off())).is_none());
    assert!(blob_retention_drift(None, Some(&retention_on("GOVERNANCE", NS_720H))).is_none());
}

#[test]
fn blob_retention_drift_is_none_when_already_converged() {
    let desired = want("GOVERNANCE", Some("720h"));
    assert!(
        blob_retention_drift(Some(&desired), Some(&retention_on("GOVERNANCE", NS_720H))).is_none()
    );
}

#[test]
fn blob_retention_drift_compares_periods_by_value_not_by_string() {
    // "720h", "43200m" and "2592000s" are the same period. A string compare would report
    // drift forever and rewrite the format blob on every single bootstrap.
    for equivalent in ["720h", "43200m", "2592000s", "2592000"] {
        let desired = want("GOVERNANCE", Some(equivalent));
        assert!(
            blob_retention_drift(Some(&desired), Some(&retention_on("GOVERNANCE", NS_720H)))
                .is_none(),
            "{equivalent} is 720h and must not read as drift"
        );
    }
}

#[test]
fn blob_retention_drift_emits_both_flags_when_either_changes() {
    // Period changed.
    let args = blob_retention_drift(
        Some(&want("GOVERNANCE", Some("1440h"))),
        Some(&retention_on("GOVERNANCE", NS_720H)),
    )
    .expect("a longer period is drift");
    assert_eq!(
        args.args(),
        vec![
            "--retention-mode",
            "GOVERNANCE",
            "--retention-period",
            "1440h"
        ]
    );

    // Mode changed — the period is re-sent even though it did not.
    let args = blob_retention_drift(
        Some(&want("COMPLIANCE", Some("720h"))),
        Some(&retention_on("GOVERNANCE", NS_720H)),
    )
    .expect("a mode change is drift");
    assert_eq!(
        args.args(),
        vec![
            "--retention-mode",
            "COMPLIANCE",
            "--retention-period",
            "720h"
        ]
    );

    // Turning it ON from off.
    assert!(
        blob_retention_drift(
            Some(&want("GOVERNANCE", Some("720h"))),
            Some(&retention_off())
        )
        .is_some()
    );
    // No observation at all → apply what is declared (set-parameters is idempotent),
    // matching epoch_drift's behavior.
    assert!(blob_retention_drift(Some(&want("GOVERNANCE", Some("720h"))), None).is_some());
}

#[test]
fn blob_retention_drift_disables_only_what_is_actually_on() {
    // Disabling is mode-only.
    let args = blob_retention_drift(
        Some(&want("none", None)),
        Some(&retention_on("GOVERNANCE", NS_720H)),
    )
    .expect("retention is on, so disabling is drift");
    assert_eq!(args.args(), vec!["--retention-mode", "none"]);

    // Already off → nothing to do. Re-applying would churn the format blob every bootstrap.
    assert!(blob_retention_drift(Some(&want("none", None)), Some(&retention_off())).is_none());

    // Deliberately asymmetric with the enable path: with NO observation we cannot tell
    // whether there is anything to disable, and a blind `--retention-mode=none` against a
    // backend that cannot object-lock hard-fails. Do nothing.
    assert!(blob_retention_drift(Some(&want("none", None)), None).is_none());
}

#[test]
fn blob_retention_spec_renders_durations_for_kopias_cli_and_drops_garbage() {
    use kopiur_api::repository::{BlobRetention as B, RetentionWindow};
    let s = BlobRetentionSpec::from_api(&B::Governance(RetentionWindow {
        period: "2592000".into(), // bare seconds — kopia REJECTS this form
    }))
    .unwrap();
    assert_eq!(s.mode, "GOVERNANCE");
    assert_eq!(
        s.period.as_deref(),
        Some("720h"),
        "must reach kopia with a unit"
    );

    // `30d` is valid for kopia's own CLI but not for kopiur's grammar; admission rejects it,
    // and if one ever slipped through it is dropped rather than forwarded as garbage.
    let s = BlobRetentionSpec::from_api(&B::Compliance(RetentionWindow {
        period: "30d".into(),
    }))
    .unwrap();
    assert_eq!(s.period, None);

    // `disabled: true` is a real instruction; `disabled: false` is not "enable" — there is
    // nothing to enable without a mode and period — so it reads as "leave it alone".
    assert_eq!(
        BlobRetentionSpec::from_api(&B::Disabled(true)),
        Some(want("none", None))
    );
    assert_eq!(BlobRetentionSpec::from_api(&B::Disabled(false)), None);
}

#[test]
fn parameters_drift_merges_epoch_and_retention_into_one_invocation() {
    // `set-parameters` rewrites the repository-global format blob and forces every other
    // kopia client to reconnect. Epoch tuning and retention must therefore ride ONE command.
    let epoch = EpochParametersSpec {
        min_duration: Some("6h".into()),
        ..Default::default()
    };
    let args = parameters_drift(
        &epoch,
        None,
        Some(&want("GOVERNANCE", Some("720h"))),
        Some(&retention_off()),
    )
    .expect("both drifted");
    assert_eq!(
        args.args(),
        vec![
            "--epoch-min-duration",
            "6h",
            "--retention-mode",
            "GOVERNANCE",
            "--retention-period",
            "720h"
        ],
        "one invocation carrying both settings"
    );

    // Either alone still works...
    assert!(parameters_drift(&epoch, None, None, None).is_some());
    assert!(
        parameters_drift(
            &EpochParametersSpec::default(),
            None,
            Some(&want("GOVERNANCE", Some("720h"))),
            None
        )
        .is_some()
    );
    // ...and declaring neither runs no command at all.
    assert!(parameters_drift(&EpochParametersSpec::default(), None, None, None).is_none());
}

#[test]
fn observed_blob_retention_renders_nanoseconds_back_to_go_durations() {
    let o = observed_blob_retention(&retention_on("GOVERNANCE", NS_720H));
    assert!(o.enabled);
    assert_eq!(o.mode, "GOVERNANCE");
    assert_eq!(o.period, "720h", "status must be comparable to spec");

    let off = observed_blob_retention(&retention_off());
    assert!(!off.enabled);
    assert_eq!(off.mode, "");
}

#[test]
fn a_work_spec_without_blob_retention_decodes_as_unmanaged() {
    // Old work-spec JSON (and every repository that never declares retention) must decode
    // to None — "leave it alone" — and NOT to a default that would read as "disable".
    let op: BootstrapRepositoryOp = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(op.blob_retention.is_none());
    assert!(
        blob_retention_drift(
            op.blob_retention.as_ref(),
            Some(&retention_on("GOVERNANCE", NS_720H))
        )
        .is_none()
    );
}
