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
    /// Delete MANY snapshots from the repository over one connect (mass-deletion
    /// protection): one mover Job per repository, rather than one per `Snapshot`
    /// CR, when a bulk deletion is approved. Each member is deleted independently
    /// via the same self-healing logic as [`Operation::SnapshotDelete`] (the
    /// mover's `delete_one`). Nothing emits this operation yet — it is wired by
    /// a later milestone; adding it here is an upgrade-safe no-op.
    SnapshotDeleteBatch(SnapshotDeleteBatchOp),
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
    /// Logical (snapshot-level) replication for a `SnapshotReplication` CR:
    /// connect to the SOURCE repository (the work-spec `repository` field)
    /// read-only with persisted credentials, connect to `destination`
    /// read-write under a second kopia config, `kopia snapshot migrate
    /// --source-config` the selected identities into the destination,
    /// post-verify (kopia exits 0 on per-source failures), reconcile dest-side
    /// `origin: replicated` Snapshot copy CRs, and prune per the carried
    /// pruning mode. PATCHes the `SnapshotReplication` `.status`. Owns its own
    /// (dual) connect lifecycle like [`Operation::Replicate`].
    SnapshotReplicate(SnapshotReplicateOp),
}

impl Operation {
    /// Stable discriminant string for logging/metrics.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Operation::Snapshot(_) => "Snapshot",
            Operation::Restore(_) => "Restore",
            Operation::SnapshotDelete(_) => "SnapshotDelete",
            Operation::SnapshotDeleteBatch(_) => "SnapshotDeleteBatch",
            Operation::BootstrapRepository(_) => "BootstrapRepository",
            Operation::Maintenance(_) => "Maintenance",
            Operation::SnapshotPin(_) => "SnapshotPin",
            Operation::Verify(_) => "Verify",
            Operation::Replicate(_) => "Replicate",
            Operation::BrowseSession(_) => "BrowseSession",
            Operation::SnapshotReplicate(_) => "SnapshotReplicate",
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
    /// `--ignore-identical-snapshots` (`files.ignoreIdenticalSnapshots`).
    ///
    /// Only ever `Some(true)`: an opt-in raises the knob at the PATH scope,
    /// which beats the `false` the mover pins at the identity scope on every
    /// run. Leaving it `None` when the user did not opt in keeps
    /// [`Self::is_empty`] meaningful, so an otherwise-unconfigured policy still
    /// skips the path-scoped `policy set` entirely. See #351.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_identical_snapshots: Option<bool>,
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
            && self.ignore_identical_snapshots.is_none()
            && self.ignore_file_errors.is_none()
            && self.ignore_dir_errors.is_none()
            && self.ignore_unknown_types.is_none()
            && self.max_parallel_snapshots.is_none()
            && self.max_parallel_file_reads.is_none()
            && self.extra_args.is_empty()
    }

    /// Convert to the kopia client's [`PolicyArgs`](kopiur_kopia::PolicyArgs).
    /// `splitter` is never set here — the object splitter is a repository property
    /// (ADR-0004 §4b removed the per-policy splitter). The six `keep_*`
    /// (create-time retention) fields are likewise never set here: `PolicyArgsSpec`
    /// is the wire work-spec, and Kopiur's `KOPIA_KEEP_MAX` pin is deliberately NOT
    /// user-configurable — the mover applies it directly at the identity scope
    /// (`crates/mover/src/main.rs`), so it never rides this struct.
    pub fn to_kopia(&self) -> kopiur_kopia::PolicyArgs {
        kopiur_kopia::PolicyArgs {
            compression: self.compression.clone(),
            splitter: None,
            ignore: self.ignore.clone(),
            never_compress: self.never_compress.clone(),
            ignore_cache_dirs: self.ignore_cache_dirs,
            ignore_identical_snapshots: self.ignore_identical_snapshots,
            ignore_file_errors: self.ignore_file_errors,
            ignore_dir_errors: self.ignore_dir_errors,
            ignore_unknown_types: self.ignore_unknown_types,
            max_parallel_snapshots: self.max_parallel_snapshots,
            max_parallel_file_reads: self.max_parallel_file_reads,
            extra_args: self.extra_args.clone(),
            ..Default::default()
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
        let (ignore, ignore_cache_dirs, ignore_identical_snapshots) = match &spec.files {
            // `ignore_cache_dirs` is a bool on the CRD; only emit the flag when true
            // (Some(true)) — an unset/false leaves kopia's default rather than forcing
            // `--no-ignore-cache-dirs`, matching the "absent = kopia default" contract.
            //
            // `ignore_identical_snapshots` follows the same only-when-true rule, but
            // for a different reason: `false` is not "leave kopia's default", it is a
            // guarantee Kopiur needs, so the mover pins it at the identity scope on
            // EVERY run instead of relying on this path-scoped spec (#351).
            Some(f) => (
                f.ignore_rules.clone(),
                f.ignore_cache_dirs.then_some(true),
                f.ignore_identical_snapshots.then_some(true),
            ),
            // The apiserver only server-side-defaults NESTED fields when the parent
            // object is present, so a `SnapshotPolicy` that omits `files:` entirely
            // (the common case) never gets `Files.ignore_rules`'s schema default
            // applied. Fall back to the SAME `default_ignore_rules()` fn the API
            // layer wires as the serde/schemars default, so there is one source of
            // truth for the OS-artifact exclude set regardless of which of the two
            // "absent" shapes (`files:` missing vs. `files: {}`) the spec took.
            None => (
                kopiur_api::snapshot_policy::default_ignore_rules(),
                None,
                None,
            ),
        };
        let eh = spec.error_handling.as_ref();
        let up = spec.upload.as_ref();
        PolicyArgsSpec {
            compression,
            ignore,
            never_compress,
            ignore_cache_dirs,
            ignore_identical_snapshots,
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
    /// The snapshotted source path — necessary but not sufficient to match
    /// once two sources share a path; see `username`/`hostname`.
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

/// Payload for a per-repository batch snapshot-delete run (mass-deletion
/// protection): one mover Job deletes MANY manifest ids over one connect,
/// instead of one Job per `Snapshot` CR. Nothing emits this yet (a later
/// milestone wires the controller dispatcher); this type landing is an
/// upgrade-safe no-op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDeleteBatchOp {
    /// Members, each with its own stale-id self-heal anchor.
    pub items: Vec<SnapshotDeleteItem>,
}

/// One member of a [`SnapshotDeleteBatchOp`]: mirrors [`SnapshotDeleteOp`]'s
/// `snapshot_id`/`anchor` shape exactly (same self-heal semantics, applied
/// per-item instead of to a single delete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDeleteItem {
    /// The snapshot manifest id to delete.
    pub snapshot_id: String,
    /// Stable identity anchors for the snapshot, used to self-heal a stale
    /// `snapshot_id` — see [`SnapshotDeleteOp::anchor`]. Empty ⇒ delete by id
    /// only.
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
    /// always reported; the entries are only returned when this is set. BOTH
    /// repository kinds set it — a `ClusterRepository`'s discovered snapshots
    /// are placed per identity hostname by the controller's catalog pass, which
    /// needs the same entries a namespaced `Repository` does.
    #[serde(default)]
    pub scan_catalog: bool,
    /// Create-time-fixed repository format knobs honored only when this bootstrap
    /// actually *creates* the repository (`auto_create` + connect-miss). The
    /// controller resolves these from `Repository.spec.create.{encryption,splitter,
    /// hash,ecc}` (ADR-0005 §13(a)); they're immutable post-create (§7).
    #[serde(default, skip_serializing_if = "CreateOptionsSpec::is_empty")]
    pub create_options: CreateOptionsSpec,
    /// MUTABLE repository parameters from `Repository.spec.parameters.epoch` (#258),
    /// re-applied on drift on every bootstrap — including a connect-to-existing, which is
    /// the whole point. The sibling of `create_options` and its opposite: those are
    /// create-time-fixed, these are the ones you can still change.
    ///
    /// Empty for a `mode: ReadOnly` repository — the controller does not send them, because
    /// `set-parameters` hard-errors on a read-only connection.
    #[serde(default, skip_serializing_if = "EpochParametersSpec::is_empty")]
    pub epoch_parameters: EpochParametersSpec,
    /// Object-lock blob retention (#332), applied through the same `set-parameters` call as
    /// `epoch_parameters`.
    ///
    /// An `Option`, NOT an `is_empty()` sentinel like its sibling above — see
    /// [`BlobRetentionSpec`] for why. `None` means "leave the repository's retention alone";
    /// disabling is the explicit `Some(mode: "none")`. Also `None` for a `mode: ReadOnly`
    /// repository, for the same reason `epoch_parameters` is empty there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_retention: Option<BlobRetentionSpec>,
    /// This cluster's `identityDefaults.cluster` (multi-cluster shared repo),
    /// carried ONLY when the controller determined cluster identity is on AND the
    /// effective `catalog.foreignSnapshots` policy is `Ignore` — never under
    /// `Fallback` (those entries must still come back so the controller can
    /// materialize them into `catalog.fallbackNamespace`). When set, the mover
    /// drops listing entries whose hostname classifies
    /// [`kopiur_api::HostClass::ForeignCluster`] against it BEFORE
    /// [`crate::bootstrap::MAX_RETURNED_SNAPSHOTS`] is applied (see
    /// [`crate::bootstrap::apply_foreign_prefilter`]); absent on old work specs
    /// (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_foreign_prefilter_cluster: Option<String>,
    /// How aggressively the connect-to-existing self-heal (see
    /// [`maintenance_restamp_target`]) may re-stamp a stale maintenance owner.
    /// Defaults to [`RestampPolicy::AnyStale`] (the pre-M6 behavior) so old
    /// work-spec JSON decodes unchanged.
    #[serde(default)]
    pub restamp_policy: RestampPolicy,
    /// Pre-derived kopia OWNER strings (`kopia_owner_for_lease(alias_lease)`,
    /// not raw lease strings) for this repository's recognized legacy leases
    /// (M6 migration path — see
    /// [`kopiur_api::maintenance::Ownership::owner_aliases`]). Consulted by
    /// [`maintenance_restamp_target`] under [`RestampPolicy::OwnFormatsOnly`].
    /// Absent on old work specs (serde default).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub maintenance_owner_aliases: Vec<String>,
    /// Connect with `--readonly` (`kopia repository connect --readonly`)
    /// instead of the normal read-write connect. Set ONLY for the bootstrap of
    /// a `mode: ReadOnly` repository (M6): bootstrap is a connect/scan probe,
    /// and read-write-connecting a consumer repo is exactly what let it clobber
    /// the primary's maintenance owner (the bug this field fixes). Never set
    /// for restore/delete movers, which legitimately write pins. Absent on old
    /// work specs (serde default ⇒ `false`, the pre-M6 behavior).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Initialize this repository from an existing replica when — and only when —
    /// the first connect reports the backend UNINITIALIZED (`Repository.spec.seed`,
    /// issue #380). The controller arms this only while `status.uniqueId` is unset,
    /// so a standing `spec.seed` on an already-bootstrapped repository is a no-op.
    ///
    /// Presence is also the **mover-skew acknowledgment token**: when this is set,
    /// the controller accepts a successful [`crate::bootstrap::BootstrapResult`]
    /// only if it carries a [`crate::bootstrap::SeedOutcome`]. An older mover image
    /// would silently drop this unknown field, fall into the create fallback and
    /// report a `Ready` but EMPTY repository — reintroducing the data-loss shape
    /// #380 exists to prevent. Absent on old work specs (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<SeedOpSpec>,
}

impl BootstrapRepositoryOp {
    /// The kopia client's [`CreateOptions`](kopiur_kopia::CreateOptions) for the
    /// create-time format knobs carried here.
    pub fn create_options(&self) -> kopiur_kopia::CreateOptions {
        self.create_options.to_kopia()
    }
}

/// The seeding payload carried on [`BootstrapRepositoryOp::seed`] — a **wire
/// mirror** of `kopiur_api::seed::SeedSpec` (the [`ReplicateOp`] rule: plain
/// serde structs, never the CRD types, so the controller↔mover JSON contract
/// cannot drift with a CRD refactor).
///
/// Everything past `from` and `source_description` is serde-defaulted so a spec
/// stamped by a controller that omits the tuning blocks still decodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedOpSpec {
    /// Where the seed reads from: a bare mirror backend (blob mode,
    /// `kopia repository sync-to`) or another repository CR (migrate mode,
    /// `kopia snapshot migrate`).
    pub from: SeedConnectSource,
    /// The source rendering the controller pinned for `status.seed.source`
    /// (`kopiur_api::seed::SeedSource::describe`), echoed back verbatim on
    /// [`crate::bootstrap::SeedOutcome::source`].
    ///
    /// Carried rather than re-derived in the mover on purpose: the controller
    /// RESOLVES a migrate reference's namespace before building this op, so a
    /// mover-side rendering would print a namespace the CRD-side rendering
    /// does not — two spellings of one repository in status and logs. One
    /// renderer, one string. (Same shape as [`ReplicationSourceRef`], carried
    /// so the mover can stamp lineage it did not itself resolve.)
    pub source_description: String,
    /// Blob-mode tuning for `kopia repository sync-to`. Ignored in migrate mode
    /// — admission refuses the mismatched pairing, so it is never both present
    /// and inert on a live CR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SeedSyncSpec>,
    /// Migrate-mode tuning for `kopia snapshot migrate`. Ignored in blob mode,
    /// for the same reason `sync` is ignored in migrate mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrate: Option<SeedMigrateSpec>,
    /// Accept a source holding zero snapshots. `false` (the default) fails the
    /// bootstrap with [`crate::bootstrap::SEED_SOURCE_EMPTY_CLASS`] instead of
    /// seeding nothing and reporting `Ready` — the failure mode #380 is about.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_empty_source: bool,
    /// RESUME a seed a previous attempt started but did not finish: re-run the
    /// copy even though this repository's backend is already initialized.
    ///
    /// Without this, an interrupted seed is unrecoverable-by-retry and silently
    /// wrong. A seed that dies after initializing the backend — a `sync-to`
    /// killed by the Job deadline, an OOM, a migrate whose `repository create`
    /// landed before the copy failed — leaves a repository that the NEXT
    /// bootstrap connects to successfully. The seed then reports itself the
    /// documented `AlreadyInitialized` no-op and the repository goes `Ready`
    /// with partial history (or none). Resuming is what makes the retry
    /// actually retry.
    ///
    /// The controller sets it (issue #380 stage C3) from a durable
    /// seed-attempt marker it stamps BEFORE creating a seeding bootstrap Job:
    /// marker present and `status.seed.seededAt` absent ⇒ a previous attempt
    /// started and never finished ⇒ `resume: true`. A repository with NO marker
    /// is an ordinary adoption — its backend was initialized by someone else —
    /// and keeps the no-clobber `AlreadyInitialized` path.
    ///
    /// Both copies are safe to re-run: `sync-to` is incremental (it copies only
    /// blobs the destination lacks) and `snapshot migrate` is idempotent by
    /// `(SourceInfo, StartTime)`. Absent on old work specs (serde default).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume: bool,
}

impl SeedOpSpec {
    /// Which copy mechanism this op selects. Exhaustive.
    pub fn mode(&self) -> SeedModeSpec {
        self.from.mode()
    }

    /// The [`SyncToOptions`](kopiur_kopia::SyncToOptions) for the blob-mode
    /// `sync-to`, from the carried tuning (absent ⇒ kopia's own defaults).
    ///
    /// `local_initialized` says whether THIS repository's backend already holds
    /// a kopia format blob — false on a first seed into an empty backend, true
    /// when resuming into one a previous attempt started.
    ///
    /// Two options are FIXED rather than exposed:
    /// * `must_exist: Some(local_initialized)` — a FIRST seed must be allowed to
    ///   initialize the destination (that IS the operation), while a RESUME must
    ///   NOT: the format blob is there, and if it has vanished between the
    ///   connect and the copy something is wrong enough that quietly
    ///   re-initializing would be the wrong answer. Rendered explicitly either
    ///   way (`--no-must-exist` / `--must-exist`) rather than relying on kopia's
    ///   default, so a future default flip cannot turn every seed into a
    ///   failure — pinned against the real binary by the `sync_to_seeds_*`
    ///   integration test.
    /// * `delete_extra: false` — a seed never prunes. On a first seed there is
    ///   nothing at the destination to prune; on a resume the destination holds
    ///   the previous attempt's partial copy, and `--delete` is the one flag that
    ///   could destroy it.
    ///
    /// `times`/`update` stay `None` (kopia's defaults): kopia already skips blobs
    /// the destination has, which is exactly what makes a resume incremental.
    pub fn sync_options(&self, local_initialized: bool) -> kopiur_kopia::SyncToOptions {
        let sync = self.sync.unwrap_or_default();
        kopiur_kopia::SyncToOptions {
            parallel: sync.parallel,
            delete_extra: false,
            must_exist: Some(local_initialized),
            times: None,
            update: None,
            max_download_speed_bytes_per_second: sync.max_download_speed_bytes_per_second,
            max_upload_speed_bytes_per_second: sync.max_upload_speed_bytes_per_second,
        }
    }
}

/// Where a seed reads from — externally tagged
/// (`{ "backend": { "s3": {...} } }` / `{ "repository": {...} }`), mirroring
/// the CRD's `SeedSource` one-of. Never `#[serde(tag)]`.
///
/// Boxed variants: a [`RepositoryConnect`] is far larger than the reference
/// beside it, and an unboxed variant would inflate every
/// [`BootstrapRepositoryOp`] — including the overwhelming majority that carry
/// no seed at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SeedConnectSource {
    /// Blob mode: a bare storage backend holding a byte-for-byte mirror of a
    /// kopia repository. Copied with `kopia repository sync-to`, so the seeded
    /// repository inherits the mirror's format and password — the mover uses
    /// THIS repository's own `KOPIA_PASSWORD` for both sides.
    Backend(Box<RepositoryConnect>),
    /// Migrate mode: another repository CR, opened read-only. Copied with
    /// `kopia snapshot migrate`, which preserves each snapshot's
    /// `username@hostname:path` identity and times.
    Repository(Box<SeedRepositoryConnect>),
}

impl SeedConnectSource {
    /// Which copy mechanism this source selects. Exhaustive — a new variant
    /// cannot compile until its mode is decided.
    pub fn mode(&self) -> SeedModeSpec {
        match self {
            SeedConnectSource::Backend(_) => SeedModeSpec::Blob,
            SeedConnectSource::Repository(_) => SeedModeSpec::Migrate,
        }
    }

    /// The backend to open as the seed source, for either mode. Exhaustive.
    pub fn connect(&self) -> &RepositoryConnect {
        match self {
            SeedConnectSource::Backend(b) => b,
            SeedConnectSource::Repository(r) => &r.connect,
        }
    }
}

/// The migrate-mode seed source: the resolved source repository CR plus the
/// backend to open it with. Mirrors [`ReplicationSourceRef`]'s kind/name/
/// namespace triple (carried for logs and error messages) and adds the connect
/// spec, since — unlike a replication source — the mover has never connected to
/// this repository before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedRepositoryConnect {
    /// `"Repository"` or `"ClusterRepository"` (the CRD kind's serde value).
    pub kind: String,
    /// Name of the source repository CR.
    pub name: String,
    /// Namespace of a namespaced source `Repository`; `None` for a
    /// `ClusterRepository`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The source repository's backend. Its storage credentials arrive
    /// `KOPIUR_SEED_`-prefixed and its kopia password on
    /// `KOPIUR_SEED_KOPIA_PASSWORD`, so neither can collide with THIS
    /// repository's identically-named ambient ones.
    pub connect: RepositoryConnect,
}

/// Which copy mechanism a seed ran. The wire strings are the same two
/// `kopiur_api::seed::SeedMode` uses (`blob`/`migrate`) so status, metrics and
/// logs share one vocabulary — pinned by
/// `tests::seed_mode_wire_labels_match_the_api_crate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SeedModeSpec {
    /// `kopia repository sync-to` from a bare mirror backend.
    Blob,
    /// `kopia snapshot migrate` from another repository CR.
    Migrate,
}

impl SeedModeSpec {
    /// Stable lowercase label for logs and metrics. Exhaustive.
    pub fn as_str(self) -> &'static str {
        match self {
            SeedModeSpec::Blob => "blob",
            SeedModeSpec::Migrate => "migrate",
        }
    }
}

/// Blob-mode tuning, a wire mirror of `kopiur_api::seed::SeedSyncOptions`.
/// Deliberately a strict subset of [`ReplicateOp`]'s knobs — see
/// [`SeedOpSpec::sync_options`] for the two that are fixed and why.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedSyncSpec {
    /// `--parallel`: concurrent blob-copy workers (kopia default `1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--max-download-speed`, bytes/sec (kopia default: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_download_speed_bytes_per_second: Option<i64>,
    /// `--max-upload-speed`, bytes/sec (kopia default: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_upload_speed_bytes_per_second: Option<i64>,
}

/// Migrate-mode tuning, a wire mirror of `kopiur_api::seed::SeedMigrateOptions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedMigrateSpec {
    /// `--parallel <n>`: sources migrated concurrently (kopia default `1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--latest-only`: copy only the newest snapshot per source identity.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub latest_only: bool,
    /// How kopia-side policies are treated. Defaults to
    /// [`PolicyCopyModeSpec::None`] — an EXPLICIT `--no-policies`, because
    /// kopia's own default copies them and an imported retention policy would
    /// delete manifests behind the operator's back.
    #[serde(default, skip_serializing_if = "PolicyCopyModeSpec::is_default")]
    pub policies: PolicyCopyModeSpec,
}

/// How aggressively the bootstrap mover's connect-to-existing self-heal (see
/// [`maintenance_restamp_target`]) may re-stamp a stale kopia maintenance
/// owner. Closed enum — a new policy cannot compile until every caller
/// accounts for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestampPolicy {
    /// Re-stamp whenever the recorded owner differs from the desired one — the
    /// pre-M6 behavior. Safe for a repository with no cluster dimension: at
    /// most one cluster's operator ever bootstraps it, so any stale owner is
    /// either the ephemeral pod identity kopia auto-assigned on create, or an
    /// older-format stamp from THIS SAME operator — never another cluster's.
    #[default]
    AnyStale,
    /// Re-stamp ONLY when the recorded owner is empty, already the desired
    /// owner, or matches one of [`BootstrapRepositoryOp::maintenance_owner_aliases`]
    /// — i.e. never clobber a foreign or unrecognized owner. Required once a
    /// repository has a cluster dimension (`identityDefaults.cluster` set): with
    /// `AnyStale`, every cluster restamping on every connect would each see the
    /// OTHER'S owner as "stale" and re-claim it — an infinite ping-pong on a
    /// shared repo. The tradeoff: an ancient/ephemeral owner this operator has
    /// never seen before is left alone rather than auto-clobbered, needing a
    /// one-time `ownership.takeoverPolicy: Force` — auto-clobber is exactly the
    /// behavior that is unsafe once more than one cluster can reach this repo.
    OwnFormatsOnly,
}

/// Decide whether the bootstrap mover should re-stamp the stable maintenance
/// owner on a *connect-to-existing* repository. Returns `Some(owner)` to stamp,
/// or `None` to leave the recorded owner alone.
///
/// `created` is `true` only for a repository this bootstrap run just CREATED —
/// its owner was already stamped unconditionally at create time, so this
/// self-heal never fires there (see the mover's create-path stamp).
///
/// `seeded` is `true` only for a repository a **blob-mode seed** (`spec.seed`
/// with a mirror backend, issue #380) just initialized by copying another
/// cluster's storage. That copy carries the SOURCE cluster's
/// `kopia.maintenance` blob, so the repository arrives owned by an operator
/// that — this being disaster recovery — no longer exists. Under
/// [`RestampPolicy::OwnFormatsOnly`] (forced whenever `identityDefaults.cluster`
/// is set) that owner is unrecognized, the self-heal would decline to touch it,
/// and maintenance would yield indefinitely on a repository nobody else can
/// claim. So a just-seeded repository restamps unconditionally: it is
/// semantically as fresh as a created one, even though `created` stays `false`
/// (only the create fallback creates). Migrate-mode seeds create the local
/// repository normally and take the `created` path instead.
///
/// Exhaustive over [`RestampPolicy`]:
/// * [`RestampPolicy::AnyStale`] — restamp whenever `current != desired`
///   (unconditional on a connect-to-existing; the pre-M6 rule).
/// * [`RestampPolicy::OwnFormatsOnly`] — the same staleness check, AND ONLY
///   when `current` is empty, or recognized as one of `aliases` (never a
///   foreign or unrecognized owner — including an ancient ephemeral one, which
///   needs a one-time `takeoverPolicy: Force` to move under this policy; see
///   the variant's doc for why auto-clobber is unsafe here).
///
/// Pulled out so the gate is unit-testable without spawning kopia, and shared
/// (via [`kopiur_mover::workspec`]) with the controller's in-process
/// (bare-path filesystem) restamp, which needs the identical decision.
pub fn maintenance_restamp_target<'a>(
    created: bool,
    seeded: bool,
    desired: Option<&'a str>,
    policy: RestampPolicy,
    aliases: &[String],
    current: &str,
) -> Option<&'a str> {
    let owner = desired?;
    if created || current == owner {
        return None;
    }
    if seeded {
        return Some(owner);
    }
    match policy {
        RestampPolicy::AnyStale => Some(owner),
        RestampPolicy::OwnFormatsOnly => {
            if current.is_empty() || aliases.iter().any(|a| a == current) {
                Some(owner)
            } else {
                None
            }
        }
    }
}

/// One MiB in bytes. kopia's `--epoch-advance-on-size-mb` flag multiplies by this — `7`
/// yields `7340032`, NOT 7_000_000 — even though its own log renders the result as "7.3 MB".
/// Getting this wrong makes the drift comparison below never converge, which would re-run
/// `set-parameters` on every bootstrap and invalidate every other client's format cache each
/// time.
const MIB: i64 = 1_048_576;

/// Serializable mirror of the epoch knobs from `Repository.spec.parameters.epoch`, carried
/// on [`BootstrapRepositoryOp`]. `serde(default)` throughout: an older controller's work
/// spec has no `parameters` key and must still decode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochParametersSpec {
    /// Go-style duration, already rendered with a unit for kopia's CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_duration: Option<String>,
    /// Go-style duration, already rendered with a unit for kopia's CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_frequency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[allow(missing_docs)]
    pub advance_on_count: Option<i64>,
    /// MiB (kopia's flag is `-mb` but means MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advance_on_size_mb: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[allow(missing_docs)]
    pub checkpoint_frequency: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[allow(missing_docs)]
    pub delete_parallelism: Option<i64>,
}

impl EpochParametersSpec {
    /// Build from the api crate's `EpochParameters`, rendering each duration through
    /// [`kopiur_api::render_go_duration`] so what reaches kopia's argv is never the user's
    /// raw text. kopia rejects a bare number (`3600`) that `parse_go_duration` accepts, so
    /// passing the string through would admit at the webhook and fail in the mover; an
    /// unparseable value (already rejected at admission) drops to `None` rather than
    /// forwarding garbage.
    pub fn from_api(e: &kopiur_api::repository::EpochParameters) -> Self {
        let render = |s: &Option<String>| -> Option<String> {
            s.as_deref()
                .and_then(kopiur_api::parse_go_duration)
                .map(kopiur_api::render_go_duration)
        };
        Self {
            min_duration: render(&e.min_duration),
            refresh_frequency: render(&e.refresh_frequency),
            advance_on_count: e.advance_on_count,
            advance_on_size_mb: e.advance_on_size_mb,
            checkpoint_frequency: e.checkpoint_frequency,
            delete_parallelism: e.delete_parallelism,
        }
    }

    /// Whether nothing is declared (so the whole set-parameters step is skipped and a
    /// repository that never mentions `spec.parameters` is completely unaffected).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The `set-parameters` flags needed to bring `observed` in line with `desired`, or `None`
/// when they already agree.
///
/// Pure and unit-tested, deliberately mirroring [`maintenance_restamp_target`]: read live
/// state, compare, mutate only on drift. That matters more here than for the maintenance
/// owner — `kopia repository set-parameters` invalidates every other client's cached format
/// blob, so an unconditional apply would churn the whole fleet on every bootstrap.
///
/// Comparison is in kopia's OWN units (nanoseconds, bytes), never on the rendered strings:
/// `"6h"`, `"360m"` and `"21600s"` are the same parameter, and a string compare would report
/// drift forever.
pub fn epoch_drift(
    desired: &EpochParametersSpec,
    observed: Option<&kopiur_kopia::model::EpochParameters>,
) -> Option<kopiur_kopia::client::SetParametersArgs> {
    if desired.is_empty() {
        return None;
    }
    // No observation (older kopia, or a status we could not read) → apply what is declared
    // rather than silently skip. `set-parameters` is idempotent.
    let Some(o) = observed else {
        return Some(kopiur_kopia::client::SetParametersArgs {
            epoch_min_duration: desired.min_duration.clone(),
            epoch_refresh_frequency: desired.refresh_frequency.clone(),
            epoch_advance_on_count: desired.advance_on_count,
            epoch_advance_on_size_mb: desired.advance_on_size_mb,
            epoch_checkpoint_frequency: desired.checkpoint_frequency,
            epoch_delete_parallelism: desired.delete_parallelism,
            // Epoch drift never touches retention; `parameters_drift` merges the two.
            retention_mode: None,
            retention_period: None,
        });
    };
    // Compare a desired duration against an observed nanosecond count.
    //
    // `try_from`, never `as`: `as` on a u128 that exceeds i64::MAX WRAPS (silently, and to
    // a negative number), which would report drift against every possible observation and
    // re-run set-parameters on every bootstrap. Admission rejects durations beyond kopia's
    // i64-nanosecond range, so this is the belt to that braces — an unrepresentable value
    // is treated as "no comparable target" rather than as garbage.
    let dur_drift = |want: &Option<String>, have_ns: i64| -> Option<String> {
        let want = want.as_deref()?;
        let want_ns = i64::try_from(kopiur_api::parse_go_duration(want)?.as_nanos()).ok()?;
        (want_ns != have_ns).then(|| want.to_string())
    };
    let num_drift = |want: Option<i64>, have: i64| -> Option<i64> { want.filter(|w| *w != have) };
    let args = kopiur_kopia::client::SetParametersArgs {
        epoch_min_duration: dur_drift(&desired.min_duration, o.min_epoch_duration_ns),
        epoch_refresh_frequency: dur_drift(
            &desired.refresh_frequency,
            o.epoch_refresh_frequency_ns,
        ),
        epoch_advance_on_count: num_drift(desired.advance_on_count, o.advance_on_count),
        // MiB on the flag, bytes in the report.
        epoch_advance_on_size_mb: num_drift(
            desired.advance_on_size_mb,
            o.advance_on_total_size_bytes / MIB,
        ),
        epoch_checkpoint_frequency: num_drift(desired.checkpoint_frequency, o.checkpoint_frequency),
        epoch_delete_parallelism: num_drift(desired.delete_parallelism, o.delete_parallelism),
        // Epoch drift never touches retention; `parameters_drift` merges the two.
        retention_mode: None,
        retention_period: None,
    };
    (!args.is_empty()).then_some(args)
}

/// Mirror kopia's reported epoch parameters into the api crate's status type, rendering
/// nanosecond durations back to Go-style strings so `status.parameters.epoch` is directly
/// comparable to `spec.parameters.epoch`.
pub fn observed_epoch(
    o: &kopiur_kopia::model::EpochParameters,
) -> kopiur_api::repository::ObservedEpochParameters {
    let dur =
        |ns: i64| kopiur_api::render_go_duration(std::time::Duration::from_nanos(ns.max(0) as u64));
    kopiur_api::repository::ObservedEpochParameters {
        enabled: o.enabled,
        min_duration: dur(o.min_epoch_duration_ns),
        refresh_frequency: dur(o.epoch_refresh_frequency_ns),
        cleanup_safety_margin: dur(o.cleanup_safety_margin_ns),
        advance_on_count: o.advance_on_count,
        advance_on_size_mb: o.advance_on_total_size_bytes / MIB,
        checkpoint_frequency: o.checkpoint_frequency,
        delete_parallelism: o.delete_parallelism,
    }
}

/// Object-lock blob retention for the work spec (#332).
///
/// **Deliberately has no `Default` impl**, and rides `BootstrapRepositoryOp` as an
/// `Option<_>` rather than as an `is_empty()` sentinel the way [`EpochParametersSpec`] does.
/// That difference is load-bearing: `mode` is mandatory, so *some* value would have to be the
/// default, and whichever one it was would make "the user never mentioned blobRetention"
/// indistinguishable from "the user asked to disable it". A repository that never declares
/// retention would then issue `--retention-mode none` and silently strip a lock configured by
/// hand. `None` = unmanaged, `Some(mode: "none")` = disable, and the absent `Default` is what
/// stops the sentinel pattern being reintroduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobRetentionSpec {
    /// kopia's argv value: `"none"`, `"GOVERNANCE"`, or `"COMPLIANCE"`.
    pub mode: String,
    /// Go-style duration, already rendered with a unit for kopia's CLI. `None` when
    /// disabling — kopia ignores a period on the `none` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
}

impl BlobRetentionSpec {
    /// Build from the api crate's `BlobRetention`, rendering the period through
    /// [`kopiur_api::render_go_duration`] for the same reason [`EpochParametersSpec::from_api`]
    /// does — what reaches kopia's argv is never the user's raw text.
    ///
    /// Returns `None` for `disabled: false`. That is not "enable": there is nothing to enable
    /// without a mode and a period, so it reads as "leave the repository alone", exactly like
    /// omitting the block. Erring toward doing nothing is the only safe reading for a
    /// ransomware control.
    pub fn from_api(r: &kopiur_api::repository::BlobRetention) -> Option<Self> {
        use kopiur_api::repository::BlobRetention as B;
        let render = |w: &kopiur_api::repository::RetentionWindow| {
            kopiur_api::parse_go_duration(&w.period).map(kopiur_api::render_go_duration)
        };
        match r {
            B::Governance(w) => Some(Self {
                mode: "GOVERNANCE".into(),
                period: render(w),
            }),
            B::Compliance(w) => Some(Self {
                mode: "COMPLIANCE".into(),
                period: render(w),
            }),
            B::Disabled(true) => Some(Self {
                mode: "none".into(),
                period: None,
            }),
            B::Disabled(false) => None,
        }
    }
}

/// The `set-parameters` flags needed to bring observed blob retention in line with `desired`,
/// or `None` when they already agree.
///
/// Same doctrine as [`epoch_drift`]: compare in kopia's OWN units (nanoseconds), never on the
/// rendered strings, and mutate only on drift because `set-parameters` invalidates every other
/// client's cached format blob.
pub fn blob_retention_drift(
    desired: Option<&BlobRetentionSpec>,
    observed: Option<&kopiur_kopia::model::BlobRetention>,
) -> Option<kopiur_kopia::client::SetParametersArgs> {
    // Unmanaged: the repository is never touched. This is the inert case that makes adding
    // the feature a no-op for every existing repository.
    let desired = desired?;

    if desired.mode == "none" {
        // Only disable what is actually on. When nothing was observed we cannot tell whether
        // there is anything to disable, and a blind `--retention-mode=none` against a backend
        // that cannot object-lock hard-fails — so do nothing. This asymmetry with the enable
        // path below (which DOES apply on no observation) is deliberate.
        let currently_on = observed.is_some_and(|o| o.is_enabled());
        return currently_on.then(|| kopiur_kopia::client::SetParametersArgs {
            retention_mode: Some("none".into()),
            ..Default::default()
        });
    }

    // `try_from`, never `as` — see `epoch_drift`: a wrapping cast reports drift against every
    // observation and re-applies on every bootstrap forever.
    let want_period = desired.period.as_deref()?;
    let want_ns = i64::try_from(kopiur_api::parse_go_duration(want_period)?.as_nanos()).ok()?;
    let converged = observed.is_some_and(|o| o.mode == desired.mode && o.period_ns == want_ns);
    // Emit BOTH flags whenever either drifts: kopia validates the merged blobcfg, and sending
    // the pair unconditionally is one fewer invariant to keep true.
    (!converged).then(|| kopiur_kopia::client::SetParametersArgs {
        retention_mode: Some(desired.mode.clone()),
        retention_period: Some(want_period.to_string()),
        ..Default::default()
    })
}

/// Every `set-parameters` flag needed this bootstrap, in ONE invocation.
///
/// Epoch tuning and blob retention are independent settings that share a single kopia
/// command, and that command rewrites the repository-global format blob — forcing every other
/// kopia client to reconnect. Applying them as two commands would pay that cost twice, so the
/// two drift results are merged here rather than dispatched separately.
pub fn parameters_drift(
    epoch_desired: &EpochParametersSpec,
    epoch_observed: Option<&kopiur_kopia::model::EpochParameters>,
    retention_desired: Option<&BlobRetentionSpec>,
    retention_observed: Option<&kopiur_kopia::model::BlobRetention>,
) -> Option<kopiur_kopia::client::SetParametersArgs> {
    let epoch = epoch_drift(epoch_desired, epoch_observed);
    let retention = blob_retention_drift(retention_desired, retention_observed);
    match (epoch, retention) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        // Disjoint field sets, so the merge is total: retention's two fields over epoch's six.
        (Some(a), Some(b)) => Some(kopiur_kopia::client::SetParametersArgs {
            retention_mode: b.retention_mode,
            retention_period: b.retention_period,
            ..a
        }),
    }
}

/// Mirror kopia's reported blob retention into the api crate's status type, rendering the
/// nanosecond period back to a Go-style string so `status.parameters.blobRetention` is
/// directly comparable to `spec.parameters.blobRetention`.
pub fn observed_blob_retention(
    o: &kopiur_kopia::model::BlobRetention,
) -> kopiur_api::repository::ObservedBlobRetention {
    kopiur_api::repository::ObservedBlobRetention {
        enabled: o.is_enabled(),
        mode: o.mode.clone(),
        period: kopiur_api::render_go_duration(std::time::Duration::from_nanos(
            o.period_ns.max(0) as u64
        )),
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
    /// Previous lease strings still recognized as SELF
    /// (`spec.ownership.ownerAliases`, M6 migration path) — see
    /// [`kopiur_api::maintenance::lease_held_by_other`]. Absent on old work
    /// specs (serde default).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_aliases: Vec<String>,
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

/// The value the mover pins every one of kopia's six retention `--keep-*`
/// fields to, at the identity scope, before the first `snapshot create` on
/// that identity (M0b, confirmed data-loss bug).
///
/// kopia's `snapshot create` unconditionally applies the *source's* retention
/// policy after every create — even under `--override-source`
/// (`policy.ApplyRetentionPolicy(ctx, rep, sourceInfo, true)`,
/// `cli/command_snapshot_create.go`) — and with no policy set, kopia's OWN
/// defaults apply (keep-latest 10, hourly 48, daily 7, weekly 4, monthly 24,
/// annual 3; `snapshot/policy/retention_policy.go`). `snapshot/policy/expire.go`
/// then deletes any manifest with no retention reason and no pin. So any
/// `SnapshotPolicy.spec.retention` window wider than kopia's defaults (e.g.
/// `keepDaily: 30`) has kopia silently deleting manifests that Succeeded
/// `Snapshot` CRs still reference — surfacing only at restore.
///
/// Kopiur's design is that CR-driven GFS (`SnapshotPolicy.spec.retention`,
/// enforced by pruning `Snapshot` CRs) is the SOLE deleter. Pinning every
/// `--keep-*` field to this value at the identity scope (the path scope
/// inherits it when unset there) makes kopia's own create-time retention
/// effectively a no-op, restoring that invariant. This is NOT
/// user-configurable — kopia-side retention stays forbidden
/// (`crates/api/src/error.rs`'s `InlineRetentionForbidden`) — so it is
/// hardcoded here rather than exposed on `PolicyArgsSpec`/any CRD.
///
/// The value itself is kopia's largest safely round-tripping retention count:
/// its policy fields are a Go `int` (`policy.OptionalInt`), and `i32::MAX` is
/// the conventional "no effective limit" sentinel for a flag of this shape —
/// comfortably larger than any real backup history, while avoiding the
/// overflow risk of reaching for an unbounded value on a numeric CLI flag.
pub const KOPIA_KEEP_MAX: i64 = i32::MAX as i64; // 2_147_483_647

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
    ///
    /// `sources` is left empty here — the tier knobs carry no identity. The
    /// mover scopes the verify to the run's resolved identity
    /// (`spec.identity.source_spec()`) at the call site (issue #250).
    pub fn to_kopia(&self) -> kopiur_kopia::VerifyOptions {
        kopiur_kopia::VerifyOptions {
            sources: Vec::new(),
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
    /// The normalized repository key (`kopiur_api::common::repo_key`) this run
    /// verifies, set ONLY for a multi-repository policy (#368): the mover then
    /// stamps its success into `status.verificationStamps[<key>]` (an
    /// entry-keyed merge-patch, so concurrent per-repo verifies never clobber)
    /// instead of the flat `status.lastVerified`. `None` = the classic
    /// single-repo flow, whose status write stays byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_key: Option<String>,
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

/// Payload for a logical (snapshot-level) replication run — the
/// `SnapshotReplication` CRD's mover contract. Like every op payload this is a
/// **wire mirror**: plain serde structs, never the CRD spec types themselves
/// (the [`ReplicateOp`] precedent), so the controller↔mover JSON contract
/// cannot drift with CRD refactors. Everything beyond the two repository
/// blocks is serde-defaulted so a spec stamped by an older controller (or with
/// selection/pruning omitted) still decodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplicateOp {
    /// The DESTINATION backend to migrate snapshots into (the same
    /// serializable wire type as the source `repository` field). The
    /// destination's credentials arrive `KOPIUR_DEST_`-prefixed (issue #200);
    /// its kopia password rides [`crate::env::DEST_KOPIA_PASSWORD`].
    pub destination: RepositoryConnect,
    /// The resolved destination repository CR — kind/name/namespace for the
    /// `spec.repository` pin stamped on every copy CR, uid for the
    /// `REPOSITORY_UID_LABEL` (and the three-label pruning candidate set).
    pub destination_repository: ReplicationRepositoryRef,
    /// The resolved SOURCE repository CR, carried for the copy CRs'
    /// `status.copiedFrom.repository` lineage. No uid: nothing keys on it.
    pub source_repository: ReplicationSourceRef,
    /// Identity include matchers (`selection.identities.include`). Empty =
    /// every identity in the source repository.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<IdentityMatcherSpec>,
    /// Identity exclude matchers (`selection.identities.exclude`). An identity
    /// matched by ANY exclude matcher is dropped even when included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<IdentityMatcherSpec>,
    /// `--latest-only`: migrate only the newest snapshot per selected identity.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub latest_only: bool,
    /// `--parallel <n>`: how many sources kopia migrates concurrently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// How kopia-side policies are treated on the destination. Defaults to
    /// [`PolicyCopyModeSpec::None`] — the operator owns policy on both sides.
    #[serde(default, skip_serializing_if = "PolicyCopyModeSpec::is_default")]
    pub policies: PolicyCopyModeSpec,
    /// Which dest-side copy CRs this run may prune. Defaults to
    /// [`PruningSpec::None`] (never prune) — absent on the wire means none.
    #[serde(default, skip_serializing_if = "PruningSpec::is_none")]
    pub pruning: PruningSpec,
}

/// A fully-resolved repository CR reference on the wire (kind + name +
/// namespace + uid). A plain-string mirror of the api crate's `RepositoryRef`
/// plus the CR uid — never the CRD type itself (wire-mirror rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationRepositoryRef {
    /// `"Repository"` or `"ClusterRepository"` (the CRD kind's serde value).
    pub kind: String,
    /// Name of the repository CR.
    pub name: String,
    /// Namespace of a namespaced `Repository`; `None` for `ClusterRepository`
    /// (or a same-namespace reference).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The repository CR's `metadata.uid` — the value the copy CRs'
    /// `REPOSITORY_UID_LABEL` carries (deletion batching / breaker keying).
    pub uid: String,
}

/// The SOURCE repository CR reference on the wire — like
/// [`ReplicationRepositoryRef`] but without a uid (nothing labels on it; it
/// only feeds `status.copiedFrom.repository`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationSourceRef {
    /// `"Repository"` or `"ClusterRepository"` (the CRD kind's serde value).
    pub kind: String,
    /// Name of the repository CR.
    pub name: String,
    /// Namespace of a namespaced `Repository`, when the reference crosses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// One identity matcher (`selection.identities.include`/`exclude` member).
/// Each present field is a component glob (`*`/`?`, non-path-crossing) matched
/// against the structured kopia triple; an absent field matches anything. An
/// all-absent matcher is webhook-refused; the mover defensively treats one as
/// matching NOTHING (an invalid matcher must never select — or exclude —
/// everything).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityMatcherSpec {
    /// Glob for the kopia `username` component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Glob for the kopia `hostname` component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Glob for the kopia source-path component (anchored; `*` matches any run
    /// of characters — matching is per structured component, so there is no
    /// separator to cross).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// How `kopia snapshot migrate` treats kopia-side policies on the destination —
/// the wire mirror of [`kopiur_kopia::MigratePolicies`]. Defaults to `None`:
/// kopia's own `--policies` default is TRUE, so the mover must always render
/// the mode explicitly (see [`Self::to_kopia`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PolicyCopyModeSpec {
    /// `--no-policies`: copy no kopia policies (the default).
    #[default]
    None,
    /// `--policies`: copy policies, keeping existing destination ones.
    Copy,
    /// `--policies --overwrite-policies`: copy, overwriting existing ones.
    CopyOverwrite,
}

impl PolicyCopyModeSpec {
    /// Whether this is the default mode (elided from the wire).
    pub fn is_default(&self) -> bool {
        *self == PolicyCopyModeSpec::None
    }

    /// Convert to the kopia client's [`MigratePolicies`](kopiur_kopia::MigratePolicies).
    /// Exhaustive — a new mode cannot compile until mapped.
    pub fn to_kopia(&self) -> kopiur_kopia::MigratePolicies {
        match self {
            PolicyCopyModeSpec::None => kopiur_kopia::MigratePolicies::None,
            PolicyCopyModeSpec::Copy => kopiur_kopia::MigratePolicies::Copy,
            PolicyCopyModeSpec::CopyOverwrite => kopiur_kopia::MigratePolicies::CopyOverwrite,
        }
    }
}

/// Which dest-side copy CRs a replication run prunes. Externally tagged
/// (`{ "none": {} }` / `{ "mirrorSource": {} }` / `{ "retention": {...} }`)
/// per the repo's discriminated-union rule, with empty marker sub-objects so
/// future knobs slot in without wire breakage. Defaults to
/// [`PruningSpec::None`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PruningSpec {
    /// Never prune (the default): copies accumulate until pruned externally.
    None(NoPruningSpec),
    /// Delete copy CRs whose `(identity, startTime)` vanished from the source.
    /// The mover deliberately stamps NO `pruned-by` annotation for these, so
    /// the deletes classify EXTERNAL and the destination repository's
    /// mass-deletion breaker holds a bulk source-vanish (ransomware at the
    /// source cannot empty the offsite copy).
    MirrorSource(MirrorSourcePruningSpec),
    /// GFS retention over the copy CRs, bucketed per identity. An OPERATOR
    /// prune: each delete is annotated `pruned-by: replication-retention`
    /// BEFORE deletion, so it bypasses the breaker like any retention prune.
    Retention(ReplicationRetentionSpec),
}

impl Default for PruningSpec {
    fn default() -> Self {
        PruningSpec::None(NoPruningSpec {})
    }
}

impl PruningSpec {
    /// Whether this is the no-pruning default (elided from the wire).
    pub fn is_none(&self) -> bool {
        matches!(self, PruningSpec::None(_))
    }
}

/// Empty marker payload for [`PruningSpec::None`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoPruningSpec {}

/// Empty marker payload for [`PruningSpec::MirrorSource`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorSourcePruningSpec {}

/// GFS keep counts for [`PruningSpec::Retention`] — a wire mirror of the api
/// crate's common `Retention` (same six fields, same semantics: union of
/// buckets, all-`None` keeps nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationRetentionSpec {
    /// Keep the N most-recent copies regardless of age.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_latest: Option<u32>,
    /// Keep one copy per hour for the most-recent N hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_hourly: Option<u32>,
    /// Keep one copy per day for the most-recent N days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_daily: Option<u32>,
    /// Keep one copy per week for the most-recent N weeks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_weekly: Option<u32>,
    /// Keep one copy per month for the most-recent N months.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_monthly: Option<u32>,
    /// Keep one copy per year for the most-recent N years.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_annual: Option<u32>,
}

impl ReplicationRetentionSpec {
    /// Convert to the api crate's GFS policy so the mover's prune selection
    /// runs through the SAME kernel (`kopiur_api::select_kept`) as the
    /// controller's `SnapshotPolicy` retention — no second GFS implementation.
    pub fn to_retention(&self) -> kopiur_api::common::Retention {
        kopiur_api::common::Retention {
            keep_latest: self.keep_latest,
            keep_hourly: self.keep_hourly,
            keep_daily: self.keep_daily,
            keep_weekly: self.keep_weekly,
            keep_monthly: self.keep_monthly,
            keep_annual: self.keep_annual,
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

impl ResolvedIdentity {
    /// The full kopia source spec (`username@hostname:path`) for this identity.
    ///
    /// This is the exact form kopia records a snapshot under (via
    /// `snapshot create --override-source`) and the form `snapshot verify
    /// --sources` / `snapshot list --source` match against, so scoping a verify
    /// or list to this string targets precisely this policy's snapshots and no
    /// other identity sharing the repository (issue #250).
    pub fn source_spec(&self) -> String {
        format!("{}@{}:{}", self.username, self.hostname, self.source_path)
    }
}

/// How to reach the repository. Externally tagged: exactly one backend.
///
/// This mirrors `kopiur_kopia::ConnectSpec` but is a *serializable* wire type
/// (the kopia client's `ConnectSpec` is intentionally not serde). The mover
/// converts one to the other. Credentials are NOT here: they arrive as env vars
/// (mounted Secret) so they never land in a ConfigMap.
///
/// The variants mirror the CRD `Backend` kinds one-to-one, so the
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
        /// PEM CA bundle content (not a reference) used to verify the
        /// endpoint's certificate. The controller resolves the CRD's
        /// `tls.caBundleRef` ConfigMap at Job-build time and inlines the PEM
        /// here, so the mover needs no ConfigMap access, no extra mounts, and
        /// no per-namespace bundle copies — a CA certificate is public key
        /// material, safe on the wire. Reaches kopia as
        /// `--root-ca-pem-base64` (see `ConnectSpec::S3::root_ca_pem`).
        /// Omitted when absent, so work specs written before this field parse.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ca_bundle_pem: Option<String>,
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
                ca_bundle_pem,
                ambient_credentials,
            } => ConnectSpec::S3 {
                bucket: bucket.clone(),
                endpoint: endpoint.clone(),
                prefix: prefix.clone(),
                region: region.clone(),
                disable_tls: *disable_tls,
                disable_tls_verification: *disable_tls_verification,
                root_ca_pem: ca_bundle_pem.clone(),
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
