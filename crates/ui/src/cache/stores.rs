//! One `kube::runtime::reflector::Store` per Kopiur kind, fed by watches under
//! the UI's own ServiceAccount.
//!
//! Readiness is gated on every store having synced, so the UI never serves a
//! still-filling cache as if it were the real fleet — an empty repository list
//! reads identically to a healthy cluster with nothing configured.
