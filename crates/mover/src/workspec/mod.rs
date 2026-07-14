//! The mover work spec: the JSON contract between the controller and a mover
//! pod.
//!
//! Per ADR §4.10, the controller writes a `ConfigMap` per `Snapshot`/`Restore`
//! run with the resolved identity, paths, hook plan, and options; the mover
//! reads it from a downward-API-mounted file. This module is **pure data** plus
//! serde — no kube, no kopia subprocess. It is exhaustively round-trip tested.
//!
//! The spec carries *resolved* values only (identity already rendered, repo
//! connect info concrete). The mover never re-derives anything: it executes
//! exactly what the controller decided.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Which operation this mover run performs. Externally tagged so exactly one
/// operation payload is representable (mirrors the api crate's enum discipline;
/// a new variant cannot compile until every `match` handles it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Operation {
    /// Create a kopia snapshot of `source` and report stats back to the Snapshot.
    Snapshot(SnapshotOp),
    /// Restore a snapshot's contents into `target`.
    Restore(RestoreOp),
    /// Delete a snapshot from the repository (finalizer path, deletionPolicy:
    /// Delete).
    SnapshotDelete(SnapshotDeleteOp),
    /// Bootstrap a repository: connect (adopt an existing repo), or — when
    /// `autoCreate` and the backend is reachable with valid creds — create it,
    /// then report its identity + catalog back to the controller. The
    /// connect/create lifecycle for object-store backends the controller cannot
    /// reach in-process (ADR §5.4). Result is written to the work-spec ConfigMap,
    /// not the CR status (the controller owns the Repository status).
    BootstrapRepository(BootstrapRepositoryOp),
    /// Run `kopia maintenance run` (quick or full) for a repository the
    /// controller cannot reach in-process. The mover reads the ownership lease,
    /// applies the takeover policy, runs maintenance when it holds the lease, and
    /// PATCHes the `Maintenance` `.status` directly (ADR §3.7/§5.4).
    Maintenance(MaintenanceOp),
    /// Reconcile a single snapshot's kopia-side pin state with `Snapshot.spec.pin`
    /// (ADR-0005 §13(c)). `pin: true` runs `kopia snapshot pin --add`, `pin: false`
    /// runs `--remove`, so kopia's own maintenance/expire honors the pin on object
    /// stores. The GFS-retention exemption is wired separately in the controller;
    /// this op is the kopia-side half. Idempotent.
    SnapshotPin(SnapshotPinOp),
    /// Verify a snapshot's restorability (ADR-0005 §4). `quick` runs `kopia snapshot
    /// verify` (blob-level); `deep` scratch-restores the latest snapshot into an
    /// ephemeral volume and (optionally) checks the result against a CEL
    /// `successExpr`. Owns its own connect lifecycle like maintenance.
    Verify(VerifyOp),
    /// Mirror the source repository's blobs to a destination backend
    /// (`kopia repository sync-to`), ADR-0005 §13(d). Connect to the source (the
    /// `repository` field), then sync to `destination`. PATCHes the
    /// `RepositoryReplication` `.status`. Owns its own connect lifecycle like
    /// maintenance.
    Replicate(ReplicateOp),
    /// Hold a **read-only** repository connection open for an interactive
    /// `kubectl kopiur browse` session (M7a). Connects with `--readonly`, writes
    /// the readiness marker ([`crate::env::READY_MARKER`]) so the pod's
    /// readinessProbe (`kopiur-mover ready`) passes, then sleeps for
    /// `ttlSeconds` and exits cleanly. The CLI drives the actual reads
    /// ([`kopiur_kopia::SessionCmd`]) via pod exec; the mover never PATCHes a
    /// status (`targetRef` names nothing the controller owns).
    BrowseSession(BrowseSessionOp),
}

impl Operation {
    /// Stable discriminant string for logging/metrics.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Operation::Snapshot(_) => "Snapshot",
            Operation::Restore(_) => "Restore",
            Operation::SnapshotDelete(_) => "SnapshotDelete",
            Operation::BootstrapRepository(_) => "BootstrapRepository",
            Operation::Maintenance(_) => "Maintenance",
            Operation::SnapshotPin(_) => "SnapshotPin",
            Operation::Verify(_) => "Verify",
            Operation::Replicate(_) => "Replicate",
            Operation::BrowseSession(_) => "BrowseSession",
        }
    }
}

/// Payload for a backup run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotOp {
    /// Absolute path inside the mover pod to snapshot (e.g. `/data`).
    pub source_path: String,
    /// Tags to attach to the snapshot (`key:value` pairs).
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// Resolved kopia `policy set` knobs to apply to this snapshot's source path
    /// before `snapshot create` (compression / never-compress / ignore rules /
    /// ignore-cache-dirs / backup-side error handling / upload parallelism /
    /// extraArgs). The controller resolves these from
    /// `SnapshotPolicy.spec.{compression,files,errorHandling,upload,extraArgs}`
    /// (ADR-0005 §13(b)/§13(f), ADR-0004 §4b). Empty ⇒ leave kopia's defaults.
    #[serde(default, skip_serializing_if = "PolicyArgsSpec::is_empty")]
    pub policy: PolicyArgsSpec,
    /// `snapshot create --[no-]fail-fast` (M4 flag sweep, issue #216 category
    /// sweep). Resolved from `SnapshotPolicy.spec.errorHandling.failFast`.
    /// `#[serde(default)]` so old-wire work-spec JSON (stamped before this
    /// field existed) still decodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_fast: Option<bool>,
    /// `snapshot create --upload-limit-mb` (M4 flag sweep). Resolved from
    /// `SnapshotPolicy.spec.upload.limitMb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_limit_mb: Option<i64>,
    /// `snapshot create --description` (M4 flag sweep). Per-invocation, from
    /// `Snapshot.spec.description` (not the recipe) — scheduled/discovered
    /// runs never set this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl SnapshotOp {
    /// Translate the carried `snapshot create` flags into the kopia client's
    /// options. Pure so the workspec→kopia-client mapping is unit-testable.
    pub fn create_options(&self) -> kopiur_kopia::SnapshotCreateOptions {
        kopiur_kopia::SnapshotCreateOptions {
            fail_fast: self.fail_fast,
            upload_limit_mb: self.upload_limit_mb,
            description: self.description.clone(),
        }
    }
}

/// Serializable mirror of [`kopiur_kopia::PolicyArgs`] for the work spec (the kopia
/// client's type isn't serde). The controller fills it from the flattened
/// `SnapshotPolicy` policy knobs; the mover converts back and runs `kopia policy
/// set` against the snapshot's source identity before creating the snapshot. This
/// is what makes `compression`/`files`/`errorHandling`/`upload`/`extraArgs`
/// actually reach kopia (no-inert-fields). ADR-0005 §13(b)/§13(f).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyArgsSpec {
    /// `--compression` algorithm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<String>,
    /// `--add-ignore` globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    /// `--add-never-compress` globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub never_compress: Vec<String>,
    /// `--[no-]ignore-cache-dirs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_cache_dirs: Option<bool>,
    /// `--[no-]ignore-file-errors`. ADR-0005 §13(b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_file_errors: Option<bool>,
    /// `--[no-]ignore-dir-errors`. ADR-0005 §13(b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_dir_errors: Option<bool>,
    /// `--[no-]ignore-unknown-types`. ADR-0005 §13(b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_unknown_types: Option<bool>,
    /// `--max-parallel-snapshots`. ADR-0005 §13(f).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_snapshots: Option<u32>,
    /// `--max-parallel-file-reads`. ADR-0005 §13(f).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_file_reads: Option<u32>,
    /// Verbatim extra `policy set` flags (the CRD escape hatch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

impl PolicyArgsSpec {
    /// Whether every knob is unset (so the mover skips `kopia policy set` entirely).
    pub fn is_empty(&self) -> bool {
        self.compression.is_none()
            && self.ignore.is_empty()
            && self.never_compress.is_empty()
            && self.ignore_cache_dirs.is_none()
            && self.ignore_file_errors.is_none()
            && self.ignore_dir_errors.is_none()
            && self.ignore_unknown_types.is_none()
            && self.max_parallel_snapshots.is_none()
            && self.max_parallel_file_reads.is_none()
            && self.extra_args.is_empty()
    }

    /// Convert to the kopia client's [`PolicyArgs`](kopiur_kopia::PolicyArgs).
    /// `splitter` is never set here — the object splitter is a repository property
    /// (ADR-0004 §4b removed the per-policy splitter).
    pub fn to_kopia(&self) -> kopiur_kopia::PolicyArgs {
        kopiur_kopia::PolicyArgs {
            compression: self.compression.clone(),
            splitter: None,
            ignore: self.ignore.clone(),
            never_compress: self.never_compress.clone(),
            ignore_cache_dirs: self.ignore_cache_dirs,
            ignore_file_errors: self.ignore_file_errors,
            ignore_dir_errors: self.ignore_dir_errors,
            ignore_unknown_types: self.ignore_unknown_types,
            max_parallel_snapshots: self.max_parallel_snapshots,
            max_parallel_file_reads: self.max_parallel_file_reads,
            extra_args: self.extra_args.clone(),
        }
    }

    /// Resolve a [`PolicyArgsSpec`] from a `SnapshotPolicy` spec's flattened policy
    /// knobs (ADR-0004 §4b, ADR-0005 §13(b)/§13(f)). The single mapping the
    /// controller uses so the policy fields are never inert. `max_parallel_*` are
    /// `i64` on the CRD (schemars) and clamped to `u32` for kopia's flag.
    pub fn from_policy(spec: &kopiur_api::SnapshotPolicySpec) -> PolicyArgsSpec {
        let (compression, never_compress) = match &spec.compression {
            Some(c) => (c.compressor.clone(), c.never_compress.clone()),
            None => (None, Vec::new()),
        };
        let (ignore, ignore_cache_dirs) = match &spec.files {
            // `ignore_cache_dirs` is a bool on the CRD; only emit the flag when true
            // (Some(true)) — an unset/false leaves kopia's default rather than forcing
            // `--no-ignore-cache-dirs`, matching the "absent = kopia default" contract.
            Some(f) => (f.ignore_rules.clone(), f.ignore_cache_dirs.then_some(true)),
            // The apiserver only server-side-defaults NESTED fields when the parent
            // object is present, so a `SnapshotPolicy` that omits `files:` entirely
            // (the common case) never gets `Files.ignore_rules`'s schema default
            // applied. Fall back to the SAME `default_ignore_rules()` fn the API
            // layer wires as the serde/schemars default, so there is one source of
            // truth for the OS-artifact exclude set regardless of which of the two
            // "absent" shapes (`files:` missing vs. `files: {}`) the spec took.
            None => (kopiur_api::snapshot_policy::default_ignore_rules(), None),
        };
        let eh = spec.error_handling.as_ref();
        let up = spec.upload.as_ref();
        PolicyArgsSpec {
            compression,
            ignore,
            never_compress,
            ignore_cache_dirs,
            ignore_file_errors: eh.and_then(|e| e.ignore_file_errors.then_some(true)),
            ignore_dir_errors: eh.and_then(|e| e.ignore_dir_errors.then_some(true)),
            ignore_unknown_types: eh.and_then(|e| e.ignore_unknown_types.then_some(true)),
            max_parallel_snapshots: up
                .and_then(|u| u.max_parallel_snapshots)
                .map(|n| n.max(0) as u32),
            max_parallel_file_reads: up
                .and_then(|u| u.max_parallel_file_reads)
                .map(|n| n.max(0) as u32),
            extra_args: spec.extra_args.clone(),
        }
    }
}

impl ThrottleSpec {
    /// Resolve a [`ThrottleSpec`] from a repository's `moverDefaults.throttle`
    /// (ADR-0005 §13(e)). `None`/absent ⇒ an empty spec (the mover skips `throttle
    /// set`). The single mapping the controller uses.
    pub fn from_mover_defaults(
        defaults: Option<&kopiur_api::common::MoverDefaults>,
    ) -> ThrottleSpec {
        match defaults.and_then(|d| d.throttle.as_ref()) {
            Some(t) => ThrottleSpec {
                upload_bytes_per_second: t.upload_bytes_per_second,
                download_bytes_per_second: t.download_bytes_per_second,
                read_ops_per_second: t.read_ops_per_second,
                write_ops_per_second: t.write_ops_per_second,
            },
            None => ThrottleSpec::default(),
        }
    }
}

impl CreateOptionsSpec {
    /// Resolve a [`CreateOptionsSpec`] from a repository's `create` behavior
    /// (ADR-0005 §13(a)). `None`/absent ⇒ an empty spec. The single mapping the
    /// controller uses so `create.{encryption,splitter,hash,ecc}` reach the
    /// bootstrap mover's `kopia repository create`.
    pub fn from_create(create: Option<&kopiur_api::common::CreateBehavior>) -> CreateOptionsSpec {
        match create {
            Some(c) => CreateOptionsSpec {
                encryption: c.encryption.clone(),
                splitter: c.splitter.clone(),
                hash: c.hash.clone(),
                ecc: c.ecc.as_ref().and_then(|e| e.algorithm.clone()),
                ecc_overhead_percent: c.ecc.as_ref().and_then(|e| e.overhead_percent),
            },
            None => CreateOptionsSpec::default(),
        }
    }
}

/// Stable identity anchors for re-resolving a snapshot's CURRENT manifest id.
///
/// kopia's `UpdateSnapshot` (pin/unpin) assigns a NEW manifest id and deletes
/// the old one, so the id recorded at create time goes stale once a snapshot is
/// pinned. A snapshot's source path and start time survive that rewrite, so the
/// mover re-matches on them (see [`crate::resolve::match_current_manifest`]) to
/// re-stamp `status.snapshot.kopiaSnapshotID` after a pin, and to self-heal a
/// stale id at delete/restore time. All fields are optional so older work specs
/// (and Snapshots with no recorded identity/timing) still round-trip and fall
/// back to the previous behavior.
///
/// `source_path` alone is NOT globally unique: the same PVC subpath repeats
/// across namespaces, and — in a shared repository — across clusters, so a
/// path-only match can select (and, in the delete path, DELETE) a different
/// identity's snapshot. `username`/`hostname` close that hole: when both are
/// present the matchers additionally require them to match; when they are
/// absent (anchors captured before this fix) matching falls back to the
/// previous path-only behavior exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotAnchor {
    /// The snapshotted source path — the authoritative match key (the
    /// mover-recorded user/host can differ from the resolved identity).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_path: String,
    /// RFC3339 start time recorded for this snapshot — the disambiguator when
    /// several snapshots share `source_path`. Absent on older work specs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// The recorded kopia `username`, when known — required (with `hostname`)
    /// to disambiguate a match by identity, not path alone. Absent on anchors
    /// captured before this fix; matchers then fall back to path-only
    /// behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// The recorded kopia `hostname`, when known. See `username`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

impl SnapshotAnchor {
    /// Whether this anchor carries nothing usable (so resolution falls back to
    /// the stored id alone).
    pub fn is_empty(&self) -> bool {
        self.source_path.is_empty() && self.start_time.is_none()
    }

    /// The anchor's `start_time` parsed to a UTC instant, if present and valid.
    pub fn start_instant(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.start_time
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
    }

    /// The `(username, hostname)` identity filter for
    /// [`crate::resolve::match_current_manifest`], or `None` when either half
    /// is missing (older anchors) — in which case matching stays path-only.
    pub fn identity_filter(&self) -> Option<(&str, &str)> {
        self.username.as_deref().zip(self.hostname.as_deref())
    }
}

/// Which snapshot a restore run targets. Externally tagged (exactly one variant)
/// so a restore is either pre-resolved by the controller or resolved in-Job —
/// never both, and a new variant can't compile until every `match` handles it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestoreSelection {
    /// A concrete kopia snapshot manifest id the controller already resolved
    /// (from a `snapshotRef` Snapshot CR, or an explicit `identity.snapshotID`).
    /// Self-heals a stale id via [`RestoreOp::anchor`].
    Snapshot(String),
    /// Resolve the snapshot in-Job by listing the repository for an identity and
    /// picking newest/offset/asOf. This is what makes "restore the latest" work
    /// for object stores: in-process listing only works for filesystem repos, so
    /// `fromPolicy`/`identity`-without-id defer the listing to the mover, which
    /// reaches every backend.
    Resolve(RestoreSelector),
}

/// An unresolved restore source: list the repository for this kopia identity and
/// pick a snapshot by `asOf` (point-in-time) then `offset` (0 = latest). Mirrors
/// the `Restore` CRD's `fromPolicy`/`identity` selection so the mover resolves it
/// exactly as the controller's filesystem path used to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSelector {
    /// The kopia `username` to match.
    pub username: String,
    /// The kopia `hostname` to match.
    pub hostname: String,
    /// The kopia source path to match; absent matches any path for the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// Restore the newest snapshot at or before this RFC3339 instant (validated at
    /// admission; the mover re-parses defensively).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// Which snapshot to pick: 0 = latest, 1 = previous, and so on.
    #[serde(default)]
    pub offset: i64,
    /// What to do when no snapshot matches once the wait window closes: `Fail`
    /// (exit non-zero) or `Continue` (leave the target empty — deploy-or-restore).
    pub on_missing: kopiur_api::restore::OnMissingSnapshot,
    /// Absolute RFC3339 instant to keep re-listing until before applying
    /// `on_missing` — the `waitTimeout` window, anchored by the controller at the
    /// Restore's creation (NOT at Job start), so it matches the `snapshotRef` path
    /// and is stable across pod retries. `None` ⇒ resolve once, no wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_deadline: Option<String>,
}

/// Payload for a restore run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOp {
    /// Which snapshot to restore: a controller-resolved id, or an in-Job selector.
    pub source: RestoreSelection,
    /// Absolute path inside the mover pod to restore into (e.g. `/data`).
    pub target_path: String,
    /// Stable identity anchors for the referenced snapshot, used to self-heal a
    /// stale id (kopia rewrites the manifest id on pin) when a
    /// [`RestoreSelection::Snapshot`] restore reports the id not found. Empty ⇒ no
    /// fallback; never set for [`RestoreSelection::Resolve`] (it lists fresh).
    #[serde(default, skip_serializing_if = "SnapshotAnchor::is_empty")]
    pub anchor: SnapshotAnchor,
    /// `--[no-]ignore-permission-errors` (Restore CRD `options`; kopia default
    /// true). `None` lets kopia use its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_permission_errors: Option<bool>,
    /// `--[no-]write-files-atomically` (Restore CRD `options`). `None` lets kopia
    /// use its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_files_atomically: Option<bool>,
    /// `--parallel` (M2 flag sweep). `#[serde(default)]` so old-wire work-spec
    /// JSON (stamped before this field existed) still decodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--[no-]write-sparse-files` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_sparse_files: Option<bool>,
    /// `--[no-]skip-owners` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_owners: Option<bool>,
    /// `--[no-]skip-permissions` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_permissions: Option<bool>,
    /// `--[no-]skip-times` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_times: Option<bool>,
    /// `--[no-]overwrite-files` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_files: Option<bool>,
    /// `--[no-]overwrite-directories` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_directories: Option<bool>,
    /// `--[no-]overwrite-symlinks` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_symlinks: Option<bool>,
    /// `--[no-]ignore-errors` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_errors: Option<bool>,
    /// `--[no-]skip-existing` (M2 flag sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_existing: Option<bool>,
    /// `--[no-]delete-extra`: mirrors Restore CRD `options.enableFileDeletion`
    /// (`false` by default). Fixes the bug where `enableFileDeletion` was
    /// settable via CRD/CLI/migrate but consumed by nothing — kopia never
    /// received `--delete-extra`, so an "exact mirror" restore was silently
    /// additive.
    #[serde(default)]
    pub delete_extra: bool,
}

impl RestoreOp {
    /// Translate the carried restore flags into the kopia client's options.
    /// Every field is mapped explicitly (no `..Default::default()` swallowing a
    /// field silently) — this is the regression guard for the M2 gap-sweep bug
    /// class: plumbing that exists end-to-end but a field never reaches it.
    ///
    /// ```
    /// use kopiur_mover::workspec::{RestoreOp, RestoreSelection};
    ///
    /// let op = RestoreOp {
    ///     source: RestoreSelection::Snapshot("k1".into()),
    ///     target_path: "/data".into(),
    ///     anchor: Default::default(),
    ///     ignore_permission_errors: Some(false),
    ///     write_files_atomically: Some(true),
    ///     parallel: Some(4),
    ///     write_sparse_files: Some(true),
    ///     skip_owners: Some(true),
    ///     skip_permissions: Some(false),
    ///     skip_times: Some(true),
    ///     overwrite_files: Some(false),
    ///     overwrite_directories: Some(false),
    ///     overwrite_symlinks: Some(true),
    ///     ignore_errors: Some(false),
    ///     skip_existing: Some(true),
    ///     delete_extra: true,
    /// };
    /// let opts = op.restore_options();
    /// assert_eq!(opts.ignore_permission_errors, Some(false));
    /// assert_eq!(opts.write_files_atomically, Some(true));
    /// assert_eq!(opts.parallel, Some(4));
    /// assert_eq!(opts.delete_extra, Some(true));
    /// ```
    pub fn restore_options(&self) -> kopiur_kopia::RestoreOptions {
        kopiur_kopia::RestoreOptions {
            ignore_permission_errors: self.ignore_permission_errors,
            write_files_atomically: self.write_files_atomically,
            parallel: self.parallel,
            write_sparse_files: self.write_sparse_files,
            skip_owners: self.skip_owners,
            skip_permissions: self.skip_permissions,
            skip_times: self.skip_times,
            overwrite_files: self.overwrite_files,
            overwrite_directories: self.overwrite_directories,
            overwrite_symlinks: self.overwrite_symlinks,
            ignore_errors: self.ignore_errors,
            skip_existing: self.skip_existing,
            // `enableFileDeletion` stays a plain bool at the CRD/work-spec layer
            // (no tri-state is exposed); `false` omits the flag entirely, exactly
            // reproducing today's (additive) argv.
            delete_extra: self.delete_extra.then_some(true),
        }
    }
}

/// Payload for a snapshot-delete run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDeleteOp {
    /// The snapshot manifest id to delete.
    pub snapshot_id: String,
    /// Stable identity anchors for the snapshot, used to self-heal a stale
    /// `snapshot_id`: kopia rewrites the manifest id on pin, so the finalizer's
    /// recorded id can point at a deleted manifest while the real (pinned)
    /// snapshot lives under a different id. Without this the idempotent
    /// "no snapshots matched" path would silently ORPHAN the live snapshot under
    /// `deletionPolicy: Delete`. Empty ⇒ delete by id only (old behavior).
    #[serde(default, skip_serializing_if = "SnapshotAnchor::is_empty")]
    pub anchor: SnapshotAnchor,
}

/// Payload for a repository-bootstrap run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapRepositoryOp {
    /// Create the repository when connect fails AND the backend is reachable
    /// with valid credentials (mirrors `Repository.spec.create.enabled`). The
    /// connect-first ordering means an existing repo is always adopted, never
    /// recreated; create is gated so a wrong password / locked repo is surfaced
    /// instead of silently spawning a second repository.
    #[serde(default)]
    pub auto_create: bool,
    /// The stable kopia maintenance owner (`user@hostname`, derived from the
    /// managed lease — `kopiur_api::maintenance::kopia_owner_for_lease`) to
    /// stamp on a repository this bootstrap CREATES. Adopted repositories are
    /// never re-stamped (their existing owner is meaningful; takeoverPolicy
    /// governs). Absent on old work specs (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_owner: Option<String>,
    /// Run `snapshot list` and return the entries so the controller can
    /// materialize `origin: discovered` Snapshot CRs. The snapshot *count* is
    /// always reported; the entries are only returned when this is set (the
    /// controller sets it for namespaced `Repository`, not `ClusterRepository`,
    /// whose cross-namespace placement is a separate concern).
    #[serde(default)]
    pub scan_catalog: bool,
    /// Create-time-fixed repository format knobs honored only when this bootstrap
    /// actually *creates* the repository (`auto_create` + connect-miss). The
    /// controller resolves these from `Repository.spec.create.{encryption,splitter,
    /// hash,ecc}` (ADR-0005 §13(a)); they're immutable post-create (§7).
    #[serde(default, skip_serializing_if = "CreateOptionsSpec::is_empty")]
    pub create_options: CreateOptionsSpec,
}

impl BootstrapRepositoryOp {
    /// The kopia client's [`CreateOptions`](kopiur_kopia::CreateOptions) for the
    /// create-time format knobs carried here.
    pub fn create_options(&self) -> kopiur_kopia::CreateOptions {
        self.create_options.to_kopia()
    }
}

/// Serializable mirror of [`kopiur_kopia::CreateOptions`] for the work spec (the
/// kopia client's type isn't serde). The controller fills it from the Repository's
/// `create.{encryption,splitter,hash,ecc}`; the mover converts back. ADR-0005 §13(a).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOptionsSpec {
    /// `--encryption` algorithm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<String>,
    /// `--object-splitter` algorithm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub splitter: Option<String>,
    /// `--block-hash` algorithm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// `--ecc` Reed-Solomon algorithm. ADR-0005 §13(a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecc: Option<String>,
    /// `--ecc-overhead-percent`. ADR-0005 §13(a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecc_overhead_percent: Option<i64>,
}

impl CreateOptionsSpec {
    /// Whether every field is unset (so it's elided from the wire entirely).
    pub fn is_empty(&self) -> bool {
        self.encryption.is_none()
            && self.splitter.is_none()
            && self.hash.is_none()
            && self.ecc.is_none()
            && self.ecc_overhead_percent.is_none()
    }

    /// Convert to the kopia client's [`CreateOptions`](kopiur_kopia::CreateOptions).
    pub fn to_kopia(&self) -> kopiur_kopia::CreateOptions {
        kopiur_kopia::CreateOptions {
            encryption: self.encryption.clone(),
            splitter: self.splitter.clone(),
            hash: self.hash.clone(),
            ecc: self.ecc.clone(),
            ecc_overhead_percent: self.ecc_overhead_percent,
        }
    }
}

/// Payload for a maintenance run.
///
/// The controller decides *which* pass is due (full subsumes quick) and passes
/// the lease parameters down; the mover makes the lease decision because reading
/// the current holder requires repo access (`kopia maintenance info`), which the
/// controller does not have for object stores. ADR §3.7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceOp {
    /// Which pass to run when the lease is held: quick (index/log) or full
    /// (content reclamation).
    pub mode: kopiur_kopia::MaintenanceMode,
    /// This `Maintenance`'s configured lease holder identity
    /// (`spec.ownership.owner`); compared against the repo's current holder.
    pub owner: String,
    /// What to do if the lease is held by a *different* owner. ADR §3.7.
    #[serde(default)]
    pub takeover_policy: kopiur_api::TakeoverPolicy,
}

/// Payload for a snapshot-pin reconcile run (ADR-0005 §13(c)).
///
/// The controller decides whether kopia's pin state needs to change (comparing
/// `Snapshot.spec.pin` against the observed pin) and, when it does, dispatches this
/// op; the mover runs `kopia snapshot pin <id> --add/--remove <pin>` so kopia's own
/// maintenance/expire respects the pin on object stores. Idempotent on the kopia
/// side, so a redundant op is harmless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPinOp {
    /// The kopia snapshot manifest id to (un)pin.
    pub snapshot_id: String,
    /// `true` → add the pin (exempt from expiry); `false` → remove it.
    pub pin: bool,
    /// Stable identity anchors for the snapshot. kopia's pin/unpin rewrites the
    /// manifest id (`UpdateSnapshot` saves a new manifest, deletes the old), so
    /// after (un)pinning the mover re-lists and re-resolves the CURRENT id via
    /// these anchors and reports it back, keeping
    /// `status.snapshot.kopiaSnapshotID` pointing at the live manifest. Empty ⇒
    /// the id is left as-is (older work specs).
    #[serde(default, skip_serializing_if = "SnapshotAnchor::is_empty")]
    pub anchor: SnapshotAnchor,
}

/// The fixed pin name kopiur applies to a `Snapshot` whose `spec.pin` is set
/// (ADR-0005 §13(c)). A stable name so add/remove target the same pin and so the
/// pin is recognizable in `kopia snapshot list` output.
pub const KOPIUR_PIN_NAME: &str = "kopiur-retain";

/// Which verification tier to run (ADR-0005 §4). Externally-tagged on the wire so
/// it round-trips as `{ "quick": {} }` / `{ "deep": {...} }` and a new tier cannot
/// compile until handled. Mirrors Maintenance's quick/full split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerifyTier {
    /// `kopia snapshot verify` (blob-level integrity), run often.
    Quick(QuickVerify),
    /// Scratch-restore the latest snapshot into an ephemeral volume, then discard.
    /// Run rarely; the heaviest, most thorough restorability proof.
    Deep(DeepVerify),
}

impl VerifyTier {
    /// Stable discriminant string for logging/metrics/status.
    pub fn kind_str(&self) -> &'static str {
        match self {
            VerifyTier::Quick(_) => "quick",
            VerifyTier::Deep(_) => "deep",
        }
    }
}

/// Quick (blob-level) verification knobs — a serializable mirror of
/// [`kopiur_kopia::VerifyOptions`] (the kopia type isn't serde).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickVerify {
    /// `--verify-files-percent`: fully read this percentage of files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_files_percent: Option<u8>,
    /// `--max-errors`: stop after this many errors (0 = never stop early).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_errors: Option<u32>,
    /// `--parallel`: verification parallelism.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--file-parallelism`: parallelism for file verification. `#[serde(default)]`
    /// so old-wire work specs (predating this field) still decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_parallelism: Option<u32>,
    /// `--file-queue-length`: queue length for file verification. `#[serde(default)]`
    /// so old-wire work specs (predating this field) still decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_queue_length: Option<u32>,
}

impl QuickVerify {
    /// Convert to the kopia client's [`VerifyOptions`](kopiur_kopia::VerifyOptions).
    pub fn to_kopia(&self) -> kopiur_kopia::VerifyOptions {
        kopiur_kopia::VerifyOptions {
            verify_files_percent: self.verify_files_percent,
            max_errors: self.max_errors,
            parallel: self.parallel,
            file_parallelism: self.file_parallelism,
            file_queue_length: self.file_queue_length,
        }
    }
}

/// Deep (scratch-restore) verification knobs (ADR-0005 §4). The latest snapshot for
/// the run's identity is restored into an ephemeral volume mounted at
/// [`Self::scratch_path`], then discarded; restore options reuse the kopia restore
/// path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepVerify {
    /// Absolute path inside the mover pod where the ephemeral scratch volume is
    /// mounted and the snapshot is restored.
    pub scratch_path: String,
    /// The snapshot manifest id to restore. Resolved by the controller (newest for
    /// the identity); `None` lets the mover resolve the latest itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    /// `restore --parallel`: restore parallelism for the scratch-restore (deep
    /// verify IS a restore under the hood). `#[serde(default)]` so old-wire work
    /// specs (predating this field) still decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
}

/// Payload for a verification run (ADR-0005 §4). Owns its own connect lifecycle
/// like maintenance: the controller decides which tier is due and passes the
/// optional CEL `successExpr` down; the mover runs the verify, evaluates the
/// predicate over the result, and PATCHes the `SnapshotPolicy` `.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyOp {
    /// Which tier to run.
    pub tier: VerifyTier,
    /// Optional CEL pass/fail predicate over the verify result (ADR-0005 §15).
    /// Validated at admission; when set and it evaluates `false`, the run fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_expr: Option<String>,
}

/// Payload for a repository-replication run (ADR-0005 §13(d)). The mover connects
/// to the **source** repository (the work-spec `repository` field), then runs
/// `kopia repository sync-to <destination>`. The destination's credentials arrive
/// via the environment like every other backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicateOp {
    /// The destination backend to mirror to (the same serializable wire type as the
    /// source `repository`). Converted to a kopia [`ConnectSpec`](kopiur_kopia::ConnectSpec)
    /// for `sync-to`.
    pub destination: RepositoryConnect,
    /// Prune destination-only blobs (`--delete`) for a true mirror. Default `false`
    /// (additive sync) — safer, so a misconfigured destination is never emptied.
    #[serde(default)]
    pub delete_extra: bool,
    /// `--parallel`: copy parallelism to the destination (issue #216; kopia
    /// default `1` — sequential). `#[serde(default)]` so old-wire work-spec JSON
    /// (stamped before this field existed) still decodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--[no-]must-exist`: fail instead of initializing the destination's
    /// repository-format blob (kopia default `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub must_exist: Option<bool>,
    /// `--[no-]times`: synchronize blob modification times to the destination
    /// (kopia default `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<bool>,
    /// `--[no-]update`: update blobs already present at the destination when the
    /// source copy is newer (kopia default `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<bool>,
    /// `--max-download-speed`: cap read throughput from the source, bytes/sec
    /// (kopia default: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_download_speed_bytes_per_second: Option<i64>,
    /// `--max-upload-speed`: cap write throughput to the destination, bytes/sec
    /// (kopia default: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_upload_speed_bytes_per_second: Option<i64>,
}

impl ReplicateOp {
    /// Project this op's sync-tuning fields into a
    /// [`SyncToOptions`](kopiur_kopia::SyncToOptions) for
    /// `KopiaClient::repository_sync_to_with_env`. Pure so the field → option
    /// mapping is unit-testable without a kopia binary (the regression guard for
    /// this whole gap class: plumbing that exists but a hardcoded `None` never
    /// reaches it).
    pub fn sync_options(&self) -> kopiur_kopia::SyncToOptions {
        kopiur_kopia::SyncToOptions {
            parallel: self.parallel,
            delete_extra: self.delete_extra,
            must_exist: self.must_exist,
            times: self.times,
            update: self.update,
            max_download_speed_bytes_per_second: self.max_download_speed_bytes_per_second,
            max_upload_speed_bytes_per_second: self.max_upload_speed_bytes_per_second,
        }
    }
}

/// Payload for a browse-session run (M7a). The session pod connects read-only,
/// signals readiness via the marker file, and idles until the TTL elapses — a
/// hard upper bound so an abandoned `kubectl kopiur browse` can never hold a
/// repository connection (and a pod) open forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseSessionOp {
    /// How long (seconds) the session pod stays alive after connecting before
    /// exiting cleanly. Defaults to [`default_browse_ttl`] (15 minutes).
    #[serde(default = "default_browse_ttl")]
    pub ttl_seconds: u64,
}

impl Default for BrowseSessionOp {
    fn default() -> Self {
        BrowseSessionOp {
            ttl_seconds: default_browse_ttl(),
        }
    }
}

/// The default browse-session TTL: 900 seconds (15 minutes).
fn default_browse_ttl() -> u64 {
    900
}

/// The resolved kopia identity (`username@hostname:path`). Pinned by the
/// controller at admission and never re-derived (ADR §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedIdentity {
    /// kopia username component.
    pub username: String,
    /// kopia hostname component.
    pub hostname: String,
    /// kopia source path component.
    pub source_path: String,
}

/// How to reach the repository. Externally tagged: exactly one backend.
///
/// This mirrors `kopiur_kopia::ConnectSpec` but is a *serializable* wire type
/// (the kopia client's `ConnectSpec` is intentionally not serde). The mover
/// converts one to the other. Credentials are NOT here: they arrive as env vars
/// (mounted Secret) so they never land in a ConfigMap.
///
/// The variants mirror the eight CRD `Backend` kinds one-to-one, so the
/// controller's `Backend -> RepositoryConnect` map is exhaustive (a new backend
/// cannot compile until it is wired through to the mover).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RepositoryConnect {
    /// Filesystem backend at a path.
    Filesystem {
        /// Absolute path to the repository root.
        path: String,
    },
    /// S3-compatible backend.
    S3 {
        /// Bucket name.
        bucket: String,
        /// Optional custom endpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        /// Optional key prefix.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// Optional region.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        /// Talk plain HTTP (`--disable-tls`) for HTTP-only endpoints.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        disable_tls: bool,
        /// Skip TLS certificate verification (`--disable-tls-verification`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        disable_tls_verification: bool,
        /// Authenticate via the ambient AWS credential chain (workload identity:
        /// IRSA / EKS Pod Identity) instead of static keys from the env. Defaults
        /// to `false` and is omitted from the wire when false, so work-spec
        /// ConfigMaps written before this field existed still parse.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        ambient_credentials: bool,
    },
    /// Azure Blob Storage backend.
    Azure {
        /// Blob container name.
        container: String,
        /// Storage account name (when not supplied via env).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_account: Option<String>,
        /// Optional object prefix.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// Google Cloud Storage backend.
    Gcs {
        /// Bucket name.
        bucket: String,
        /// Optional object prefix.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// Backblaze B2 backend.
    B2 {
        /// Bucket name.
        bucket: String,
        /// Optional object prefix.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// SFTP/SSH backend.
    Sftp {
        /// Server hostname.
        host: String,
        /// Path to the repository on the server.
        path: String,
        /// Server port (defaults to 22 when absent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// SSH username.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        /// Path to a private key file inside the mover pod.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keyfile: Option<String>,
    },
    /// WebDAV backend.
    WebDav {
        /// WebDAV server URL.
        url: String,
    },
    /// Rclone backend.
    Rclone {
        /// Rclone `remote:path`.
        remote_path: String,
        /// Go-duration for kopia's `--rclone-startup-timeout`; `None` leaves
        /// kopia's default (`15s`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        startup_timeout: Option<String>,
    },
    /// Google Drive backend (kopia native `gdrive`).
    Gdrive {
        /// Drive folder id that holds the repository.
        folder_id: String,
    },
}

impl RepositoryConnect {
    /// Stable backend discriminant for logging. Exhaustive: a new backend
    /// variant fails to compile until handled.
    pub fn kind_str(&self) -> &'static str {
        match self {
            RepositoryConnect::Filesystem { .. } => "Filesystem",
            RepositoryConnect::S3 { .. } => "S3",
            RepositoryConnect::Azure { .. } => "Azure",
            RepositoryConnect::Gcs { .. } => "Gcs",
            RepositoryConnect::B2 { .. } => "B2",
            RepositoryConnect::Sftp { .. } => "Sftp",
            RepositoryConnect::WebDav { .. } => "WebDav",
            RepositoryConnect::Rclone { .. } => "Rclone",
            RepositoryConnect::Gdrive { .. } => "Gdrive",
        }
    }

    /// Convert to the kopia client's connect spec. Exhaustive: a new backend
    /// variant fails to compile until handled.
    ///
    /// ```
    /// use kopiur_mover::workspec::RepositoryConnect;
    /// use kopiur_kopia::ConnectSpec;
    ///
    /// let wire = RepositoryConnect::Filesystem { path: "/repo".into() };
    /// assert_eq!(wire.kind_str(), "Filesystem");
    /// assert_eq!(
    ///     wire.to_connect_spec(),
    ///     ConnectSpec::Filesystem { path: "/repo".into() },
    /// );
    /// ```
    pub fn to_connect_spec(&self) -> kopiur_kopia::ConnectSpec {
        use kopiur_kopia::ConnectSpec;
        match self {
            RepositoryConnect::Filesystem { path } => ConnectSpec::Filesystem { path: path.into() },
            RepositoryConnect::S3 {
                bucket,
                endpoint,
                prefix,
                region,
                disable_tls,
                disable_tls_verification,
                ambient_credentials,
            } => ConnectSpec::S3 {
                bucket: bucket.clone(),
                endpoint: endpoint.clone(),
                prefix: prefix.clone(),
                region: region.clone(),
                disable_tls: *disable_tls,
                disable_tls_verification: *disable_tls_verification,
                ambient_credentials: *ambient_credentials,
            },
            RepositoryConnect::Azure {
                container,
                storage_account,
                prefix,
            } => ConnectSpec::Azure {
                container: container.clone(),
                storage_account: storage_account.clone(),
                prefix: prefix.clone(),
            },
            RepositoryConnect::Gcs { bucket, prefix } => ConnectSpec::Gcs {
                bucket: bucket.clone(),
                prefix: prefix.clone(),
                // The service-account JSON path is materialized by the mover from
                // the credentials Secret at runtime (see `crate::credentials`).
                credentials_file: None,
            },
            RepositoryConnect::B2 { bucket, prefix } => ConnectSpec::B2 {
                bucket: bucket.clone(),
                prefix: prefix.clone(),
            },
            RepositoryConnect::Sftp {
                host,
                path,
                port,
                username,
                keyfile,
            } => ConnectSpec::Sftp {
                host: host.clone(),
                path: path.clone(),
                port: *port,
                username: username.clone(),
                keyfile: keyfile.clone(),
                // keyfile/known_hosts are materialized by the mover from the
                // credentials Secret at runtime (see `crate::credentials`).
                known_hosts: None,
            },
            RepositoryConnect::WebDav { url } => ConnectSpec::WebDav { url: url.clone() },
            RepositoryConnect::Rclone {
                remote_path,
                startup_timeout,
            } => ConnectSpec::Rclone {
                remote_path: remote_path.clone(),
                // rclone.conf is materialized by the mover from the config Secret
                // at runtime (see `crate::credentials`).
                config_file: None,
                startup_timeout: startup_timeout.clone(),
            },
            RepositoryConnect::Gdrive { folder_id } => ConnectSpec::Gdrive {
                folder_id: folder_id.clone(),
                // The service-account JSON path is materialized by the mover from
                // the credentials Secret at runtime (see `crate::credentials`).
                credentials_file: None,
            },
        }
    }
}

/// A reference to the `Snapshot` or `Restore` CR whose `.status` the mover
/// PATCHes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetRef {
    /// The CR's `apiVersion` (e.g. `kopiur.home-operations.com/v1alpha1`).
    pub api_version: String,
    /// The CR kind (`Snapshot` or `Restore`).
    pub kind: String,
    /// The CR name.
    pub name: String,
    /// The CR namespace.
    pub namespace: String,
}

/// A summary of the hook plan the workload pod will execute. The mover does
/// *not* run hooks (ADR §4.8 — hooks run in the workload pod); it carries this
/// summary only for status/observability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookPlanSummary {
    /// Names of pre-hooks (executed by the controller in the workload pod).
    #[serde(default)]
    pub pre: Vec<String>,
    /// Names of post-hooks.
    #[serde(default)]
    pub post: Vec<String>,
}

/// Tunable options for the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoverOptions {
    /// How often (seconds) to PATCH progress to the CR status. ADR §4.13 uses
    /// ~5s; configurable here.
    #[serde(default = "default_progress_interval_secs")]
    pub progress_interval_secs: u64,
    /// Overall timeout (seconds) for the kopia operation; `None` = no timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_timeout_secs: Option<u64>,
}

fn default_progress_interval_secs() -> u64 {
    5
}

impl Default for MoverOptions {
    fn default() -> Self {
        MoverOptions {
            progress_interval_secs: default_progress_interval_secs(),
            operation_timeout_secs: None,
        }
    }
}

/// The full work spec the controller writes for one mover run.
///
/// This is the controller↔mover JSON contract (ADR §4.10): the controller
/// serializes it into a `ConfigMap`, the mover deserializes it from a mounted
/// file. It round-trips losslessly, and externally-tagged enums keep the wire
/// shape `{ "snapshot": {...} }` / `{ "filesystem": {...} }`:
///
/// ```
/// use std::collections::BTreeMap;
/// use kopiur_mover::workspec::*;
///
/// let spec = MoverWorkSpec {
///     version: 1,
///     operation: Operation::Snapshot(SnapshotOp {
///         source_path: "/data".into(),
///         tags: BTreeMap::new(),
///         policy: Default::default(),
///         fail_fast: None,
///         upload_limit_mb: None,
///         description: None,
///     }),
///     identity: ResolvedIdentity {
///         username: "mydb".into(),
///         hostname: "prod".into(),
///         source_path: "/data".into(),
///     },
///     repository: RepositoryConnect::Filesystem { path: "/repo".into() },
///     target_ref: TargetRef {
///         api_version: "kopiur.home-operations.com/v1alpha1".into(),
///         kind: "Snapshot".into(),
///         name: "mydb-20260601".into(),
///         namespace: "prod".into(),
///     },
///     hook_plan: HookPlanSummary::default(),
///     options: MoverOptions::default(),
///     cache: kopiur_kopia::CacheTuning::default(),
///     throttle: Default::default(),
/// };
///
/// // Round-trips through serde_json unchanged.
/// let json = serde_json::to_string(&spec).unwrap();
/// let back: MoverWorkSpec = serde_json::from_str(&json).unwrap();
/// assert_eq!(back, spec);
///
/// // Externally tagged on the wire (camelCase keys).
/// let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
/// assert_eq!(v["operation"]["snapshot"]["sourcePath"], "/data");
/// assert_eq!(v["repository"]["filesystem"]["path"], "/repo");
/// assert_eq!(spec.operation.kind_str(), "Snapshot");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoverWorkSpec {
    /// Schema version for forward compatibility.
    #[serde(default = "default_spec_version")]
    pub version: u32,
    /// The operation to perform.
    pub operation: Operation,
    /// The resolved kopia identity.
    pub identity: ResolvedIdentity,
    /// How to connect to the repository.
    pub repository: RepositoryConnect,
    /// The CR to PATCH status onto.
    pub target_ref: TargetRef,
    /// Hook plan summary (informational).
    #[serde(default)]
    pub hook_plan: HookPlanSummary,
    /// Run options.
    #[serde(default)]
    pub options: MoverOptions,
    /// kopia cache budgets applied when this mover connects to the repository
    /// (`--content-cache-size-mb` / `--metadata-cache-size-mb`). The controller
    /// resolves these from the repository's `cacheDefaults` overlaid by the run's
    /// `mover.cache`. Unset leaves kopia's defaults.
    #[serde(default)]
    pub cache: kopiur_kopia::CacheTuning,
    /// Repository throttle limits applied after connect (`kopia repository throttle
    /// set`) so a run doesn't saturate the link / hammer the object store. Resolved
    /// from the repository's `moverDefaults.throttle` (ADR-0005 §13(e)). All-`None`
    /// ⇒ the mover skips the throttle call (kopia keeps its current limits).
    #[serde(default, skip_serializing_if = "ThrottleSpec::is_empty")]
    pub throttle: ThrottleSpec,
}

/// Serializable mirror of [`kopiur_kopia::ThrottleArgs`] for the work spec. The
/// controller fills it from `moverDefaults.throttle`; the mover converts back and
/// runs `kopia repository throttle set` after connecting. ADR-0005 §13(e).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleSpec {
    /// `--upload-bytes-per-second`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_bytes_per_second: Option<i64>,
    /// `--download-bytes-per-second`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_bytes_per_second: Option<i64>,
    /// `--read-requests-per-second`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_ops_per_second: Option<i64>,
    /// `--write-requests-per-second`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_ops_per_second: Option<i64>,
}

impl ThrottleSpec {
    /// Whether no limits are set (so the mover skips `throttle set`).
    pub fn is_empty(&self) -> bool {
        self.upload_bytes_per_second.is_none()
            && self.download_bytes_per_second.is_none()
            && self.read_ops_per_second.is_none()
            && self.write_ops_per_second.is_none()
    }

    /// Convert to the kopia client's [`ThrottleArgs`](kopiur_kopia::ThrottleArgs).
    pub fn to_kopia(&self) -> kopiur_kopia::ThrottleArgs {
        kopiur_kopia::ThrottleArgs {
            upload_bytes_per_second: self.upload_bytes_per_second,
            download_bytes_per_second: self.download_bytes_per_second,
            read_ops_per_second: self.read_ops_per_second,
            write_ops_per_second: self.write_ops_per_second,
        }
    }
}

fn default_spec_version() -> u32 {
    // v2: RestoreOp carries `source: RestoreSelection` (was a bare `snapshot_id`)
    // so object-store restores resolve "latest" in-Job. A work spec is written and
    // read by a single controller+mover image pair per Job, so v1 and v2 never mix
    // within one run — no cross-version deserializer is needed.
    2
}

#[cfg(test)]
mod tests;
