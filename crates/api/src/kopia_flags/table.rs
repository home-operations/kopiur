//! The reviewed flag tables. One `const` per kopia command (or shared group), one
//! row per flag kopia's help lists for it, in help order. Edit these when the
//! ratchet tests name a flag: decide, write the reason, done.

use super::{Arity, FlagDisposition, FlagSpec};

const B: Arity = Arity::Bool;
const V: Arity = Arity::Value;

const fn modeled(name: &'static str, arity: Arity, spec_path: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        arity,
        disposition: FlagDisposition::Modeled { spec_path },
    }
}

const fn owned(name: &'static str, arity: Arity, why: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        arity,
        disposition: FlagDisposition::OwnedByKopiur { why },
    }
}

const fn global(name: &'static str, arity: Arity, why: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        arity,
        disposition: FlagDisposition::Global { why },
    }
}

const fn rejected(
    name: &'static str,
    arity: Arity,
    why: &'static str,
    use_instead: Option<&'static str>,
) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        arity,
        disposition: FlagDisposition::Rejected { why, use_instead },
    }
}

const fn unmodeled(name: &'static str, arity: Arity, why: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        arity,
        disposition: FlagDisposition::Unmodeled { why },
    }
}

/// Attach a single-letter alias.
const fn short(mut spec: FlagSpec, c: char) -> FlagSpec {
    spec.short = Some(c);
    spec
}

// Reasons repeated across tables.
const JSON: &str = "Kopiur parses kopia's JSON output; without it the result is unreadable";
const CREDS_ENV: &str = "credentials reach kopia as environment variables projected from the \
     backend's auth Secret, never on the command line where they would show up in the \
     process list and Job spec";
const EMBED: &str = "would copy the credential into kopia's config file on the mover's cache \
     volume, outliving the Secret it came from";
const FLAT: &str = "on-disk layout chosen when the repository was created; kopia's default \
     sharded layout is what Kopiur creates";
const SPEED_DOWN: &str = "RepositoryReplication:spec.sync.maxDownloadSpeedBytesPerSecond";
const SPEED_UP: &str = "RepositoryReplication:spec.sync.maxUploadSpeedBytesPerSecond";
const POINT_IN_TIME: &str = "a read-only view of versioned storage at a past time; Kopiur \
     restores by snapshot, which already pins the point in time";
const ACTIONS: &str = "kopia snapshot actions run inside the mover, which has none of the \
     workload's binaries; Kopiur runs hooks in the workload instead";
const LOG_DETAIL: &str = "per-entry log verbosity in the mover's log; changes no data";
const SCHEDULER: &str = "kopia's own snapshot scheduler never runs in a mover; Kopiur's \
     SnapshotSchedule decides when snapshots happen";
const KEEP: &str = "kopia-side retention would expire snapshots behind Kopiur's back (it pins \
     kopia's keep-* to effectively infinite at the identity scope); retention is GFS on the \
     SnapshotPolicy, enforced by pruning Snapshot CRs";
const REPLACED: &str = "Kopiur REPLACES the per-source policy on every run (delete, then set), \
     so remove/clear has nothing to act on — and kopia 0.23.1's `--clear-ignore` returns \
     before applying the `--add-ignore` beside it, storing an EMPTY list";
const ROOT_TARGET: &str = "Kopiur names the exact source/target itself";

/// Flags kopia accepts on every command.
pub const GLOBAL_FLAGS: &[FlagSpec] = &[
    owned("help", B, "prints help and exits without doing the work"),
    owned(
        "version",
        B,
        "prints the version and exits without doing the work",
    ),
    global(
        "log-file",
        V,
        "where kopia writes its file log; changes no data",
    ),
    global(
        "disable-file-logging",
        B,
        "file logging only; changes no data",
    ),
    global(
        "disable-content-log",
        B,
        "content logging only; changes no data",
    ),
    global(
        "log-dir",
        V,
        "where kopia writes its file logs; changes no data",
    ),
    global("log-level", V, "console log verbosity; changes no data"),
    global("file-log-level", V, "file log verbosity; changes no data"),
    owned(
        "help-full",
        B,
        "prints help and exits without doing the work",
    ),
    owned(
        "config-file",
        V,
        "the mover connects through its own config file; pointing elsewhere would \
         operate on a different repository connection",
    ),
    short(
        owned(
            "password",
            V,
            "the repository password comes from spec.encryption.passwordSecretRef as \
             KOPIA_PASSWORD, never on the command line",
        ),
        'p',
    ),
    owned(
        "persist-credentials",
        B,
        "Kopiur decides per connection whether the password is written beside the config \
         file (only for read-only browse sessions)",
    ),
    owned(
        "use-keyring",
        B,
        "movers have no keyring; the password comes from the repository Secret",
    ),
    global(
        "progress",
        B,
        "progress output on the console only; changes no data",
    ),
];

pub(super) const SNAPSHOT_CREATE: &[FlagSpec] = &[
    owned(
        "all",
        B,
        "Kopiur snapshots exactly one source per run; `--all` would snapshot every \
         source this identity ever backed up",
    ),
    modeled("upload-limit-mb", V, "SnapshotPolicy:spec.upload.limitMb"),
    modeled("description", V, "Snapshot:spec.description"),
    modeled("fail-fast", B, "SnapshotPolicy:spec.errorHandling.failFast"),
    modeled(
        "force-hash",
        V,
        "SnapshotPolicy:spec.upload.forceHashPercent",
    ),
    rejected(
        "parallel",
        V,
        "overrides the policy's file-read parallelism for one run, so the policy no \
         longer describes what ran",
        Some("SnapshotPolicy:spec.upload.maxParallelFileReads"),
    ),
    rejected(
        "start-time",
        V,
        "falsifies the snapshot's recorded time, which GFS retention buckets and \
         point-in-time restores are computed from",
        None,
    ),
    rejected(
        "end-time",
        V,
        "falsifies the snapshot's recorded time, which GFS retention buckets and \
         point-in-time restores are computed from",
        None,
    ),
    owned(
        "stdin-file",
        V,
        "set for stream sources from spec.sources[].stream; on a path source it would \
         snapshot the mover's empty stdin instead of the volume",
    ),
    modeled("tags", V, "Snapshot:spec.tags"),
    rejected(
        "pin",
        V,
        "a pin set behind Kopiur's back is invisible to the Snapshot CR that owns the \
         snapshot",
        Some("Snapshot:spec.pin"),
    ),
    owned(
        "override-source",
        V,
        "Kopiur sets the recorded source from the resolved identity; overriding it \
         re-identifies the snapshot and breaks retention, restore and adoption",
    ),
    unmodeled(
        "send-snapshot-report",
        B,
        "sends a report through notification profiles configured in the repository; \
         nothing Kopiur relies on",
    ),
    unmodeled("log-dir-detail", V, LOG_DETAIL),
    unmodeled("log-entry-detail", V, LOG_DETAIL),
    owned("json", B, JSON),
];

pub(super) const SNAPSHOT_RESTORE: &[FlagSpec] = &[
    modeled(
        "overwrite-directories",
        B,
        "Restore:spec.options.overwriteDirectories",
    ),
    modeled("overwrite-files", B, "Restore:spec.options.overwriteFiles"),
    modeled(
        "overwrite-symlinks",
        B,
        "Restore:spec.options.overwriteSymlinks",
    ),
    modeled(
        "write-sparse-files",
        B,
        "Restore:spec.options.writeSparseFiles",
    ),
    unmodeled(
        "consistent-attributes",
        B,
        "only matters when a path source matches several snapshots; Kopiur restores one \
         snapshot by id",
    ),
    unmodeled(
        "mode",
        V,
        "restore output mode override; kopia picks local-filesystem output for a \
         directory target, which is the only target Kopiur uses",
    ),
    modeled("parallel", V, "Restore:spec.options.parallel"),
    modeled("skip-owners", B, "Restore:spec.options.skipOwners"),
    modeled(
        "skip-permissions",
        B,
        "Restore:spec.options.skipPermissions",
    ),
    modeled("skip-times", B, "Restore:spec.options.skipTimes"),
    modeled(
        "ignore-permission-errors",
        B,
        "Restore:spec.options.ignorePermissionErrors",
    ),
    modeled(
        "write-files-atomically",
        B,
        "Restore:spec.options.writeFilesAtomically",
    ),
    modeled("ignore-errors", B, "Restore:spec.options.ignoreErrors"),
    modeled("skip-existing", B, "Restore:spec.options.skipExisting"),
    modeled("delete-extra", B, "Restore:spec.options.enableFileDeletion"),
    rejected(
        "shallow",
        V,
        "writes placeholder files instead of data; the restored volume would not hold \
         the backup",
        None,
    ),
    rejected(
        "shallow-minsize",
        V,
        "only meaningful with `--shallow`, which writes placeholders instead of data",
        None,
    ),
    owned(
        "snapshot-time",
        V,
        "Kopiur resolves the snapshot (by id, offset or asOf) before restoring and \
         passes its id",
    ),
    unmodeled(
        "flush-files",
        B,
        "fsyncs restored files; the Job exits and the volume is unmounted cleanly anyway",
    ),
];

pub(super) const SNAPSHOT_VERIFY: &[FlagSpec] = &[
    modeled(
        "max-errors",
        V,
        "SnapshotPolicy:spec.verification.quick.maxErrors",
    ),
    unmodeled(
        "directory-id",
        V,
        "verifies raw object ids; Kopiur verifies by source",
    ),
    unmodeled(
        "file-id",
        V,
        "verifies raw object ids; Kopiur verifies by source",
    ),
    owned(
        "sources",
        V,
        "Kopiur passes the policy's resolved identities so a verify covers exactly its \
         own snapshots",
    ),
    modeled(
        "parallel",
        V,
        "SnapshotPolicy:spec.verification.quick.parallel",
    ),
    modeled(
        "file-queue-length",
        V,
        "SnapshotPolicy:spec.verification.quick.fileQueueLength",
    ),
    modeled(
        "file-parallelism",
        V,
        "SnapshotPolicy:spec.verification.quick.fileParallelism",
    ),
    modeled(
        "verify-files-percent",
        V,
        "SnapshotPolicy:spec.verification.verifyFilesPercent",
    ),
    unmodeled(
        "json",
        B,
        "Kopiur reads the verify's exit code and log lines, not a JSON report",
    ),
];

pub(super) const SNAPSHOT_LIST: &[FlagSpec] = &[
    short(
        owned(
            "incomplete",
            B,
            "set only by the catalog scan that looks for interrupted snapshots",
        ),
        'i',
    ),
    unmodeled("human-readable", B, "display formatting; Kopiur reads JSON"),
    short(
        unmodeled("delta", B, "display formatting; Kopiur reads JSON"),
        'd',
    ),
    short(
        unmodeled("manifest-id", B, "display formatting; Kopiur reads JSON"),
        'm',
    ),
    unmodeled("retention", B, "display formatting; Kopiur reads JSON"),
    unmodeled("mtime", B, "display formatting; Kopiur reads JSON"),
    unmodeled("owner", B, "display formatting; Kopiur reads JSON"),
    short(
        unmodeled("show-identical", B, "display formatting; Kopiur reads JSON"),
        'l',
    ),
    unmodeled(
        "storage-stats",
        B,
        "extra per-snapshot storage statistics; expensive and unused",
    ),
    unmodeled("reverse", B, "display order; Kopiur sorts itself"),
    short(
        owned(
            "all",
            B,
            "the catalog scan lists every identity in the repository",
        ),
        'a',
    ),
    short(
        unmodeled(
            "max-results",
            V,
            "truncates the listing; Kopiur needs every snapshot to reconcile the catalog",
        ),
        'n',
    ),
    unmodeled("tags", V, "listing filter; Kopiur filters in code"),
    owned("json", B, JSON),
];

pub(super) const SNAPSHOT_DELETE: &[FlagSpec] = &[
    rejected(
        "all-snapshots-for-source",
        B,
        "deletes every snapshot of a source at once; Kopiur deletes exactly the snapshots \
         whose Snapshot CRs are being deleted",
        None,
    ),
    owned(
        "delete",
        B,
        "the confirmation kopia requires; Kopiur passes it only for snapshots it was \
         asked to delete",
    ),
];

pub(super) const SNAPSHOT_PIN: &[FlagSpec] = &[
    modeled("add", V, "Snapshot:spec.pin"),
    modeled("remove", V, "Snapshot:spec.pin"),
];

pub(super) const SNAPSHOT_EXPIRE: &[FlagSpec] = &[
    owned("all", B, "Kopiur expires across every source in one pass"),
    owned(
        "delete",
        B,
        "the confirmation kopia requires; Kopiur decides when an expire really deletes",
    ),
];

pub(super) const SNAPSHOT_ESTIMATE: &[FlagSpec] = &[
    unmodeled("show-files", B, "display only"),
    short(unmodeled("quiet", B, "display only"), 'q'),
    unmodeled(
        "upload-speed",
        V,
        "only changes the estimate's printed time",
    ),
    unmodeled("max-examples-per-bucket", V, "display only"),
];

pub(super) const SNAPSHOT_MIGRATE: &[FlagSpec] = &[
    owned(
        "source-config",
        V,
        "Kopiur connects the source repository itself and passes that config",
    ),
    modeled(
        "sources",
        V,
        "SnapshotReplication:spec.selection.identities",
    ),
    owned(
        "all",
        B,
        "set when the replication selects every identity (no `selection.identities`)",
    ),
    modeled("policies", B, "SnapshotReplication:spec.migrate.policies"),
    modeled(
        "overwrite-policies",
        B,
        "SnapshotReplication:spec.migrate.policies",
    ),
    modeled(
        "latest-only",
        B,
        "SnapshotReplication:spec.selection.latestOnly",
    ),
    modeled("parallel", V, "SnapshotReplication:spec.migrate.parallel"),
    unmodeled(
        "apply-ignore-rules",
        B,
        "re-filters migrated snapshots through the destination's ignore rules; a \
         replica should be a faithful copy",
    ),
];

pub(super) const POLICY_SET: &[FlagSpec] = &[
    owned(
        "global",
        B,
        "would rewrite the repository-wide global policy every source inherits, not this \
         policy's source",
    ),
    rejected(
        "inherit",
        V,
        "turning inheritance off cuts this source off from the identity-scope policy \
         Kopiur pins (kopia retention disabled, identical snapshots kept)",
        None,
    ),
    rejected(
        "before-folder-action",
        V,
        ACTIONS,
        Some("SnapshotPolicy:spec.hooks"),
    ),
    rejected(
        "after-folder-action",
        V,
        ACTIONS,
        Some("SnapshotPolicy:spec.hooks"),
    ),
    rejected(
        "before-snapshot-root-action",
        V,
        ACTIONS,
        Some("SnapshotPolicy:spec.hooks"),
    ),
    rejected(
        "after-snapshot-root-action",
        V,
        ACTIONS,
        Some("SnapshotPolicy:spec.hooks"),
    ),
    rejected(
        "action-command-timeout",
        V,
        ACTIONS,
        Some("SnapshotPolicy:spec.hooks"),
    ),
    rejected(
        "action-command-mode",
        V,
        ACTIONS,
        Some("SnapshotPolicy:spec.hooks"),
    ),
    rejected(
        "persist-action-script",
        B,
        ACTIONS,
        Some("SnapshotPolicy:spec.hooks"),
    ),
    modeled(
        "compression",
        V,
        "SnapshotPolicy:spec.compression.compressor",
    ),
    unmodeled(
        "compression-min-size",
        V,
        "size floor for attempting compression; kopia's default compresses everything",
    ),
    unmodeled(
        "compression-max-size",
        V,
        "size ceiling for attempting compression; kopia's default has none",
    ),
    unmodeled(
        "add-only-compress",
        V,
        "restricts compression to listed extensions; kopia's default compresses all",
    ),
    rejected("remove-only-compress", V, REPLACED, None),
    rejected("clear-only-compress", B, REPLACED, None),
    modeled(
        "add-never-compress",
        V,
        "SnapshotPolicy:spec.compression.neverCompress",
    ),
    rejected(
        "remove-never-compress",
        V,
        REPLACED,
        Some("SnapshotPolicy:spec.compression.neverCompress"),
    ),
    rejected(
        "clear-never-compress",
        B,
        REPLACED,
        Some("SnapshotPolicy:spec.compression.neverCompress"),
    ),
    unmodeled(
        "metadata-compression",
        V,
        "compression for kopia's own metadata; kopia's default is fine",
    ),
    rejected(
        "splitter",
        V,
        "the object splitter is a repository property; a per-source override defeats \
         deduplication against every other source",
        Some("Repository:spec.create.splitter"),
    ),
    modeled(
        "ignore-file-errors",
        V,
        "SnapshotPolicy:spec.errorHandling.ignoreFileErrors",
    ),
    modeled(
        "ignore-dir-errors",
        V,
        "SnapshotPolicy:spec.errorHandling.ignoreDirErrors",
    ),
    modeled(
        "ignore-unknown-types",
        V,
        "SnapshotPolicy:spec.errorHandling.ignoreUnknownTypes",
    ),
    modeled("add-ignore", V, "SnapshotPolicy:spec.files.ignoreRules"),
    rejected(
        "remove-ignore",
        V,
        REPLACED,
        Some("SnapshotPolicy:spec.files.ignoreRules"),
    ),
    rejected(
        "clear-ignore",
        B,
        REPLACED,
        Some("SnapshotPolicy:spec.files.ignoreRules"),
    ),
    unmodeled(
        "add-dot-ignore",
        V,
        "names extra `.kopiaignore`-style files to honor inside the source",
    ),
    rejected("remove-dot-ignore", V, REPLACED, None),
    rejected("clear-dot-ignore", B, REPLACED, None),
    unmodeled(
        "max-file-size",
        V,
        "skips files above a size; kopia's default backs up every file",
    ),
    unmodeled(
        "one-file-system",
        V,
        "stays on the source's filesystem; a mover's source mount is one filesystem \
         already unless the volume nests mounts",
    ),
    modeled(
        "ignore-cache-dirs",
        V,
        "SnapshotPolicy:spec.files.ignoreCacheDirs",
    ),
    unmodeled("log-dir-snapshotted", V, LOG_DETAIL),
    unmodeled("log-dir-ignored", V, LOG_DETAIL),
    unmodeled("log-entry-snapshotted", V, LOG_DETAIL),
    unmodeled("log-entry-ignored", V, LOG_DETAIL),
    unmodeled("log-entry-cache-hit", V, LOG_DETAIL),
    unmodeled("log-entry-cache-miss", V, LOG_DETAIL),
    rejected(
        "keep-latest",
        V,
        KEEP,
        Some("SnapshotPolicy:spec.retention"),
    ),
    rejected(
        "keep-hourly",
        V,
        KEEP,
        Some("SnapshotPolicy:spec.retention"),
    ),
    rejected("keep-daily", V, KEEP, Some("SnapshotPolicy:spec.retention")),
    rejected(
        "keep-weekly",
        V,
        KEEP,
        Some("SnapshotPolicy:spec.retention"),
    ),
    rejected(
        "keep-monthly",
        V,
        KEEP,
        Some("SnapshotPolicy:spec.retention"),
    ),
    rejected(
        "keep-annual",
        V,
        KEEP,
        Some("SnapshotPolicy:spec.retention"),
    ),
    modeled(
        "ignore-identical-snapshots",
        V,
        "SnapshotPolicy:spec.files.ignoreIdenticalSnapshots",
    ),
    rejected(
        "snapshot-interval",
        V,
        SCHEDULER,
        Some("SnapshotSchedule:spec.schedule"),
    ),
    rejected(
        "snapshot-time",
        V,
        SCHEDULER,
        Some("SnapshotSchedule:spec.schedule"),
    ),
    rejected(
        "snapshot-time-crontab",
        V,
        SCHEDULER,
        Some("SnapshotSchedule:spec.schedule"),
    ),
    rejected(
        "run-missed",
        V,
        SCHEDULER,
        Some("SnapshotSchedule:spec.schedule"),
    ),
    rejected(
        "manual",
        B,
        SCHEDULER,
        Some("SnapshotSchedule:spec.schedule"),
    ),
    rejected(
        "enable-volume-shadow-copy",
        V,
        "Windows Volume Shadow Copy; movers run Linux",
        Some("SnapshotPolicy:spec.copyMethod"),
    ),
    modeled(
        "max-parallel-file-reads",
        V,
        "SnapshotPolicy:spec.upload.maxParallelFileReads",
    ),
    modeled(
        "max-parallel-snapshots",
        V,
        "SnapshotPolicy:spec.upload.maxParallelSnapshots",
    ),
    unmodeled(
        "parallel-upload-above-size-mib",
        V,
        "splits large files into parallel uploads; kopia's default threshold is fine",
    ),
];

pub(super) const POLICY_DELETE: &[FlagSpec] = &[
    owned(
        "global",
        B,
        "Kopiur only ever deletes the per-source policy it owns, never the global one",
    ),
    short(
        owned("dry-run", B, "Kopiur's delete must actually delete"),
        'n',
    ),
];

pub(super) const POLICY_SHOW: &[FlagSpec] =
    &[owned("global", B, ROOT_TARGET), owned("json", B, JSON)];

pub(super) const MAINTENANCE_RUN: &[FlagSpec] = &[
    modeled("full", B, "Maintenance:spec.schedule.full"),
    rejected(
        "safety",
        V,
        "`--safety=none` skips the safety margins that protect content still referenced \
         by in-flight snapshots",
        None,
    ),
];

pub(super) const MAINTENANCE_SET: &[FlagSpec] = &[
    modeled("owner", V, "Maintenance:spec.ownership.owner"),
    owned(
        "enable-quick",
        V,
        "the Maintenance CR's schedule decides when maintenance runs; kopia's own \
         scheduler is bypassed",
    ),
    owned(
        "enable-full",
        V,
        "the Maintenance CR's schedule decides when maintenance runs; kopia's own \
         scheduler is bypassed",
    ),
    owned(
        "quick-interval",
        V,
        "the Maintenance CR's schedule decides when maintenance runs",
    ),
    owned(
        "full-interval",
        V,
        "the Maintenance CR's schedule decides when maintenance runs",
    ),
    owned(
        "pause-quick",
        V,
        "pause maintenance by suspending the Maintenance CR's schedule",
    ),
    owned(
        "pause-full",
        V,
        "pause maintenance by suspending the Maintenance CR's schedule",
    ),
    unmodeled(
        "max-retained-log-count",
        V,
        "how many kopia maintenance log sessions the repository keeps",
    ),
    unmodeled(
        "max-retained-log-age",
        V,
        "how long kopia keeps maintenance log sessions",
    ),
    unmodeled(
        "max-retained-log-size-mb",
        V,
        "how much space kopia's maintenance log sessions may use",
    ),
    unmodeled(
        "extend-object-locks",
        V,
        "extends object-lock retention during full maintenance; relevant only to \
         object-locked buckets, whose retention is set via spec.parameters.blobRetention",
    ),
    unmodeled(
        "list-parallelism",
        V,
        "blob-listing parallelism for maintenance; kopia's default is fine",
    ),
];

pub(super) const MAINTENANCE_INFO: &[FlagSpec] = &[owned("json", B, JSON)];

/// The connection flags kopia repeats on `repository connect`, `repository
/// create` and `server start`.
pub(super) const CONNECT_COMMON: &[FlagSpec] = &[
    owned(
        "cache-directory",
        V,
        "the cache lives on the volume Kopiur mounts (KOPIA_CACHE_DIRECTORY)",
    ),
    modeled(
        "content-cache-size-mb",
        V,
        "Repository:spec.moverDefaults.cache.contentCacheSize",
    ),
    unmodeled(
        "content-cache-size-limit-mb",
        V,
        "hard cap above the soft content-cache budget; the soft budget is sized to fit \
         the cache volume",
    ),
    unmodeled(
        "content-min-sweep-age",
        V,
        "minimum age before a content-cache item may be evicted",
    ),
    modeled(
        "metadata-cache-size-mb",
        V,
        "Repository:spec.moverDefaults.cache.metadataCacheSize",
    ),
    unmodeled(
        "metadata-cache-size-limit-mb",
        V,
        "hard cap above the soft metadata-cache budget; the soft budget is sized to fit \
         the cache volume",
    ),
    unmodeled(
        "metadata-min-sweep-age",
        V,
        "minimum age before a metadata-cache item may be evicted",
    ),
    unmodeled(
        "index-min-sweep-age",
        V,
        "minimum age before an index-cache item may be evicted",
    ),
    unmodeled(
        "max-list-cache-duration",
        V,
        "how long blob listings are cached",
    ),
    owned(
        "check-for-updates",
        B,
        "always off (KOPIA_CHECK_FOR_UPDATES=false): movers make no calls to GitHub",
    ),
    owned(
        "readonly",
        B,
        "set for read-only connections only (restores, browse sessions, replication \
         sources, read-only bootstraps), which must never write to the repository",
    ),
    unmodeled(
        "description",
        V,
        "a human label stored in the mover's throwaway config file",
    ),
    rejected(
        "enable-actions",
        B,
        ACTIONS,
        Some("SnapshotPolicy:spec.hooks"),
    ),
];

pub(super) const REPOSITORY_CREATE: &[FlagSpec] = &[
    modeled("block-hash", V, "Repository:spec.create.hash"),
    modeled("encryption", V, "Repository:spec.create.encryption"),
    modeled("ecc", V, "Repository:spec.create.ecc.algorithm"),
    modeled(
        "ecc-overhead-percent",
        V,
        "Repository:spec.create.ecc.overheadPercent",
    ),
    modeled("object-splitter", V, "Repository:spec.create.splitter"),
    short(
        owned(
            "create-only",
            B,
            "Kopiur creates and then connects in one bootstrap step",
        ),
        'c',
    ),
    unmodeled(
        "format-version",
        V,
        "forces an older repository format; kopia's default is the current stable one",
    ),
    modeled(
        "retention-mode",
        V,
        "Repository:spec.parameters.blobRetention",
    ),
    modeled(
        "retention-period",
        V,
        "Repository:spec.parameters.blobRetention",
    ),
    unmodeled(
        "format-block-key-derivation-algorithm",
        V,
        "key-derivation algorithm for the format blob; kopia's default is fine",
    ),
];

pub(super) const REPOSITORY_SYNC_TO: &[FlagSpec] = &[
    modeled("update", B, "RepositoryReplication:spec.sync.update"),
    modeled("delete", B, "RepositoryReplication:spec.sync.deleteExtra"),
    short(
        owned(
            "dry-run",
            B,
            "a replication run must copy; a dry run would report success with nothing copied",
        ),
        'n',
    ),
    modeled("parallel", V, "RepositoryReplication:spec.sync.parallel"),
    modeled("must-exist", B, "RepositoryReplication:spec.sync.mustExist"),
    modeled("times", B, "RepositoryReplication:spec.sync.times"),
];

pub(super) const REPOSITORY_STATUS: &[FlagSpec] = &[
    short(
        rejected(
            "reconnect-token",
            B,
            "prints a token that reconnects to the repository into the mover's log",
            None,
        ),
        't',
    ),
    short(
        rejected(
            "reconnect-token-with-password",
            B,
            "prints the repository password into the mover's log",
            None,
        ),
        's',
    ),
    owned("json", B, JSON),
];

pub(super) const REPOSITORY_THROTTLE_SET: &[FlagSpec] = &[
    modeled(
        "download-bytes-per-second",
        V,
        "Repository:spec.moverDefaults.throttle.downloadBytesPerSecond",
    ),
    modeled(
        "upload-bytes-per-second",
        V,
        "Repository:spec.moverDefaults.throttle.uploadBytesPerSecond",
    ),
    modeled(
        "read-requests-per-second",
        V,
        "Repository:spec.moverDefaults.throttle.readOpsPerSecond",
    ),
    modeled(
        "write-requests-per-second",
        V,
        "Repository:spec.moverDefaults.throttle.writeOpsPerSecond",
    ),
    unmodeled(
        "list-requests-per-second",
        V,
        "caps blob listings per second; unthrottled by default",
    ),
    unmodeled(
        "concurrent-reads",
        V,
        "caps concurrent blob reads; unthrottled by default",
    ),
    unmodeled(
        "concurrent-writes",
        V,
        "caps concurrent blob writes; unthrottled by default",
    ),
];

pub(super) const REPOSITORY_SET_CLIENT: &[FlagSpec] = &[
    owned(
        "read-only",
        B,
        "Kopiur flips the client read-only bit itself around operations that must not \
         write",
    ),
    owned(
        "read-write",
        B,
        "Kopiur flips the client read-only bit itself around operations that must not \
         write",
    ),
    unmodeled(
        "description",
        V,
        "a human label in the mover's throwaway config file",
    ),
    owned(
        "username",
        V,
        "the kopia identity is resolved from the policy and repository identityDefaults",
    ),
    owned(
        "hostname",
        V,
        "the kopia identity is resolved from the policy and repository identityDefaults",
    ),
    unmodeled(
        "repository-format-cache-duration",
        V,
        "how long the format blob is cached in the mover",
    ),
    unmodeled(
        "disable-repository-format-cache",
        B,
        "disables the format-blob cache in the mover",
    ),
];

pub(super) const REPOSITORY_SET_PARAMETERS: &[FlagSpec] = &[
    unmodeled(
        "max-pack-size-mb",
        V,
        "pack file size; kopia's default suits object storage",
    ),
    unmodeled(
        "index-version",
        V,
        "index format version; kopia's default is the current one",
    ),
    modeled(
        "retention-mode",
        V,
        "Repository:spec.parameters.blobRetention",
    ),
    modeled(
        "retention-period",
        V,
        "Repository:spec.parameters.blobRetention",
    ),
    rejected(
        "upgrade",
        B,
        "an irreversible repository format upgrade that every client must support first; \
         do it deliberately with the kopia CLI, not from a reconcile",
        None,
    ),
    modeled(
        "epoch-refresh-frequency",
        V,
        "Repository:spec.parameters.epoch.refreshFrequency",
    ),
    modeled(
        "epoch-min-duration",
        V,
        "Repository:spec.parameters.epoch.minDuration",
    ),
    unmodeled(
        "epoch-cleanup-safety-margin",
        V,
        "safety margin before old epochs are cleaned up; kopia's default is the safe one",
    ),
    modeled(
        "epoch-advance-on-count",
        V,
        "Repository:spec.parameters.epoch.advanceOnCount",
    ),
    modeled(
        "epoch-advance-on-size-mb",
        V,
        "Repository:spec.parameters.epoch.advanceOnSizeMiB",
    ),
    modeled(
        "epoch-delete-parallelism",
        V,
        "Repository:spec.parameters.epoch.deleteParallelism",
    ),
    modeled(
        "epoch-checkpoint-frequency",
        V,
        "Repository:spec.parameters.epoch.checkpointFrequency",
    ),
];

pub(super) const REPOSITORY_VALIDATE_PROVIDER: &[FlagSpec] = &[
    unmodeled("num-storage-connections", V, "storage stress-test tuning"),
    unmodeled("concurrency-test-duration", V, "storage stress-test tuning"),
    unmodeled("put-blob-workers", V, "storage stress-test tuning"),
    unmodeled("get-blob-workers", V, "storage stress-test tuning"),
    unmodeled("get-metadata-workers", V, "storage stress-test tuning"),
];

pub(super) const INDEX_LIST: &[FlagSpec] = &[
    unmodeled(
        "summary",
        B,
        "display only; Kopiur counts index blobs from JSON",
    ),
    unmodeled(
        "superseded",
        B,
        "would count compacted index blobs too, inflating the health signal",
    ),
    unmodeled("sort", V, "display order only"),
    owned("json", B, JSON),
];

pub(super) const LS: &[FlagSpec] = &[
    short(
        unmodeled(
            "long",
            B,
            "display only; Kopiur only asks whether the path resolves",
        ),
        'l',
    ),
    short(
        unmodeled(
            "human-readable",
            B,
            "display only; Kopiur only asks whether the path resolves",
        ),
        'h',
    ),
    short(
        unmodeled(
            "recursive",
            B,
            "would walk the whole subtree just to answer whether the path is a directory",
        ),
        'r',
    ),
    short(
        unmodeled(
            "show-object-id",
            B,
            "display only; Kopiur only asks whether the path resolves",
        ),
        'o',
    ),
    unmodeled(
        "error-summary",
        B,
        "display only; Kopiur only asks whether the path resolves",
    ),
];

pub(super) const SERVER_START: &[FlagSpec] = &[
    rejected(
        "html",
        V,
        "serves arbitrary HTML from the repository server",
        None,
    ),
    owned(
        "ui",
        B,
        "Kopiur enables kopia's UI on the repository server it runs for spec.server",
    ),
    unmodeled("grpc", B, "kopia's gRPC API; on by default"),
    unmodeled("control-api", B, "kopia's control API; on by default"),
    unmodeled(
        "refresh-interval",
        V,
        "how often the server refreshes repository status",
    ),
    unmodeled(
        "max-concurrency",
        V,
        "server goroutine cap; kopia's default is fine",
    ),
    unmodeled(
        "server-control-username",
        V,
        "control-API credentials; the control API is not exposed through the Service",
    ),
    unmodeled(
        "server-control-password",
        V,
        "control-API credentials; the control API is not exposed through the Service",
    ),
    rejected(
        "tls-cert-file",
        V,
        "in-pod TLS conflicts with the always-on `--insecure`; terminate TLS at your ingress",
        Some("Repository:spec.server.service"),
    ),
    rejected(
        "tls-key-file",
        V,
        "in-pod TLS conflicts with the always-on `--insecure`; terminate TLS at your ingress",
        Some("Repository:spec.server.service"),
    ),
    unmodeled(
        "persistent-logs",
        B,
        "writes server logs to a file in the (ephemeral) pod",
    ),
    unmodeled(
        "ui-preferences-file",
        V,
        "where the UI stores its preferences; ephemeral in the pod",
    ),
    unmodeled(
        "shutdown-grace-period",
        V,
        "grace period on shutdown; the pod's termination grace applies anyway",
    ),
    rejected(
        "kopiaui-notifications",
        B,
        "desktop KopiaUI integration; prints notifications to stdout",
        None,
    ),
    owned(
        "address",
        V,
        "the server binds where the Service Kopiur creates expects it",
    ),
    modeled("server-username", V, "Repository:spec.server.auth"),
    owned(
        "server-password",
        V,
        "appended at exec time from spec.server.auth's Secret, never in the Pod spec",
    ),
];

// --- storage backends ---------------------------------------------------------

pub(super) const STORAGE_FILESYSTEM: &[FlagSpec] = &[
    modeled("path", V, "Repository:spec.backend.filesystem.path"),
    unmodeled(
        "owner-uid",
        V,
        "repository files are owned by the mover's runAsUser",
    ),
    unmodeled(
        "owner-gid",
        V,
        "repository files are owned by the mover's runAsGroup",
    ),
    unmodeled("file-mode", V, "kopia's default 0600"),
    unmodeled("dir-mode", V, "kopia's default 0700"),
    unmodeled("flat", B, FLAT),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
];

pub(super) const STORAGE_S3: &[FlagSpec] = &[
    modeled("bucket", V, "Repository:spec.backend.s3.bucket"),
    modeled("endpoint", V, "Repository:spec.backend.s3.endpoint"),
    modeled("region", V, "Repository:spec.backend.s3.region"),
    owned("access-key", V, CREDS_ENV),
    owned("secret-access-key", V, CREDS_ENV),
    owned("session-token", V, CREDS_ENV),
    modeled("prefix", V, "Repository:spec.backend.s3.prefix"),
    modeled(
        "disable-tls",
        B,
        "Repository:spec.backend.s3.tls.disableTls",
    ),
    modeled(
        "disable-tls-verification",
        B,
        "Repository:spec.backend.s3.tls.insecureSkipVerify",
    ),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
    unmodeled("point-in-time", V, POINT_IN_TIME),
    modeled(
        "root-ca-pem-base64",
        V,
        "Repository:spec.backend.s3.tls.caBundleRef",
    ),
    rejected(
        "root-ca-pem-path",
        V,
        "names a file that does not exist in the mover image",
        Some("Repository:spec.backend.s3.tls.caBundleRef"),
    ),
];

pub(super) const STORAGE_AZURE: &[FlagSpec] = &[
    modeled("container", V, "Repository:spec.backend.azure.container"),
    modeled(
        "storage-account",
        V,
        "Repository:spec.backend.azure.storageAccount",
    ),
    owned("storage-key", V, CREDS_ENV),
    unmodeled(
        "storage-domain",
        V,
        "sovereign-cloud storage domain; settable as AZURE_STORAGE_DOMAIN in the auth Secret",
    ),
    owned("sas-token", V, CREDS_ENV),
    modeled("prefix", V, "Repository:spec.backend.azure.prefix"),
    owned("tenant-id", V, CREDS_ENV),
    owned("client-id", V, CREDS_ENV),
    owned("client-secret", V, CREDS_ENV),
    owned("client-cert", V, CREDS_ENV),
    owned(
        "azure-federated-token-file",
        V,
        "projected by the workload-identity webhook from \
         spec.backend.azure.auth.workloadIdentity",
    ),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
    unmodeled("point-in-time", V, POINT_IN_TIME),
];

pub(super) const STORAGE_GCS: &[FlagSpec] = &[
    modeled("bucket", V, "Repository:spec.backend.gcs.bucket"),
    modeled("prefix", V, "Repository:spec.backend.gcs.prefix"),
    unmodeled(
        "read-only",
        B,
        "requests a read-only OAuth scope; Kopiur's read-only connections use kopia's \
         client read-only bit instead",
    ),
    owned(
        "credentials-file",
        V,
        "mounted from spec.backend.gcs.auth's Secret",
    ),
    rejected("embed-credentials", B, EMBED, None),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
    unmodeled("point-in-time", V, POINT_IN_TIME),
];

pub(super) const STORAGE_B2: &[FlagSpec] = &[
    modeled("bucket", V, "Repository:spec.backend.b2.bucket"),
    owned("key-id", V, CREDS_ENV),
    owned("key", V, CREDS_ENV),
    modeled("prefix", V, "Repository:spec.backend.b2.prefix"),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
];

pub(super) const STORAGE_SFTP: &[FlagSpec] = &[
    modeled("path", V, "Repository:spec.backend.sftp.path"),
    modeled("host", V, "Repository:spec.backend.sftp.host"),
    modeled("port", V, "Repository:spec.backend.sftp.port"),
    modeled("username", V, "Repository:spec.backend.sftp.username"),
    owned("sftp-password", V, CREDS_ENV),
    owned("keyfile", V, "mounted from spec.backend.sftp.auth's Secret"),
    owned("key-data", V, CREDS_ENV),
    owned(
        "known-hosts",
        V,
        "mounted from spec.backend.sftp.auth's Secret",
    ),
    owned("known-hosts-data", V, CREDS_ENV),
    rejected("embed-credentials", B, EMBED, None),
    rejected(
        "external",
        B,
        "the mover image ships no ssh client; Kopiur uses kopia's built-in SFTP",
        None,
    ),
    rejected(
        "ssh-command",
        V,
        "the mover image ships no ssh client; Kopiur uses kopia's built-in SFTP",
        None,
    ),
    rejected(
        "ssh-args",
        V,
        "the mover image ships no ssh client; Kopiur uses kopia's built-in SFTP",
        None,
    ),
    unmodeled("flat", B, FLAT),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
];

pub(super) const STORAGE_WEBDAV: &[FlagSpec] = &[
    modeled("url", V, "Repository:spec.backend.webDav.url"),
    unmodeled("flat", B, FLAT),
    owned("webdav-username", V, CREDS_ENV),
    owned("webdav-password", V, CREDS_ENV),
    unmodeled(
        "atomic-writes",
        B,
        "a server-capability claim; kopia's default does not assume atomic writes",
    ),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
];

pub(super) const STORAGE_RCLONE: &[FlagSpec] = &[
    modeled(
        "remote-path",
        V,
        "Repository:spec.backend.rclone.remotePath",
    ),
    unmodeled("flat", B, FLAT),
    owned(
        "rclone-exe",
        V,
        "the rclone binary shipped in the mover image",
    ),
    owned(
        "rclone-args",
        V,
        "Kopiur passes `--config` pointing at the mounted spec.backend.rclone.configSecretRef",
    ),
    unmodeled(
        "rclone-env",
        V,
        "extra environment for rclone; the config Secret carries rclone's settings",
    ),
    rejected("embed-rclone-config", V, EMBED, None),
    unmodeled(
        "atomic-writes",
        B,
        "a provider-capability claim; kopia's default does not assume atomic writes",
    ),
    modeled(
        "rclone-startup-timeout",
        V,
        "Repository:spec.backend.rclone.startupTimeout",
    ),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
];

pub(super) const STORAGE_GDRIVE: &[FlagSpec] = &[
    modeled("folder-id", V, "Repository:spec.backend.gdrive.folderId"),
    unmodeled(
        "read-only",
        B,
        "requests a read-only OAuth scope; Kopiur's read-only connections use kopia's \
         client read-only bit instead",
    ),
    owned(
        "credentials-file",
        V,
        "mounted from spec.backend.gdrive.credentialsSecretRef",
    ),
    rejected("embed-credentials", B, EMBED, None),
    modeled("max-download-speed", V, SPEED_DOWN),
    modeled("max-upload-speed", V, SPEED_UP),
];

pub(super) const STORAGE_FROM_CONFIG: &[FlagSpec] = &[
    owned(
        "file",
        V,
        "only used to reconnect from a config Kopiur itself wrote",
    ),
    owned(
        "token",
        V,
        "only used to reconnect from a token Kopiur itself minted",
    ),
    owned(
        "token-file",
        V,
        "only used to reconnect from a token Kopiur itself minted",
    ),
    owned(
        "token-stdin",
        B,
        "only used to reconnect from a token Kopiur itself minted",
    ),
];

pub(super) const STORAGE_SERVER: &[FlagSpec] = &[
    owned(
        "url",
        V,
        "browse sessions connect to the repository server Kopiur runs for spec.server",
    ),
    owned(
        "server-cert-fingerprint",
        V,
        "pinned by Kopiur for the repository server it runs",
    ),
];
