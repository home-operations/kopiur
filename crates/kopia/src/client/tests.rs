use super::*;

#[test]
fn filesystem_backend_args() {
    let spec = ConnectSpec::Filesystem {
        path: PathBuf::from("/repo"),
    };
    assert_eq!(spec.backend_args(), vec!["filesystem", "--path", "/repo"]);
}

#[test]
fn s3_backend_args_minimal() {
    let spec = ConnectSpec::S3 {
        bucket: "b".into(),
        endpoint: None,
        prefix: None,
        region: None,
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
        root_ca_pem: None,
    };
    assert_eq!(spec.backend_args(), vec!["s3", "--bucket", "b"]);
}

#[test]
fn s3_backend_args_ambient_credentials_pass_empty_key_flags() {
    // Workload identity: kopia 0.23 requires --access-key/--secret-access-key
    // at flag-parse time, but its storage layer skips empty static creds and
    // falls through minio-go's ambient chain (IRSA / Pod Identity / IMDS).
    // The flags must be the exact `=`-joined empty tokens.
    let spec = ConnectSpec::S3 {
        bucket: "b".into(),
        endpoint: None,
        prefix: None,
        region: Some("us-east-1".into()),
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: true,
        root_ca_pem: None,
    };
    assert_eq!(
        spec.backend_args(),
        vec![
            "s3",
            "--bucket",
            "b",
            "--region",
            "us-east-1",
            "--access-key=",
            "--secret-access-key=",
        ]
    );
}

#[test]
fn s3_backend_args_full() {
    let spec = ConnectSpec::S3 {
        bucket: "b".into(),
        endpoint: Some("https://minio".into()),
        prefix: Some("kopiur/".into()),
        region: Some("us-east-1".into()),
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
        root_ca_pem: None,
    };
    assert_eq!(
        spec.backend_args(),
        vec![
            "s3",
            "--bucket",
            "b",
            "--endpoint",
            "https://minio",
            "--prefix",
            "kopiur/",
            "--region",
            "us-east-1"
        ]
    );
}

#[test]
fn s3_backend_args_root_ca_pem_emits_base64_flag_only_when_set() {
    use base64::Engine as _;
    const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBfake\n-----END CERTIFICATE-----\n";
    // Set: the PEM rides argv base64-encoded (`--root-ca-pem-base64`) — a CA
    // certificate is public key material, and kopia persists it into the
    // connection config so every subsequent verb (including the exec'd
    // server) inherits the trust without re-passing the flag.
    let spec = ConnectSpec::S3 {
        bucket: "b".into(),
        endpoint: Some("https://minio.internal".into()),
        prefix: None,
        region: None,
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
        root_ca_pem: Some(PEM.into()),
    };
    let args = spec.backend_args();
    let flag_at = args
        .iter()
        .position(|a| a == "--root-ca-pem-base64")
        .expect("--root-ca-pem-base64 must be emitted when the CA is set");
    assert_eq!(
        args[flag_at + 1],
        base64::engine::general_purpose::STANDARD.encode(PEM),
        "the flag value must be the standard-base64 of the exact PEM"
    );

    // None: no such flag — TLS verification stays on the system trust store.
    let plain = ConnectSpec::S3 {
        bucket: "b".into(),
        endpoint: Some("https://minio.internal".into()),
        prefix: None,
        region: None,
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
        root_ca_pem: None,
    };
    assert!(
        !plain
            .backend_args()
            .iter()
            .any(|a| a.contains("--root-ca-pem")),
        "no CA flag may appear when root_ca_pem is None: {:?}",
        plain.backend_args()
    );
}

#[test]
fn s3_backend_args_disable_tls_flags() {
    // Plain-HTTP endpoint (in-cluster MinIO/RustFS): emit --disable-tls.
    let spec = ConnectSpec::S3 {
        bucket: "b".into(),
        endpoint: Some("minio:9000".into()),
        prefix: None,
        region: None,
        disable_tls: true,
        disable_tls_verification: true,
        ambient_credentials: false,
        root_ca_pem: None,
    };
    let args = spec.backend_args();
    assert!(args.contains(&"--disable-tls".to_string()));
    assert!(args.contains(&"--disable-tls-verification".to_string()));
}

#[test]
fn extract_json_skips_leading_progress() {
    let out = "Snapshotting root@host:/p ...\n{\"id\":\"abc\"}\n";
    assert_eq!(extract_json(out), Some("{\"id\":\"abc\"}"));
}

#[test]
fn extract_json_array() {
    assert_eq!(
        extract_json("[\n {\"id\":\"x\"}\n]"),
        Some("[\n {\"id\":\"x\"}\n]")
    );
}

#[test]
fn extract_json_none_when_no_brace() {
    assert_eq!(extract_json("Finished quick maintenance.\n"), None);
}

#[test]
fn builder_defaults_binary() {
    let c = KopiaClient::builder().build();
    assert_eq!(c.binary(), &PathBuf::from("kopia"));
}

#[test]
fn builder_always_disables_auto_maintenance() {
    // kopia's hidden default-on `--auto-maintenance` opportunistically runs a
    // maintenance pass as a side effect of ops like `snapshot create`/`delete`/
    // `expire` — and, verified against the pinned kopia 0.23.1 binary, even a
    // bare `policy set` — whenever the connected identity is the repository's
    // designated maintenance owner. Only the Maintenance CR's explicit
    // `maintenance run` may trigger maintenance, so every `KopiaClient` must
    // carry `--no-auto-maintenance` regardless of what the caller configured.
    let c = KopiaClient::builder().build();
    assert!(
        c.common_args().iter().any(|a| a == "--no-auto-maintenance"),
        "{:?}",
        c.common_args()
    );

    // A caller's own common args survive alongside it.
    let c = KopiaClient::builder()
        .common_arg("--some-other-flag")
        .build();
    assert!(c.common_args().iter().any(|a| a == "--some-other-flag"));
    assert!(c.common_args().iter().any(|a| a == "--no-auto-maintenance"));
}

// --- backend_args: one assertion per backend variant. A new ConnectSpec
// variant must be added here (and to kind_str) or these tests fail to cover
// it, preserving the "every backend is wired" guarantee. ---

#[test]
fn azure_backend_args() {
    let spec = ConnectSpec::Azure {
        container: "c".into(),
        storage_account: Some("acct".into()),
        prefix: Some("p/".into()),
    };
    assert_eq!(
        spec.backend_args(),
        vec![
            "azure",
            "--container",
            "c",
            "--storage-account",
            "acct",
            "--prefix",
            "p/"
        ]
    );
    // Optional fields omitted when None.
    let minimal = ConnectSpec::Azure {
        container: "c".into(),
        storage_account: None,
        prefix: None,
    };
    assert_eq!(minimal.backend_args(), vec!["azure", "--container", "c"]);
}

#[test]
fn gcs_and_b2_backend_args() {
    assert_eq!(
        ConnectSpec::Gcs {
            bucket: "b".into(),
            prefix: Some("k/".into()),
            credentials_file: None,
        }
        .backend_args(),
        vec!["gcs", "--bucket", "b", "--prefix", "k/"]
    );
    // The materialized service-account JSON path becomes `--credentials-file`.
    assert_eq!(
        ConnectSpec::Gcs {
            bucket: "b".into(),
            prefix: None,
            credentials_file: Some("/var/cache/kopia/creds/gcs.json".into()),
        }
        .backend_args(),
        vec![
            "gcs",
            "--bucket",
            "b",
            "--credentials-file",
            "/var/cache/kopia/creds/gcs.json"
        ]
    );
    assert_eq!(
        ConnectSpec::B2 {
            bucket: "b".into(),
            prefix: None
        }
        .backend_args(),
        vec!["b2", "--bucket", "b"]
    );
}

#[test]
fn sftp_backend_args() {
    let spec = ConnectSpec::Sftp {
        host: "h".into(),
        path: "/repo".into(),
        port: Some(2222),
        username: Some("u".into()),
        keyfile: Some("/keys/id".into()),
        known_hosts: Some("/keys/known_hosts".into()),
    };
    assert_eq!(
        spec.backend_args(),
        vec![
            "sftp",
            "--host",
            "h",
            "--path",
            "/repo",
            "--port",
            "2222",
            "--username",
            "u",
            "--keyfile",
            "/keys/id",
            "--known-hosts",
            "/keys/known_hosts"
        ]
    );
}

#[test]
fn webdav_rclone_gdrive_backend_args() {
    assert_eq!(
        ConnectSpec::WebDav {
            url: "https://dav".into()
        }
        .backend_args(),
        vec!["webdav", "--url", "https://dav"]
    );
    assert_eq!(
        ConnectSpec::Rclone {
            remote_path: "r:bucket".into(),
            config_file: None,
            startup_timeout: None,
        }
        .backend_args(),
        vec!["rclone", "--remote-path", "r:bucket"]
    );
    // The materialized rclone.conf path is forwarded to rclone via --rclone-args,
    // and the startup timeout is kopia's own connect flag (not an rclone arg).
    assert_eq!(
        ConnectSpec::Rclone {
            remote_path: "r:bucket".into(),
            config_file: Some("/var/cache/kopia/creds/rclone.conf".into()),
            startup_timeout: Some("2m".into()),
        }
        .backend_args(),
        vec![
            "rclone",
            "--remote-path",
            "r:bucket",
            // One token: a separate `--config=…` value would be misparsed as a flag.
            "--rclone-args=--config=/var/cache/kopia/creds/rclone.conf",
            "--rclone-startup-timeout=2m"
        ]
    );
    assert_eq!(
        ConnectSpec::Gdrive {
            folder_id: "fid".into(),
            credentials_file: None,
        }
        .backend_args(),
        vec!["gdrive", "--folder-id", "fid"]
    );
    // The materialized service-account JSON path is passed via --credentials-file.
    assert_eq!(
        ConnectSpec::Gdrive {
            folder_id: "fid".into(),
            credentials_file: Some("/var/cache/kopia/creds/gdrive-credentials.json".into()),
        }
        .backend_args(),
        vec![
            "gdrive",
            "--folder-id",
            "fid",
            "--credentials-file",
            "/var/cache/kopia/creds/gdrive-credentials.json"
        ]
    );
}

#[test]
fn from_config_and_server_backend_args() {
    assert_eq!(
        ConnectSpec::FromConfig {
            file: Some("/c.conf".into()),
            token: None
        }
        .backend_args(),
        vec!["from-config", "--file", "/c.conf"]
    );
    assert_eq!(
        ConnectSpec::Server {
            url: "https://srv".into(),
            fingerprint: Some("ab12".into())
        }
        .backend_args(),
        vec![
            "server",
            "--url",
            "https://srv",
            "--server-cert-fingerprint",
            "ab12"
        ]
    );
}

#[test]
fn cache_tuning_args_and_serde() {
    // Unset → no flags (kopia defaults).
    assert!(CacheTuning::default().is_unset());
    assert!(CacheTuning::default().args().is_empty());
    // Set budgets → stable content-then-metadata flag order.
    let t = CacheTuning {
        content_cache_size_mb: Some(8192),
        metadata_cache_size_mb: Some(2048),
    };
    assert!(!t.is_unset());
    assert_eq!(
        t.args(),
        vec![
            "--content-cache-size-mb",
            "8192",
            "--metadata-cache-size-mb",
            "2048",
        ]
    );
    // Only one set → only that flag.
    assert_eq!(
        CacheTuning {
            metadata_cache_size_mb: Some(512),
            ..Default::default()
        }
        .args(),
        vec!["--metadata-cache-size-mb", "512"]
    );
    // camelCase wire shape, round-trips, and omits unset fields.
    let json = serde_json::to_value(t).unwrap();
    assert_eq!(json["contentCacheSizeMb"], 8192);
    assert_eq!(json["metadataCacheSizeMb"], 2048);
    assert_eq!(
        serde_json::to_value(CacheTuning::default()).unwrap(),
        serde_json::json!({})
    );
    let back: CacheTuning = serde_json::from_value(json).unwrap();
    assert_eq!(back, t);
}

#[test]
fn kind_str_covers_every_variant() {
    // Exhaustiveness witness: each variant yields a distinct, stable string.
    let all = [
        ConnectSpec::Filesystem { path: "/r".into() },
        ConnectSpec::S3 {
            bucket: "b".into(),
            endpoint: None,
            prefix: None,
            region: None,
            disable_tls: false,
            disable_tls_verification: false,
            ambient_credentials: false,
            root_ca_pem: None,
        },
        ConnectSpec::Azure {
            container: "c".into(),
            storage_account: None,
            prefix: None,
        },
        ConnectSpec::Gcs {
            bucket: "b".into(),
            prefix: None,
            credentials_file: None,
        },
        ConnectSpec::B2 {
            bucket: "b".into(),
            prefix: None,
        },
        ConnectSpec::Sftp {
            host: "h".into(),
            path: "/p".into(),
            port: None,
            username: None,
            keyfile: None,
            known_hosts: None,
        },
        ConnectSpec::WebDav { url: "u".into() },
        ConnectSpec::Rclone {
            remote_path: "r".into(),
            config_file: None,
            startup_timeout: None,
        },
        ConnectSpec::Gdrive {
            folder_id: "f".into(),
            credentials_file: None,
        },
        ConnectSpec::FromConfig {
            file: None,
            token: None,
        },
        ConnectSpec::Server {
            url: "u".into(),
            fingerprint: None,
        },
    ];
    let kinds: Vec<&str> = all.iter().map(|s| s.kind_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "filesystem",
            "s3",
            "azure",
            "gcs",
            "b2",
            "sftp",
            "webdav",
            "rclone",
            "gdrive",
            "from-config",
            "server"
        ]
    );
}

// --- verb arg builders ---

#[test]
fn restore_args_default_is_bare() {
    assert_eq!(
        restore_args("snap1", "/data", &RestoreOptions::default()),
        vec!["snapshot", "restore", "snap1", "/data"]
    );
}

#[test]
fn restore_args_tristate_and_flags() {
    let opts = RestoreOptions {
        ignore_permission_errors: Some(false),
        write_files_atomically: Some(true),
        overwrite_files: Some(false),
        skip_existing: Some(true),
        parallel: Some(4),
        ..Default::default()
    };
    assert_eq!(
        restore_args("s", "/t", &opts),
        vec![
            "snapshot",
            "restore",
            "s",
            "/t",
            "--no-ignore-permission-errors",
            "--write-files-atomically",
            "--no-overwrite-files",
            "--skip-existing",
            "--parallel",
            "4"
        ]
    );
}

#[test]
fn restore_args_m2_flag_sweep_all_new_tristates_and_delete_extra() {
    // M2 flag sweep (issue #216 gap analysis): every new tri-state, in the
    // `Some(false)` → `--no-*` form, plus `delete_extra` — the
    // `enableFileDeletion` bug-fix's client-layer regression guard.
    let opts = RestoreOptions {
        overwrite_directories: Some(false),
        overwrite_symlinks: Some(false),
        write_sparse_files: Some(false),
        skip_owners: Some(false),
        skip_permissions: Some(false),
        skip_times: Some(false),
        ignore_errors: Some(false),
        delete_extra: Some(false),
        ..Default::default()
    };
    assert_eq!(
        restore_args("s", "/t", &opts),
        vec![
            "snapshot",
            "restore",
            "s",
            "/t",
            "--no-overwrite-directories",
            "--no-overwrite-symlinks",
            "--no-write-sparse-files",
            "--no-skip-owners",
            "--no-skip-permissions",
            "--no-skip-times",
            "--no-ignore-errors",
            "--no-delete-extra",
        ]
    );

    // The `Some(true)` form emits the bare (non-negated) flag — this is the
    // regression test for the `enableFileDeletion` bug: today's code has NO
    // path that can ever produce `--delete-extra` on argv.
    let all_true = RestoreOptions {
        overwrite_directories: Some(true),
        overwrite_symlinks: Some(true),
        write_sparse_files: Some(true),
        skip_owners: Some(true),
        skip_permissions: Some(true),
        skip_times: Some(true),
        ignore_errors: Some(true),
        skip_existing: Some(true),
        delete_extra: Some(true),
        ..Default::default()
    };
    let argv = restore_args("s", "/t", &all_true);
    for flag in [
        "--overwrite-directories",
        "--overwrite-symlinks",
        "--write-sparse-files",
        "--skip-owners",
        "--skip-permissions",
        "--skip-times",
        "--ignore-errors",
        "--skip-existing",
        "--delete-extra",
    ] {
        assert!(
            argv.contains(&flag.to_string()),
            "{flag} missing in {argv:?}"
        );
    }

    // All-`None` (the zero-value default) is byte-for-byte identical to the
    // pre-M2 bare argv (no dormant knob sneaks a flag in when nothing was set).
    assert_eq!(
        restore_args("s", "/t", &RestoreOptions::default()),
        vec!["snapshot", "restore", "s", "/t"]
    );
}

#[test]
fn snapshot_create_args_default_is_todays_argv() {
    // M4 flag sweep (issue #216 category sweep): all-default `opts` must
    // reproduce the pre-M4 argv byte-for-byte — no dormant knob sneaks a flag
    // in when nothing was set.
    let mut tags = BTreeMap::new();
    tags.insert("app".to_string(), "db".to_string());
    assert_eq!(
        snapshot_create_args("/data", &tags, None, &SnapshotCreateOptions::default()),
        vec!["snapshot", "create", "/data", "--json", "--tags", "app:db"]
    );
    assert_eq!(
        snapshot_create_args(
            "/data",
            &BTreeMap::new(),
            Some("u@h:/data"),
            &SnapshotCreateOptions::default()
        ),
        vec![
            "snapshot",
            "create",
            "/data",
            "--json",
            "--override-source",
            "u@h:/data"
        ]
    );
}

#[test]
fn snapshot_create_args_fail_fast_upload_limit_and_description() {
    // Smoke-tested against pinned kopia 0.23.1: `snapshot create --fail-fast
    // --upload-limit-mb 100 --description "smoke test"` is accepted.
    let opts = SnapshotCreateOptions {
        fail_fast: Some(true),
        upload_limit_mb: Some(100),
        description: Some("smoke test".to_string()),
    };
    assert_eq!(
        snapshot_create_args("/data", &BTreeMap::new(), None, &opts),
        vec![
            "snapshot",
            "create",
            "/data",
            "--json",
            "--fail-fast",
            "--upload-limit-mb",
            "100",
            "--description",
            "smoke test"
        ]
    );
    // `fail_fast: Some(false)` emits the negated kingpin form, same grammar as
    // `snapshot restore`'s tri-states (push_tristate), not `policy set`'s
    // valued tri-states.
    let opts_false = SnapshotCreateOptions {
        fail_fast: Some(false),
        ..Default::default()
    };
    assert_eq!(
        snapshot_create_args("/data", &BTreeMap::new(), None, &opts_false),
        vec!["snapshot", "create", "/data", "--json", "--no-fail-fast"]
    );
}

#[test]
fn verify_args_builds_flags() {
    assert_eq!(
        verify_args(&VerifyOptions::default()),
        vec!["snapshot", "verify"]
    );
    let opts = VerifyOptions {
        sources: vec!["app-config@app:/pvc/app-config".into()],
        verify_files_percent: Some(10),
        max_errors: Some(3),
        parallel: Some(8),
        file_parallelism: None,
        file_queue_length: None,
    };
    assert_eq!(
        verify_args(&opts),
        vec![
            "snapshot",
            "verify",
            // `--sources` scopes the verify to one policy's identity (issue #250);
            // emitted first, in kopia's `snapshot verify --help` flag order.
            "--sources",
            "app-config@app:/pvc/app-config",
            "--verify-files-percent",
            "10",
            "--max-errors",
            "3",
            "--parallel",
            "8"
        ]
    );

    // The two new knobs (M3 / issue #216 category sweep) — each independently absent
    // by default, and both emitted in kopia's `snapshot verify --help` order when set.
    let opts_full = VerifyOptions {
        // Empty `sources` (the default) must NOT emit `--sources`, even when
        // other flags are set — the whole-repository verify is still reachable.
        sources: vec![],
        verify_files_percent: None,
        max_errors: Some(1),
        parallel: Some(2),
        file_parallelism: Some(4),
        file_queue_length: Some(100),
    };
    assert_eq!(
        verify_args(&opts_full),
        vec![
            "snapshot",
            "verify",
            "--max-errors",
            "1",
            "--parallel",
            "2",
            "--file-parallelism",
            "4",
            "--file-queue-length",
            "100"
        ]
    );
}

#[test]
fn direct_credential_env_names_per_backend() {
    let s3 = ConnectSpec::S3 {
        bucket: "b".into(),
        endpoint: None,
        prefix: None,
        region: None,
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
        root_ca_pem: None,
    };
    assert_eq!(
        s3.direct_credential_env_names(),
        &[
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN"
        ]
    );
    // Ambient-chain hints are NOT listed (they belong to the pod SA, not a Secret).
    assert!(
        !s3.direct_credential_env_names()
            .contains(&"AWS_WEB_IDENTITY_TOKEN_FILE")
    );

    let b2 = ConnectSpec::B2 {
        bucket: "b".into(),
        prefix: None,
    };
    assert_eq!(b2.direct_credential_env_names(), &["B2_KEY_ID", "B2_KEY"]);

    // File-delivered and credential-free backends read no direct credential env var.
    let gcs = ConnectSpec::Gcs {
        bucket: "b".into(),
        prefix: None,
        credentials_file: None,
    };
    assert!(gcs.direct_credential_env_names().is_empty());
    let fs = ConnectSpec::Filesystem {
        path: "/repo".into(),
    };
    assert!(fs.direct_credential_env_names().is_empty());
}

#[test]
fn sync_to_args_builds_destination_and_flags() {
    // ADR-0005 §13(d) / issue #216: destination backend args + tuning flags.
    // All-`None`/`false` opts must yield EXACTLY today's argv (no dormant knob
    // sneaks a flag in when nothing was configured).
    let dest = ConnectSpec::S3 {
        bucket: "mirror".into(),
        endpoint: Some("https://offsite".into()),
        prefix: None,
        region: Some("us-east-1".into()),
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
        root_ca_pem: None,
    };
    assert_eq!(
        sync_to_args(&dest, &SyncToOptions::default()),
        vec![
            "repository",
            "sync-to",
            "s3",
            "--bucket",
            "mirror",
            "--endpoint",
            "https://offsite",
            "--region",
            "us-east-1",
        ]
    );
    // No `must-exist`/`times`/`update` flag at all when unset.
    assert!(
        !sync_to_args(&dest, &SyncToOptions::default())
            .iter()
            .any(|a| a.contains("must-exist") || a.contains("times") || a.contains("update"))
    );
    // delete_extra appends --delete (a true mirror).
    let fs = ConnectSpec::Filesystem {
        path: "/mirror".into(),
    };
    assert_eq!(
        sync_to_args(
            &fs,
            &SyncToOptions {
                delete_extra: true,
                ..Default::default()
            }
        ),
        vec![
            "repository",
            "sync-to",
            "filesystem",
            "--path",
            "/mirror",
            "--delete"
        ]
    );
}

#[test]
fn sync_to_args_builds_parallel_and_tristates_and_speeds() {
    // #216: --parallel is the headline fix; the tri-states use the SAME
    // --no-* negated form as `snapshot restore` (push_tristate), not the
    // `policy set` `--flag=false` grammar.
    let fs = ConnectSpec::Filesystem {
        path: "/mirror".into(),
    };
    let opts = SyncToOptions {
        parallel: Some(8),
        delete_extra: false,
        must_exist: Some(false),
        times: Some(true),
        update: Some(false),
        max_download_speed_bytes_per_second: Some(1_000_000),
        max_upload_speed_bytes_per_second: Some(500_000),
    };
    assert_eq!(
        sync_to_args(&fs, &opts),
        vec![
            "repository",
            "sync-to",
            "filesystem",
            "--path",
            "/mirror",
            "--parallel",
            "8",
            "--no-must-exist",
            "--times",
            "--no-update",
            "--max-download-speed",
            "1000000",
            "--max-upload-speed",
            "500000",
        ]
    );
    // The `Some(true)` form emits the bare (non-negated) flag.
    assert!(
        sync_to_args(
            &fs,
            &SyncToOptions {
                must_exist: Some(true),
                ..Default::default()
            }
        )
        .contains(&"--must-exist".to_string())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sync_to_env_overlay_sets_destination_and_unsets_source_only_vars() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    // A shim standing in for `kopia`: it records the two credential env vars it was
    // spawned with, then exits 0 (a successful sync-to).
    let dir = std::env::temp_dir().join(format!("kopiur-syncenv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("kopia");
    let out = dir.join("env.out");
    {
        let mut f = std::fs::File::create(&shim).unwrap();
        write!(
            f,
            "#!/bin/sh\necho \"KEY=${{AWS_ACCESS_KEY_ID:-<unset>}} TOKEN=${{AWS_SESSION_TOKEN:-<unset>}}\" > \"$KOPIUR_SYNCENV_OUT\"\nexit 0\n"
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Source credentials arrive as common_env (as they do in the mover pod).
    let client = KopiaClient::builder()
        .binary(&shim)
        .env("AWS_ACCESS_KEY_ID", "source-key")
        .env("AWS_SESSION_TOKEN", "source-token")
        .env("KOPIUR_SYNCENV_OUT", out.to_str().unwrap())
        .build();
    let dest = ConnectSpec::Filesystem {
        path: "/mirror".into(),
    };

    // No overlay → the source credentials pass through unchanged.
    client
        .repository_sync_to(&dest, &SyncToOptions::default())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&out).unwrap().trim(),
        "KEY=source-key TOKEN=source-token"
    );

    // Overlay → the destination key replaces the source's, and the session token
    // (which the destination does not set) is unset so it can't leak.
    let overlay = BTreeMap::from([
        (
            "AWS_ACCESS_KEY_ID".to_string(),
            Some("dest-key".to_string()),
        ),
        ("AWS_SESSION_TOKEN".to_string(), None),
    ]);
    client
        .repository_sync_to_with_env(&dest, &SyncToOptions::default(), &overlay)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&out).unwrap().trim(),
        "KEY=dest-key TOKEN=<unset>"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn connect_args_appends_readonly_only_for_readonly_connects() {
    let spec = ConnectSpec::Filesystem {
        path: PathBuf::from("/repo"),
    };
    // Read-write (every mover but browse): NO --readonly anywhere.
    assert_eq!(
        connect_args(&spec, CacheTuning::default(), false),
        vec!["repository", "connect", "filesystem", "--path", "/repo"]
    );
    // Read-only (browse sessions): --readonly appended after backend+cache args.
    assert_eq!(
        connect_args(&spec, CacheTuning::default(), true),
        vec![
            "repository",
            "connect",
            "filesystem",
            "--path",
            "/repo",
            "--readonly"
        ]
    );
    // Cache tuning args still precede the flag.
    let tuned = connect_args(
        &spec,
        CacheTuning {
            content_cache_size_mb: Some(100),
            metadata_cache_size_mb: None,
        },
        true,
    );
    assert_eq!(
        tuned.last().map(String::as_str),
        Some("--readonly"),
        "{tuned:?}"
    );
    assert!(tuned.iter().any(|a| a == "--content-cache-size-mb"));
}

#[test]
fn policy_set_args_builds_keep_flags() {
    // M0b: the six create-time retention `--keep-*` fields, pinned to
    // kopia's largest safely round-tripping value at the identity scope
    // (mover's `KOPIA_KEEP_MAX`). Space-separated valued flags (verified
    // against the pinned kopia 0.23.1: `--keep-latest 2147483647` is
    // accepted; unlike the `--flag=value` grammar of the tri-state knobs
    // below, these are plain `--flag N` like `--max-parallel-snapshots`).
    let policy = PolicyArgs {
        keep_latest: Some(2_147_483_647),
        keep_hourly: Some(2_147_483_647),
        keep_daily: Some(2_147_483_647),
        keep_weekly: Some(2_147_483_647),
        keep_monthly: Some(2_147_483_647),
        keep_annual: Some(2_147_483_647),
        ..Default::default()
    };
    assert_eq!(
        policy_set_args("user@host", &policy),
        vec![
            "policy",
            "set",
            "user@host",
            "--keep-latest",
            "2147483647",
            "--keep-hourly",
            "2147483647",
            "--keep-daily",
            "2147483647",
            "--keep-weekly",
            "2147483647",
            "--keep-monthly",
            "2147483647",
            "--keep-annual",
            "2147483647",
        ]
    );
    // Unset (the common case for a path-scoped policy) emits nothing.
    assert_eq!(
        policy_set_args("--global", &PolicyArgs::default()),
        vec!["policy", "set", "--global"]
    );
}

#[test]
fn policy_set_args_emits_ignore_identical_snapshots_as_a_valued_tristate() {
    // kopia's `--ignore-identical-snapshots` takes a VALUE (`true|false|inherit`),
    // like the other policy tri-states and unlike the `--keep-*` pairs. A bare
    // `--ignore-identical-snapshots` fails with "expected argument for flag".
    //
    // `false` must be emitted EXPLICITLY, not by omission: omitting it inherits,
    // and inheriting a repository-global `true` is exactly how #351 was
    // reachable without the CRD field ever being wired.
    let off = PolicyArgs {
        ignore_identical_snapshots: Some(false),
        ..Default::default()
    };
    assert_eq!(
        policy_set_args("user@host", &off),
        vec![
            "policy",
            "set",
            "user@host",
            "--ignore-identical-snapshots=false",
        ]
    );
    let on = PolicyArgs {
        ignore_identical_snapshots: Some(true),
        ..Default::default()
    };
    assert_eq!(
        policy_set_args("user@host:/pvc/data", &on),
        vec![
            "policy",
            "set",
            "user@host:/pvc/data",
            "--ignore-identical-snapshots=true",
        ]
    );
    // Unset emits nothing at all (kopia inherits).
    assert_eq!(
        policy_set_args("--global", &PolicyArgs::default()),
        vec!["policy", "set", "--global"]
    );
}

#[test]
fn policy_set_args_builds_flags() {
    let policy = PolicyArgs {
        compression: Some("zstd".into()),
        splitter: Some("DYNAMIC-4M-BUZHASH".into()),
        ignore: vec!["*.tmp".into(), "cache/".into()],
        never_compress: vec!["*.gz".into()],
        extra_args: vec!["--one-file-system".into()],
        ..Default::default()
    };
    assert_eq!(
        policy_set_args("user@host:/p", &policy),
        vec![
            "policy",
            "set",
            "user@host:/p",
            "--compression",
            "zstd",
            "--splitter",
            "DYNAMIC-4M-BUZHASH",
            "--add-ignore",
            "*.tmp",
            "--add-ignore",
            "cache/",
            "--add-never-compress",
            "*.gz",
            "--one-file-system"
        ]
    );
    // Empty policy is just the bare command.
    assert_eq!(
        policy_set_args("--global", &PolicyArgs::default()),
        vec!["policy", "set", "--global"]
    );
}

#[test]
fn tristate_helpers_match_each_command_grammar() {
    // `snapshot restore` flags are kingpin `--[no-]flag` booleans…
    let mut a = Vec::new();
    push_tristate(&mut a, "flag", Some(true));
    push_tristate(&mut a, "flag", Some(false));
    push_tristate(&mut a, "flag", None);
    assert_eq!(a, vec!["--flag", "--no-flag"]);
    // …while `policy set` knobs are VALUED flags (`--flag=true|false`,
    // verified against `kopia policy set --help` 0.23). The bare/`--no-`
    // forms are rejected with "expected argument for flag" — the
    // policy_knobs e2e regression.
    let mut a = Vec::new();
    push_valued_tristate(&mut a, "flag", Some(true));
    push_valued_tristate(&mut a, "flag", Some(false));
    push_valued_tristate(&mut a, "flag", None);
    assert_eq!(a, vec!["--flag=true", "--flag=false"]);
}

#[test]
fn split_policy_scopes_moves_max_parallel_snapshots_to_identity() {
    // kopia: "max parallel snapshots cannot be specified for paths, only
    // global, username@hostname or @hostname" (the policy_knobs e2e
    // regression) — that one knob must be applied at the identity scope.
    let policy = PolicyArgs {
        compression: Some("zstd".into()),
        max_parallel_snapshots: Some(2),
        max_parallel_file_reads: Some(4),
        ..Default::default()
    };
    let (path, identity) = split_policy_scopes(policy);
    assert_eq!(path.compression.as_deref(), Some("zstd"));
    // file-reads IS path-legal and stays put; snapshots moves out.
    assert_eq!(path.max_parallel_file_reads, Some(4));
    assert_eq!(path.max_parallel_snapshots, None);
    let identity = identity.expect("identity-scoped policy present");
    assert_eq!(
        identity,
        PolicyArgs {
            max_parallel_snapshots: Some(2),
            ..Default::default()
        }
    );

    // Without the knob there is no identity-scoped policy at all.
    let (path, identity) = split_policy_scopes(PolicyArgs {
        compression: Some("zstd".into()),
        ..Default::default()
    });
    assert_eq!(path.compression.as_deref(), Some("zstd"));
    assert!(identity.is_none());
}

#[test]
fn policy_set_args_builds_error_handling_and_upload_flags() {
    // ADR-0005 §13(b)/§13(f): backup-side error handling + upload parallelism.
    let policy = PolicyArgs {
        ignore_cache_dirs: Some(true),
        ignore_file_errors: Some(true),
        ignore_dir_errors: Some(false),
        ignore_unknown_types: Some(true),
        max_parallel_snapshots: Some(4),
        max_parallel_file_reads: Some(8),
        ..Default::default()
    };
    assert_eq!(
        policy_set_args("u@h:/p", &policy),
        vec![
            "policy",
            "set",
            "u@h:/p",
            "--ignore-cache-dirs=true",
            "--ignore-file-errors=true",
            "--ignore-dir-errors=false",
            "--ignore-unknown-types=true",
            "--max-parallel-snapshots",
            "4",
            "--max-parallel-file-reads",
            "8"
        ]
    );
}

#[test]
fn create_options_args_builds_ecc_and_algos() {
    // ADR-0005 §13(a): ECC + create-time algorithms.
    let opts = CreateOptions {
        encryption: Some("AES256-GCM-HMAC-SHA256".into()),
        splitter: Some("DYNAMIC-4M-BUZHASH".into()),
        hash: Some("BLAKE2B-256".into()),
        ecc: Some("REED-SOLOMON-CRC32".into()),
        ecc_overhead_percent: Some(2),
    };
    assert_eq!(
        opts.args(),
        vec![
            "--encryption",
            "AES256-GCM-HMAC-SHA256",
            "--object-splitter",
            "DYNAMIC-4M-BUZHASH",
            "--block-hash",
            "BLAKE2B-256",
            "--ecc",
            "REED-SOLOMON-CRC32",
            "--ecc-overhead-percent",
            "2"
        ]
    );
    // Empty options ⇒ no flags.
    assert!(CreateOptions::default().args().is_empty());
}

#[test]
fn throttle_args_builds_per_second_flags_and_empties() {
    // ADR-0005 §13(e).
    let t = ThrottleArgs {
        upload_bytes_per_second: Some(10_000_000),
        download_bytes_per_second: Some(20_000_000),
        read_ops_per_second: Some(50),
        write_ops_per_second: Some(25),
    };
    assert_eq!(
        t.args(),
        vec![
            "--upload-bytes-per-second",
            "10000000",
            "--download-bytes-per-second",
            "20000000",
            "--read-requests-per-second",
            "50",
            "--write-requests-per-second",
            "25"
        ]
    );
    assert!(!t.is_empty());
    assert!(ThrottleArgs::default().is_empty());
    assert!(ThrottleArgs::default().args().is_empty());
}

#[test]
fn server_start_args_password_mode() {
    let spec = ServerStartSpec {
        address: "0.0.0.0:51515".into(),
        auth: ServerAuthMode::Password {
            username: "kopia".into(),
        },
        ui: true,
    };
    assert_eq!(
        server_start_args(&spec),
        vec![
            "server",
            "start",
            "--address",
            "0.0.0.0:51515",
            "--insecure",
            "--ui",
            "--server-username",
            "kopia",
        ]
    );
    // The password is NEVER in the pure builder output.
    assert!(
        !server_start_args(&spec)
            .iter()
            .any(|a| a.contains("password"))
    );
}

#[test]
fn server_start_args_no_auth_mode() {
    let spec = ServerStartSpec {
        address: "0.0.0.0:8080".into(),
        auth: ServerAuthMode::None,
        ui: false,
    };
    // No --ui when disabled; no-auth emits --without-password (with --insecure)
    // plus kopia 0.23's escape hatch for binding a non-loopback address.
    assert_eq!(
        server_start_args(&spec),
        vec![
            "server",
            "start",
            "--address",
            "0.0.0.0:8080",
            "--insecure",
            "--without-password",
            "--allow-extremely-dangerous-unauthenticated-server-on-the-network",
        ]
    );
}

#[test]
fn server_start_args_always_insecure() {
    // --insecure (no-TLS) is required in EVERY mode.
    for auth in [
        ServerAuthMode::None,
        ServerAuthMode::Password {
            username: "u".into(),
        },
    ] {
        let spec = ServerStartSpec {
            address: "0.0.0.0:51515".into(),
            auth,
            ui: true,
        };
        assert!(server_start_args(&spec).iter().any(|a| a == "--insecure"));
    }
}

#[test]
fn set_parameters_args_render_every_epoch_flag_in_a_stable_order() {
    let opts = SetParametersArgs {
        epoch_min_duration: Some("6h".into()),
        epoch_refresh_frequency: Some("20m".into()),
        epoch_advance_on_count: Some(20),
        epoch_advance_on_size_mb: Some(10),
        epoch_checkpoint_frequency: Some(7),
        epoch_delete_parallelism: Some(4),
        // Exhaustive on purpose (no `..Default::default()`): this test is the change-detector
        // that forces a new set-parameters flag to be positioned deliberately in `args()`.
        retention_mode: None,
        retention_period: None,
    };
    assert_eq!(
        opts.args(),
        vec![
            "--epoch-min-duration",
            "6h",
            "--epoch-refresh-frequency",
            "20m",
            "--epoch-advance-on-count",
            "20",
            "--epoch-advance-on-size-mb",
            "10",
            "--epoch-checkpoint-frequency",
            "7",
            "--epoch-delete-parallelism",
            "4"
        ]
    );
    // Nothing set ⇒ no flags, and `repository_set_parameters` then skips the whole
    // invocation. That is what keeps a repository that never mentions spec.parameters
    // completely untouched by this feature.
    assert!(SetParametersArgs::default().args().is_empty());
    assert!(SetParametersArgs::default().is_empty());
}

#[test]
fn set_parameters_emits_only_the_flags_that_are_set() {
    // The drift comparator sends ONLY the parameters that actually differ, so a partial
    // set must not smuggle defaults in for the rest.
    let opts = SetParametersArgs {
        epoch_min_duration: Some("6h".into()),
        ..Default::default()
    };
    assert_eq!(opts.args(), vec!["--epoch-min-duration", "6h"]);
}

#[test]
fn set_parameters_durations_always_carry_a_unit() {
    // kopia's time.ParseDuration REJECTS a bare number: `--epoch-min-duration=3600` fails
    // with `time: missing unit in duration "3600"`. Durations reach this builder
    // pre-rendered (kopiur_api::render_go_duration) precisely so that cannot happen —
    // this pins the contract at the boundary where it would otherwise be violated.
    let opts = SetParametersArgs {
        epoch_min_duration: Some("6h".into()),
        epoch_refresh_frequency: Some("20m".into()),
        retention_period: Some("720h".into()),
        retention_mode: None,
        ..Default::default()
    };
    for v in opts.args().iter().filter(|a| !a.starts_with("--")) {
        assert!(
            v.ends_with(['h', 'm', 's']),
            "a duration passed to kopia must carry a unit, got {v:?}"
        );
    }
}

#[test]
fn set_parameters_renders_the_blob_retention_flags() {
    let opts = SetParametersArgs {
        retention_mode: Some("GOVERNANCE".into()),
        retention_period: Some("720h".into()),
        ..Default::default()
    };
    assert_eq!(
        opts.args(),
        vec![
            "--retention-mode",
            "GOVERNANCE",
            "--retention-period",
            "720h"
        ]
    );

    // Retention rides the SAME invocation as the epoch flags — `set-parameters` rewrites
    // the format blob and invalidates every other client's cached copy, so the two must
    // never be applied as two separate commands.
    let both = SetParametersArgs {
        epoch_min_duration: Some("6h".into()),
        retention_mode: Some("COMPLIANCE".into()),
        retention_period: Some("8760h".into()),
        ..Default::default()
    };
    assert_eq!(
        both.args(),
        vec![
            "--epoch-min-duration",
            "6h",
            "--retention-mode",
            "COMPLIANCE",
            "--retention-period",
            "8760h"
        ],
        "epoch flags stay first so the existing positional assertions keep holding"
    );
}

#[test]
fn set_parameters_disable_emits_the_mode_alone_and_is_not_empty() {
    // Disabling is mode-only: kopia's `--retention-mode=none` path clears mode AND period
    // and short-circuits before its own validation, so sending a period would be noise.
    let off = SetParametersArgs {
        retention_mode: Some("none".into()),
        ..Default::default()
    };
    assert_eq!(off.args(), vec!["--retention-mode", "none"]);
    // Load-bearing: `repository_set_parameters` early-returns on `is_empty()`. If disabling
    // read as empty, turning retention OFF would silently no-op forever.
    assert!(
        !off.is_empty(),
        "a disable must still invoke set-parameters"
    );

    // And the genuinely-inert case still skips the invocation entirely.
    assert!(SetParametersArgs::default().is_empty());
}

// --- timed-out subprocess reaping (Greptile P1 on PR #287): the timeout branch
// used `start_kill()` and returned immediately, so the SIGKILLed kopia child was
// never `wait()`ed — a zombie per attempt, for the parent's whole lifetime. With
// the controller's 120s default_timeout, a hung backend would mint zombies on
// every retry. The child must be killed AND reaped before Timeout returns. ---
#[cfg(target_os = "linux")]
#[tokio::test]
async fn timed_out_child_is_killed_and_reaped_not_left_a_zombie() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    // A shim standing in for `kopia`: records its PID, then hangs far past the
    // timeout.
    let dir = std::env::temp_dir().join(format!("kopiur-reap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("kopia");
    let pid_file = dir.join("pid");
    let _ = std::fs::remove_file(&pid_file);
    {
        let mut f = std::fs::File::create(&shim).unwrap();
        write!(f, "#!/bin/sh\necho $$ > \"$KOPIUR_REAP_PID\"\nsleep 300\n").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let client = KopiaClient::builder()
        .binary(&shim)
        .env("KOPIUR_REAP_PID", pid_file.to_str().unwrap())
        .default_timeout(Duration::from_millis(300))
        .build();

    let err = client
        .run_ok(&["repository".into(), "status".into()])
        .await
        .expect_err("the hanging shim must time out");
    assert!(
        matches!(err, KopiaError::Timeout { .. }),
        "expected Timeout, got {err:?}"
    );

    let pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("the shim wrote its PID before hanging")
        .trim()
        .parse()
        .unwrap();

    // The child must be reaped BEFORE Timeout returns (`kill().await` =
    // SIGKILL + wait), so this immediate — deliberately un-polled — check is
    // deterministic: /proc/<pid> is already gone. On the pre-fix code
    // (`start_kill()` + return) the child is still present here as a dying/
    // zombie process; tokio's SIGCHLD-driven orphan reaper only cleans it up
    // best-effort LATER, and this assertion runs synchronously before the
    // signal driver can — pinning the deterministic-reap contract rather than
    // the racy transient.
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        let state = stat
            .rsplit(") ")
            .next()
            .and_then(|rest| rest.chars().next())
            .unwrap_or('?');
        panic!(
            "timed-out kopia child (pid {pid}) still present (state '{state}') when \
             Timeout returned — killed but not reaped before returning"
        );
    }
}
