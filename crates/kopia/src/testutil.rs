//! Test support for code that runs a fake `kopia` through the real
//! [`KopiaClient`](crate::KopiaClient) subprocess path. Compiled for this
//! crate's own tests and behind the `testutil` feature for downstream crates'
//! tests — never enable it in a shipped binary.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Create `path` as an executable (0755) file holding `contents`, WITHOUT this
/// process ever holding a writable fd to it.
///
/// Why not `std::fs::write` + `set_permissions`: tests run on many threads, and
/// another test spawning its own subprocess forks this whole process. A fork
/// that lands while the writer fd is open hands the child a copy of it, and
/// that copy lives until the child execs (`O_CLOEXEC` only closes it AT exec).
/// If this test execs its shim in that window, the kernel sees an open writer
/// and fails with ETXTBSY ("Text file busy"). [`KopiaClient`](crate::KopiaClient)
/// retries that errno briefly, but a loaded machine can keep a forked child
/// between fork and exec longer than the retry budget, which is the flake this
/// helper exists to remove.
///
/// Here a short-lived `/bin/sh` child does the write and the chmod, so the
/// writer only ever exists inside that child, and it is gone once we have
/// waited for the child. Forks by other test threads copy only this process's
/// fds, so they can never inherit a writer to the file.
///
/// # Panics
/// If `/bin/sh` cannot be run or the write fails. This is test support, and a
/// shim that was not written must fail the test loudly.
pub fn write_executable(path: &Path, contents: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn /bin/sh to write the shim");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(contents.as_bytes())
        .expect("write shim contents to /bin/sh");
    // The stdin handle was dropped above, so `cat` sees EOF and exits.
    let status = child.wait().expect("wait for the shim writer");
    assert!(
        status.success(),
        "writing shim {} failed: {status}",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_an_executable_with_exact_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shim.sh");
        let body = "#!/bin/sh\necho \"$@\"\n";
        write_executable(&path, body);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let out = Command::new(&path).arg("hi").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    }
}
