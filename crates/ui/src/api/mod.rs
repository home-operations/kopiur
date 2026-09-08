//! The `/api/v1` surface: one module per resource family, plus the shared error
//! shape.
//!
//! Every handler splits into two halves. `load_*` does the IO — reading from the
//! reflector cache or through an impersonated client, filtered by what the caller
//! may see — and `view_*` is a pure function from Kubernetes objects to
//! `kopiur_ui_model` wire types, so the mapping is fixture-testable without a
//! cluster.
//!
//! The router itself is assembled in [`crate::app`].

pub mod doctor;
pub mod events;
pub mod gates;
pub mod graph;
pub mod maintenance;
pub mod me;
pub mod policies;
pub mod problem;
pub mod replications;
pub mod repositories;
pub mod restores;
pub mod schedules;
pub mod snapshots;
pub mod status;
