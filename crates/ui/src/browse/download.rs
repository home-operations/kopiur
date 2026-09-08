//! Streaming one file out of a snapshot.
//!
//! The kopia exec writes into a `tokio::io::duplex` pipe that becomes the
//! response body, capped at exactly the entry's recorded size: a short or overlong
//! copy is counted and surfaced as an error rather than handed to the browser as a
//! silently-truncated file with a plausible name.
