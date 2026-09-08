#![warn(missing_docs)]
//! `kopiur-ops` — the shared, kube-aware operations layer behind `kubectl kopiur`
//! and the web UI. Pure "typed CRs → report/decision" cores with thin kube IO.

pub mod actions;
pub mod browse;
pub mod ctx;
pub mod doctor;
pub mod error;
pub mod maintenance;
pub mod replication;
pub mod snapshots;
pub mod status;
pub mod suspend;
pub mod wait;

pub use ctx::{OpsCtx, Scope};
pub use error::{OpsError, OpsErrorKind, classify_kube, scope_suffix};
