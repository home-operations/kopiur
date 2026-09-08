//! The browse data-plane: read-only access to a snapshot's files.
//!
//! Every reader — `kubectl kopiur ls|cat|download|browse` and the web UI —
//! goes through one [`SnapshotAccess`] trait, so the path walk, the manifest
//! parsing, and the "is this a file or a directory" decisions are written once
//! and unit-tested against a fake:
//! - [`session::ExecSession`] (the default): a warm in-cluster mover Job holds
//!   a **read-only** repository connection; reads are pod-exec'd through the
//!   closed [`SessionCmd`](kopiur_kopia::SessionCmd) surface. Credentials never
//!   leave the cluster.
//! - the CLI's `--local` transport: a local kopia binary connects read-only
//!   from the user's machine. It lives in `kubectl kopiur` (it needs `get
//!   secrets` on the caller's own RBAC), and implements the same trait.

pub mod resolve;
pub mod session;

use std::future::Future;

use tokio::io::AsyncWrite;

use kopiur_kopia::{DirEntry, DirManifest, ObjectId, SessionCmd, SnapshotListEntry};

use crate::error::OpsError;

/// kopia's directory-manifest stream marker.
const DIR_STREAM: &str = "kopia:directory";

/// Transport-agnostic snapshot reads. Every transport implements this, so
/// `ls`/`cat`/`download`/`browse` (CLI or web UI) share one core, tested
/// against a fake.
///
/// The associated [`SnapshotAccess::Error`] lets a transport carry its own
/// error type (the CLI's `--local` path fails in ways an in-cluster session
/// cannot), while `From<OpsError>` guarantees the shared walk below can always
/// raise the shared, actionable failures. Futures are `Send` so a server can
/// drive a read from a multi-threaded runtime.
pub trait SnapshotAccess {
    /// How this transport fails. Must absorb [`OpsError`], which the shared
    /// walk raises.
    type Error: std::error::Error + Send + Sync + 'static + From<OpsError>;

    /// The root directory object id of `kopia_snapshot_id`, from the
    /// repository's own catalog. Already validated: an [`ObjectId`], not a
    /// string a caller has to re-parse (and could forget to).
    fn snapshot_root(
        &mut self,
        kopia_snapshot_id: &str,
    ) -> impl Future<Output = Result<ObjectId, Self::Error>> + Send;

    /// A directory object's manifest.
    fn list_dir(
        &mut self,
        oid: &ObjectId,
    ) -> impl Future<Output = Result<DirManifest, Self::Error>> + Send;

    /// Stream a file object's raw bytes into `sink`, returning the byte count.
    fn read_file(
        &mut self,
        oid: &ObjectId,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> impl Future<Output = Result<u64, Self::Error>> + Send;
}

/// Parse `kopia snapshot list --json --all` output and find the root oid of
/// `id`. Pure.
pub fn root_oid_from_list(bytes: &[u8], id: &str) -> Result<ObjectId, OpsError> {
    let entries: Vec<SnapshotListEntry> =
        serde_json::from_slice(bytes).map_err(|e| OpsError::UnexpectedKopiaOutput {
            what: "the repository snapshot list".to_string(),
            detail: format!("not valid snapshot-list JSON: {e}"),
        })?;
    let entry = entries
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| OpsError::SnapshotMissingInRepo { id: id.to_string() })?;
    match entry.root_entry {
        Some(root) if !root.obj.is_empty() => parse_oid(&root.obj),
        _ => Err(OpsError::UnexpectedKopiaOutput {
            what: format!("snapshot {id}"),
            detail: "the catalog entry carries no root object id".to_string(),
        }),
    }
}

/// Parse a `kopia show <dir-oid>` payload into a [`DirManifest`], verifying
/// the directory stream marker. Pure.
pub fn parse_dir_manifest(bytes: &[u8], oid: &ObjectId) -> Result<DirManifest, OpsError> {
    let manifest: DirManifest =
        serde_json::from_slice(bytes).map_err(|e| OpsError::UnexpectedKopiaOutput {
            what: format!("directory manifest {}", oid.as_str()),
            detail: format!("not a directory manifest: {e}"),
        })?;
    if manifest.stream != DIR_STREAM {
        return Err(OpsError::UnexpectedKopiaOutput {
            what: format!("directory manifest {}", oid.as_str()),
            detail: format!(
                "stream marker was {:?}, expected {DIR_STREAM:?}",
                manifest.stream
            ),
        });
    }
    Ok(manifest)
}

/// Validate an object id read out of a repository manifest before it reaches
/// the session argv. A manifest entry is untrusted input: a value starting with
/// `-` would otherwise be parsed by kopia as a flag.
pub fn parse_oid(oid: &str) -> Result<ObjectId, OpsError> {
    ObjectId::parse(oid).map_err(|e| OpsError::UnexpectedKopiaOutput {
        what: "a directory manifest entry".into(),
        detail: e.to_string(),
    })
}

impl SnapshotAccess for session::ExecSession {
    type Error = OpsError;

    async fn snapshot_root(&mut self, kopia_snapshot_id: &str) -> Result<ObjectId, OpsError> {
        let out = self.exec_capture(SessionCmd::SnapshotListJson).await?;
        root_oid_from_list(&out, kopia_snapshot_id)
    }

    async fn list_dir(&mut self, oid: &ObjectId) -> Result<DirManifest, OpsError> {
        let out = self
            .exec_capture(SessionCmd::ShowObject { oid: oid.clone() })
            .await?;
        parse_dir_manifest(&out, oid)
    }

    async fn read_file(
        &mut self,
        oid: &ObjectId,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<u64, OpsError> {
        self.exec_stream(SessionCmd::ShowObject { oid: oid.clone() }, sink)
            .await
    }
}

/// Split a user path into components, rejecting absolute paths and `..`
/// (paths are always relative to the snapshot root; there is nothing above
/// it). `.` and empty components (`a//b`) are skipped. Pure.
pub fn validate_rel_path(path: &str) -> Result<Vec<String>, OpsError> {
    if path.starts_with('/') {
        return Err(OpsError::InvalidPath {
            path: path.to_string(),
            reason: "absolute paths are not allowed".to_string(),
        });
    }
    let mut components = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(OpsError::InvalidPath {
                    path: path.to_string(),
                    reason: "`..` components are not allowed".to_string(),
                });
            }
            other => components.push(other.to_string()),
        }
    }
    Ok(components)
}

/// Where a walk landed: the snapshot root itself, or a named entry.
#[derive(Debug)]
pub enum Walked {
    /// The snapshot root directory (an empty path).
    Root {
        /// The root directory object id.
        oid: ObjectId,
    },
    /// A directory/file entry below the root.
    Entry {
        /// The full path walked, for messages.
        path: String,
        /// The manifest entry.
        entry: DirEntry,
    },
}

/// Walk `components` down from `root_oid`, one manifest level at a time.
/// Transport-agnostic and tested against a fake [`SnapshotAccess`].
pub async fn walk<A: SnapshotAccess + ?Sized>(
    access: &mut A,
    root_oid: &ObjectId,
    components: &[String],
) -> Result<Walked, A::Error> {
    let Some((last, parents)) = components.split_last() else {
        return Ok(Walked::Root {
            oid: root_oid.clone(),
        });
    };
    let mut dir_oid = root_oid.clone();
    let mut walked: Vec<&str> = Vec::new();
    for parent in parents {
        let manifest = access.list_dir(&dir_oid).await?;
        walked.push(parent);
        let entry = manifest
            .entries
            .into_iter()
            .find(|e| &e.name == parent)
            .ok_or_else(|| OpsError::PathNotFound {
                path: walked.join("/"),
            })?;
        if entry.entry_type != "d" {
            return Err(OpsError::NotADirectory {
                path: walked.join("/"),
                entry_type: entry.entry_type,
            }
            .into());
        }
        dir_oid = parse_oid(&entry.obj)?;
    }
    let manifest = access.list_dir(&dir_oid).await?;
    walked.push(last);
    let entry = manifest
        .entries
        .into_iter()
        .find(|e| &e.name == last)
        .ok_or_else(|| OpsError::PathNotFound {
            path: walked.join("/"),
        })?;
    Ok(Walked::Entry {
        path: walked.join("/"),
        entry,
    })
}

/// Walk to a path and list it: the root for an empty path, or a `d` entry's
/// object (a file is refused with the ls hint). Returns the directory's object
/// id alongside its manifest, so a caller that navigates (the REPL) can keep
/// the id without re-walking.
pub async fn walk_to_dir<A: SnapshotAccess + ?Sized>(
    access: &mut A,
    root_oid: &ObjectId,
    components: &[String],
) -> Result<(ObjectId, DirManifest), A::Error> {
    let oid = match walk(access, root_oid, components).await? {
        Walked::Root { oid } => oid,
        Walked::Entry { path, entry } => {
            if entry.entry_type == "d" {
                parse_oid(&entry.obj)?
            } else {
                return Err(OpsError::NotADirectory {
                    path,
                    entry_type: entry.entry_type,
                }
                .into());
            }
        }
    };
    let manifest = access.list_dir(&oid).await?;
    Ok((oid, manifest))
}

/// Walk to a path that must be a regular file, returning its validated object
/// id and manifest entry.
pub async fn walk_to_file<A: SnapshotAccess + ?Sized>(
    access: &mut A,
    root_oid: &ObjectId,
    components: &[String],
    original: &str,
) -> Result<(ObjectId, DirEntry), A::Error> {
    if components.is_empty() {
        return Err(OpsError::IsADirectory {
            path: original.to_string(),
        }
        .into());
    }
    match walk(access, root_oid, components).await? {
        Walked::Root { .. } => Err(OpsError::IsADirectory {
            path: original.to_string(),
        }
        .into()),
        Walked::Entry { path, entry } => match entry.entry_type.as_str() {
            "f" => {
                let oid = parse_oid(&entry.obj)?;
                Ok((oid, entry))
            }
            "d" => Err(OpsError::IsADirectory { path }.into()),
            other => Err(OpsError::NotAFile {
                path,
                entry_type: other.to_string(),
            }
            .into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tokio::io::AsyncWriteExt;

    /// A fake transport: a map of dir-oid → manifest and file-oid → bytes.
    struct FakeAccess {
        root: String,
        dirs: BTreeMap<String, DirManifest>,
        files: BTreeMap<String, Vec<u8>>,
    }

    impl SnapshotAccess for FakeAccess {
        type Error = OpsError;

        async fn snapshot_root(&mut self, _id: &str) -> Result<ObjectId, OpsError> {
            parse_oid(&self.root)
        }

        async fn list_dir(&mut self, oid: &ObjectId) -> Result<DirManifest, OpsError> {
            self.dirs
                .get(oid.as_str())
                .cloned()
                .ok_or_else(|| OpsError::UnexpectedKopiaOutput {
                    what: format!("dir {}", oid.as_str()),
                    detail: "fake: unknown dir oid".into(),
                })
        }

        async fn read_file(
            &mut self,
            oid: &ObjectId,
            sink: &mut (dyn AsyncWrite + Unpin + Send),
        ) -> Result<u64, OpsError> {
            let bytes = self.files.get(oid.as_str()).cloned().unwrap_or_default();
            sink.write_all(&bytes).await.unwrap();
            Ok(bytes.len() as u64)
        }
    }

    fn manifest(entries: serde_json::Value) -> DirManifest {
        serde_json::from_value(serde_json::json!({
            "stream": "kopia:directory",
            "entries": entries
        }))
        .unwrap()
    }

    /// root/
    ///   a.txt        (file, 16B)
    ///   sub/         (dir)
    ///     b.txt      (file, 11B)
    fn fake() -> FakeAccess {
        let root = manifest(serde_json::json!([
            { "name": "a.txt", "type": "f", "obj": "kfile-a", "size": 16,
              "mtime": "2026-06-11T01:02:03.123456789Z" },
            { "name": "sub", "type": "d", "obj": "kdir-sub",
              "summ": { "size": 11, "files": 1, "dirs": 1 } }
        ]));
        let sub = manifest(serde_json::json!([
            { "name": "b.txt", "type": "f", "obj": "kfile-b", "size": 11 }
        ]));
        FakeAccess {
            root: "kroot".into(),
            dirs: BTreeMap::from([("kroot".to_string(), root), ("kdir-sub".to_string(), sub)]),
            files: BTreeMap::from([
                ("kfile-a".to_string(), b"hello kopiur e2e".to_vec()),
                ("kfile-b".to_string(), b"nested data".to_vec()),
            ]),
        }
    }

    fn oid(s: &str) -> ObjectId {
        parse_oid(s).expect("test oid")
    }

    #[test]
    fn manifest_object_ids_from_real_repositories_pass_the_argv_gate() {
        // The gate fires on every id the walk follows, so a legitimate kopia id
        // that it rejected would break browsing outright. Dashes (entry-derived
        // ids), commas (section ids) and indirect ids must all pass; only a
        // flag-shaped or argv-hostile value is refused.
        for good in [
            "kfile-a",
            "kdir-sub",
            "kroot-other",
            "S12,34,kabc",
            "Ixdead",
        ] {
            assert_eq!(parse_oid(good).expect("valid oid").as_str(), good);
        }
        let err = parse_oid("--config-file=/x").unwrap_err();
        assert!(
            matches!(err, OpsError::UnexpectedKopiaOutput { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("--config-file=/x"), "{err}");
    }

    #[test]
    fn rel_path_validation_rejects_escapes_and_normalizes() {
        assert_eq!(validate_rel_path("").unwrap(), Vec::<String>::new());
        assert_eq!(validate_rel_path("a/b").unwrap(), vec!["a", "b"]);
        assert_eq!(validate_rel_path("./a//b/.").unwrap(), vec!["a", "b"]);
        assert!(matches!(
            validate_rel_path("/etc/passwd"),
            Err(OpsError::InvalidPath { .. })
        ));
        assert!(matches!(
            validate_rel_path("a/../b"),
            Err(OpsError::InvalidPath { .. })
        ));
    }

    #[tokio::test]
    async fn walk_finds_nested_entries_and_reports_missing_paths() {
        let mut access = fake();
        let root = oid("kroot");
        // Root.
        let Walked::Root { oid } = walk(&mut access, &root, &[]).await.unwrap() else {
            panic!("empty path walks to the root");
        };
        assert_eq!(oid.as_str(), "kroot");
        // Nested file.
        let comps = validate_rel_path("sub/b.txt").unwrap();
        let Walked::Entry { path, entry } = walk(&mut access, &root, &comps).await.unwrap() else {
            panic!("expected an entry");
        };
        assert_eq!(path, "sub/b.txt");
        assert_eq!(entry.obj, "kfile-b");
        // Missing leaf and missing parent both name the walked path.
        let missing = validate_rel_path("sub/nope").unwrap();
        let err = walk(&mut access, &root, &missing).await.unwrap_err();
        assert!(matches!(err, OpsError::PathNotFound { ref path } if path == "sub/nope"));
        let missing = validate_rel_path("nope/deep").unwrap();
        let err = walk(&mut access, &root, &missing).await.unwrap_err();
        assert!(matches!(err, OpsError::PathNotFound { ref path } if path == "nope"));
        // Walking *through* a file is refused — with the NOT-a-directory
        // variant (the file IS a file; the problem is it isn't a directory).
        let through = validate_rel_path("a.txt/x").unwrap();
        assert!(matches!(
            walk(&mut access, &root, &through).await.unwrap_err(),
            OpsError::NotADirectory { .. }
        ));
    }

    #[tokio::test]
    async fn walk_to_dir_lists_the_root_and_a_named_directory() {
        let mut access = fake();
        let root = oid("kroot");
        let (dir, manifest) = walk_to_dir(&mut access, &root, &[]).await.unwrap();
        assert_eq!(dir.as_str(), "kroot");
        assert_eq!(manifest.entries.len(), 2);
        let comps = validate_rel_path("sub").unwrap();
        let (dir, manifest) = walk_to_dir(&mut access, &root, &comps).await.unwrap();
        assert_eq!(dir.as_str(), "kdir-sub");
        assert_eq!(manifest.entries[0].name, "b.txt");
        // A file is refused with the entry type named.
        let comps = validate_rel_path("a.txt").unwrap();
        let err = walk_to_dir(&mut access, &root, &comps).await.unwrap_err();
        assert!(matches!(err, OpsError::NotADirectory { ref path, .. } if path == "a.txt"));
    }

    #[tokio::test]
    async fn walk_to_file_refuses_directories_and_the_root() {
        let mut access = fake();
        let root = oid("kroot");
        let err = walk_to_file(&mut access, &root, &[], "").await.unwrap_err();
        assert!(matches!(err, OpsError::IsADirectory { .. }));
        let comps = validate_rel_path("sub").unwrap();
        let err = walk_to_file(&mut access, &root, &comps, "sub")
            .await
            .unwrap_err();
        assert!(matches!(err, OpsError::IsADirectory { ref path } if path == "sub"));
        // A regular file returns its validated oid and its entry.
        let comps = validate_rel_path("sub/b.txt").unwrap();
        let (oid, entry) = walk_to_file(&mut access, &root, &comps, "sub/b.txt")
            .await
            .unwrap();
        assert_eq!(oid.as_str(), "kfile-b");
        assert_eq!(entry.size, Some(11));
    }

    #[test]
    fn snapshot_list_root_resolution_finds_the_id_or_says_its_gone() {
        let list = serde_json::json!([
            {
                "id": "kother",
                "source": { "host": "h", "userName": "u", "path": "/d" },
                "startTime": "2026-06-01T00:00:00Z",
                "endTime": "2026-06-01T00:00:01Z",
                "rootEntry": { "name": "d", "type": "d", "obj": "kroot-other" }
            },
            {
                "id": "kwanted",
                "source": { "host": "h", "userName": "u", "path": "/d" },
                "startTime": "2026-06-02T00:00:00Z",
                "endTime": "2026-06-02T00:00:01Z",
                "rootEntry": { "name": "d", "type": "d", "obj": "kroot-wanted" }
            }
        ]);
        let bytes = serde_json::to_vec(&list).unwrap();
        assert_eq!(
            root_oid_from_list(&bytes, "kwanted").unwrap().as_str(),
            "kroot-wanted"
        );
        assert!(matches!(
            root_oid_from_list(&bytes, "kgone").unwrap_err(),
            OpsError::SnapshotMissingInRepo { .. }
        ));
        assert!(matches!(
            root_oid_from_list(b"not json", "k").unwrap_err(),
            OpsError::UnexpectedKopiaOutput { .. }
        ));
    }

    #[test]
    fn dir_manifest_parsing_checks_the_stream_marker() {
        let good = serde_json::to_vec(&serde_json::json!({
            "stream": "kopia:directory",
            "entries": [{ "name": "x", "type": "f", "obj": "k1", "size": 1 }]
        }))
        .unwrap();
        assert_eq!(
            parse_dir_manifest(&good, &oid("kdir"))
                .unwrap()
                .entries
                .len(),
            1
        );
        let wrong = serde_json::to_vec(&serde_json::json!({
            "stream": "kopia:other", "entries": []
        }))
        .unwrap();
        let msg = parse_dir_manifest(&wrong, &oid("kdir"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("kopia:other"), "{msg}");
    }
}
