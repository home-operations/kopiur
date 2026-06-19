//! Source-readability preflight — the one place a backup's permission-compatibility can be
//! *certainly* validated, because the mover runs **as** its resolved UID/GID with the source
//! PVC mounted, so it can simply try to read the files.
//!
//! securityContext reasoning (in `kopiur_api::secctx_compat`) can't see file mode bits, so it
//! is mostly `Unknown`. Here we sample the mounted tree as ourselves: a *wholly* unreadable
//! source is a near-certain mismatch that would otherwise become a **silently incomplete**
//! snapshot (kopia skips unreadable files and can still "succeed"). Catching it before
//! `snapshot create` turns that data-loss-shaped outcome into a clear, classified failure
//! that names the fix.
//!
//! Bounded: at most [`SAMPLE_LIMIT`] entries are visited regardless of tree size, in a
//! deterministic (sorted) order so a given tree always yields the same verdict.

use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Maximum filesystem entries the preflight visits before stopping. Large enough to be
/// representative, small enough to be cheap on a multi-million-file volume.
pub const SAMPLE_LIMIT: usize = 2000;

/// The outcome of sampling a source tree for readability by the current process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadabilityReport {
    /// Entries inspected (files opened + directories traversed).
    pub sampled: usize,
    /// Entries the mover could read.
    pub readable: usize,
    /// Entries the mover could NOT read (`EACCES`/`EPERM`).
    pub unreadable: usize,
    /// A representative unreadable entry's `(uid, gid, mode)`, for the diagnostic.
    pub sample_owner: Option<(u32, u32, u32)>,
    /// A representative unreadable entry's path.
    pub sample_path: Option<PathBuf>,
}

impl ReadabilityReport {
    /// The source is *wholly* unreadable: we sampled at least one entry and **none** were
    /// readable. Conservative on purpose — a partially-readable tree (some `0644`, some
    /// `0600`) is NOT "clearly unreadable" (kopia will skip the unreadable files and we only
    /// warn). This is the fail-fast trigger that prevents a silently-incomplete backup.
    pub fn is_clearly_unreadable(&self) -> bool {
        self.unreadable > 0 && self.readable == 0
    }

    /// Some sampled entries were unreadable (the backup would skip them).
    pub fn has_unreadable(&self) -> bool {
        self.unreadable > 0
    }

    fn note_unreadable(&mut self, path: &Path) {
        self.unreadable += 1;
        if self.sample_owner.is_none() {
            // `stat` (metadata) only needs `+x` on the parent, not read on the entry, so we
            // can usually report the owner even when we can't read the file itself.
            if let Ok(md) = fs::metadata(path) {
                self.sample_owner = Some((md.uid(), md.gid(), md.mode() & 0o7777));
                self.sample_path = Some(path.to_path_buf());
            } else {
                self.sample_path = Some(path.to_path_buf());
            }
        }
    }
}

/// Whether an IO error is a permission denial (the readability signal we care about).
fn is_permission_denied(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied)
}

/// Sample up to `limit` entries under `root`, classifying each as readable/unreadable by the
/// current process. Deterministic: directory children are visited in sorted order. Best-effort
/// — IO errors other than permission denials (e.g. a vanished file mid-walk) are ignored, so a
/// transient race never manufactures a false "unreadable" verdict.
pub fn sample_readability(root: &Path, limit: usize) -> ReadabilityReport {
    let mut report = ReadabilityReport::default();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(root.to_path_buf());

    while let Some(path) = queue.pop_front() {
        if report.sampled >= limit {
            break;
        }
        // Classify by type without following symlinks (a dangling/again-unreadable symlink
        // target shouldn't be charged as a source-permission problem).
        let md = match fs::symlink_metadata(&path) {
            Ok(md) => md,
            Err(_) => continue, // vanished / not a permission issue — skip
        };
        let ft = md.file_type();
        if ft.is_symlink() {
            continue; // kopia records the link itself; its target readability isn't ours to judge
        }
        if ft.is_dir() {
            report.sampled += 1;
            match fs::read_dir(&path) {
                Ok(entries) => {
                    report.readable += 1;
                    let mut children: Vec<PathBuf> =
                        entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
                    children.sort(); // deterministic visit order
                    for c in children {
                        queue.push_back(c);
                    }
                }
                Err(e) if is_permission_denied(&e) => report.note_unreadable(&path),
                Err(_) => {} // transient / not a permission issue
            }
        } else if ft.is_file() {
            report.sampled += 1;
            match fs::File::open(&path) {
                Ok(_) => report.readable += 1,
                Err(e) if is_permission_denied(&e) => report.note_unreadable(&path),
                Err(_) => {}
            }
        }
        // Sockets/fifos/devices: not data we back up — ignore.
    }
    report
}

/// The current process's effective UID, read from `/proc/self/status` (Linux; the mover image
/// is distroless Linux). `None` if unavailable — used only to enrich the diagnostic.
pub fn current_euid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // "Uid:\t<real>\t<effective>\t<saved>\t<fs>"
            return rest.split_whitespace().nth(1).and_then(|s| s.parse().ok());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut f = fs::File::create(path).unwrap();
        f.write_all(b"data").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn all_readable_tree_is_compatible() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("a.txt"), 0o644);
        write_file(&dir.path().join("b.txt"), 0o644);
        let r = sample_readability(dir.path(), SAMPLE_LIMIT);
        assert!(r.readable > 0);
        assert_eq!(r.unreadable, 0);
        assert!(!r.is_clearly_unreadable());
        assert!(!r.has_unreadable());
    }

    #[test]
    fn unreadable_file_is_counted() {
        // Running as non-root, a 0000-mode file we own is still openable by the owner; to
        // exercise the unreadable path deterministically across CI we check the counters via
        // a file owned by us but with no perms only when NOT root. When root (CI often is),
        // skip — root bypasses mode bits, which is itself the `RootMover` compatible case.
        if current_euid() == Some(0) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("secret"), 0o000);
        let r = sample_readability(dir.path(), SAMPLE_LIMIT);
        assert!(
            r.has_unreadable(),
            "a 0000 file must be unreadable by its non-root owner"
        );
        assert!(
            r.sample_owner.is_some(),
            "the owner uid/gid/mode must be captured"
        );
    }

    #[test]
    fn empty_tree_makes_no_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let r = sample_readability(dir.path(), SAMPLE_LIMIT);
        // Only the (readable) root dir was sampled — no unreadable entries, no fail-fast.
        assert!(!r.is_clearly_unreadable());
    }

    #[test]
    fn sample_limit_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            write_file(&dir.path().join(format!("f{i}")), 0o644);
        }
        let r = sample_readability(dir.path(), 10);
        assert!(r.sampled <= 10, "must stop at the sample limit");
    }

    #[test]
    fn verdict_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            write_file(&dir.path().join(format!("f{i}")), 0o644);
        }
        let a = sample_readability(dir.path(), SAMPLE_LIMIT);
        let b = sample_readability(dir.path(), SAMPLE_LIMIT);
        assert_eq!(a, b, "the same tree must yield an identical report");
    }
}
