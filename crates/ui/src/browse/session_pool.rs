//! Bounding what browse can ask of the cluster: a semaphore over session-pod
//! creation, single-flight so concurrent clicks on the same snapshot start one
//! pod rather than several, and per-identity plus global caps on in-flight
//! `pods/exec` calls.

/// The start and exec bounds shared by every browse request.
///
/// Filled in by Task 6.
#[derive(Debug, Default)]
pub struct SessionPool {}
