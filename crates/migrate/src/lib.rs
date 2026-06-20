//! Pure VolSync→kopiur translation core (kube-free).
//!
//! Extracted from `kopiur-cli`'s `migrate volsync` command: the CLI keeps the
//! kube IO (`cmd/migrate/{mod,io}.rs`) and consumes this crate's pure translation
//! surface. This crate has no `tokio` and no `kube` dependency — it only maps
//! VolSync `ReplicationSource`/`ReplicationDestination` specs (restic or
//! fork-kopia movers) into kopiur `SnapshotPolicy`/`Restore`/`Repository` shapes.
//!
//! The three modules are flat siblings under the crate root so the existing
//! `super::translate::…` / `super::volsync_types::…` paths inside them resolve
//! unchanged after the move.

pub mod kopia;
pub mod translate;
pub mod volsync_types;
