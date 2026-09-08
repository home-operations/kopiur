//! Verifying that a request really arrived through the configured proxy.
//!
//! Identity headers are only trustworthy if nothing else can set them, and in a
//! cluster anything that can reach the Service can. The proxy therefore proves it
//! knows a shared secret, compared in constant time — a byte-by-byte `==` returns
//! early on the first mismatch and leaks the matched prefix length, which is
//! enough to recover the secret one byte at a time.
