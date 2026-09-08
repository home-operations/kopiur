//! Browsing a snapshot's contents: listing directories and downloading files out
//! of a running kopia session pod.
//!
//! Sessions are shared with `kubectl kopiur browse` — both go through
//! `kopiur_ops::browse` — so opening one in the UI while one is open on a
//! terminal does not start a second pod.

pub mod download;
pub mod session_pool;
