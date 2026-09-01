#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

pub mod client;
pub mod env;
pub mod error;
pub mod humanize;
pub mod model;
pub mod selection;
pub mod session;

pub use client::{
    CacheTuning, ConnectOptions, ConnectSpec, CreateOptions, KopiaClient, KopiaClientBuilder,
    MaintenanceMode, MigratePolicies, MigrateSources, PolicyArgs, RestoreOptions, ServerAuthMode,
    ServerStartSpec, SnapshotCreateOptions, SnapshotMigrateOptions, SyncToOptions, ThrottleArgs,
    VerifyOptions, split_policy_scopes,
};
pub use error::{
    KopiaError, KopiaErrorClass, notfound_is_uninitialized, snapshot_skipped_unchanged,
};
pub use humanize::{exit_code_desc, humanize_tail};
pub use model::{
    BlobRetention, ClientOptions, ContentFormat, DirEntry, DirManifest, DirSummary, DirSummaryLite,
    EntryError, IndexBlobEntry, MaintenanceCadence, MaintenanceInfo, MaintenanceSchedule,
    RepositoryStatus, RootEntry, SnapshotCreateOutcome, SnapshotCreateResult, SnapshotListEntry,
    SnapshotSource, SnapshotStats, StorageInfo, user_tags,
};
pub use selection::{filter_as_of, pick_offset};
pub use session::SessionCmd;
