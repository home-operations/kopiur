//! The mutating endpoints: snapshot-now, restore, suspend/resume, maintenance
//! run, replication run, catalog scan, and snapshot deletion.
//!
//! Each one builds the same request type the CLI does (`kopiur_ops::actions`) and
//! applies it under the caller's impersonated identity with the
//! [`crate::config::FIELD_MANAGER`] field manager, so a change made in the UI is
//! attributable in `managedFields` and identical to the one `kubectl kopiur`
//! would have made.
