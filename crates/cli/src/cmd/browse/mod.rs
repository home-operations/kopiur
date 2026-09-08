//! `kubectl kopiur ls|cat|download|browse|session end` — the read-only
//! snapshot data-plane.
//!
//! The reads themselves — the [`SnapshotAccess`] trait, the path walk, the
//! manifest parsing — live in [`kopiur_ops::browse`], shared with the web UI.
//! What lives here is the CLI's own surface: the two transports it can choose
//! between, the rendering, the REPL, and writing files to the user's machine.
//! - [`session::ExecSession`] (default): a warm in-cluster mover Job holds a
//!   **read-only** repository connection; reads are pod-exec'd through the
//!   closed [`SessionCmd`] surface. Credentials never leave the cluster.
//! - [`local::LocalSession`] (`--local`): a local kopia binary connects
//!   read-only from this machine (credentials are fetched here; needs
//!   `get secrets` + backend reachability).

pub mod local;
pub mod resolve;
pub mod session;

use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use kopiur_kopia::{DirEntry, DirManifest, ObjectId, SessionCmd};
use kopiur_ops::OpsError;
use kopiur_ops::browse::{
    SnapshotAccess, parse_dir_manifest, parse_oid, root_oid_from_list, validate_rel_path,
    walk_to_dir, walk_to_file,
};

use crate::CmdOutput;
use crate::cli::{BrowseArgs, BrowseCommonArgs, CatArgs, DownloadArgs, LsArgs, SessionEndArgs};
use crate::context::{KubeCtx, Scope};
use crate::error::CliError;
use crate::output::{EMPTY_CELL, OutputFormat, Table, human_bytes};

impl SnapshotAccess for local::LocalSession {
    type Error = CliError;

    async fn snapshot_root(&mut self, kopia_snapshot_id: &str) -> Result<String, CliError> {
        let out = self.run_capture(SessionCmd::SnapshotListJson).await?;
        Ok(root_oid_from_list(&out, kopia_snapshot_id)?
            .as_str()
            .to_string())
    }

    async fn list_dir(&mut self, oid: &ObjectId) -> Result<DirManifest, CliError> {
        let out = self
            .run_capture(SessionCmd::ShowObject { oid: oid.clone() })
            .await?;
        Ok(parse_dir_manifest(&out, oid)?)
    }

    async fn read_file(
        &mut self,
        oid: &ObjectId,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<u64, CliError> {
        self.run_stream(SessionCmd::ShowObject { oid: oid.clone() }, sink)
            .await
    }
}

/// The selected transport. Exhaustive: a new transport must implement every
/// read before it compiles.
pub enum Transport {
    /// The in-cluster warm session pod (the default).
    Session(session::ExecSession),
    /// A local kopia binary (`--local`).
    Local(local::LocalSession),
}

impl SnapshotAccess for Transport {
    type Error = CliError;

    async fn snapshot_root(&mut self, id: &str) -> Result<String, CliError> {
        match self {
            Transport::Session(s) => Ok(s.snapshot_root(id).await?),
            Transport::Local(l) => l.snapshot_root(id).await,
        }
    }

    async fn list_dir(&mut self, oid: &ObjectId) -> Result<DirManifest, CliError> {
        match self {
            Transport::Session(s) => Ok(s.list_dir(oid).await?),
            Transport::Local(l) => l.list_dir(oid).await,
        }
    }

    async fn read_file(
        &mut self,
        oid: &ObjectId,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<u64, CliError> {
        match self {
            Transport::Session(s) => Ok(s.read_file(oid, sink).await?),
            Transport::Local(l) => l.read_file(oid, sink).await,
        }
    }
}

/// Open the transport the flags select.
async fn open_transport(
    ctx: &KubeCtx,
    common: &BrowseCommonArgs,
    target: &resolve::BrowseTarget,
) -> Result<Transport, CliError> {
    if common.local {
        Ok(Transport::Local(
            local::LocalSession::connect(ctx, target, common.kopia_bin.as_deref()).await?,
        ))
    } else {
        Ok(Transport::Session(
            session::ExecSession::ensure(
                ctx,
                target,
                common.ttl(),
                // Resolve-only: there is no `--image` flag, so a session always
                // runs the mover image the operator runs.
                &session::MoverImageSource::DiscoverFromControllerDeployment,
                &session::StderrProgress::for_repository(&target.repo.name),
            )
            .await?,
        ))
    }
}

/// The snapshot root, as the validated object id the walk needs.
async fn snapshot_root_oid<A: SnapshotAccess + ?Sized>(
    access: &mut A,
    kopia_snapshot_id: &str,
) -> Result<ObjectId, A::Error> {
    let root = access.snapshot_root(kopia_snapshot_id).await?;
    Ok(parse_oid(&root)?)
}

/// A directory entry's size for the table: a file's own size, or a directory
/// subtree's aggregate. Pure.
fn entry_size(entry: &DirEntry) -> Option<i64> {
    entry
        .size
        .or_else(|| entry.summ.as_ref().and_then(|s| s.size))
}

/// Trim a kopia mtime (RFC3339 with nanoseconds) to a stable, second-precision
/// display form; unparseable values pass through verbatim. Pure.
fn format_mtime(mtime: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(mtime) {
        Ok(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        Err(_) => mtime.to_string(),
    }
}

/// Human entry-type label. kopia's single letters are kept recognizable but
/// spelled out for the table. Pure.
fn entry_type_label(t: &str) -> String {
    match t {
        "d" => "dir".to_string(),
        "f" => "file".to_string(),
        "s" => "symlink".to_string(),
        other => other.to_string(),
    }
}

/// Render a directory manifest per the output format. Exhaustive over
/// [`OutputFormat`]. Pure.
pub fn render_manifest(manifest: &DirManifest, format: OutputFormat) -> Result<String, CliError> {
    match format {
        OutputFormat::Table | OutputFormat::Wide => {
            if manifest.entries.is_empty() {
                return Ok("(empty directory)\n".to_string());
            }
            let wide = format == OutputFormat::Wide;
            let mut headers = vec!["NAME", "TYPE", "SIZE", "MODIFIED"];
            if wide {
                headers.push("OBJECT");
            }
            let mut table = Table::new(headers);
            for e in &manifest.entries {
                let name = if e.entry_type == "d" {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                };
                let mut row = vec![
                    name,
                    entry_type_label(&e.entry_type),
                    entry_size(e)
                        .map(human_bytes)
                        .unwrap_or_else(|| EMPTY_CELL.to_string()),
                    e.mtime
                        .as_deref()
                        .map(format_mtime)
                        .unwrap_or_else(|| EMPTY_CELL.to_string()),
                ];
                if wide {
                    row.push(e.obj.clone());
                }
                table.push(row);
            }
            Ok(table.render())
        }
        // The manifest verbatim — what `kopia show` emitted, machine-readable.
        OutputFormat::Json => serde_json::to_string_pretty(manifest)
            .map(|s| s + "\n")
            .map_err(|e| {
                OpsError::Serialization {
                    what: "directory manifest",
                    source: e.into(),
                }
                .into()
            }),
        OutputFormat::Yaml => serde_yaml::to_string(manifest).map_err(|e| {
            OpsError::Serialization {
                what: "directory manifest",
                source: e.into(),
            }
            .into()
        }),
        // `-o name`: bare entry names, one per line (dirs keep the `/` marker).
        OutputFormat::Name => Ok(manifest
            .entries
            .iter()
            .map(|e| {
                if e.entry_type == "d" {
                    format!("{}/\n", e.name)
                } else {
                    format!("{}\n", e.name)
                }
            })
            .collect()),
    }
}

/// Refuse `-A` for the single-object browse commands.
fn reject_all_namespaces(ctx: &KubeCtx, command: &'static str) -> Result<(), CliError> {
    match &ctx.scope {
        Scope::All => Err(CliError::AllNamespacesNotApplicable { command }),
        Scope::Namespace(_) => Ok(()),
    }
}

/// `kubectl kopiur ls <SNAPSHOT> [PATH]`.
pub async fn ls(ctx: &KubeCtx, args: &LsArgs, output: OutputFormat) -> Result<CmdOutput, CliError> {
    reject_all_namespaces(ctx, "ls")?;
    let components = validate_rel_path(args.path.as_deref().unwrap_or(""))?;
    let target = resolve::resolve(ctx, &ctx.namespace, &args.common.snapshot).await?;
    let mut access = open_transport(ctx, &args.common, &target).await?;
    let root = snapshot_root_oid(&mut access, &target.kopia_snapshot_id).await?;
    let (_oid, manifest) = walk_to_dir(&mut access, &root, &components).await?;
    Ok(CmdOutput::ok(render_manifest(&manifest, output)?))
}

/// `kubectl kopiur cat <SNAPSHOT> <PATH>` — stream one file to stdout.
pub async fn cat(ctx: &KubeCtx, args: &CatArgs) -> Result<CmdOutput, CliError> {
    reject_all_namespaces(ctx, "cat")?;
    let components = validate_rel_path(&args.path)?;
    let target = resolve::resolve(ctx, &ctx.namespace, &args.common.snapshot).await?;
    let mut access = open_transport(ctx, &args.common, &target).await?;
    let root = snapshot_root_oid(&mut access, &target.kopia_snapshot_id).await?;
    let (oid, _entry) = walk_to_file(&mut access, &root, &components, &args.path).await?;
    let mut stdout = tokio::io::stdout();
    access.read_file(&oid, &mut stdout).await?;
    stdout.flush().await.map_err(|source| CliError::LocalIo {
        what: "flushing stdout".to_string(),
        source,
    })?;
    Ok(CmdOutput {
        text: String::new(),
        exit: 0,
    })
}

/// `kubectl kopiur download <SNAPSHOT> <PATH> [DEST]` — write one file
/// locally, verifying the byte count against the manifest.
pub async fn download(ctx: &KubeCtx, args: &DownloadArgs) -> Result<CmdOutput, CliError> {
    reject_all_namespaces(ctx, "download")?;
    let components = validate_rel_path(&args.path)?;
    let target = resolve::resolve(ctx, &ctx.namespace, &args.common.snapshot).await?;
    let mut access = open_transport(ctx, &args.common, &target).await?;
    let root = snapshot_root_oid(&mut access, &target.kopia_snapshot_id).await?;
    let (oid, entry) = walk_to_file(&mut access, &root, &components, &args.path).await?;

    let dest: PathBuf = match &args.dest {
        Some(d) => d.clone(),
        // Default: the file's own name in the current directory.
        None => PathBuf::from(
            components
                .last()
                .expect("walk_to_file rejects empty paths")
                .clone(),
        ),
    };
    eprintln!("downloading {} to {}…", args.path, dest.display());
    let written = download_to_file(&mut access, &oid, &entry, &args.path, &dest).await?;
    Ok(CmdOutput::ok(format!(
        "wrote {written} bytes to {}\n",
        dest.display()
    )))
}

/// Stream `entry` into `dest`, verifying the byte count against the manifest
/// size when present. A mismatch removes the partial file and errors.
async fn download_to_file<A: SnapshotAccess<Error = CliError> + ?Sized>(
    access: &mut A,
    oid: &ObjectId,
    entry: &DirEntry,
    path: &str,
    dest: &Path,
) -> Result<u64, CliError> {
    // Stream into a sibling `.part` file and rename on success: a failed or
    // short download must never destroy an existing file at `dest`.
    let part = dest.with_extension(match dest.extension() {
        Some(ext) => format!("{}.part", ext.to_string_lossy()),
        None => "part".to_string(),
    });
    let mut file = tokio::fs::File::create(&part)
        .await
        .map_err(|source| CliError::LocalIo {
            what: format!("creating {}", part.display()),
            source,
        })?;
    let cleanup_part = |part: std::path::PathBuf| async move {
        let _ = tokio::fs::remove_file(&part).await;
    };
    let written = match access.read_file(oid, &mut file).await {
        Ok(n) => n,
        Err(e) => {
            // Never leave a partial file behind on a failed stream.
            drop(file);
            cleanup_part(part).await;
            return Err(e);
        }
    };
    if let Err(source) = file.flush().await {
        drop(file);
        cleanup_part(part.clone()).await;
        return Err(CliError::LocalIo {
            what: format!("flushing {}", part.display()),
            source,
        });
    }
    drop(file);
    if let Some(expected) = entry.size
        && expected >= 0
        && written != expected as u64
    {
        cleanup_part(part).await;
        return Err(CliError::DownloadIncomplete {
            path: path.to_string(),
            expected,
            actual: written,
            dest: dest.display().to_string(),
        });
    }
    tokio::fs::rename(&part, dest)
        .await
        .map_err(|source| CliError::LocalIo {
            what: format!("renaming {} to {}", part.display(), dest.display()),
            source,
        })?;
    Ok(written)
}

// --- the interactive browse REPL -------------------------------------------

/// REPL navigation state: the root oid plus the stack of entered directories.
pub struct ReplState {
    root_oid: ObjectId,
    /// `(name, oid)` of each directory below the root, in order.
    stack: Vec<(String, ObjectId)>,
}

/// What one REPL step decided.
#[derive(Debug)]
pub enum ReplOutcome {
    /// Keep reading commands.
    Continue,
    /// Leave the REPL.
    Quit,
}

/// The REPL help text.
const REPL_HELP: &str = "commands:\n  ls            list the current directory\n  cd <dir>      enter a directory (cd .. to go up)\n  cat <file>    print a file to stdout\n  get <file> [dest]  download a file\n  pwd           print the current path\n  help          this help\n  quit          leave (also: exit, q, ctrl-d)\n";

impl ReplState {
    /// Start at the snapshot root.
    pub fn new(root_oid: ObjectId) -> Self {
        ReplState {
            root_oid,
            stack: Vec::new(),
        }
    }

    /// The current path, `/`-rooted for display.
    pub fn pwd(&self) -> String {
        if self.stack.is_empty() {
            "/".to_string()
        } else {
            format!(
                "/{}",
                self.stack
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            )
        }
    }

    /// The current directory's object id.
    fn cwd_oid(&self) -> &ObjectId {
        self.stack
            .last()
            .map(|(_, oid)| oid)
            .unwrap_or(&self.root_oid)
    }

    /// Handle one command line. Textual results (listings, messages) land in
    /// `out`; `cat` streams into `file_sink` (stdout in the real REPL, a
    /// buffer in tests). Errors are returned so the caller can print them
    /// WITHOUT leaving the REPL.
    pub async fn step<A: SnapshotAccess<Error = CliError> + ?Sized>(
        &mut self,
        access: &mut A,
        line: &str,
        out: &mut String,
        file_sink: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<ReplOutcome, CliError> {
        let mut parts = line.split_whitespace();
        let Some(cmd) = parts.next() else {
            return Ok(ReplOutcome::Continue);
        };
        let arg1 = parts.next();
        let arg2 = parts.next();
        match cmd {
            "quit" | "exit" | "q" => return Ok(ReplOutcome::Quit),
            "help" | "?" => out.push_str(REPL_HELP),
            "pwd" => {
                out.push_str(&self.pwd());
                out.push('\n');
            }
            "ls" => {
                let manifest = match arg1 {
                    // `ls <dir>` lists without entering.
                    Some(path) => {
                        let comps = self.resolve_repl_path(path)?;
                        walk_to_dir(access, &self.root_oid, &comps).await?.1
                    }
                    None => access.list_dir(self.cwd_oid()).await?,
                };
                out.push_str(&render_manifest(&manifest, OutputFormat::Table)?);
            }
            "cd" => {
                let target = arg1.unwrap_or("");
                if target.is_empty() {
                    self.stack.clear(); // bare `cd` → root, like a shell's ~
                } else {
                    self.cd(access, target).await?;
                }
            }
            "cat" => {
                let Some(path) = arg1 else {
                    out.push_str("usage: cat <file>\n");
                    return Ok(ReplOutcome::Continue);
                };
                let comps = self.resolve_repl_path(path)?;
                let (oid, _entry) = walk_to_file(access, &self.root_oid, &comps, path).await?;
                access.read_file(&oid, file_sink).await?;
            }
            "get" => {
                let Some(path) = arg1 else {
                    out.push_str("usage: get <file> [dest]\n");
                    return Ok(ReplOutcome::Continue);
                };
                let comps = self.resolve_repl_path(path)?;
                let (oid, entry) = walk_to_file(access, &self.root_oid, &comps, path).await?;
                let dest = arg2.map(PathBuf::from).unwrap_or_else(|| {
                    PathBuf::from(comps.last().expect("non-empty file path").clone())
                });
                let written = download_to_file(access, &oid, &entry, path, &dest).await?;
                out.push_str(&format!("wrote {written} bytes to {}\n", dest.display()));
            }
            other => {
                out.push_str(&format!("unknown command {other:?} — try `help`\n"));
            }
        }
        Ok(ReplOutcome::Continue)
    }

    /// Enter a (possibly multi-component) directory path. `..` pops one level
    /// — REPL navigation can go *up*, but never above the root.
    async fn cd<A: SnapshotAccess<Error = CliError> + ?Sized>(
        &mut self,
        access: &mut A,
        path: &str,
    ) -> Result<(), CliError> {
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    // Above the root there is nothing; popping at root is a no-op.
                    self.stack.pop();
                }
                name => {
                    let manifest = access.list_dir(self.cwd_oid()).await?;
                    let entry = manifest
                        .entries
                        .into_iter()
                        .find(|e| e.name == name)
                        .ok_or_else(|| OpsError::PathNotFound {
                            path: name.to_string(),
                        })?;
                    if entry.entry_type != "d" {
                        return Err(OpsError::NotADirectory {
                            path: name.to_string(),
                            entry_type: entry.entry_type,
                        }
                        .into());
                    }
                    self.stack.push((name.to_string(), parse_oid(&entry.obj)?));
                }
            }
        }
        Ok(())
    }

    /// A REPL-relative path → root-relative components (current stack + the
    /// validated relative path). `..` is still refused inside ls/cat/get
    /// arguments — use `cd ..` to navigate up.
    fn resolve_repl_path(&self, path: &str) -> Result<Vec<String>, CliError> {
        let rel = validate_rel_path(path)?;
        let mut comps: Vec<String> = self.stack.iter().map(|(n, _)| n.clone()).collect();
        comps.extend(rel);
        Ok(comps)
    }
}

/// `kubectl kopiur browse <SNAPSHOT>` — the interactive REPL.
pub async fn browse(ctx: &KubeCtx, args: &BrowseArgs) -> Result<CmdOutput, CliError> {
    reject_all_namespaces(ctx, "browse")?;
    let target = resolve::resolve(ctx, &ctx.namespace, &args.common.snapshot).await?;
    let mut access = open_transport(ctx, &args.common, &target).await?;
    let root = snapshot_root_oid(&mut access, &target.kopia_snapshot_id).await?;
    let mut state = ReplState::new(root);

    eprintln!(
        "browsing snapshot {} (kopia {}) — read-only; `help` lists commands, `quit` leaves",
        target.snapshot, target.kopia_snapshot_id
    );
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        // Prompt on stderr so piped stdout stays clean file/listing data.
        eprint!("kopiur:{}> ", state.pwd());
        let Some(line) = lines
            .next_line()
            .await
            .map_err(|source| CliError::LocalIo {
                what: "reading from stdin".to_string(),
                source,
            })?
        else {
            break; // EOF (ctrl-d)
        };
        let mut out = String::new();
        let mut stdout = tokio::io::stdout();
        match state.step(&mut access, &line, &mut out, &mut stdout).await {
            Ok(ReplOutcome::Continue) => {
                let _ = stdout.flush().await;
                print!("{out}");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
            }
            Ok(ReplOutcome::Quit) => break,
            // A failed step (bad path, read error) is printed, not fatal.
            Err(e) => eprintln!("error: {e}"),
        }
    }

    // Session lifecycle on exit: end it unless --keep (--local has no session).
    let text = match (&access, args.keep) {
        (Transport::Session(s), false) => {
            session::delete_session(ctx, &s.namespace, &s.job_name).await?;
            format!("session {} ended\n", s.job_name)
        }
        (Transport::Session(s), true) => format!(
            "session {} kept warm (expires after its TTL; end it early with \
             `kubectl kopiur session end {}`)\n",
            s.job_name, target.snapshot
        ),
        (Transport::Local(_), _) => String::new(),
    };
    Ok(CmdOutput::ok(text))
}

/// `kubectl kopiur session end <SNAPSHOT|--repository NAME>`.
pub async fn session_end(ctx: &KubeCtx, args: &SessionEndArgs) -> Result<CmdOutput, CliError> {
    reject_all_namespaces(ctx, "session end")?;
    // Resolve which repository's session to end. clap enforces exactly one
    // selector; the match stays exhaustive over the two.
    let (kind, repo_namespace, repo_name) = match (&args.snapshot, &args.repository) {
        (Some(snapshot), None) => {
            let target = resolve::resolve(ctx, &ctx.namespace, snapshot).await?;
            (
                target.repo.kind,
                target.repo.namespace.clone(),
                target.repo.name,
            )
        }
        (None, Some(repository)) => {
            let kind = args.repository_kind.into();
            let repo = resolve::resolve_repo(ctx, kind, repository).await?;
            (repo.kind, repo.namespace.clone(), repo.name)
        }
        // Unreachable thanks to the clap group, but the match stays total.
        _ => {
            return Err(OpsError::AmbiguousTarget {
                what: "session end needs exactly one of SNAPSHOT or --repository".to_string(),
                candidates: "pass one of them".to_string(),
            }
            .into());
        }
    };

    let ns = ctx.namespace.clone();
    match session::find_session_job(ctx, &ns, kind, repo_namespace.as_deref(), &repo_name).await? {
        Some(job) => {
            let job_name = kube::ResourceExt::name_any(&job);
            session::delete_session(ctx, &ns, &job_name).await?;
            Ok(CmdOutput::ok(format!(
                "session {job_name} ended (Job deleted)\n"
            )))
        }
        None => Ok(CmdOutput::ok(format!(
            "no browse session is open for {} {repo_name} in namespace {ns} — nothing to end\n",
            match kind {
                kopiur_api::common::RepositoryKind::Repository => "Repository",
                kopiur_api::common::RepositoryKind::ClusterRepository => "ClusterRepository",
            }
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A fake transport: a map of dir-oid → manifest and file-oid → bytes.
    /// The shared walk is tested against `kopiur_ops`' own fake; this one
    /// carries the CLI's error type, so the REPL and the download path (which
    /// raise CLI-only failures) are testable without a cluster.
    struct FakeAccess {
        root: String,
        dirs: BTreeMap<String, DirManifest>,
        files: BTreeMap<String, Vec<u8>>,
    }

    impl SnapshotAccess for FakeAccess {
        type Error = CliError;

        async fn snapshot_root(&mut self, _id: &str) -> Result<String, CliError> {
            Ok(self.root.clone())
        }

        async fn list_dir(&mut self, oid: &ObjectId) -> Result<DirManifest, CliError> {
            self.dirs.get(oid.as_str()).cloned().ok_or_else(|| {
                OpsError::UnexpectedKopiaOutput {
                    what: format!("dir {}", oid.as_str()),
                    detail: "fake: unknown dir oid".into(),
                }
                .into()
            })
        }

        async fn read_file(
            &mut self,
            oid: &ObjectId,
            sink: &mut (dyn AsyncWrite + Unpin + Send),
        ) -> Result<u64, CliError> {
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

    fn oid(s: &str) -> ObjectId {
        parse_oid(s).expect("test oid")
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

    #[test]
    fn ls_table_renders_name_type_size_modified_and_wide_adds_object() {
        let m = fake().dirs["kroot"].clone();
        let table = render_manifest(&m, OutputFormat::Table).unwrap();
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].starts_with("NAME"), "{table}");
        assert!(
            lines[0].contains("TYPE") && lines[0].contains("SIZE"),
            "{table}"
        );
        assert!(lines[0].contains("MODIFIED") && !lines[0].contains("OBJECT"));
        let a = lines.iter().find(|l| l.starts_with("a.txt")).unwrap();
        assert!(a.contains("file") && a.contains("16 B"), "{a}");
        // mtime trimmed to seconds.
        assert!(a.contains("2026-06-11 01:02:03"), "{a}");
        // Directory row: trailing slash, aggregate size from summ.
        let sub = lines.iter().find(|l| l.starts_with("sub/")).unwrap();
        assert!(sub.contains("dir") && sub.contains("11 B"), "{sub}");

        let wide = render_manifest(&m, OutputFormat::Wide).unwrap();
        assert!(wide.lines().next().unwrap().contains("OBJECT"), "{wide}");
        assert!(
            wide.contains("kfile-a") && wide.contains("kdir-sub"),
            "{wide}"
        );
    }

    #[test]
    fn ls_json_emits_the_manifest_verbatim_and_name_lists_names() {
        let m = fake().dirs["kroot"].clone();
        let json = render_manifest(&m, OutputFormat::Json).unwrap();
        let parsed: DirManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, m, "-o json is the manifest verbatim");
        let names = render_manifest(&m, OutputFormat::Name).unwrap();
        assert_eq!(names, "a.txt\nsub/\n");
        // Empty dir table output is honest, not a bare header.
        let empty = manifest(serde_json::json!([]));
        assert_eq!(
            render_manifest(&empty, OutputFormat::Table).unwrap(),
            "(empty directory)\n"
        );
    }

    #[tokio::test]
    async fn repl_navigates_lists_and_reads() {
        let mut access = fake();
        let mut state = ReplState::new(oid("kroot"));
        let mut sink: Vec<u8> = Vec::new();

        // pwd at root.
        let mut out = String::new();
        state
            .step(&mut access, "pwd", &mut out, &mut sink)
            .await
            .unwrap();
        assert_eq!(out, "/\n");

        // cd into sub, pwd reflects it, ls shows b.txt.
        let mut out = String::new();
        state
            .step(&mut access, "cd sub", &mut out, &mut sink)
            .await
            .unwrap();
        state
            .step(&mut access, "pwd", &mut out, &mut sink)
            .await
            .unwrap();
        assert!(out.contains("/sub"), "{out}");
        let mut out = String::new();
        state
            .step(&mut access, "ls", &mut out, &mut sink)
            .await
            .unwrap();
        assert!(out.contains("b.txt"), "{out}");

        // cat is REPL-relative and streams to the sink.
        state
            .step(&mut access, "cat b.txt", &mut String::new(), &mut sink)
            .await
            .unwrap();
        assert_eq!(sink, b"nested data");

        // cd .. pops; popping at the root stays at the root.
        let mut out = String::new();
        state
            .step(&mut access, "cd ..", &mut out, &mut sink)
            .await
            .unwrap();
        assert_eq!(state.pwd(), "/");
        state
            .step(&mut access, "cd ..", &mut out, &mut sink)
            .await
            .unwrap();
        assert_eq!(state.pwd(), "/", "the REPL can never climb above the root");

        // A bad path errors WITHOUT quitting (the loop prints and continues).
        let err = state
            .step(&mut access, "cat nope.txt", &mut String::new(), &mut sink)
            .await
            .unwrap_err();
        assert!(matches!(err, CliError::Ops(OpsError::PathNotFound { .. })));

        // quit / unknown commands.
        let mut out = String::new();
        assert!(matches!(
            state
                .step(&mut access, "frobnicate", &mut out, &mut sink)
                .await
                .unwrap(),
            ReplOutcome::Continue
        ));
        assert!(out.contains("unknown command"), "{out}");
        assert!(matches!(
            state
                .step(&mut access, "quit", &mut out, &mut sink)
                .await
                .unwrap(),
            ReplOutcome::Quit
        ));
    }

    #[tokio::test]
    async fn download_verifies_the_byte_count_and_removes_partials() {
        let mut access = fake();
        // Lie about the size: the manifest says 999, the stream yields 16.
        let entry: DirEntry = serde_json::from_value(serde_json::json!({
            "name": "a.txt", "type": "f", "obj": "kfile-a", "size": 999
        }))
        .unwrap();
        let dest =
            std::env::temp_dir().join(format!("kopiur-dl-test-{}-a.txt", std::process::id()));
        let err = download_to_file(&mut access, &oid("kfile-a"), &entry, "a.txt", &dest)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                CliError::DownloadIncomplete {
                    expected: 999,
                    actual: 16,
                    ..
                }
            ),
            "{err}"
        );
        assert!(!dest.exists(), "the partial file must be removed");

        // Honest size: the file lands with the right bytes.
        let entry: DirEntry = serde_json::from_value(serde_json::json!({
            "name": "a.txt", "type": "f", "obj": "kfile-a", "size": 16
        }))
        .unwrap();
        let n = download_to_file(&mut access, &oid("kfile-a"), &entry, "a.txt", &dest)
            .await
            .unwrap();
        assert_eq!(n, 16);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello kopiur e2e");
        let _ = std::fs::remove_file(&dest);
    }
}
