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
    PolicyArgs, RestoreOptions, ServerAuthMode, ServerStartSpec, ThrottleArgs, VerifyOptions,
    split_policy_scopes,
};
pub use error::{KopiaError, KopiaErrorClass, notfound_is_uninitialized};
pub use model::{
    ClientOptions, ContentFormat, DirEntry, DirManifest, DirSummary, DirSummaryLite, EntryError,
    IndexBlobEntry, MaintenanceCadence, MaintenanceInfo, MaintenanceSchedule, RepositoryStatus,
    RootEntry, SnapshotCreateResult, SnapshotListEntry, SnapshotSource, SnapshotStats, StorageInfo,
};
pub use selection::{filter_as_of, pick_offset};
pub use session::SessionCmd;
