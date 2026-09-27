//! Snapshot → browse-target resolution.
//!
//! The resolution itself lives in [`kopiur_ops::browse::resolve`] — the web UI
//! resolves the same target from the same CRs — and is re-exported here so
//! every CLI call site keeps reading `browse::resolve::…`.

pub use kopiur_ops::browse::resolve::*;
