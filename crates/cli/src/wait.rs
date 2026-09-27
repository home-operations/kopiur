//! The shared single-object wait loop, re-exported from `kopiur-ops` so the
//! CLI's `--wait` paths keep reaching for it by this path.

pub use kopiur_ops::wait::{DEFAULT_WAIT_TIMEOUT, wait_for};
