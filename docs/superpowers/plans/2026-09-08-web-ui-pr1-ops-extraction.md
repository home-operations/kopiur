# Web UI PR1 — `kopiur-ops` extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move every cross-CRD join, matcher, report builder, browse core and action builder that the future web UI needs out of `crates/cli` into a new shared `crates/ops` crate, with the CLI staying behavior-identical.

**Architecture:** `kopiur-ops` sits between `kopiur-api`/`kopiur-kopia`/`kopiur-mover` and the two consumers (`kopiur-cli` today, `kopiur-ui` in PR2). It owns `OpsError` (the moved what/why/fix variants), `OpsCtx` (the moved `KubeCtx`), and one module per concern. The CLI keeps clap, kubeconfig resolution, rendering, `--local` transport, logs, migrate. Tests move with their code; message texts are unchanged.

**Tech Stack:** Rust 1.98 (mise), kube 4 (`client, runtime, ws, rustls-tls`), thiserror, tokio, serde. No new third-party crates in `kopiur-ops` beyond what `kopiur-cli` already pulls (it is cross-built for musl/darwin).

**Spec:** `/home/perf3ct/.claude/plans/please-help-me-plan-floating-marble.md` (sections "Architecture", "PR1"). Read it first.

## Global Constraints

- `kopiur-api` stays free of `tokio`/`kube::Client` (unchanged).
- `kopiur-ops` may depend only on: `kopiur-api`, `kopiur-kopia`, `kopiur-mover`, `kube` (features `client, runtime, rustls-tls, ws`), `k8s-openapi` (`v1_33`), `futures`, `serde`, `serde_json`, `tokio`, `thiserror`, `chrono`, `tracing`. Nothing else.
- Every moved error message is byte-identical; existing text tests move and must pass unchanged (they assert the literal `--local` token in browse fixes — keep it).
- No `_ =>` or `if let` over phase enums in new code; `mise run phase-check` and `wiring-check` must stay green, and `crates/xtask/src/{phases,wiring}.rs` scan lists must include `"ops"`.
- `mise run complexity-check` (BUDGET=29, workspace-wide) must not grow: moving code keeps its count; do not add new offenders.
- Commit on the current branch after every task with a conventional-commit message. Never push.
- Verification command for every task: `mise run test` (hermetic) and `mise run clippy`; grep the output for `FAILED`/`error` — never pipe to `head`.

## File Structure

```
crates/ops/
  Cargo.toml
  src/lib.rs          `#![warn(missing_docs)]`, declares every module below (stubs are pre-created in Task 1)
  src/error.rs        OpsError, OpsErrorKind, classify_kube, scope_suffix  (+ moved error tests)
  src/ctx.rs          OpsCtx { client, namespace, scope }, Scope           (moved KubeCtx)
  src/wait.rs         wait_for, DEFAULT_WAIT_TIMEOUT                       (moved)
  src/status.rs       StatusReport + rows + matchers + build_report + gather
  src/snapshots.rs    RepoFilter, matches_repository, sort_key, resolve_repo_filter_for, ...
  src/maintenance.rs  covers_repository, run_patch, answered, failure_detail, yield_note, resolve_by_repo
  src/replication.rs  ReplicationTarget, run_patch, answered, failure_detail, detect_kind, ReplicationKind
  src/suspend.rs      SuspendableKind, KindMeta, kind_meta, patch_for, SuspendReport, toggle
  src/doctor.rs       DoctorCheck, Outcome, CheckResult, DoctorReport, RepoSummary, Work, DoctorParams, run_all, pure cores
  src/actions/mod.rs
  src/actions/snapshot.rs  SnapshotNowRequest, build_snapshot(_for), plan_snapshots, terminal, summaries
  src/actions/restore.rs   RestoreRequest, build_restore, terminal, summaries
  src/actions/catalog.rs   scan_patch
  src/browse/mod.rs        SnapshotAccess, root_oid_from_list, parse_dir_manifest, validate_rel_path, Walked, walk*
  src/browse/resolve.rs    RepoHandle, BrowseTarget, kopia_id_of, session_creds_secrets, resolve, resolve_ca_bundle, resolve_repo, OperatorNamespace
  src/browse/session.rs    session_*, ExecSession, MoverImageSource, SessionProgress, find_session_job, delete_session
crates/cli/                thin wrappers; re-exports where the old path is used widely
crates/api/src/consts.rs   + CATALOG_SCAN_REQUESTED_ANNOTATION
crates/api/src/lib.rs      testutil behind feature "testutil"
crates/kopia/src/session.rs ObjectId newtype
crates/xtask/src/phases.rs, wiring.rs   + "ops"
crates/xtask/phase-allowlist.yaml       paths crates/cli/... → crates/ops/...
```

Task dependency graph: Task 1 first (sequential). Tasks 2–6 are independent and run concurrently in separate worktrees branched from Task 1's commit — each touches only its own `crates/ops/src/<module>` files and its own `crates/cli/src/cmd/<module>` files. Task 7 runs after all of them are merged.

---

### Task 1: Groundwork + `kopiur-ops` scaffold (error, ctx, wait)

**Files:**
- Modify: `Cargo.toml` (workspace members + `kopiur-ops` dep)
- Create: `crates/ops/Cargo.toml`, `crates/ops/src/lib.rs`, `crates/ops/src/error.rs`, `crates/ops/src/ctx.rs`, `crates/ops/src/wait.rs`, stub files `crates/ops/src/{status,snapshots,maintenance,replication,suspend,doctor}.rs`, `crates/ops/src/actions/{mod,snapshot,restore,catalog}.rs`, `crates/ops/src/browse/{mod,resolve,session}.rs`
- Modify: `crates/cli/Cargo.toml` (add `kopiur-ops`), `crates/cli/src/context.rs`, `crates/cli/src/wait.rs`, `crates/cli/src/error/mod.rs`, `crates/cli/src/error/tests.rs`
- Modify: `crates/api/src/consts.rs`, `crates/api/src/lib.rs`, `crates/api/Cargo.toml` (feature `testutil`), `crates/controller/src/consts.rs`
- Modify: `crates/kopia/src/session.rs`
- Modify: `crates/xtask/src/phases.rs:168`, `crates/xtask/src/wiring.rs:74`

**Interfaces produced (later tasks rely on these exact names):**
```rust
// crates/ops/src/ctx.rs
pub enum Scope { Namespace(String), All }            // moved verbatim incl. flag_suffix()
pub struct OpsCtx { pub client: kube::Client, pub namespace: String, pub scope: Scope }
// crates/ops/src/error.rs
pub enum OpsError { /* see step 3 */ }
pub enum OpsErrorKind { Forbidden, NotFound, KindNotInstalled, Admission, Conflict, Invalid, Upstream, Timeout, Internal }
impl OpsError { pub fn kind(&self) -> OpsErrorKind; }
pub fn classify_kube(verb: &'static str, kind: &'static str, resource: &'static str, namespace: Option<&str>, name: Option<&str>, source: kube::Error) -> OpsError;
pub fn scope_suffix(namespace: Option<&str>) -> String;
// crates/ops/src/wait.rs
pub const DEFAULT_WAIT_TIMEOUT: Duration;
pub async fn wait_for<K, T>(api: &Api<K>, name: &str, what: String, hint: String, timeout: Duration, check: impl Fn(&K) -> Option<T>) -> Result<T, OpsError>;
// crates/kopia/src/session.rs
pub struct ObjectId(String); impl ObjectId { pub fn parse(s: &str) -> Result<Self, InvalidObjectId>; pub fn as_str(&self) -> &str; }
pub enum SessionCmd { SnapshotListJson, ShowObject { oid: ObjectId } }
// crates/api
pub const CATALOG_SCAN_REQUESTED_ANNOTATION: &str = "kopiur.home-operations.com/catalog-scan-requested-at";
#[cfg(any(test, feature = "testutil"))] pub mod testutil { pub fn from_yaml<T: DeserializeOwned>(yaml: &str) -> T }
// crates/cli re-exports so untouched CLI modules keep compiling:
// crates/cli/src/context.rs:  pub use kopiur_ops::ctx::{OpsCtx as KubeCtx, Scope};  (connect() stays)
// crates/cli/src/wait.rs:     pub use kopiur_ops::wait::{DEFAULT_WAIT_TIMEOUT, wait_for};  (thin file)
// crates/cli/src/error/mod.rs: pub use kopiur_ops::error::{classify_kube, scope_suffix}; + CliError::Ops(#[from] OpsError)
```

- [ ] **Step 1: Workspace + crate skeleton**

`Cargo.toml` (workspace): add `"crates/ops"` to `members` (after `"crates/cli"`) and `kopiur-ops = { path = "crates/ops" }` under `# Internal crates`.

`crates/ops/Cargo.toml`:
```toml
[package]
name = "kopiur-ops"
description = "Shared client-side operations for kopiur (CLI + web UI): cross-CRD joins, reports, browse data-plane, action builders"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
kopiur-api = { workspace = true }
kopiur-kopia = { workspace = true }
kopiur-mover = { workspace = true }
kube = { workspace = true, features = ["client", "runtime", "rustls-tls", "ws"] }
k8s-openapi = { workspace = true, features = ["v1_33"] }
futures = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
thiserror = { workspace = true }
chrono = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
kopiur-api = { workspace = true, features = ["testutil"] }
serde_yaml = { workspace = true }
```

`crates/ops/src/lib.rs`:
```rust
#![warn(missing_docs)]
//! `kopiur-ops` — the shared, kube-aware operations layer behind `kubectl kopiur`
//! and the web UI. Pure "typed CRs → report/decision" cores with thin kube IO.

pub mod actions;
pub mod browse;
pub mod ctx;
pub mod doctor;
pub mod error;
pub mod maintenance;
pub mod replication;
pub mod snapshots;
pub mod status;
pub mod suspend;
pub mod wait;

pub use ctx::{OpsCtx, Scope};
pub use error::{OpsError, OpsErrorKind, classify_kube, scope_suffix};
```
Create every stub listed in File Structure with just a `//! <one line>` doc comment (`actions/mod.rs` declares `pub mod catalog; pub mod restore; pub mod snapshot;`; `browse/mod.rs` declares `pub mod resolve; pub mod session;`). Stubs exist so concurrent tasks never edit `lib.rs`.

- [ ] **Step 2: Move `Scope`/`KubeCtx` and `wait_for`**

Move `Scope` and the struct at `crates/cli/src/context.rs:14-44` verbatim into `crates/ops/src/ctx.rs`, renaming the struct `KubeCtx` → `OpsCtx` (keep field docs). `connect()` stays in the CLI; the CLI file becomes: `pub use kopiur_ops::ctx::{OpsCtx as KubeCtx, Scope};` plus `connect()` (which now returns `KubeCtx` = `OpsCtx`). Move `crates/cli/src/wait.rs` verbatim to `crates/ops/src/wait.rs` with `CliError` → `OpsError`; the CLI `wait.rs` becomes a two-line re-export. Keep the `#[cfg(test)]` module of `context.rs` in the CLI (it tests `connect`).

- [ ] **Step 3: `OpsError`**

Create `crates/ops/src/error.rs` by moving these variants **verbatim** (doc comments, `#[error(...)]` strings, fields) from `crates/cli/src/error/mod.rs`: `SelectorMatchedNothing`, `UnknownPolicyRepository`, `Forbidden`, `KindNotInstalled`, `NotFound`, `Api`, `AdmissionDenied`, `WaitTimeout`, `GoneWhileWaiting`, `AmbiguousTarget`, `RepoOutsideSessionNamespace`, `MoverImageUnresolvable`, `OperatorNamespaceUnresolvable`, `CaBundleUnresolvable`, `NotADirectory`, `Serialization`, `SnapshotNotBrowsable`, `RepositoryUnderivable`, `CredsOutsideSessionNamespace`, `ClusterRepoSecretNamespaceMissing`, `SessionPodFailed`, `SessionNotReady`, `SessionExec`, `InvalidPath`, `PathNotFound`, `IsADirectory`, `NotAFile`, `SnapshotMissingInRepo`, `UnexpectedKopiaOutput`. Add one new variant for the exec-stream copy path (the CLI keeps `LocalIo` for local file writes):
```rust
/// An I/O error while copying bytes between the session pod and the caller.
#[error("I/O error while {what}: {source}")]
StreamIo { what: String, #[source] source: std::io::Error },
```
Move `scope_suffix` and `classify_kube` verbatim (they only construct moved variants). Add:
```rust
/// Coarse class of a failure, for HTTP status mapping and retry decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpsErrorKind { Forbidden, NotFound, KindNotInstalled, Admission, Conflict, Invalid, Upstream, Timeout, Internal }
impl OpsError {
    /// Exhaustive — a new variant must pick a kind here.
    pub fn kind(&self) -> OpsErrorKind { match self { /* every variant, no `_` */ } }
}
```
Mapping: Forbidden→Forbidden; NotFound, SnapshotMissingInRepo, PathNotFound→NotFound; KindNotInstalled→KindNotInstalled; AdmissionDenied→Admission; RepoOutsideSessionNamespace, CredsOutsideSessionNamespace, ClusterRepoSecretNamespaceMissing, AmbiguousTarget→Conflict; SelectorMatchedNothing, UnknownPolicyRepository, InvalidPath, IsADirectory, NotADirectory, NotAFile, SnapshotNotBrowsable, RepositoryUnderivable→Invalid; WaitTimeout, SessionNotReady→Timeout; Api, SessionExec, SessionPodFailed, MoverImageUnresolvable, OperatorNamespaceUnresolvable, CaBundleUnresolvable, GoneWhileWaiting→Upstream; Serialization, UnexpectedKopiaOutput, StreamIo→Internal.

In `crates/cli/src/error/mod.rs`: **do not delete the old variants yet** (concurrent tasks still compile against them until Task 7). Add:
```rust
/// A failure raised by the shared operations layer; its text is already what/why/fix.
#[error(transparent)]
Ops(#[from] kopiur_ops::OpsError),
```
and `pub use kopiur_ops::error::{classify_kube, scope_suffix};` replacing the local definitions (delete the local `classify_kube`/`scope_suffix` bodies; the CLI-only call sites keep compiling because the CLI's own `?` converts `OpsError` → `CliError`). Move the `error/tests.rs` tests that exercise `classify_kube`/moved variants into `crates/ops/src/error.rs` `#[cfg(test)]`, asserting on `OpsError`; leave tests of CLI-only variants where they are. Add a test that `OpsError::kind()` returns the table above for one representative variant per kind.

- [ ] **Step 4: `ObjectId` in `kopiur-kopia`**

In `crates/kopia/src/session.rs` add:
```rust
/// A kopia object id as it appears in manifests (`k…`/`x…` hex-ish). Validated so a
/// repository entry can never smuggle a flag onto the session argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectId(String);
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid kopia object id {0:?}: expected only ASCII letters and digits")]
pub struct InvalidObjectId(pub String);
impl ObjectId {
    pub fn parse(s: &str) -> Result<Self, InvalidObjectId> {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()) { Ok(Self(s.to_string())) } else { Err(InvalidObjectId(s.to_string())) }
    }
    pub fn as_str(&self) -> &str { &self.0 }
}
```
Change `SessionCmd::ShowObject { oid: String }` → `{ oid: ObjectId }`; `argv()` uses `oid.as_str()`. Export `ObjectId`, `InvalidObjectId` from `crates/kopia/src/lib.rs`. Tests: `parse("k1a2b3")` ok; `parse("--config-file=/x")`, `parse("")`, `parse("a b")` err; existing `no_variant_renders_a_mutating_verb` test updated. Fix the CLI call site (`crates/cli/src/cmd/browse/mod.rs` `ShowObject { oid: ... }`) to `ObjectId::parse(oid).map_err(|e| CliError::UnexpectedKopiaOutput { what: "a directory manifest entry".into(), detail: e.to_string() })?` — this is temporary; Task 6 moves that code.

- [ ] **Step 5: api + controller + xtask plumbing**

`crates/api/src/consts.rs`: add `CATALOG_SCAN_REQUESTED_ANNOTATION` with the doc comment from `crates/controller/src/consts.rs:191`; the controller file becomes `pub use kopiur_api::consts::CATALOG_SCAN_REQUESTED_ANNOTATION;` (keep `AWAIT_CATALOG_SCAN_ACTION` where it is). `crates/api/Cargo.toml`: add `[features] testutil = []`. `crates/api/src/lib.rs:131-136`: `#[cfg(any(test, feature = "testutil"))] pub mod testutil { pub fn from_yaml<T: serde::de::DeserializeOwned>(yaml: &str) -> T { … } }` (make the fn `pub`; `serde_yaml` must be a normal dependency when the feature is on — add `serde_yaml = { workspace = true, optional = true }` and `testutil = ["dep:serde_yaml"]`; check the existing dev-dependency and keep tests green). `crates/xtask/src/phases.rs:168`: `&["api", "cli", "controller", "kopia", "mover", "ops", "webhook"]`. `crates/xtask/src/wiring.rs:74`: `&["controller", "mover", "kopia", "webhook", "cli", "ops"]`.

- [ ] **Step 6: Verify and commit**

Run: `mise run build && mise run test && mise run clippy && mise run phase-check && mise run wiring-check && mise run complexity-check`
Expected: all pass; grep the test log for `FAILED` returns nothing.
```bash
git add -A && git commit -m "refactor(ops): scaffold kopiur-ops (OpsError, OpsCtx, wait) and groundwork for the web UI"
```

---

### Task 2: Move `status` + `snapshots` cores

**Files:**
- Create/fill: `crates/ops/src/snapshots.rs`, `crates/ops/src/status.rs`
- Modify: `crates/cli/src/cmd/snapshots.rs`, `crates/cli/src/cmd/status.rs`

**Interfaces:**
- Consumes: `OpsCtx`, `Scope`, `OpsError`, `classify_kube` (Task 1).
- Produces:
```rust
// ops::snapshots
pub struct RepoFilter { pub uid: String, pub name: String, pub kind: RepositoryKind, pub namespace: Option<String> }
pub struct SnapshotListFilter { pub policy: Option<String>, pub origin: Option<Origin> }   // clap-free
pub fn label_selector(f: &SnapshotListFilter) -> Option<String>;
pub fn matches_repository(snap: &Snapshot, filter: &RepoFilter) -> bool;
pub fn sort_key(snap: &Snapshot) -> DateTime<Utc>;
pub async fn resolve_repo_filter_for(ctx: &OpsCtx, name: &str, kind: RepositoryKind, repository_namespace: Option<&str>) -> Result<RepoFilter, OpsError>;
pub async fn list_snapshots(ctx: &OpsCtx, selector: Option<&str>) -> Result<Vec<Snapshot>, OpsError>;
// ops::status
pub struct StatusReport { … moved, `kind: &'static str` fields become `String` … }   // + RepoRow, PolicyRow, ScheduleRow, SnapshotReplicationRow, InFlight, StalledRow (Serialize)
pub struct StatusInputs { pub repositories: Vec<Repository>, pub cluster_repositories: Vec<ClusterRepository>, pub policies: Vec<SnapshotPolicy>, pub schedules: Vec<SnapshotSchedule>, pub snapshot_replications: Option<Vec<SnapshotReplication>>, pub snapshots: Vec<Snapshot>, pub restores: Vec<Restore> }
pub fn build_report(inputs: &StatusInputs, repo_filter: Option<&RepoFilter>) -> StatusReport;   // pure
pub async fn gather(ctx: &OpsCtx) -> Result<StatusInputs, OpsError>;                            // IO only; None for a 404 SnapshotReplication CRD
```

- [ ] **Step 1: Write the failing `build_report` test in `crates/ops/src/status.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;
    #[test]
    fn build_report_counts_in_flight_and_stalled() {
        let repo: Repository = from_yaml(r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: nas, namespace: default }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
status: { phase: Ready, backend: filesystem }
"#);
        let snap: Snapshot = from_yaml(r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: s1, namespace: default }
spec: { policyRef: { name: nightly } }
status: { phase: Running }
"#);
        let inputs = StatusInputs { repositories: vec![repo], cluster_repositories: vec![], policies: vec![], schedules: vec![], snapshot_replications: None, snapshots: vec![snap], restores: vec![] };
        let report = build_report(&inputs, None);
        assert_eq!(report.repositories.len(), 1);
        assert_eq!(report.in_flight.snapshots, 1);
        assert!(report.snapshot_replications.is_empty());
    }
}
```
Run: `cargo test -p kopiur-ops status::` → FAIL (nothing defined).

- [ ] **Step 2: Move `snapshots.rs` cores**

Move to `crates/ops/src/snapshots.rs`: `RepoFilter`, `matches_repository`, `meta_time`, `sort_key`, `resolve_repo_filter_for`, `resolve_repo_filter`, `get_repo`, and the list IO (`list` body up to the render call becomes `list_snapshots`). `label_selector` takes the new clap-free `SnapshotListFilter` (the CLI builds it from `SnapshotsListArgs`: `policy: args.policy.clone(), origin: args.origin.map(Origin::from)` — `OriginFilter` → `Origin` conversion exists in the CLI; if not, add `impl From<OriginFilter> for Origin` in `crates/cli/src/cli.rs`). Rendering (`headers`, `row`, `*_cell`, `render_list`) stays in the CLI. Tests for matchers/sort/`label_selector` move; render tests stay. `CliError` → `OpsError`, `KubeCtx` → `OpsCtx`.

- [ ] **Step 3: Move `status.rs` cores and split `gather`**

Move `StatusReport`, all row structs, `condition`, `snapshot_in_flight`, `restore_in_flight`, `repo_matches`, `policy_repository_display`, `policy_matches`, `replication_matches`, `snapshot_replication_row`, `restore_matches`, `repo_row` verbatim. Rewrite `gather` (`crates/cli/src/cmd/status.rs:409-726`) as two functions: `gather(ctx) -> StatusInputs` does only the seven `Api::list` calls (keeping the `list!` macro shape and the SnapshotReplication 404 → `None` rule at lines 614-640), and `build_report(inputs, repo_filter)` contains every loop body unchanged, iterating over the slices. `render`, `humanize_rfc3339`, `run` stay in the CLI; `run` becomes: resolve filter (ops) → `gather` → `build_report` → `render`/JSON. The `StatusReport` `kind: &'static str` fields in `RepoRow`/`StalledRow` become `String` (`kind: "Repository".to_string()`), keeping serde output identical.

- [ ] **Step 4: Verify**

Run: `cargo test -p kopiur-ops && cargo test -p kopiur-cli && mise run clippy` → PASS. Run `mise run phase-check` (the moved file must still be exhaustive).

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "refactor(ops): move status report and snapshot matchers into kopiur-ops"
```

---

### Task 3: Move `maintenance`, `replication`, `suspend` cores

**Files:**
- Fill: `crates/ops/src/maintenance.rs`, `crates/ops/src/replication.rs`, `crates/ops/src/suspend.rs`
- Modify: `crates/cli/src/cmd/maintenance.rs`, `crates/cli/src/cmd/replication.rs`, `crates/cli/src/cmd/suspend.rs`, `crates/cli/src/cli.rs` (`SuspendableKind`, `ReplicationKindArg` become newtypes/`From` impls)

**Interfaces produced:**
```rust
// ops::maintenance
pub fn covers_repository(m: &Maintenance, kind: RepositoryKind, name: &str) -> bool;
pub fn run_patch(requested_at: &str, mode: ManualRunMode) -> serde_json::Value;
pub fn answered(m: &Maintenance, requested_at: &str) -> Option<Result<Box<Maintenance>, Box<Maintenance>>>;  // same shape as today
pub fn failure_detail(m: &Maintenance, requested_at: &str) -> String;
pub fn yield_note(m: &Maintenance) -> Option<String>;
pub enum MaintenanceTarget { Named(String), ByRepository { kind: RepositoryKind, name: String } }
pub async fn resolve(ctx: &OpsCtx, target: &MaintenanceTarget) -> Result<Maintenance, OpsError>;
pub async fn request_run(ctx: &OpsCtx, m: &Maintenance, mode: ManualRunMode, now: DateTime<Utc>) -> Result<String /*requested_at*/, OpsError>;
// ops::replication
pub trait ReplicationTarget: …  (moved verbatim)
pub enum ReplicationKind { RepositoryReplication, SnapshotReplication }   // clap-free
pub fn run_patch(requested_at: &str) -> serde_json::Value;
pub async fn detect_kind(ctx: &OpsCtx, name: &str) -> Result<ReplicationKind, OpsError>;
pub async fn request_run<K: ReplicationTarget>(ctx: &OpsCtx, name: &str, now: DateTime<Utc>) -> Result<String, OpsError>;
// ops::suspend
pub enum SuspendableKind { Policy, Schedule, Repository, ClusterRepository, Replication, SnapshotReplication }
pub struct KindMeta { … moved … }
pub fn kind_meta(kind: SuspendableKind) -> KindMeta;
pub fn patch_for(kind: SuspendableKind, desired: bool) -> serde_json::Value;
pub struct SuspendReport { … moved … }
pub async fn set_suspended(ctx: &OpsCtx, kind: SuspendableKind, namespace: Option<&str>, name: &str, desired: bool) -> Result<SuspendReport, OpsError>;
```

- [ ] **Step 1: Move the pure fns and their tests** (`covers_repository`, `run_patch`, `answered`, `failure_detail`, `yield_note`; `ReplicationTarget` + impls + `run_patch`/`answered`/`failure_detail`; `KindMeta`/`kind_meta`/`patch_for`/`SuspendReport`) verbatim. `SuspendableKind` moves to ops; in `crates/cli/src/cli.rs` keep the clap `ValueEnum` enum but rename it `SuspendableKindArg` with `impl From<SuspendableKindArg> for kopiur_ops::suspend::SuspendableKind` (exhaustive match), and update the alias table test at `cli.rs:1016-1023`. Same for `ReplicationKindArg` → `From<…> for ReplicationKind`.
- [ ] **Step 2: Split the IO**: the `Api` construction + `patch` calls from `maintenance::run`, `replication::run_kind`, `suspend::toggle` move into the ops fns above; the CLI `run` fns keep `--wait` (`wait_for` via `crate::wait`), text rendering, `CmdOutput`, `eprintln!`.
- [ ] **Step 3: Verify**: `cargo test -p kopiur-ops -p kopiur-cli && mise run clippy && mise run phase-check` → PASS.
- [ ] **Step 4: Commit**: `git add -A && git commit -m "refactor(ops): move maintenance/replication/suspend cores into kopiur-ops"`

---

### Task 4: Move `doctor`

**Files:**
- Fill: `crates/ops/src/doctor.rs`
- Modify: `crates/cli/src/cmd/doctor.rs` (becomes: args → `DoctorParams` → `run_all` → render), `crates/xtask/phase-allowlist.yaml:302-313` (two `file:` paths → `crates/ops/src/doctor.rs`)

**Interfaces produced:**
```rust
pub enum DoctorCheck { … moved, with title() … }
pub enum Outcome { Pass, Warn(String), Fail { what: String, why: String, fix: String } }
pub struct CheckResult { pub check: DoctorCheck, pub outcome: Outcome }
pub struct DoctorReport { pub checks: Vec<CheckResult> }   impl DoctorReport { pub fn exit_code(&self) -> u8 }
pub struct RepoSummary { … now pub with pub fields … }
pub struct Work { … pub … }
pub struct DoctorParams { pub stuck_threshold: std::time::Duration, pub failure_lookback: std::time::Duration, pub operator_namespace: Option<String> }
pub async fn run_all(ctx: &OpsCtx, params: &DoctorParams, now: DateTime<Utc>) -> DoctorReport;
pub fn check_repos_ready(repos: &[RepoSummary]) -> Outcome;
pub fn evaluate_snapshot_replications(...) -> Outcome;   // signature unchanged
pub fn check_stuck(work: &Work, threshold: Duration, now: DateTime<Utc>) -> Outcome;
pub fn merge_degradation(base: Outcome, degraded: &[Outcome]) -> Outcome;
```

- [ ] **Step 1: Move everything except `render` and `run`'s output handling.** Every private fn/type from `crates/cli/src/cmd/doctor.rs:49-1600` moves verbatim; the ones listed above become `pub` with a one-line doc. **Keep the RBAC-degrade path exactly as is**: `warn_for` matches raw `kube::Error` 403 (`doctor.rs:195-213`) — do not route it through `classify_kube`. `run` (`:1602-1714`) splits into `run_all` (ops; returns the report) and the CLI `run` (builds `DoctorParams` from `DoctorArgs`, renders text/JSON, computes exit code). Tests (`:1715+`) move.
- [ ] **Step 2: Update the two allowlist entries** to `file: crates/ops/src/doctor.rs` (snippets unchanged). Run `mise run phase-check` — it must report nothing uncovered and nothing stale.
- [ ] **Step 3: Verify**: `cargo test -p kopiur-ops -p kopiur-cli && mise run clippy && mise run phase-check` → PASS.
- [ ] **Step 4: Commit**: `git add -A && git commit -m "refactor(ops): move doctor checks into kopiur-ops"`

---

### Task 5: Move action builders (`snapshot now`, `restore`) + new `catalog`

**Files:**
- Fill: `crates/ops/src/actions/{snapshot,restore,catalog}.rs`
- Modify: `crates/cli/src/cmd/snapshot.rs`, `crates/cli/src/cmd/restore.rs`, `crates/cli/src/cli.rs` (conversions), `crates/xtask/phase-allowlist.yaml:314` (`crates/ops/src/actions/snapshot.rs`)

**Interfaces produced:**
```rust
// ops::actions::snapshot
pub struct SnapshotNowRequest { pub policy: String, pub name: Option<String>, pub tags: Vec<(String, String)>, pub deletion_policy: Option<DeletionPolicy>, pub pin: bool, pub description: Option<String>, pub failure_policy: Option<FailurePolicy>, pub repository: Option<String> }
pub fn build_snapshot(req: &SnapshotNowRequest, namespace: &str, now: DateTime<Utc>) -> Snapshot;
pub fn build_snapshot_for(req: &SnapshotNowRequest, namespace: &str, now: DateTime<Utc>, cell: Option<&MintCell>) -> Snapshot;
pub async fn plan_snapshots(ctx: &OpsCtx, req: &SnapshotNowRequest, policy: &SnapshotPolicy, namespace: &str, now: DateTime<Utc>) -> Result<Vec<Snapshot>, OpsError>;
pub async fn create_snapshots(ctx: &OpsCtx, namespace: &str, planned: Vec<Snapshot>) -> Result<Vec<Snapshot>, OpsError>;
pub fn terminal(s: &Snapshot) -> Option<Result<Box<Snapshot>, Box<Snapshot>>>;
pub fn success_summary(s: &Snapshot) -> String;  pub fn failure_detail(s: &Snapshot) -> String;
// ops::actions::restore
pub struct RestoreRequest { pub source: RestoreSource, pub target: RestoreTarget, pub repository: Option<RepositoryRef>, pub options: Option<RestoreOptions>, pub policy: Option<RestorePolicy>, pub credential_projection: bool, pub failure_policy: Option<FailurePolicy>, pub name: Option<String> }
pub fn build_restore(req: &RestoreRequest, namespace: &str, now: DateTime<Utc>) -> Restore;
pub async fn create_restore(ctx: &OpsCtx, namespace: &str, restore: Restore) -> Result<Restore, OpsError>;
pub fn terminal(r: &Restore) -> …;  pub fn success_summary(r: &Restore) -> String;  pub fn failure_detail(r: &Restore) -> String;
// ops::actions::catalog
pub fn scan_patch(now: DateTime<Utc>) -> serde_json::Value;   // {"metadata":{"annotations":{CATALOG_SCAN_REQUESTED_ANNOTATION: rfc3339}}}
pub async fn request_scan(ctx: &OpsCtx, kind: RepositoryKind, namespace: Option<&str>, name: &str, now: DateTime<Utc>) -> Result<String, OpsError>;
```

- [ ] **Step 1: Write the failing `scan_patch` test**
```rust
#[test]
fn scan_patch_sets_the_promoted_annotation() {
    let now = chrono::Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap();
    let v = scan_patch(now);
    assert_eq!(v["metadata"]["annotations"][kopiur_api::consts::CATALOG_SCAN_REQUESTED_ANNOTATION], "2026-09-08T12:00:00Z");
}
```
- [ ] **Step 2: Move `snapshot.rs`/`restore.rs` builders** verbatim, replacing `&SnapshotNowArgs`/`&RestoreArgs` parameters with the request structs. In the CLI add `impl From<&SnapshotNowArgs> for SnapshotNowRequest` and `impl TryFrom<&RestoreArgs> for RestoreRequest` (the clap ArgGroup exclusivity makes the `RestoreSource`/`RestoreTarget` construction total; the existing `unreachable!` moves into the conversion). `plan_snapshots`/`planned_cells` move (they do a PVC list). The CLI `run` fns keep `--wait`, `--logs`, rendering. Tests: builder tests move and run against the request structs; add one CLI conversion test per source/target combination (3×3) and one for `SnapshotNowArgs`.
- [ ] **Step 3: Implement `catalog.rs`** (`scan_patch`, `request_scan` = merge-patch via `Api<Repository>`/`Api<ClusterRepository>` exhaustive over `RepositoryKind`, `PatchParams::apply("kopiur-ops")` is NOT used — use `Patch::Merge` with `PatchParams::default()` like the other run patches).
- [ ] **Step 4: Update allowlist path**, verify: `cargo test -p kopiur-ops -p kopiur-cli && mise run clippy && mise run phase-check` → PASS.
- [ ] **Step 5: Commit**: `git add -A && git commit -m "refactor(ops): move snapshot-now/restore builders into kopiur-ops; add catalog scan patch"`

---

### Task 6: Move the browse core (`mod`, `resolve`, `session`)

**Files:**
- Fill: `crates/ops/src/browse/{mod,resolve,session}.rs`
- Modify: `crates/cli/src/cmd/browse/{mod,local,resolve,session}.rs` (the CLI keeps `local.rs`, `Transport`, `ls/cat/download/browse/session_end`, `ReplState`, `render_manifest`; `resolve.rs`/`session.rs` become re-export shims)

**Interfaces produced:**
```rust
// ops::browse
pub trait SnapshotAccess {
    type Error: std::error::Error + Send + Sync + 'static + From<OpsError>;
    fn snapshot_root(&mut self, kopia_snapshot_id: &str) -> impl Future<Output = Result<String, Self::Error>> + Send;
    fn list_dir(&mut self, oid: &ObjectId) -> impl Future<Output = Result<DirManifest, Self::Error>> + Send;
    fn read_file(&mut self, oid: &ObjectId, sink: &mut (dyn AsyncWrite + Unpin + Send)) -> impl Future<Output = Result<u64, Self::Error>> + Send;
}
pub fn root_oid_from_list(bytes: &[u8], id: &str) -> Result<ObjectId, OpsError>;
pub fn parse_dir_manifest(bytes: &[u8], oid: &ObjectId) -> Result<DirManifest, OpsError>;
pub fn validate_rel_path(path: &str) -> Result<Vec<String>, OpsError>;
pub enum Walked { Root { oid: ObjectId }, Entry { path: String, entry: DirEntry } }
pub async fn walk<A: SnapshotAccess + ?Sized>(access: &mut A, root: &ObjectId, parts: &[String]) -> Result<Walked, A::Error>;
pub async fn walk_to_dir<A: …>(…) -> Result<(ObjectId, DirManifest), A::Error>;
pub async fn walk_to_file<A: …>(…) -> Result<(ObjectId, DirEntry), A::Error>;
// ops::browse::resolve
pub struct RepoHandle { … pub fields … }   pub struct BrowseTarget { … pub … }
pub fn kopia_id_of(snap: &Snapshot) -> Result<String, OpsError>;
pub fn session_creds_secrets(repo: &RepoHandle, session_namespace: &str) -> Result<Vec<String>, OpsError>;
pub async fn resolve(ctx: &OpsCtx, namespace: &str, snapshot_name: &str) -> Result<BrowseTarget, OpsError>;
pub enum OperatorNamespace { Known(String), DiscoverFromControllerDeployment }
pub async fn resolve_ca_bundle(ctx: &OpsCtx, repo: &RepoHandle, operator_ns: &OperatorNamespace) -> Result<Option<String>, OpsError>;
pub async fn resolve_repo(ctx: &OpsCtx, kind: RepositoryKind, name: &str) -> Result<RepoHandle, OpsError>;
// ops::browse::session
pub enum MoverImageSource { Fixed(String), DiscoverFromControllerDeployment }
pub trait SessionProgress: Send + Sync { fn reusing(&self, job: &str); fn starting(&self, job: &str); fn joining(&self, job: &str); }
pub struct TracingProgress;   // impl SessionProgress via tracing::info!
pub struct ExecSession { pub namespace: String, pub job_name: String, pub pod: String, … }
impl ExecSession {
    pub async fn ensure(ctx: &OpsCtx, target: &BrowseTarget, ttl: Duration, image: &MoverImageSource, progress: &dyn SessionProgress) -> Result<Self, OpsError>;
    pub async fn exec_capture(&self, cmd: SessionCmd) -> Result<Vec<u8>, OpsError>;
    pub async fn exec_stream(&self, cmd: SessionCmd, sink: &mut (dyn AsyncWrite + Unpin + Send)) -> Result<u64, OpsError>;
}
pub fn session_job_name(…); pub fn session_labels(…); pub fn session_pod_metadata(…); pub fn session_work_spec(…); pub fn job_is_terminal(…); pub fn mover_image_from_deployments(…);
pub async fn find_session_job(ctx: &OpsCtx, namespace: &str, kind: RepositoryKind, repo_namespace: Option<&str>, repo_name: &str) -> Result<Option<Job>, OpsError>;
pub async fn delete_session(ctx: &OpsCtx, namespace: &str, job_name: &str) -> Result<(), OpsError>;
```

- [ ] **Step 1: Move `mod.rs` cores** (`SnapshotAccess` with the associated `Error` type and `+ Send` futures; `root_oid_from_list`, `parse_dir_manifest`, `validate_rel_path`, `Walked`, `walk`, `walk_to_dir`, `walk_to_file`; the `FakeAccess` test double and its tests). `ExecSession`'s impl uses `Error = OpsError`. In the CLI, `LocalSession` and `Transport` implement the trait with `type Error = CliError` (`From<OpsError>` comes from `CliError::Ops`). `ls/cat/download/browse/session_end`, `ReplState`, `render_manifest`, `DIR_STREAM` usage in rendering stay in the CLI, importing the moved items from `kopiur_ops::browse`.
- [ ] **Step 2: Move `resolve.rs`** verbatim with: `resolve(ctx, namespace, name)` taking the namespace explicitly (CLI passes `ctx.namespace`); `resolve_ca_bundle` taking `&OperatorNamespace` (CLI passes `DiscoverFromControllerDeployment`; the existing `operator_namespace_for_ca` becomes the `Discover…` arm). Tests move.
- [ ] **Step 3: Move `session.rs`** verbatim with: `ensure(..., image, progress)`; `resolve_mover_image` becomes `match image { Fixed(s) => Ok(s.clone()), DiscoverFromControllerDeployment => <existing body> }`; every `eprintln!` (`session.rs:257,470,478,606`) becomes a `progress.*()` call (the CLI passes a `StderrProgress` that prints the identical strings); `LocalIo` in the exec copy path → `OpsError::StreamIo`. Tests (`:712+`) move.
- [ ] **Step 4: Verify**: `cargo test -p kopiur-ops -p kopiur-cli && mise run clippy && mise run phase-check` → PASS. Run the CLI text tests that assert `--local` in browse fixes — they must still pass unchanged.
- [ ] **Step 5: Commit**: `git add -A && git commit -m "refactor(ops): move the browse data-plane core into kopiur-ops"`

---

### Task 7: `CliError` cleanup + full gate (after Tasks 2–6 are merged)

**Files:**
- Modify: `crates/cli/src/error/mod.rs`, `crates/cli/src/error/tests.rs`, `crates/cli/src/lib.rs` (docs), `CLAUDE.md` (workspace layout line for `ops/`)

- [ ] **Step 1: Delete every `CliError` variant that no CLI code constructs any more** (grep `CliError::<Name>` across `crates/cli/src`; delete the variant if the only hits are in `error/mod.rs`). Expected survivors: `KubeConfig`, `LogStreamInterrupted`, `MigrationInput`, `AllNamespacesNotApplicable`, `LocalKopiaMissing`, `LocalKopia`, `LocalRepoVolume`, `LocalWorkloadIdentity`, `SecretsForbidden`, `DownloadIncomplete`, `LocalIo`, `Ops`. Move any remaining duplicated text tests to ops or delete them if already moved.
- [ ] **Step 2: Docs**: `crates/cli/src/lib.rs` module doc mentions `kopiur-ops`; `CLAUDE.md` "Workspace layout" gains `ops/  Shared client-side operations (reports, matchers, browse data-plane, action builders) used by cli + ui.` and the "Pinned deps" line says Rust 1.98.
- [ ] **Step 3: Full gate**: `mise run ci` and `mise run complexity-check`; grep for `FAILED`. Expected: green, offender count unchanged.
- [ ] **Step 4: Commit**: `git add -A && git commit -m "refactor(cli): drop CliError variants now owned by kopiur-ops"`
