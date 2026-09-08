//! The impersonation layer: the outermost middleware on every apiserver call.
//!
//! It removes every inbound `Impersonate-*` header before inserting its own, so a
//! caller can never choose the identity kopiur-ui asserts, and hands back a
//! per-identity `kube::Client` from a bounded LRU cache. The bound is not a
//! latency optimisation: each client owns a connection pool, so an unbounded
//! cache would be an unbounded socket and memory footprint keyed by whatever
//! usernames the proxy sends.
