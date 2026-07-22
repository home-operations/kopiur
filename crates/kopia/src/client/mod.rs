//! The `tokio::process`-based kopia client.
//!
//! `KopiaClient` is controller-agnostic: it knows how to invoke the `kopia`
//! binary, stream its output, and parse the trailing JSON on stdout into the
//! typed [`crate::model`] structs. It has **no** kube/k8s-openapi dependency
//! (SKILL "keep it controller-agnostic").
//!
//! Per ADR §5.4, kopia prints progress to **stderr** and the `--json` result to
//! **stdout**. We capture both: stdout is parsed as JSON, stderr is retained so
//! a failure can carry the tail of the real error message.
//!
//! Secrets (the repository password) are passed via the environment
//! (`KOPIA_PASSWORD`), never on argv, so they never leak into process listings
//! or error messages.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::error::{KopiaError, KopiaErrorClass, tail_lines};
use crate::model::{
    IndexBlobEntry, MaintenanceInfo, RepositoryStatus, SnapshotCreateResult, SnapshotListEntry,
    SnapshotSource,
};

/// Which maintenance pass to run.
///
/// `Serialize`/`Deserialize` so the mover work-spec can carry the mode as one
/// shared type (no parallel enum in `kopiur-mover`). Wire form is the camelCase
/// variant name (`"quick"` / `"full"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MaintenanceMode {
    /// `kopia maintenance run --no-full` — index compaction, epoch advance.
    Quick,
    /// `kopia maintenance run --full` — content GC + rewrite.
    Full,
}

/// A typed description of how to reach a kopia repository. This is the input to
/// [`KopiaClient::repository_connect`] / [`KopiaClient::repository_create`].
/// Externally-tagged so exactly one backend is representable (mirrors the API
/// crate's `Backend` discipline, though this is a separate, simpler type with
/// no kube dependency).
///
/// ## Credentials are NOT here
///
/// Secrets are supplied two ways, never on argv. Env-delivered secrets (set with
/// [`KopiaClientBuilder::env`]) cover the backends kopia reads from the
/// environment; file-delivered secrets are written to a file by the caller (the
/// mover) and the *path* is passed in the relevant `ConnectSpec` field. Either
/// way only non-secret identifiers (bucket, host, path, …) and file *paths* live
/// in `ConnectSpec`, so a secret never leaks into a ConfigMap, a process listing,
/// or an error message. The relevant kopia inputs by backend:
///   * S3:    `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` (env)
///   * Azure: `AZURE_STORAGE_KEY` / `AZURE_STORAGE_SAS_TOKEN` (env; or SP env)
///   * B2:    `B2_KEY_ID`, `B2_KEY` (env)
///   * WebDAV:`KOPIA_WEBDAV_USERNAME`, `KOPIA_WEBDAV_PASSWORD` (env)
///   * GCS:   `Gcs::credentials_file` → `--credentials-file` (a JSON file path)
///   * SFTP:  `Sftp::keyfile`/`known_hosts` → `--keyfile`/`--known-hosts` (file paths)
///   * rclone:`Rclone::config_file` → rclone `--config` (a file path)
///   * all:   `KOPIA_PASSWORD` (the repository encryption password; env)
///
/// This is the full set of kopia 0.23 `repository connect/create` backends. The
/// operator's CRD `Backend` enum maps onto the first eight; `Gdrive`,
/// `FromConfig`, and `Server` are exposed for client completeness (a kopia
/// client connecting to an existing kopia API server is a legitimate backend —
/// distinct from *running* a server, which the operator deliberately does not do).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectSpec {
    /// Filesystem backend at a local path (used in-cluster for hostPath/PVC
    /// repos and in tests).
    Filesystem {
        /// Absolute path to the repository root.
        path: PathBuf,
    },
    /// S3-compatible backend.
    S3 {
        /// Bucket name.
        bucket: String,
        /// Optional custom endpoint (for MinIO / non-AWS).
        endpoint: Option<String>,
        /// Optional key prefix within the bucket.
        prefix: Option<String>,
        /// Region, if required by the endpoint.
        region: Option<String>,
        /// Talk plain HTTP to the endpoint (`--disable-tls`). For HTTP-only
        /// endpoints (in-cluster MinIO/RustFS); kopia otherwise assumes HTTPS.
        disable_tls: bool,
        /// Skip TLS certificate verification (`--disable-tls-verification`).
        disable_tls_verification: bool,
        /// Authenticate via the ambient AWS credential chain (IRSA web-identity,
        /// EKS Pod Identity, IMDS) instead of static keys — the workload-identity
        /// path. kopia 0.23 marks `--access-key`/`--secret-access-key` as
        /// *required* flags (env-bound to `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`),
        /// but its storage layer skips empty static credentials and falls through
        /// minio-go's chain — so this renders the flags **explicitly empty**
        /// (`--access-key=`), which satisfies the parser and engages the chain.
        ambient_credentials: bool,
    },
    /// Azure Blob Storage backend.
    Azure {
        /// Blob container name.
        container: String,
        /// Storage account name (when not supplied via env).
        storage_account: Option<String>,
        /// Optional object prefix.
        prefix: Option<String>,
    },
    /// Google Cloud Storage backend.
    Gcs {
        /// Bucket name.
        bucket: String,
        /// Optional object prefix.
        prefix: Option<String>,
        /// Path to a JSON service-account credentials file inside the mover pod
        /// (`--credentials-file`). The mover materializes this from the
        /// credentials Secret at runtime; `None` falls back to ambient ADC.
        credentials_file: Option<String>,
    },
    /// Backblaze B2 backend.
    B2 {
        /// Bucket name.
        bucket: String,
        /// Optional object prefix.
        prefix: Option<String>,
    },
    /// SFTP/SSH backend.
    Sftp {
        /// Server hostname.
        host: String,
        /// Path to the repository on the server.
        path: String,
        /// Server port (defaults to 22 when `None`).
        port: Option<u16>,
        /// SSH username.
        username: Option<String>,
        /// Path to a private key file inside the mover pod (`--keyfile`). The
        /// mover materializes this from the credentials Secret at runtime.
        keyfile: Option<String>,
        /// Path to a `known_hosts` file inside the mover pod (`--known-hosts`),
        /// pinning the server host key. The mover materializes this from the
        /// credentials Secret at runtime.
        known_hosts: Option<String>,
    },
    /// WebDAV backend.
    WebDav {
        /// WebDAV server URL.
        url: String,
    },
    /// Rclone backend (shells out to an `rclone` binary).
    Rclone {
        /// Rclone `remote:path`.
        remote_path: String,
        /// Path to an `rclone.conf` inside the mover pod, forwarded to rclone via
        /// `--rclone-args=--config=<path>`. The mover materializes this from the
        /// config Secret at runtime; `None` uses rclone's default config lookup.
        config_file: Option<String>,
        /// Go-duration value for kopia's `--rclone-startup-timeout` (how long to
        /// wait for the embedded `rclone serve` to come up). `None` leaves kopia's
        /// default (`15s`).
        startup_timeout: Option<String>,
    },
    /// Google Drive backend.
    Gdrive {
        /// Drive folder id that holds the repository.
        folder_id: String,
        /// Path to a Google service-account JSON inside the mover pod, passed as
        /// `--credentials-file`. The mover materializes this from the credentials
        /// Secret at runtime; `None` uses kopia's ambient credential lookup.
        credentials_file: Option<String>,
    },
    /// Reconnect from a kopia configuration token/file (`repository connect
    /// from-config`). Exactly one of `file`/`token` is meaningful.
    FromConfig {
        /// Path to a kopia config file.
        file: Option<String>,
        /// A kopia configuration token.
        token: Option<String>,
    },
    /// Connect to an existing kopia API server as a client.
    Server {
        /// Server URL.
        url: String,
        /// Expected server TLS certificate fingerprint (sha256 hex).
        fingerprint: Option<String>,
    },
}

/// Per-connection kopia cache budgets, applied at `repository connect`/`create`
/// time (`--content-cache-size-mb` / `--metadata-cache-size-mb`). Each mover pod
/// connects fresh, so these size that pod's local cache. `None` leaves kopia's
/// default. Serializable so it rides the mover work spec from controller to mover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheTuning {
    /// `--content-cache-size-mb`: content (data) cache budget in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_cache_size_mb: Option<i64>,
    /// `--metadata-cache-size-mb`: metadata cache budget in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_cache_size_mb: Option<i64>,
}

impl CacheTuning {
    /// Whether no budgets are set (so the connect command adds no cache flags).
    pub fn is_unset(&self) -> bool {
        self.content_cache_size_mb.is_none() && self.metadata_cache_size_mb.is_none()
    }

    /// The `--content-cache-size-mb` / `--metadata-cache-size-mb` args for the set
    /// budgets, in a stable order. Empty when nothing is set.
    fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(mb) = self.content_cache_size_mb {
            a.push("--content-cache-size-mb".into());
            a.push(mb.to_string());
        }
        if let Some(mb) = self.metadata_cache_size_mb {
            a.push("--metadata-cache-size-mb".into());
            a.push(mb.to_string());
        }
        a
    }
}

impl ConnectSpec {
    /// Stable discriminant string for logging/metrics (mirrors
    /// `kopiur_api::backend::Backend::kind_str`).
    ///
    /// ```
    /// use std::path::PathBuf;
    /// use kopiur_kopia::ConnectSpec;
    ///
    /// let fs = ConnectSpec::Filesystem { path: PathBuf::from("/repo") };
    /// assert_eq!(fs.kind_str(), "filesystem");
    ///
    /// let s3 = ConnectSpec::S3 {
    ///     bucket: "backups".into(),
    ///     endpoint: Some("https://minio.local".into()),
    ///     prefix: None,
    ///     region: None,
    ///     disable_tls: false,
    ///     disable_tls_verification: false,
    ///     ambient_credentials: false,
    /// };
    /// assert_eq!(s3.kind_str(), "s3");
    /// ```
    pub fn kind_str(&self) -> &'static str {
        match self {
            ConnectSpec::Filesystem { .. } => "filesystem",
            ConnectSpec::S3 { .. } => "s3",
            ConnectSpec::Azure { .. } => "azure",
            ConnectSpec::Gcs { .. } => "gcs",
            ConnectSpec::B2 { .. } => "b2",
            ConnectSpec::Sftp { .. } => "sftp",
            ConnectSpec::WebDav { .. } => "webdav",
            ConnectSpec::Rclone { .. } => "rclone",
            ConnectSpec::Gdrive { .. } => "gdrive",
            ConnectSpec::FromConfig { .. } => "from-config",
            ConnectSpec::Server { .. } => "server",
        }
    }

    /// The environment-variable names kopia reads this backend's credentials from
    /// **directly** (no intermediate file). These are exactly the vars the
    /// replication mover remaps from their `KOPIUR_DEST_`-prefixed copies onto their
    /// plain names for the `sync-to` subprocess, so the destination authenticates
    /// with its own keys instead of the source's identically named ones (issue #200).
    ///
    /// Exhaustive over [`ConnectSpec`] so a new backend cannot compile until its
    /// credential-delivery mechanism is decided. File-based backends (GCS/SFTP/
    /// Rclone/Gdrive) deliver credentials as a materialized *file* whose path is on
    /// argv, so they read no direct credential env var and return `&[]` — the mover
    /// stages their destination file separately. `AWS_WEB_IDENTITY_TOKEN_FILE` and
    /// the other ambient-chain *hints* are deliberately excluded: they belong to the
    /// pod's ServiceAccount (a workload-identity destination), not to a credential
    /// Secret, and must never be remapped or unset.
    pub fn direct_credential_env_names(&self) -> &'static [&'static str] {
        match self {
            ConnectSpec::S3 { .. } => &[
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
            ],
            // Static shared key / SAS token, plus the service-principal env trio
            // (the same names kopia's static Azure auth reads).
            ConnectSpec::Azure { .. } => &[
                "AZURE_STORAGE_KEY",
                "AZURE_STORAGE_SAS_TOKEN",
                "AZURE_TENANT_ID",
                "AZURE_CLIENT_ID",
                "AZURE_CLIENT_SECRET",
            ],
            ConnectSpec::B2 { .. } => &["B2_KEY_ID", "B2_KEY"],
            ConnectSpec::WebDav { .. } => &["KOPIA_WEBDAV_USERNAME", "KOPIA_WEBDAV_PASSWORD"],
            // File-delivered (materialized to a path) or credential-free.
            ConnectSpec::Filesystem { .. }
            | ConnectSpec::Gcs { .. }
            | ConnectSpec::Sftp { .. }
            | ConnectSpec::Rclone { .. }
            | ConnectSpec::Gdrive { .. }
            | ConnectSpec::FromConfig { .. }
            | ConnectSpec::Server { .. } => &[],
        }
    }

    /// The kopia subcommand args that select this backend, e.g.
    /// `["filesystem", "--path", "/repo"]`. Used by both connect and create.
    /// Credentials are expected in the environment, never here (see the type
    /// docs). A new backend variant cannot compile until it is handled.
    fn backend_args(&self) -> Vec<String> {
        // Push `--flag value` only when the optional value is present.
        fn opt(a: &mut Vec<String>, flag: &str, value: &Option<String>) {
            if let Some(v) = value {
                a.push(flag.into());
                a.push(v.clone());
            }
        }
        match self {
            ConnectSpec::Filesystem { path } => {
                vec![
                    "filesystem".into(),
                    "--path".into(),
                    path.display().to_string(),
                ]
            }
            ConnectSpec::S3 {
                bucket,
                endpoint,
                prefix,
                region,
                disable_tls,
                disable_tls_verification,
                ambient_credentials,
            } => {
                let mut a = vec!["s3".into(), "--bucket".into(), bucket.clone()];
                opt(&mut a, "--endpoint", endpoint);
                opt(&mut a, "--prefix", prefix);
                opt(&mut a, "--region", region);
                if *disable_tls {
                    a.push("--disable-tls".into());
                }
                if *disable_tls_verification {
                    a.push("--disable-tls-verification".into());
                }
                if *ambient_credentials {
                    // Single `=`-joined tokens: an empty value as a separate argv
                    // token (`--access-key ""`) would be consumed as the flag's
                    // value either way, but the joined form is unambiguous to
                    // kingpin and to a human reading the Job args. Satisfies the
                    // Required() flags with empty values so kopia's storage layer
                    // falls through to the ambient chain (IRSA / Pod Identity / IMDS).
                    a.push("--access-key=".into());
                    a.push("--secret-access-key=".into());
                }
                a
            }
            ConnectSpec::Azure {
                container,
                storage_account,
                prefix,
            } => {
                let mut a = vec!["azure".into(), "--container".into(), container.clone()];
                opt(&mut a, "--storage-account", storage_account);
                opt(&mut a, "--prefix", prefix);
                a
            }
            ConnectSpec::Gcs {
                bucket,
                prefix,
                credentials_file,
            } => {
                let mut a = vec!["gcs".into(), "--bucket".into(), bucket.clone()];
                opt(&mut a, "--prefix", prefix);
                opt(&mut a, "--credentials-file", credentials_file);
                a
            }
            ConnectSpec::B2 { bucket, prefix } => {
                let mut a = vec!["b2".into(), "--bucket".into(), bucket.clone()];
                opt(&mut a, "--prefix", prefix);
                a
            }
            ConnectSpec::Sftp {
                host,
                path,
                port,
                username,
                keyfile,
                known_hosts,
            } => {
                let mut a = vec![
                    "sftp".into(),
                    "--host".into(),
                    host.clone(),
                    "--path".into(),
                    path.clone(),
                ];
                if let Some(p) = port {
                    a.push("--port".into());
                    a.push(p.to_string());
                }
                opt(&mut a, "--username", username);
                opt(&mut a, "--keyfile", keyfile);
                opt(&mut a, "--known-hosts", known_hosts);
                a
            }
            ConnectSpec::WebDav { url } => {
                vec!["webdav".into(), "--url".into(), url.clone()]
            }
            ConnectSpec::Rclone {
                remote_path,
                config_file,
                startup_timeout,
            } => {
                let mut a = vec!["rclone".into(), "--remote-path".into(), remote_path.clone()];
                // Forward the rclone config path to the embedded rclone. Must be a
                // SINGLE `--rclone-args=<value>` token: kopia's CLI parser treats a
                // separate value starting with `--` as the next flag, so
                // `--rclone-args --config=…` fails with "expected argument".
                if let Some(cfg) = config_file {
                    a.push(format!("--rclone-args=--config={cfg}"));
                }
                // kopia's own connect flag (not an rclone arg): how long to wait
                // for the embedded `rclone serve` before failing the connect.
                if let Some(t) = startup_timeout {
                    a.push(format!("--rclone-startup-timeout={t}"));
                }
                a
            }
            ConnectSpec::Gdrive {
                folder_id,
                credentials_file,
            } => {
                let mut a = vec!["gdrive".into(), "--folder-id".into(), folder_id.clone()];
                opt(&mut a, "--credentials-file", credentials_file);
                a
            }
            ConnectSpec::FromConfig { file, token } => {
                let mut a = vec!["from-config".into()];
                opt(&mut a, "--file", file);
                opt(&mut a, "--token", token);
                a
            }
            ConnectSpec::Server { url, fingerprint } => {
                let mut a = vec!["server".into(), "--url".into(), url.clone()];
                opt(&mut a, "--server-cert-fingerprint", fingerprint);
                a
            }
        }
    }
}

/// Options for `kopia snapshot verify`. All fields default to kopia's defaults
/// when `None`/empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyOptions {
    /// `--sources`: restrict verification to these kopia sources
    /// (`username@hostname:path`). Empty (the default) verifies EVERY snapshot
    /// in the repository — kopia's own default. For a per-`SnapshotPolicy`
    /// verify against a shared repository that is both wrong (it re-verifies
    /// every other policy's data under a different identity) and expensive
    /// (`verifyFilesPercent` then samples the WHOLE repository, not just this
    /// policy's snapshots — issue #250), so the controller always scopes a
    /// quick verify to the policy's resolved identity.
    pub sources: Vec<String>,
    /// `--verify-files-percent`: randomly fully-read this percentage of files.
    pub verify_files_percent: Option<u8>,
    /// `--max-errors`: stop after this many errors (0 = never stop early).
    pub max_errors: Option<u32>,
    /// `--parallel`: verification parallelism (kopia default: 8).
    pub parallel: Option<u32>,
    /// `--file-parallelism`: parallelism for file verification (kopia default: unset).
    pub file_parallelism: Option<u32>,
    /// `--file-queue-length`: queue length for file verification (kopia default: 20000).
    pub file_queue_length: Option<u32>,
}

/// Options for `kopia snapshot create` (M4 flag sweep, issue #216 category
/// sweep). All-default reproduces kopia's own defaults / today's argv:
/// `fail_fast: None` (kopia default: keep going past per-file errors, subject
/// to the `errorHandling.ignore*Errors` policy knobs), `upload_limit_mb: None`
/// (kopia default: unlimited), `description: None` (kopia default: empty).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotCreateOptions {
    /// `--[no-]fail-fast`: abort the snapshot at the first error instead of
    /// collecting and continuing (kopia default: false — collect and continue).
    pub fail_fast: Option<bool>,
    /// `--upload-limit-mb`: abort the snapshot once this many MB have been
    /// uploaded (kopia default: 0 — unlimited).
    pub upload_limit_mb: Option<i64>,
    /// `--description`: free-form text recorded on the snapshot manifest
    /// (kopia default: empty).
    pub description: Option<String>,
}

/// Options for `kopia restore` / `kopia snapshot restore`. The tri-state
/// booleans map to kopia's `--[no-]flag` form: `Some(true)` → `--flag`,
/// `Some(false)` → `--no-flag`, `None` → omit (kopia default). M2 flag sweep
/// (issue #216 gap analysis) added everything below `overwrite_files`; all of
/// them, plus `delete_extra`, were previously either absent or dormant (the
/// mover's `RestoreOp::restore_options()` dropped them via `..Default::default()`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreOptions {
    /// `--[no-]ignore-permission-errors` (kopia default: true).
    pub ignore_permission_errors: Option<bool>,
    /// `--[no-]write-files-atomically`.
    pub write_files_atomically: Option<bool>,
    /// `--[no-]overwrite-files` (kopia default: true).
    pub overwrite_files: Option<bool>,
    /// `--[no-]overwrite-directories` (kopia default: true).
    pub overwrite_directories: Option<bool>,
    /// `--[no-]overwrite-symlinks` (kopia default: true).
    pub overwrite_symlinks: Option<bool>,
    /// `--[no-]write-sparse-files` (kopia default: false).
    pub write_sparse_files: Option<bool>,
    /// `--[no-]skip-owners` (kopia default: false).
    pub skip_owners: Option<bool>,
    /// `--[no-]skip-permissions` (kopia default: false).
    pub skip_permissions: Option<bool>,
    /// `--[no-]skip-times` (kopia default: false).
    pub skip_times: Option<bool>,
    /// `--[no-]ignore-errors` (kopia default: false).
    pub ignore_errors: Option<bool>,
    /// `--[no-]skip-existing`: skip files/symlinks that already exist in the
    /// target (kopia default: false). A genuine kingpin tri-state, not a
    /// presence-only flag — widened from a bare `bool`.
    pub skip_existing: Option<bool>,
    /// `--[no-]delete-extra`: delete files/directories/symlinks present in the
    /// restore path but absent from the snapshot (kopia default: false). Backs
    /// `Restore.spec.options.enableFileDeletion`, which was previously a silent
    /// no-op — this struct had no field for it at all.
    pub delete_extra: Option<bool>,
    /// `--parallel`: restore parallelism (1 disables).
    pub parallel: Option<u32>,
}

/// Options for `kopia repository sync-to` (ADR-0005 §13(d) / issue #216). Every
/// field's `None`/`false` reproduces kopia's own default — an all-`None`,
/// `delete_extra: false` instance yields the exact same argv `sync_to_args`
/// produced before this struct existed. The tri-state booleans map to kopia's
/// `--[no-]flag` grammar, same as [`RestoreOptions`]: `Some(true)` → `--flag`,
/// `Some(false)` → `--no-flag`, `None` → omit (kopia default).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncToOptions {
    /// `--parallel`: copy parallelism to the destination (kopia default `1` —
    /// sequential; the root cause of #216's multi-week initial-seed times).
    pub parallel: Option<u32>,
    /// `--delete`: prune destination-only blobs for a true mirror (kopia
    /// default `false` — additive sync, never removes destination content).
    pub delete_extra: bool,
    /// `--[no-]must-exist`: fail instead of initializing the destination's
    /// repository-format blob (kopia default `false`).
    pub must_exist: Option<bool>,
    /// `--[no-]times`: synchronize blob modification times to the destination,
    /// when supported (kopia default `true`).
    pub times: Option<bool>,
    /// `--[no-]update`: update blobs already present at the destination when
    /// the source copy is newer (kopia default `true`).
    pub update: Option<bool>,
    /// `--max-download-speed`: cap read throughput from the source, bytes/sec
    /// (kopia default: unlimited).
    pub max_download_speed_bytes_per_second: Option<i64>,
    /// `--max-upload-speed`: cap write throughput to the destination, bytes/sec
    /// (kopia default: unlimited).
    pub max_upload_speed_bytes_per_second: Option<i64>,
}

/// Policy fields kopia applies via `kopia policy set`. Mirrors the operator's
/// `SnapshotPolicy.spec.policy` without depending on the api crate, so the kopia
/// crate stays controller-agnostic. The caller translates the CRD policy into
/// this and the controller applies it before the first snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyArgs {
    /// `--compression` algorithm (e.g. `zstd`, `none`).
    pub compression: Option<String>,
    /// `--splitter` algorithm.
    pub splitter: Option<String>,
    /// `--add-ignore` glob patterns.
    pub ignore: Vec<String>,
    /// `--add-never-compress` glob patterns.
    pub never_compress: Vec<String>,
    /// `--ignore-cache-dirs` tri-state (honor `CACHEDIR.TAG`). `None` leaves kopia's default.
    pub ignore_cache_dirs: Option<bool>,
    /// Backup-side error handling (`--ignore-file-errors`) tri-state. ADR-0005 §13(b).
    pub ignore_file_errors: Option<bool>,
    /// `--ignore-dir-errors` tri-state. ADR-0005 §13(b).
    pub ignore_dir_errors: Option<bool>,
    /// `--ignore-unknown-types` tri-state. ADR-0005 §13(b).
    pub ignore_unknown_types: Option<bool>,
    /// `--max-parallel-snapshots` upload parallelism. ADR-0005 §13(f).
    pub max_parallel_snapshots: Option<u32>,
    /// `--max-parallel-file-reads` upload parallelism. ADR-0005 §13(f).
    pub max_parallel_file_reads: Option<u32>,
    /// `--keep-latest`: most-recent-N backups to keep per source.
    ///
    /// This field (and its five siblings below) exists ONLY so the mover can
    /// pin kopia's own create-time retention to effectively-infinite at the
    /// identity scope — see `kopiur_mover::workspec::KOPIA_KEEP_MAX`'s doc
    /// comment for the full hazard. There is deliberately no CRD/workspec
    /// surface that lets a user set these: kopia-side retention stays
    /// forbidden (`crates/api/src/error.rs`'s `InlineRetentionForbidden`),
    /// and `PolicyArgsSpec::to_kopia` never populates them.
    pub keep_latest: Option<i64>,
    /// `--keep-hourly`. See [`Self::keep_latest`].
    pub keep_hourly: Option<i64>,
    /// `--keep-daily`. See [`Self::keep_latest`].
    pub keep_daily: Option<i64>,
    /// `--keep-weekly`. See [`Self::keep_latest`].
    pub keep_weekly: Option<i64>,
    /// `--keep-monthly`. See [`Self::keep_latest`].
    pub keep_monthly: Option<i64>,
    /// `--keep-annual`. See [`Self::keep_latest`].
    pub keep_annual: Option<i64>,
    /// Verbatim extra `policy set` flags (the CRD escape hatch).
    pub extra_args: Vec<String>,
}

/// Create-time-fixed repository options applied at `kopia repository create`
/// (ADR-0005 §13(a)): the encryption/splitter/hash algorithms baked into the repo
/// format, plus optional Reed-Solomon ECC parity guarding blobs against backend
/// bit-rot. All fields are immutable post-create (webhook-enforced, §7); kopia only
/// honors them at create time. Pure args builder so it's unit-testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateOptions {
    /// `--encryption` algorithm (e.g. `AES256-GCM-HMAC-SHA256`).
    pub encryption: Option<String>,
    /// `--object-splitter` algorithm.
    pub splitter: Option<String>,
    /// `--block-hash` content-hash algorithm.
    pub hash: Option<String>,
    /// `--ecc` Reed-Solomon algorithm (e.g. `REED-SOLOMON-CRC32`). ADR-0005 §13(a).
    pub ecc: Option<String>,
    /// `--ecc-overhead-percent` parity overhead. ADR-0005 §13(a).
    pub ecc_overhead_percent: Option<i64>,
}

impl CreateOptions {
    /// The create-time `--encryption`/`--object-splitter`/`--block-hash`/`--ecc`/
    /// `--ecc-overhead-percent` args, in a stable order. Empty when nothing is set.
    pub fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(v) = &self.encryption {
            a.push("--encryption".into());
            a.push(v.clone());
        }
        if let Some(v) = &self.splitter {
            a.push("--object-splitter".into());
            a.push(v.clone());
        }
        if let Some(v) = &self.hash {
            a.push("--block-hash".into());
            a.push(v.clone());
        }
        if let Some(v) = &self.ecc {
            a.push("--ecc".into());
            a.push(v.clone());
        }
        if let Some(p) = self.ecc_overhead_percent {
            a.push("--ecc-overhead-percent".into());
            a.push(p.to_string());
        }
        a
    }
}

/// Repository throttling limits applied via `kopia repository throttle set`
/// (ADR-0005 §13(e)). Each `None` leaves kopia's current value untouched. Pure args
/// builder so it's unit-testable; an all-`None` instance yields no flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThrottleArgs {
    /// `--upload-bytes-per-second`.
    pub upload_bytes_per_second: Option<i64>,
    /// `--download-bytes-per-second`.
    pub download_bytes_per_second: Option<i64>,
    /// `--read-requests-per-second`.
    pub read_ops_per_second: Option<i64>,
    /// `--write-requests-per-second`.
    pub write_ops_per_second: Option<i64>,
}

impl ThrottleArgs {
    /// The `--*-per-second` flags for the set limits, in a stable order. Empty when
    /// nothing is set (the caller then skips the `throttle set` invocation).
    pub fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(v) = self.upload_bytes_per_second {
            a.push("--upload-bytes-per-second".into());
            a.push(v.to_string());
        }
        if let Some(v) = self.download_bytes_per_second {
            a.push("--download-bytes-per-second".into());
            a.push(v.to_string());
        }
        if let Some(v) = self.read_ops_per_second {
            a.push("--read-requests-per-second".into());
            a.push(v.to_string());
        }
        if let Some(v) = self.write_ops_per_second {
            a.push("--write-requests-per-second".into());
            a.push(v.to_string());
        }
        a
    }

    /// Whether no limits are set (so `throttle set` is skipped).
    pub fn is_empty(&self) -> bool {
        self.args().is_empty()
    }
}

/// Flags for `kopia repository set-parameters`. Modeled on [`ThrottleArgs`]: an all-`None`
/// builder whose caller skips the invocation entirely when nothing is set.
///
/// Durations are pre-rendered **strings** with a unit, not numbers — kopia's
/// `time.ParseDuration` rejects a bare number (`--epoch-min-duration=3600` →
/// `time: missing unit in duration "3600"`), so the caller must render them
/// (`kopiur_api::render_go_duration`) rather than pass user text through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetParametersArgs {
    /// `--epoch-min-duration` (e.g. `"6h"`).
    pub epoch_min_duration: Option<String>,
    /// `--epoch-refresh-frequency` (e.g. `"20m"`).
    pub epoch_refresh_frequency: Option<String>,
    /// `--epoch-advance-on-count`.
    pub epoch_advance_on_count: Option<i64>,
    /// `--epoch-advance-on-size-mb`. **MiB**, despite the flag name — kopia multiplies by
    /// 1048576.
    pub epoch_advance_on_size_mb: Option<i64>,
    /// `--epoch-checkpoint-frequency`.
    pub epoch_checkpoint_frequency: Option<i64>,
    /// `--epoch-delete-parallelism`.
    pub epoch_delete_parallelism: Option<i64>,
}

impl SetParametersArgs {
    /// The flags for the set parameters, in a stable order. Empty when nothing is set (the
    /// caller then skips the `set-parameters` invocation).
    pub fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(v) = &self.epoch_min_duration {
            a.push("--epoch-min-duration".into());
            a.push(v.clone());
        }
        if let Some(v) = &self.epoch_refresh_frequency {
            a.push("--epoch-refresh-frequency".into());
            a.push(v.clone());
        }
        if let Some(v) = self.epoch_advance_on_count {
            a.push("--epoch-advance-on-count".into());
            a.push(v.to_string());
        }
        if let Some(v) = self.epoch_advance_on_size_mb {
            a.push("--epoch-advance-on-size-mb".into());
            a.push(v.to_string());
        }
        if let Some(v) = self.epoch_checkpoint_frequency {
            a.push("--epoch-checkpoint-frequency".into());
            a.push(v.to_string());
        }
        if let Some(v) = self.epoch_delete_parallelism {
            a.push("--epoch-delete-parallelism".into());
            a.push(v.to_string());
        }
        a
    }

    /// Whether no parameters are set (so `set-parameters` is skipped).
    pub fn is_empty(&self) -> bool {
        self.args().is_empty()
    }
}

/// UI authentication mode for `kopia server start`. Controller-agnostic mirror of
/// the api crate's `ServerAuth` (this crate has no kube dependency).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerAuthMode {
    /// Require a UI login. `username` goes on argv; the password is supplied
    /// separately to [`KopiaClient::server_start`] (it is never baked into the pure
    /// arg builder, nor into a ConfigMap).
    Password {
        /// HTTP basic-auth username for the UI (`--server-username`).
        username: String,
    },
    /// No UI authentication (`--without-password`). kopia requires `--insecure`
    /// alongside it, which [`server_start_args`] always emits.
    None,
}

impl ServerAuthMode {
    /// Stable discriminant for logging.
    pub fn kind_str(&self) -> &'static str {
        match self {
            ServerAuthMode::Password { .. } => "password",
            ServerAuthMode::None => "none",
        }
    }
}

/// A typed description of how to run `kopia server start` (the web UI).
///
/// ## Why this is its own non-returning path (not `run_ok`)
///
/// `server start` is a long-running process that never exits on success, so the
/// `run_ok`/`run_json` "spawn, read to EOF, wait for exit code" pattern would hang
/// forever. [`KopiaClient::server_start`] instead `exec`s the binary so kopia takes
/// over this PID and receives `SIGTERM` directly from the kubelet on pod shutdown.
///
/// ## TLS and auth
///
/// The server always runs with `--insecure` (no in-pod TLS): TLS is terminated by
/// the user's ingress and the Service speaks plain HTTP. `--insecure` is kopia's
/// *no-TLS* switch — it is required in every mode, and is **not** the no-auth knob
/// (that is `--without-password`, selected by [`ServerAuthMode::None`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStartSpec {
    /// Listen address — must be non-loopback (e.g. `0.0.0.0:51515`) to be reachable
    /// through a Service.
    pub address: String,
    /// UI authentication mode.
    pub auth: ServerAuthMode,
    /// Serve the embedded HTML UI (`--ui`). Defaults to enabled.
    pub ui: bool,
}

impl Default for ServerStartSpec {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:51515".to_string(),
            auth: ServerAuthMode::None,
            ui: true,
        }
    }
}

/// Builder for [`KopiaClient`].
#[derive(Debug, Clone, Default)]
pub struct KopiaClientBuilder {
    binary: Option<PathBuf>,
    common_env: BTreeMap<String, String>,
    common_args: Vec<String>,
    default_timeout: Option<Duration>,
}

impl KopiaClientBuilder {
    /// Set the path to the kopia binary. Injectable so tests can point at a
    /// fake shim. Defaults to `kopia` (resolved via `PATH`).
    pub fn binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = Some(binary.into());
        self
    }

    /// Add an environment variable applied to every invocation. Use this for
    /// `KOPIA_PASSWORD`, `KOPIA_CONFIG_PATH`, cache dirs, and S3 credentials.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.common_env.insert(key.into(), value.into());
        self
    }

    /// Add a global arg applied (after the subcommand tokens) to every
    /// invocation. Must be a flag kopia accepts on *every* subcommand (e.g. a
    /// global flag); per-subcommand flags belong on the specific method.
    /// Prefer env vars (e.g. `KOPIA_CHECK_FOR_UPDATES=false`) for cross-cutting
    /// behavior.
    pub fn common_arg(mut self, arg: impl Into<String>) -> Self {
        self.common_args.push(arg.into());
        self
    }

    /// Default per-invocation timeout. `None` means no timeout.
    pub fn default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }

    /// Finalize.
    pub fn build(self) -> KopiaClient {
        let mut common_args = self.common_args;
        // kopia's hidden default-on `--auto-maintenance` opportunistically runs
        // a maintenance pass as a side effect of other commands (`snapshot
        // create`/`delete`/`expire`, and — verified against the pinned kopia
        // 0.23.1 binary — even a bare `policy set`) whenever the connected
        // client identity equals the repository's designated maintenance
        // owner. Only the Maintenance CR's own `maintenance run` may trigger
        // maintenance (that explicit subcommand is unaffected by this flag —
        // also verified against the pinned binary), so every `KopiaClient`
        // carries `--no-auto-maintenance` on every invocation, unconditionally,
        // rather than relying on each call site to remember it.
        common_args.push("--no-auto-maintenance".into());
        KopiaClient {
            binary: self.binary.unwrap_or_else(|| PathBuf::from("kopia")),
            common_env: self.common_env,
            common_args,
            default_timeout: self.default_timeout,
        }
    }
}

/// A kopia client backed by the real `kopia` binary via `tokio::process`.
///
/// Construction is pure — building a client never spawns a process. Only the
/// `async` methods invoke `kopia`. The builder defaults the binary to `kopia`
/// (resolved via `PATH`); inject a path for tests or non-standard images:
///
/// ```
/// use std::path::PathBuf;
/// use kopiur_kopia::KopiaClient;
///
/// let client = KopiaClient::builder().build();
/// assert_eq!(client.binary(), &PathBuf::from("kopia"));
///
/// let custom = KopiaClient::builder()
///     .binary("/usr/local/bin/kopia")
///     .env("KOPIA_PASSWORD", "s3cr3t")
///     .build();
/// assert_eq!(custom.binary(), &PathBuf::from("/usr/local/bin/kopia"));
/// ```
#[derive(Debug, Clone)]
pub struct KopiaClient {
    binary: PathBuf,
    common_env: BTreeMap<String, String>,
    common_args: Vec<String>,
    default_timeout: Option<Duration>,
}

/// The raw outcome of running a kopia subprocess.
struct RawOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl KopiaClient {
    /// Start building a client.
    pub fn builder() -> KopiaClientBuilder {
        KopiaClientBuilder::default()
    }

    /// The configured binary path (useful for diagnostics / tests).
    pub fn binary(&self) -> &PathBuf {
        &self.binary
    }

    /// The environment applied to every invocation (useful for tests asserting
    /// that the cache/log/config dirs were injected).
    pub fn common_env(&self) -> &BTreeMap<String, String> {
        &self.common_env
    }

    /// The global args appended after the subcommand on every invocation
    /// (useful for tests asserting e.g. `--no-auto-maintenance` is always
    /// present).
    pub fn common_args(&self) -> &[String] {
        &self.common_args
    }

    /// The timeout applied to every invocation when set (useful for tests
    /// asserting a caller time-bounds its subprocesses).
    pub fn default_timeout(&self) -> Option<Duration> {
        self.default_timeout
    }

    /// Run kopia with the given subcommand args, returning raw output. Applies
    /// `common_env` and inserts `common_args` immediately after the subcommand,
    /// plus a per-invocation environment overlay (`Some(value)` sets a variable,
    /// `None` **unsets** an otherwise-inherited one — pass an empty map for the
    /// common case). stdout and stderr are fully captured. Honors the default
    /// timeout if set. Used by the replication mover
    /// to point `kopia repository sync-to` at the *destination* backend's
    /// credentials (remapped from their `KOPIUR_DEST_`-prefixed copies) while
    /// clearing any source credential the destination does not set, so a stale
    /// source `AWS_SESSION_TOKEN` (etc.) cannot leak into the destination auth.
    async fn run_with_env(
        &self,
        args: &[String],
        env_overlay: &BTreeMap<String, Option<String>>,
    ) -> Result<RawOutput, KopiaError> {
        let display_args = args.join(" ");
        let mut cmd = Command::new(&self.binary);
        // Do not inherit the ambient environment's KOPIA_* unless the caller
        // set it explicitly; but we *do* inherit PATH etc. by default, which is
        // fine. We only override what common_env specifies.
        for (k, v) in &self.common_env {
            cmd.env(k, v);
        }
        // Per-invocation overlay wins over both the inherited env and common_env.
        for (k, v) in env_overlay {
            match v {
                Some(value) => cmd.env(k, value),
                None => cmd.env_remove(k),
            };
        }
        cmd.args(args);
        // Append common args (e.g. --no-check-for-updates) after the subcommand
        // tokens the caller passed.
        cmd.args(&self.common_args);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Spawn with a bounded retry on transient errnos. ETXTBSY (26) and
        // EAGAIN (11) are not "the binary is wrong" failures — they're transient
        // races: ETXTBSY appears when another thread in a multithreaded process
        // forks-for-exec while the target file still has a writable fd open
        // elsewhere (the classic fork/exec race), and EAGAIN appears under fork
        // pressure on a busy node. A real bad-binary error (ENOENT, EACCES) is
        // returned immediately. Retries are quick and capped.
        let mut child = {
            let mut attempt = 0u32;
            loop {
                match cmd.spawn() {
                    Ok(c) => break c,
                    Err(e) if matches!(e.raw_os_error(), Some(26) | Some(11)) && attempt < 10 => {
                        attempt += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    Err(source) => {
                        return Err(KopiaError::Spawn {
                            binary: self.binary.display().to_string(),
                            source,
                        });
                    }
                }
            }
        };

        // Take the pipes so we can read both concurrently without deadlocking
        // on a full pipe buffer.
        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let stderr_pipe = child.stderr.take().expect("stderr piped");

        let read_out = async {
            let mut buf = String::new();
            stdout_pipe.read_to_string(&mut buf).await.map(|_| buf)
        };
        let read_err = async {
            // Stream kopia's stderr line-by-line so its real progress and log
            // output is visible in `kubectl logs` (at debug, target `kopia`) for
            // both the controller's short ops and the long-running mover Job —
            // while still accumulating the full text byte-for-byte for the
            // failure tail carried by `KopiaError::NonZeroExit`.
            let mut reader = BufReader::new(stderr_pipe);
            let mut buf = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\n', '\r']);
                        if !trimmed.is_empty() {
                            tracing::debug!(target: "kopia", "{trimmed}");
                        }
                        buf.push_str(&line);
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(buf)
        };

        let wait_with_io = async {
            let (out, err, status) = tokio::join!(read_out, read_err, child.wait());
            Ok::<_, std::io::Error>((out?, err?, status?))
        };

        let (stdout, stderr, status) = match self.default_timeout {
            Some(t) => match tokio::time::timeout(t, wait_with_io).await {
                Ok(res) => res.map_err(|source| KopiaError::Spawn {
                    binary: self.binary.display().to_string(),
                    source,
                })?,
                Err(_) => {
                    // Best-effort kill; ignore the result since we're erroring
                    // out regardless.
                    let _ = child.start_kill();
                    return Err(KopiaError::Timeout {
                        args: display_args,
                        seconds: t.as_secs(),
                    });
                }
            },
            None => wait_with_io.await.map_err(|source| KopiaError::Spawn {
                binary: self.binary.display().to_string(),
                source,
            })?,
        };

        Ok(RawOutput {
            code: status.code(),
            stdout,
            stderr,
        })
    }

    /// Run kopia and require a zero exit code, returning stdout. On a non-zero
    /// exit, builds a structured [`KopiaError::NonZeroExit`] with the stderr
    /// tail and a best-effort error class.
    async fn run_ok(&self, args: &[String]) -> Result<String, KopiaError> {
        self.run_ok_with_env(args, &BTreeMap::new()).await
    }

    /// [`Self::run_ok`] with a per-invocation environment overlay (see
    /// [`Self::run_with_env`]).
    async fn run_ok_with_env(
        &self,
        args: &[String],
        env_overlay: &BTreeMap<String, Option<String>>,
    ) -> Result<String, KopiaError> {
        let out = self.run_with_env(args, env_overlay).await?;
        if out.code == Some(0) {
            Ok(out.stdout)
        } else {
            Err(KopiaError::NonZeroExit {
                args: args.join(" "),
                code: out.code,
                class: KopiaErrorClass::classify(&out.stderr),
                stderr_tail: tail_lines(&out.stderr),
            })
        }
    }

    /// Run kopia, require success, and parse the trailing JSON value on stdout
    /// into `T`. Kopia prints the result as the *last* JSON value on stdout
    /// (progress goes to stderr), so we parse from the first `{`/`[`.
    async fn run_json<T: DeserializeOwned>(
        &self,
        args: &[String],
        context: &str,
    ) -> Result<T, KopiaError> {
        let stdout = self.run_ok(args).await?;
        let json = extract_json(&stdout).ok_or_else(|| KopiaError::EmptyOutput {
            context: context.to_string(),
        })?;
        serde_json::from_str::<T>(json).map_err(|source| KopiaError::Json {
            context: context.to_string(),
            source,
        })
    }

    /// Connect to an existing repository (`kopia repository connect <backend>`).
    /// `cache` sizes this connection's local kopia cache; pass
    /// [`CacheTuning::default`] to leave kopia's defaults.
    pub async fn repository_connect(
        &self,
        spec: &ConnectSpec,
        cache: CacheTuning,
    ) -> Result<(), KopiaError> {
        self.run_ok(&connect_args(spec, cache, false))
            .await
            .map(|_| ())
    }

    /// Connect to an existing repository **read-only** (`kopia repository
    /// connect <backend> --readonly`). The read-only bit persists in kopia's
    /// client config, so every subsequent invocation on this connection is
    /// structurally unable to mutate the repository — the connect mode for
    /// browse sessions. Every other mover flow stays on the read-write
    /// [`Self::repository_connect`].
    pub async fn repository_connect_readonly(
        &self,
        spec: &ConnectSpec,
        cache: CacheTuning,
    ) -> Result<(), KopiaError> {
        self.run_ok(&connect_args(spec, cache, true))
            .await
            .map(|_| ())
    }

    /// Create a new repository (`kopia repository create <backend>`). `cache` sizes
    /// the creating connection's local cache; pass [`CacheTuning::default`] to leave
    /// kopia's defaults. `create_opts` carries the create-time-fixed knobs
    /// (encryption/splitter/hash algorithms, ECC) baked into the repository format.
    pub async fn repository_create(
        &self,
        spec: &ConnectSpec,
        cache: CacheTuning,
        create_opts: &CreateOptions,
    ) -> Result<(), KopiaError> {
        let mut args = vec!["repository".into(), "create".into()];
        args.extend(spec.backend_args());
        args.extend(cache.args());
        args.extend(create_opts.args());
        self.run_ok(&args).await.map(|_| ())
    }

    /// Set the repository's throttling limits (`kopia repository throttle set`).
    /// Caps upload/download bytes-per-sec and read/list/upload ops-per-sec so a run
    /// doesn't saturate a link or hammer an object store (ADR-0005 §13(e)). A no-op
    /// (skips the call) when nothing is set.
    pub async fn repository_throttle_set(&self, throttle: &ThrottleArgs) -> Result<(), KopiaError> {
        let flags = throttle.args();
        if flags.is_empty() {
            return Ok(());
        }
        let mut args = vec!["repository".into(), "throttle".into(), "set".into()];
        args.extend(flags);
        self.run_ok(&args).await.map(|_| ())
    }

    /// Rewrite mutable repository parameters on the CONNECTED repository
    /// (`kopia repository set-parameters [flags]`), issue #258. No-op when nothing is set.
    ///
    /// Two properties the caller must respect:
    ///
    /// - **Never on a read-only connection.** kopia hard-errors (`unable to write blobcfg
    ///   blob: PutBlob() failed for "kopia.blobcfg": storage is read-only`), so a
    ///   `mode: ReadOnly` repository must not reach here.
    /// - **This invalidates every other client's cached format blob.** kopia says so itself
    ///   ("you must disconnect and re-connect all other Kopia clients") and drops the local
    ///   `kopia.repository`/`kopia.blobcfg` cache. Other clients re-read within
    ///   `formatBlobCacheDuration` (15m) on their own, but this is why the caller applies
    ///   only on observed drift rather than unconditionally.
    ///
    /// Needs no maintenance ownership/lease.
    pub async fn repository_set_parameters(
        &self,
        params: &SetParametersArgs,
    ) -> Result<(), KopiaError> {
        let flags = params.args();
        if flags.is_empty() {
            return Ok(());
        }
        let mut args = vec!["repository".into(), "set-parameters".into()];
        args.extend(flags);
        self.run_ok(&args).await.map(|_| ())
    }

    /// Mirror the *connected* repository's blobs to a destination backend
    /// (`kopia repository sync-to <destination> [flags]`), ADR-0005 §13(d) / issue
    /// #216. The caller must already be connected to the **source** repository;
    /// this copies its blobs to `destination`. The destination's backend args are
    /// built by `ConnectSpec::backend_args` (the same builder connect/create use),
    /// so a new backend variant is wired through automatically. `opts` carries the
    /// tuning knobs (parallelism, `--delete`, the must-exist/times/update
    /// tri-states, throughput caps) — see [`SyncToOptions`]. Destination
    /// credentials are supplied via the environment, never on argv, exactly like
    /// connect/create. Success is exit code 0.
    pub async fn repository_sync_to(
        &self,
        destination: &ConnectSpec,
        opts: &SyncToOptions,
    ) -> Result<(), KopiaError> {
        self.repository_sync_to_with_env(destination, opts, &BTreeMap::new())
            .await
    }

    /// [`Self::repository_sync_to`] with a per-invocation environment overlay
    /// applied to the `sync-to` subprocess only. The replication mover uses this to
    /// give the **destination** backend its own credentials: it maps each of the
    /// destination backend's env-delivered credential vars (`AWS_*`, `AZURE_*`, …)
    /// from the `KOPIUR_DEST_`-prefixed copy in its environment, and unsets any that
    /// the destination doesn't provide so a source credential cannot leak. The
    /// *source* repository is read from the persisted connection config (kopia bakes
    /// the source storage credentials in at `repository connect`), so overlaying the
    /// plain credential names here cannot disturb the source read. `Some(v)` sets a
    /// var, `None` removes it. Credentials travel via env, never argv.
    pub async fn repository_sync_to_with_env(
        &self,
        destination: &ConnectSpec,
        opts: &SyncToOptions,
        dest_env: &BTreeMap<String, Option<String>>,
    ) -> Result<(), KopiaError> {
        let args = sync_to_args(destination, opts);
        self.run_ok_with_env(&args, dest_env).await.map(|_| ())
    }

    /// Create a snapshot of `source_path` with the given `tags`
    /// (`key:value`) and kopia's own defaults for `snapshot create`'s tuning
    /// knobs. Returns the parsed create result.
    ///
    /// `override_source`, when set, is passed to kopia as `--override-source`
    /// (format `username@hostname:path`). This is how Kopiur records snapshots
    /// under the operator-*resolved* identity (ADR §4.2 / anchoring principle 9)
    /// rather than the mover pod's ambient `user@host`. Without it kopia would
    /// attribute the snapshot to the pod, breaking the identity model that the
    /// whole catalog/retention/restore machinery keys on.
    pub async fn snapshot_create(
        &self,
        source_path: &str,
        tags: &BTreeMap<String, String>,
        override_source: Option<&str>,
    ) -> Result<SnapshotCreateResult, KopiaError> {
        self.snapshot_create_with(
            source_path,
            tags,
            override_source,
            &SnapshotCreateOptions::default(),
        )
        .await
    }

    /// Create a snapshot honoring the operator's [`SnapshotCreateOptions`]
    /// (`failFast`, `uploadLimitMb`, `description` — M4 flag sweep, issue #216).
    /// Same identity/tags contract as [`Self::snapshot_create`], which now
    /// delegates here with an all-default `opts` (byte-for-byte the same argv
    /// as before this option struct existed).
    pub async fn snapshot_create_with(
        &self,
        source_path: &str,
        tags: &BTreeMap<String, String>,
        override_source: Option<&str>,
        opts: &SnapshotCreateOptions,
    ) -> Result<SnapshotCreateResult, KopiaError> {
        let args = snapshot_create_args(source_path, tags, override_source, opts);
        self.run_json(&args, "snapshot create result").await
    }

    /// List snapshots, optionally filtered by source identity. With no filter
    /// this lists all snapshots in the repository.
    pub async fn snapshot_list(
        &self,
        filter: Option<&SnapshotSource>,
    ) -> Result<Vec<SnapshotListEntry>, KopiaError> {
        let mut args = vec!["snapshot".into(), "list".into(), "--json".into()];
        if let Some(src) = filter {
            // kopia accepts the identity string as a positional source filter.
            args.push(src.identity());
        }
        self.run_json(&args, "snapshot list").await
    }

    /// Delete a single snapshot by manifest id. kopia's `snapshot delete`
    /// requires `--delete` to actually remove (otherwise it dry-runs) and does
    /// not support `--json`; success is signaled by exit code 0.
    ///
    /// IDEMPOTENT: an already-absent snapshot (`no snapshots matched <id>` on
    /// stderr) is success — that IS the goal state. kopia dedups
    /// identical-content snapshot manifests, so several `Snapshot` CRs can
    /// legitimately pin the SAME kopia id; when GFS retention prunes more than
    /// one of them, the first finalizer's delete removes the manifest and the
    /// rest would otherwise fail terminally, wedging their CRs in `Deleting`
    /// forever (caught by the retention e2e under suite load).
    pub async fn snapshot_delete(&self, id: &str) -> Result<(), KopiaError> {
        let args = vec![
            "snapshot".into(),
            "delete".into(),
            id.to_string(),
            "--delete".into(),
        ];
        match self.run_ok(&args).await {
            Ok(_) => Ok(()),
            Err(KopiaError::NonZeroExit { stderr_tail, .. })
                if stderr_tail.contains("no snapshots matched") =>
            {
                tracing::debug!(%id, "snapshot already absent; delete is idempotent");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Restore a snapshot's contents to a target directory with kopia's default
    /// options. kopia's `snapshot restore` does not emit JSON; success is exit
    /// code 0.
    pub async fn snapshot_restore(&self, id: &str, target_dir: &str) -> Result<(), KopiaError> {
        self.snapshot_restore_with(id, target_dir, &RestoreOptions::default())
            .await
    }

    /// Restore a snapshot honoring the operator's [`RestoreOptions`]
    /// (`enableFileDeletion`, `ignorePermissionErrors`, `writeFilesAtomically`,
    /// …). Success is exit code 0.
    pub async fn snapshot_restore_with(
        &self,
        id: &str,
        target_dir: &str,
        opts: &RestoreOptions,
    ) -> Result<(), KopiaError> {
        let args = restore_args(id, target_dir, opts);
        self.run_ok(&args).await.map(|_| ())
    }

    /// Verify repository/snapshot integrity (`kopia snapshot verify`). Success is
    /// exit code 0; a verification failure surfaces as a non-zero exit.
    pub async fn snapshot_verify(&self, opts: &VerifyOptions) -> Result<(), KopiaError> {
        let args = verify_args(opts);
        self.run_ok(&args).await.map(|_| ())
    }

    /// Estimate the size/scope of snapshotting `source_path`
    /// (`kopia snapshot estimate`). Best-effort; success is exit code 0.
    pub async fn snapshot_estimate(&self, source_path: &str) -> Result<(), KopiaError> {
        let args = vec![
            "snapshot".into(),
            "estimate".into(),
            source_path.to_string(),
        ];
        self.run_ok(&args).await.map(|_| ())
    }

    /// Add a pin to a snapshot so maintenance/expiration never deletes it
    /// (`kopia snapshot pin <id> --add <pin>`). Used to protect snapshots whose
    /// `Snapshot` carries `deletionPolicy: Retain`.
    pub async fn snapshot_pin(&self, id: &str, pin: &str) -> Result<(), KopiaError> {
        let args = vec![
            "snapshot".into(),
            "pin".into(),
            id.to_string(),
            "--add".into(),
            pin.to_string(),
        ];
        self.run_ok(&args).await.map(|_| ())
    }

    /// Remove a pin from a snapshot (`kopia snapshot pin <id> --remove <pin>`).
    pub async fn snapshot_unpin(&self, id: &str, pin: &str) -> Result<(), KopiaError> {
        let args = vec![
            "snapshot".into(),
            "pin".into(),
            id.to_string(),
            "--remove".into(),
            pin.to_string(),
        ];
        self.run_ok(&args).await.map(|_| ())
    }

    /// Expire snapshots per the repository's policy
    /// (`kopia snapshot expire --all`). When `delete` is false this is a dry-run
    /// (kopia requires `--delete` to actually remove). Success is exit code 0.
    pub async fn snapshot_expire(&self, delete: bool) -> Result<(), KopiaError> {
        let mut args = vec!["snapshot".into(), "expire".into(), "--all".into()];
        if delete {
            args.push("--delete".into());
        }
        self.run_ok(&args).await.map(|_| ())
    }

    /// Validate that the connected storage provider behaves correctly
    /// (`kopia repository validate-provider`). A good Repository-readiness
    /// preflight for object-store backends. Success is exit code 0.
    pub async fn repository_validate_provider(&self) -> Result<(), KopiaError> {
        let args = vec!["repository".into(), "validate-provider".into()];
        self.run_ok(&args).await.map(|_| ())
    }

    /// Apply a policy to `target` (an identity string, a path, or `--global`)
    /// via `kopia policy set`. The operator calls this before the first snapshot
    /// so `SnapshotPolicy.spec.policy` (compression/splitter/ignore) is honored.
    pub async fn policy_set(&self, target: &str, policy: &PolicyArgs) -> Result<(), KopiaError> {
        let args = policy_set_args(target, policy);
        self.run_ok(&args).await.map(|_| ())
    }

    /// Show the effective policy for `target` (`kopia policy show <target>
    /// --json`), parsed as a generic JSON value.
    pub async fn policy_show(&self, target: &str) -> Result<serde_json::Value, KopiaError> {
        let args = vec![
            "policy".into(),
            "show".into(),
            target.to_string(),
            "--json".into(),
        ];
        self.run_json(&args, "policy show").await
    }

    /// Get repository status (`kopia repository status --json`).
    ///
    /// This spawns `kopia`, so the example is `no_run` (it would need a real
    /// binary + connected repository):
    ///
    /// ```no_run
    /// # async fn run() -> Result<(), kopiur_kopia::KopiaError> {
    /// use kopiur_kopia::KopiaClient;
    ///
    /// let client = KopiaClient::builder()
    ///     .env("KOPIA_PASSWORD", "s3cr3t")
    ///     .build();
    /// let status = client.repository_status().await?;
    /// println!("repository unique id: {}", status.unique_id_hex);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn repository_status(&self) -> Result<RepositoryStatus, KopiaError> {
        let args = vec!["repository".into(), "status".into(), "--json".into()];
        self.run_json(&args, "repository status").await
    }

    /// Get maintenance info (`kopia maintenance info --json`).
    pub async fn maintenance_info(&self) -> Result<MaintenanceInfo, KopiaError> {
        let args = vec!["maintenance".into(), "info".into(), "--json".into()];
        self.run_json(&args, "maintenance info").await
    }

    /// Count the repository's content-index blobs (`kopia index list --json`,
    /// array length). kopia's index is compacted by periodic maintenance; when
    /// maintenance stops (e.g. a stale lease owner), this grows unbounded and
    /// kopia eventually warns "Found too many index blobs (N), ensure periodic
    /// repository maintenance". The operator surfaces a Kubernetes Warning when
    /// this crosses a configurable threshold. Cheap — lists index-blob metadata
    /// only, no content read. Verified against kopia 0.23 (`index list
    /// --[no-]json`).
    pub async fn index_blob_count(&self) -> Result<i64, KopiaError> {
        let args = vec!["index".into(), "list".into(), "--json".into()];
        let entries: Vec<IndexBlobEntry> = self.run_json(&args, "index list").await?;
        Ok(entries.len() as i64)
    }

    /// Claim the repository's maintenance ownership for the *currently connected*
    /// identity (`kopia maintenance set --owner me`). kopia ties "who may run
    /// maintenance" to the connected user@hostname and rejects a `maintenance run`
    /// from anyone but the designated owner ("maintenance must be run by designated
    /// user: …"). A repo bootstrapped by the controller in-process is owned by the
    /// controller's identity, so a mover Job (a different pod) MUST claim ownership
    /// before it can run maintenance. Idempotent; no JSON, success is exit 0.
    pub async fn maintenance_set_owner_me(&self) -> Result<(), KopiaError> {
        let args = vec![
            "maintenance".into(),
            "set".into(),
            "--owner".into(),
            "me".into(),
        ];
        self.run_ok(&args).await.map(|_| ())
    }

    /// Set the repository's maintenance owner to an EXPLICIT `user@hostname`
    /// (`kopia maintenance set --owner <owner>`). Used by the bootstrap mover
    /// right after `repository create` to stamp the stable, lease-derived
    /// owner (`kopiur_api::maintenance::kopia_owner_for_lease`) instead of the
    /// creating pod's ephemeral identity — without this, every later
    /// maintenance mover sees a foreign owner and `takeoverPolicy: Never`
    /// yields forever. Verified against kopia 0.23 `maintenance set --help`
    /// (`--owner=OWNER  Set maintenance owner user@hostname`).
    pub async fn maintenance_set_owner(&self, owner: &str) -> Result<(), KopiaError> {
        let args = vec![
            "maintenance".into(),
            "set".into(),
            "--owner".into(),
            owner.into(),
        ];
        self.run_ok(&args).await.map(|_| ())
    }

    /// Switch the CONNECTED client identity (`kopia repository set-client
    /// --username … --hostname …`). The maintenance mover assumes the stable
    /// lease-derived identity this way (kopia 0.23 has no identity override on
    /// `repository connect`; the OS user@pod-hostname is ephemeral), so
    /// `maintenance set --owner me` records a stable string and the
    /// designated-user check passes on every later run. Verified against
    /// kopia 0.23 `repository set-client --help`.
    pub async fn repository_set_client_identity(
        &self,
        username: &str,
        hostname: &str,
    ) -> Result<(), KopiaError> {
        let args = vec![
            "repository".into(),
            "set-client".into(),
            format!("--username={username}"),
            format!("--hostname={hostname}"),
        ];
        self.run_ok(&args).await.map(|_| ())
    }

    /// Run a maintenance pass. kopia's `maintenance run` does not emit JSON;
    /// success is exit code 0. The caller must already be the designated
    /// maintenance owner (see [`maintenance_set_owner_me`](Self::maintenance_set_owner_me)).
    pub async fn maintenance_run(&self, mode: MaintenanceMode) -> Result<(), KopiaError> {
        let mut args = vec!["maintenance".into(), "run".into()];
        match mode {
            MaintenanceMode::Quick => args.push("--no-full".into()),
            MaintenanceMode::Full => args.push("--full".into()),
        }
        self.run_ok(&args).await.map(|_| ())
    }

    /// Run kopia with `args` and stream stdout **byte-for-byte** into `sink`
    /// (no line splitting, no UTF-8 assumption), returning the byte count on a
    /// zero exit. This is the file-content path for `kopia show <file-oid>`
    /// (the browse data-plane's `cat`/`download`), where stdout is the raw
    /// object bytes — buffering it whole or splitting it into lines would
    /// corrupt/clamp arbitrarily large binary files.
    ///
    /// stderr is accumulated like every other invocation; a non-zero exit
    /// yields [`KopiaError::NonZeroExit`] with the stderr tail. NOTE: bytes
    /// already streamed before a late failure have reached the sink — callers
    /// writing to a file should verify the count and discard partial output on
    /// error (kopia's `show` either streams the object or fails up front, so in
    /// practice a failure produces no payload). Honors `default_timeout`.
    pub async fn run_raw_streaming(
        &self,
        args: &[String],
        sink: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> Result<u64, KopiaError> {
        let display_args = args.join(" ");
        let mut cmd = Command::new(&self.binary);
        for (k, v) in &self.common_env {
            cmd.env(k, v);
        }
        cmd.args(args);
        cmd.args(&self.common_args);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Same bounded transient-errno retry as `run` (ETXTBSY/EAGAIN are
        // fork/exec races, not bad-binary failures).
        let mut child = {
            let mut attempt = 0u32;
            loop {
                match cmd.spawn() {
                    Ok(c) => break c,
                    Err(e) if matches!(e.raw_os_error(), Some(26) | Some(11)) && attempt < 10 => {
                        attempt += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    Err(source) => {
                        return Err(KopiaError::Spawn {
                            binary: self.binary.display().to_string(),
                            source,
                        });
                    }
                }
            }
        };
        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");

        let copy_out = tokio::io::copy(&mut stdout_pipe, sink);
        let read_err = async {
            let mut buf = String::new();
            stderr_pipe.read_to_string(&mut buf).await.map(|_| buf)
        };
        let wait_with_io = async {
            let (copied, err, status) = tokio::join!(copy_out, read_err, child.wait());
            Ok::<_, std::io::Error>((copied?, err?, status?))
        };

        let (bytes, stderr, status) = match self.default_timeout {
            Some(t) => match tokio::time::timeout(t, wait_with_io).await {
                Ok(res) => res.map_err(|source| KopiaError::Spawn {
                    binary: self.binary.display().to_string(),
                    source,
                })?,
                Err(_) => {
                    let _ = child.start_kill();
                    return Err(KopiaError::Timeout {
                        args: display_args,
                        seconds: t.as_secs(),
                    });
                }
            },
            None => wait_with_io.await.map_err(|source| KopiaError::Spawn {
                binary: self.binary.display().to_string(),
                source,
            })?,
        };

        if status.code() == Some(0) {
            Ok(bytes)
        } else {
            Err(KopiaError::NonZeroExit {
                args: display_args,
                code: status.code(),
                class: KopiaErrorClass::classify(&stderr),
                stderr_tail: tail_lines(&stderr),
            })
        }
    }

    /// Run `kopia server start`, **replacing this process** with kopia via `exec`.
    ///
    /// On success this never returns (kopia takes over the PID and runs until it is
    /// signalled). It returns a [`KopiaError`] only if `exec` itself fails (e.g. the
    /// binary is missing). The repository must already be connected (call
    /// [`KopiaClient::repository_connect`] first) — the server reads the connected
    /// repo from the kopia config file.
    ///
    /// `password` is the UI password for [`ServerAuthMode::Password`]; it is appended
    /// to argv **here**, inside the server pod, so it never reaches the controller,
    /// a ConfigMap, or the pure [`server_start_args`] builder. For
    /// [`ServerAuthMode::None`] it is ignored.
    #[cfg(unix)]
    pub fn server_start(&self, spec: &ServerStartSpec, password: Option<&str>) -> KopiaError {
        use std::os::unix::process::CommandExt;

        let mut args = server_start_args(spec);
        if let ServerAuthMode::Password { .. } = &spec.auth
            && let Some(pw) = password
        {
            args.push("--server-password".into());
            args.push(pw.to_string());
        }

        let mut cmd = std::process::Command::new(&self.binary);
        for (k, v) in &self.common_env {
            cmd.env(k, v);
        }
        cmd.args(&args);
        cmd.args(&self.common_args);

        // exec(2) replaces the current image. It returns ONLY on failure.
        let source = cmd.exec();
        KopiaError::Spawn {
            binary: self.binary.display().to_string(),
            source,
        }
    }
}

/// Push a kingpin `--[no-]flag` boolean tri-state (`Some(true)` → `--flag`,
/// `Some(false)` → `--no-flag`). This is `kopia snapshot restore`'s flag
/// grammar (`--[no-]overwrite-files`, …) — NOT `policy set`'s (see
/// [`push_valued_tristate`]); the two commands genuinely differ.
fn push_tristate(args: &mut Vec<String>, flag: &str, value: Option<bool>) {
    match value {
        Some(true) => args.push(format!("--{flag}")),
        Some(false) => args.push(format!("--no-{flag}")),
        None => {}
    }
}

/// Push a kopia `policy set` boolean knob. These are VALUED flags
/// (`--flag=true|false`, "inherit" being the unset state) — NOT kingpin
/// `--flag/--no-flag` booleans: a bare `--ignore-file-errors` fails with
/// "expected argument for flag" (caught by the `policy_knobs` e2e; the old
/// `--no-` form never reached kopia). Verified against
/// `kopia policy set --help` (0.23).
fn push_valued_tristate(args: &mut Vec<String>, flag: &str, value: Option<bool>) {
    match value {
        Some(true) => args.push(format!("--{flag}=true")),
        Some(false) => args.push(format!("--{flag}=false")),
        None => {}
    }
}

/// Split [`PolicyArgs`] into the path-scoped part and an optional
/// identity-scoped part. kopia rejects `--max-parallel-snapshots` on a
/// path-scoped policy ("max parallel snapshots cannot be specified for paths,
/// only global, username@hostname or @hostname" — the `policy_knobs` e2e
/// regression), so that one knob must be applied in a second `policy set`
/// against the bare `username@hostname` identity. Pure.
pub fn split_policy_scopes(mut policy: PolicyArgs) -> (PolicyArgs, Option<PolicyArgs>) {
    let identity = policy.max_parallel_snapshots.take().map(|n| PolicyArgs {
        max_parallel_snapshots: Some(n),
        ..Default::default()
    });
    (policy, identity)
}

/// Build the args for `kopia snapshot restore <id> <target>` plus options. Pure
/// so it is unit-testable without spawning kopia. Every `--[no-]flag` form here
/// was smoke-tested against the pinned kopia 0.23.1 (`kopia snapshot restore
/// --help`); the real-kopia integration test in
/// `crates/kopia/tests/integration_roundtrip.rs` is the permanent guard that
/// kopia actually accepts them, not just that the argv shape looks right.
fn restore_args(id: &str, target_dir: &str, opts: &RestoreOptions) -> Vec<String> {
    let mut args = vec![
        "snapshot".into(),
        "restore".into(),
        id.to_string(),
        target_dir.to_string(),
    ];
    push_tristate(
        &mut args,
        "ignore-permission-errors",
        opts.ignore_permission_errors,
    );
    push_tristate(
        &mut args,
        "write-files-atomically",
        opts.write_files_atomically,
    );
    push_tristate(&mut args, "overwrite-files", opts.overwrite_files);
    push_tristate(
        &mut args,
        "overwrite-directories",
        opts.overwrite_directories,
    );
    push_tristate(&mut args, "overwrite-symlinks", opts.overwrite_symlinks);
    push_tristate(&mut args, "write-sparse-files", opts.write_sparse_files);
    push_tristate(&mut args, "skip-owners", opts.skip_owners);
    push_tristate(&mut args, "skip-permissions", opts.skip_permissions);
    push_tristate(&mut args, "skip-times", opts.skip_times);
    push_tristate(&mut args, "ignore-errors", opts.ignore_errors);
    push_tristate(&mut args, "skip-existing", opts.skip_existing);
    push_tristate(&mut args, "delete-extra", opts.delete_extra);
    if let Some(p) = opts.parallel {
        args.push("--parallel".into());
        args.push(p.to_string());
    }
    args
}

/// Build the args for `kopia snapshot create <source> --json [flags]` plus
/// options. Pure so it is unit-testable without spawning kopia. `--fail-fast`
/// is a kingpin `--[no-]flag` tri-state (smoke-tested against the pinned
/// kopia 0.23.1: `snapshot create --fail-fast --upload-limit-mb 100
/// --description "smoke test"` is accepted; the real-kopia integration test
/// in `crates/kopia/tests/integration_roundtrip.rs` is the permanent guard).
/// All-default `opts` reproduces the pre-M4 argv byte-for-byte (tested).
fn snapshot_create_args(
    source_path: &str,
    tags: &BTreeMap<String, String>,
    override_source: Option<&str>,
    opts: &SnapshotCreateOptions,
) -> Vec<String> {
    let mut args = vec![
        "snapshot".into(),
        "create".into(),
        source_path.to_string(),
        "--json".into(),
    ];
    if let Some(src) = override_source {
        args.push("--override-source".into());
        args.push(src.to_string());
    }
    for (k, v) in tags {
        args.push("--tags".into());
        args.push(format!("{k}:{v}"));
    }
    push_tristate(&mut args, "fail-fast", opts.fail_fast);
    if let Some(mb) = opts.upload_limit_mb {
        args.push("--upload-limit-mb".into());
        args.push(mb.to_string());
    }
    if let Some(desc) = &opts.description {
        args.push("--description".into());
        args.push(desc.clone());
    }
    args
}

/// Build the args for `kopia snapshot verify` plus options. Pure.
fn verify_args(opts: &VerifyOptions) -> Vec<String> {
    let mut args = vec!["snapshot".into(), "verify".into()];
    for src in &opts.sources {
        args.push("--sources".into());
        args.push(src.clone());
    }
    if let Some(pct) = opts.verify_files_percent {
        args.push("--verify-files-percent".into());
        args.push(pct.to_string());
    }
    if let Some(m) = opts.max_errors {
        args.push("--max-errors".into());
        args.push(m.to_string());
    }
    if let Some(p) = opts.parallel {
        args.push("--parallel".into());
        args.push(p.to_string());
    }
    if let Some(p) = opts.file_parallelism {
        args.push("--file-parallelism".into());
        args.push(p.to_string());
    }
    if let Some(q) = opts.file_queue_length {
        args.push("--file-queue-length".into());
        args.push(q.to_string());
    }
    args
}

/// Build the args for `kopia repository connect <backend> [flags]`. Pure so the
/// read-only vs read-write argv split is unit-testable without spawning kopia.
/// `--readonly` (kopia's persistent read-only client-config bit) is appended
/// only for read-only (browse) connects.
fn connect_args(spec: &ConnectSpec, cache: CacheTuning, readonly: bool) -> Vec<String> {
    let mut args = vec!["repository".into(), "connect".into()];
    args.extend(spec.backend_args());
    args.extend(cache.args());
    if readonly {
        args.push("--readonly".into());
    }
    args
}

/// Build the args for `kopia repository sync-to <destination> [flags]`. Pure so it
/// is unit-testable without spawning kopia (ADR-0005 §13(d) / issue #216). The
/// destination's backend selection reuses `ConnectSpec::backend_args`, so every
/// backend is wired through. `--must-exist`/`--times`/`--update` are kopia
/// (kingpin) BOOLEAN flags: `--must-exist=false` is a parse error (`unexpected
/// false`) — but the `--no-must-exist`/`--no-times`/`--no-update` negated forms
/// ARE accepted (smoke-tested against kopia 0.23.1), so [`push_tristate`] is used
/// for all three exactly like `snapshot restore`'s tri-states. `None` on any
/// field omits its flag entirely, leaving kopia's own default in effect.
fn sync_to_args(destination: &ConnectSpec, opts: &SyncToOptions) -> Vec<String> {
    let mut args = vec!["repository".into(), "sync-to".into()];
    args.extend(destination.backend_args());
    if let Some(p) = opts.parallel {
        args.push("--parallel".into());
        args.push(p.to_string());
    }
    if opts.delete_extra {
        args.push("--delete".into());
    }
    push_tristate(&mut args, "must-exist", opts.must_exist);
    push_tristate(&mut args, "times", opts.times);
    push_tristate(&mut args, "update", opts.update);
    if let Some(s) = opts.max_download_speed_bytes_per_second {
        args.push("--max-download-speed".into());
        args.push(s.to_string());
    }
    if let Some(s) = opts.max_upload_speed_bytes_per_second {
        args.push("--max-upload-speed".into());
        args.push(s.to_string());
    }
    args
}

/// Build the args for `kopia policy set <target>` plus flags. Pure.
fn policy_set_args(target: &str, policy: &PolicyArgs) -> Vec<String> {
    let mut args = vec!["policy".into(), "set".into(), target.to_string()];
    if let Some(c) = &policy.compression {
        args.push("--compression".into());
        args.push(c.clone());
    }
    if let Some(s) = &policy.splitter {
        args.push("--splitter".into());
        args.push(s.clone());
    }
    for pat in &policy.ignore {
        args.push("--add-ignore".into());
        args.push(pat.clone());
    }
    for pat in &policy.never_compress {
        args.push("--add-never-compress".into());
        args.push(pat.clone());
    }
    push_valued_tristate(&mut args, "ignore-cache-dirs", policy.ignore_cache_dirs);
    push_valued_tristate(&mut args, "ignore-file-errors", policy.ignore_file_errors);
    push_valued_tristate(&mut args, "ignore-dir-errors", policy.ignore_dir_errors);
    push_valued_tristate(
        &mut args,
        "ignore-unknown-types",
        policy.ignore_unknown_types,
    );
    if let Some(n) = policy.max_parallel_snapshots {
        args.push("--max-parallel-snapshots".into());
        args.push(n.to_string());
    }
    if let Some(n) = policy.max_parallel_file_reads {
        args.push("--max-parallel-file-reads".into());
        args.push(n.to_string());
    }
    if let Some(n) = policy.keep_latest {
        args.push("--keep-latest".into());
        args.push(n.to_string());
    }
    if let Some(n) = policy.keep_hourly {
        args.push("--keep-hourly".into());
        args.push(n.to_string());
    }
    if let Some(n) = policy.keep_daily {
        args.push("--keep-daily".into());
        args.push(n.to_string());
    }
    if let Some(n) = policy.keep_weekly {
        args.push("--keep-weekly".into());
        args.push(n.to_string());
    }
    if let Some(n) = policy.keep_monthly {
        args.push("--keep-monthly".into());
        args.push(n.to_string());
    }
    if let Some(n) = policy.keep_annual {
        args.push("--keep-annual".into());
        args.push(n.to_string());
    }
    args.extend(policy.extra_args.iter().cloned());
    args
}

/// Build the args for `kopia server start` (everything except the secret password,
/// which [`KopiaClient::server_start`] appends at exec time). Pure and unit-testable.
///
/// Always emits `--insecure` (no in-pod TLS; the user's ingress terminates TLS).
/// [`ServerAuthMode::Password`] emits `--server-username`; [`ServerAuthMode::None`]
/// emits `--without-password`.
fn server_start_args(spec: &ServerStartSpec) -> Vec<String> {
    let mut args = vec![
        "server".into(),
        "start".into(),
        "--address".into(),
        spec.address.clone(),
        // No in-pod TLS — this is kopia's *no-TLS* switch, required in every mode.
        "--insecure".into(),
    ];
    if spec.ui {
        args.push("--ui".into());
    }
    match &spec.auth {
        ServerAuthMode::Password { username } => {
            args.push("--server-username".into());
            args.push(username.clone());
        }
        ServerAuthMode::None => {
            args.push("--without-password".into());
            // kopia 0.23+ refuses to bind a non-loopback address with
            // `--insecure --without-password` unless this escape hatch is set
            // (it is exactly the "exposed unauthenticated server" the project gates
            // behind `acknowledgeInsecure`). We always bind `0.0.0.0` so the Service
            // can reach the server, so the flag is required here.
            args.push("--allow-extremely-dangerous-unauthenticated-server-on-the-network".into());
        }
    }
    args
}

/// Extract the JSON result from kopia stdout. kopia prints a single JSON object
/// or array; progress goes to stderr. We find the first `{` or `[` and return
/// the trimmed remainder, which is the JSON value. Returns `None` if stdout
/// contains no `{`/`[`.
fn extract_json(stdout: &str) -> Option<&str> {
    let trimmed = stdout.trim();
    let start = trimmed.find(['{', '['])?;
    Some(trimmed[start..].trim())
}

#[cfg(test)]
mod tests;
