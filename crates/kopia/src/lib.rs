#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

pub mod client;
pub mod env;
pub mod error;
pub mod model;
pub mod selection;
pub mod session;

pub use client::{
    CacheTuning, ConnectSpec, CreateOptions, KopiaClient, KopiaClientBuilder, MaintenanceMode,
    PolicyArgs, RestoreOptions, ServerAuthMode, ServerStartSpec, SnapshotCreateOptions,
    SyncToOptions, ThrottleArgs, VerifyOptions, split_policy_scopes,
};
pub use error::{
    KopiaError, KopiaErrorClass, notfound_is_uninitialized, snapshot_skipped_unchanged,
};
pub use model::{
    BlobRetention, ClientOptions, ContentFormat, DirEntry, DirManifest, DirSummary, DirSummaryLite,
    EntryError, IndexBlobEntry, MaintenanceCadence, MaintenanceInfo, MaintenanceSchedule,
    RepositoryStatus, RootEntry, SnapshotCreateOutcome, SnapshotCreateResult, SnapshotListEntry,
    SnapshotSource, SnapshotStats, StorageInfo, user_tags,
};
pub use selection::{filter_as_of, pick_offset};
pub use session::SessionCmd;
