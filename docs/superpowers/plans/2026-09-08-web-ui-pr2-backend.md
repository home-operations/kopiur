# Web UI PR2 — `kopiur-ui-model` + `kopiur-ui` backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the `kopiur-ui` server binary: an axum backend-for-frontend that authenticates via trusted proxy headers, impersonates the user against the apiserver, serves a typed `/api/v1` JSON API (reads from a SAR-gated reflector cache, writes and browse via impersonation), and embeds the SPA (placeholder until PR3).

**Architecture:** Two new crates. `crates/ui-model` (`kopiur-ui-model`) holds every wire type with `serde` + `ts_rs::TS` derives and an `export_all(dir)` used later by xtask to emit TypeScript. `crates/ui` (`kopiur-ui`) is the server: `config.rs` (all env), `auth/` (identity → impersonated `kube::Client`, proxy secret, CSRF, redaction), `cache/` (reflector stores + SubjectAccessReview gate), `api/` (handlers split into IO `load_*` and pure `view_*`), `browse/` (session pool + streaming download over `kopiur_ops::browse`), `actions/` (POST/DELETE → `kopiur_ops` builders), `static_files.rs` + `build.rs` (rust-embed). Every kube call for a user goes through a per-identity client built by `ImpersonateLayer`; the only calls under the UI's own ServiceAccount are the reflector watches and SubjectAccessReview creation.

**Tech Stack:** Rust 1.98, axum 0.8, tower-http 0.6 (`trace, compression-gzip, set-header, limit, timeout, cors`), kube 4 (`client, runtime, ws, rustls-tls`), kube-runtime reflector, ts-rs 12 (`serde-compat`), rust-embed 8 (`interpolate-folder-path`), tokio-util (`io`), constant_time_eq, thiserror, kopiur-ops (PR1).

**Spec:** `/home/perf3ct/.claude/plans/please-help-me-plan-floating-marble.md` — sections "Security model" (binding, item by item) and "PR2". Read it first; every rule there is a requirement here.

## Global Constraints

- **Security model is binding** (spec §Security model 1–9): proxy-secret header required in header mode unless `KOPIUR_UI_ACKNOWLEDGE_NO_PROXY_SECRET=true`; `system:` users/groups rejected except `system:authenticated`; `system:masters` never impersonable; inbound `Impersonate-*` never read; anonymous identity only in anonymous-only mode or with explicit `KOPIUR_UI_ANONYMOUS_FALLBACK=true`; every non-GET needs identity + `X-Kopiur-Request: 1` + JSON content-type on bodies + same-origin `Sec-Fetch-Site`/`Origin` when present; session creation is `POST`; `GET /file` requires `Sec-Fetch-Site ∈ {same-origin, none}` or absent; CSP with `frame-ancestors 'none'`, `X-Frame-Options: DENY`, `nosniff`, `Cache-Control: no-store` on `/api`; caps: `MAX_MANIFEST_BYTES` 64 MiB, `MAX_DOWNLOAD_BYTES` 1 GiB, per-identity exec 4, global exec 64, session starts 4, snapshot list cap 5000; the UI never reads Secrets; `Cookie`/`Authorization`/`X-Forwarded-Access-Token` stripped before handlers; no header values logged.
- Every env var name is a `pub const *_ENV: &str` in `crates/ui/src/config.rs`; flags `--kebab` with `env = …`; empty string means unset; `resolve()` returns typed `UiConfig` or a what/why/fix `ConfigError`.
- Every error type is a thiserror enum with exhaustive `match` in its mappers (no `_ =>`), message text tested. Phase enums from `kopiur-api` are never re-exported on the wire: view models use `*PhaseView` enums with an explicit `Unknown { raw: String }` variant, mapped exhaustively.
- `crates/ui-model` depends only on `serde`, `ts-rs`, `kopiur-api` (for `RepositoryKind`, `Origin`, and consts only — never on k8s-openapi types). `crates/ui` must not depend on `kopiur-controller`.
- `kopiur-api` stays tokio-free. Add `"ui"` and `"ui-model"` to `crates/xtask/src/phases.rs` `SCAN_CRATES` and `crates/xtask/src/wiring.rs` `CONSUMER_CRATES` (Task 2).
- `mise run test` stays node-free: `build.rs` never runs pnpm; a missing `web/dist` yields a placeholder page; `KOPIUR_UI_REQUIRE_WEB=1` makes it a hard build error.
- `complexity-check` (BUDGET=29, workspace-wide) must not grow; `mise run phase-check`, `wiring-check`, `clippy -D warnings`, `fmt-check` green after every task.
- Commit on the current branch after every task; never push. Grep test output for `FAILED`; never truncate.
- PR1 interfaces (already merged, verified in `crates/ops/src`) that this plan consumes: `kopiur_ops::{OpsCtx { client, namespace, scope, field_manager: String }, Scope, merge_patch_params(&str) -> PatchParams, OpsError, OpsErrorKind, classify_kube}` — the UI constructs `OpsCtx` with `field_manager: FIELD_MANAGER ("kopiur-ui")`; `kopiur_ops::format::{human_bytes, EMPTY_CELL}`; `kopiur_ops::status::{StatusInputs, StatusReport, build_report, gather}`; `kopiur_ops::snapshots::{RepoFilter, SnapshotListFilter, label_selector, matches_repository, sort_key, resolve_repo_filter_for, list_snapshots}`; `kopiur_ops::doctor::{DoctorReport, DoctorParams { stuck_threshold, failure_lookback, operator_namespace }, run_all, list_repos, list_work, RepoSummary, Work, Outcome, DoctorCheck}`; `kopiur_ops::actions::snapshot::{SnapshotNowRequest, build_snapshot, plan_snapshots, create_snapshots(ctx, ns, &[Snapshot])}`; `kopiur_ops::actions::restore::{RestoreRequest, build_restore, create_restore}`; `kopiur_ops::actions::catalog::request_scan(ctx, kind, namespace, name, now)`; `kopiur_ops::suspend::{SuspendableKind, set_suspended(ctx, kind, namespace, name, desired)}`; `kopiur_ops::maintenance::{MaintenanceTarget, resolve, request_run(ctx, &Maintenance, mode, now), covers_repository}`; `kopiur_ops::replication::{ReplicationKind, detect_kind, request_run_by_kind(ctx, kind, name, now)}`; `kopiur_ops::browse::{SnapshotAccess (type Error; snapshot_root -> Result<ObjectId, Error>; list_dir(&ObjectId); read_file(&ObjectId, sink)), parse_oid, validate_rel_path, walk_to_dir, walk_to_file, resolve::{resolve(ctx, namespace, name), BrowseTarget, OperatorNamespace, known_operator_namespace, resolve_repo}, session::{ExecSession::{ensure, exec_capture, exec_stream}, MoverImageSource, pinned_mover_image, SessionProgress, find_session_job, delete_session, session_job_name}}`; `kopiur_kopia::{ObjectId, DirManifest, DirEntry, SessionCmd}`. Two error texts carry CLI flag grammar (`UnknownPolicyRepository` says `--repository`, `detect_kind`'s ambiguity says `--kind`), and `NotFound` mentions `--namespace/--context`: the UI's problem mapper rewrites `fix` for those variants. Doctor fix strings mention `kubectl krew upgrade kopiur`, `--stuck-threshold`, `--failure-lookback`: `DoctorReportView` passes them through (the UI labels them "CLI hint").
- **Final-review notes on ops (PR1) that PR2 must honor:** (1) namespace routing is uneven — `browse::resolve(ctx, ns, name)` takes the namespace, but `browse::resolve_repo`, `maintenance::resolve`, `replication::*`, `snapshots::list_snapshots`, `status::gather` read `ctx.namespace`/`ctx.scope` — so the server builds a **per-request `OpsCtx`** (`Client` is `Clone`; `namespace` = the request's namespace or `KOPIUR_NAMESPACE`, `scope` = `All` for list endpoints without `?namespace=`). (2) `browse::resolve` hard-codes `OperatorNamespace::DiscoverFromControllerDeployment` for the CA bundle; **Task 6 of this plan adds `kopiur_ops::browse::resolve::resolve_with(ctx, ns, name, &OperatorNamespace)`** (the existing `resolve` becomes a one-line wrapper passing `DiscoverFromControllerDeployment`) so the UI never lists Deployments cluster-wide. (3) pass `DoctorParams { operator_namespace: Some(KOPIUR_NAMESPACE) }` so the controller check is namespaced. (4) creates (`create_snapshots`, `create_restore`, session Job) use `PostParams::default()` by ruling — only patches carry `field_manager`.

## File Structure

```
crates/ui-model/
  Cargo.toml
  src/lib.rs        pub mod problem, identity, graph, views, requests; pub fn export_all(dir: &Path) -> Result<(), ts_rs::ExportError>
  src/problem.rs    Problem
  src/identity.rs   Me, Capabilities, IdentitySource
  src/graph.rs      RepositoryGraph, GraphNode, GraphEdge, NodeKind, EdgeKind, Health, GateHit
  src/views.rs      RepositoryPhaseView, SnapshotPhaseView, RestorePhaseView, ReplicationPhaseView, OriginView, RepositorySummary, RepositoryDetail, SnapshotRow, SnapshotDetail, Page<T>, PolicyRow, PolicyDetail, ScheduleRow, RestoreRow, MaintenanceRow, RepositoryReplicationRow, SnapshotReplicationRow, DoctorReportView, GateDescriptor, EventRow, DirListing, DirEntryView, EntryKind, SessionInfo, ActionReceipt, StatusOverview
  src/requests.rs   SnapshotNowBody, RestoreBody, SuspendBody, MaintenanceRunBody, ReplicationRunBody, ScanCatalogBody, SessionCreateBody
crates/ui/
  Cargo.toml        [[bin]] kopiur-ui + [lib]
  build.rs
  src/main.rs, src/lib.rs (app(), AppState)
  src/config.rs
  src/metrics.rs
  src/auth/{mod,identity,proxy_secret,impersonate,csrf,redact}.rs
  src/cache/{mod,stores,authz}.rs
  src/api/{mod,problem,me,graph,status,repositories,snapshots,policies,schedules,restores,maintenance,replications,doctor,gates,events}.rs
  src/browse/{mod,session_pool,download}.rs
  src/actions/mod.rs
  src/static_files.rs
  src/ops_listener.rs   /metrics /healthz /readyz
web/                  (PR3) — this PR only needs `crates/ui/web/.gitkeep` so build.rs has a root
```

Task order: T1 (ui-model) → T2 (ui skeleton) sequential; then T3 (auth) ∥ T4 (cache+authz) concurrent; then T5 (read API) ∥ T6 (browse) ∥ T7 (actions) concurrent; T8 (wiring + router tests) last. Concurrent tasks touch disjoint modules; T2 pre-declares every module and dependency.

---

### Task 1: `crates/ui-model` — wire types + ts-rs export

**Files:** create `crates/ui-model/Cargo.toml`, `crates/ui-model/src/{lib,problem,identity,graph,views,requests}.rs`; modify workspace `Cargo.toml` (member + `kopiur-ui-model` dep + `ts-rs = { version = "12", features = ["serde-compat"] }`).

**Interfaces produced (exact):**
```rust
// Every type: #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)] #[serde(rename_all = "camelCase")] #[ts(export)]
// problem.rs
pub struct Problem { pub r#type: String, pub title: String, pub status: u16, pub detail: String, pub what: String, pub why: String, pub fix: String, pub instance: Option<String>, pub kube_reason: Option<String> }
// identity.rs
pub enum IdentitySource { TrustedHeaders, Anonymous }
pub struct Capabilities { pub create_snapshots: bool, pub delete_snapshots: bool, pub create_restores: bool, pub patch_policies: bool, pub exec_sessions: bool }
pub struct Me { pub user: String, pub groups: Vec<String>, pub email: Option<String>, pub source: IdentitySource, pub namespace: Option<String>, pub can: Capabilities }
// graph.rs
pub enum NodeKind { Repository, ClusterRepository, Backend, Policy, Namespace, NamespaceSelector }
pub enum EdgeKind { SnapshotReplication, RepositoryReplication, Seed, PolicyMembership, AllowedNamespace }
pub enum Health { Healthy, Degraded, Failed, Suspended, Pending, Unknown }
pub struct GateHit { pub condition: String, pub reason: String, pub severity: String, pub message: String }
pub struct GraphNode { pub id: String, pub kind: NodeKind, pub name: String, pub namespace: Option<String>, pub label: String, pub health: Health, pub missing: bool, pub allows_all_namespaces: bool, pub backend_kind: Option<String>, pub gates: Vec<GateHit> }
pub struct GraphEdge { pub id: String, pub from: String, pub to: String, pub kind: EdgeKind, pub label: Option<String>, pub health: Health }
pub struct RepositoryGraph { pub nodes: Vec<GraphNode>, pub edges: Vec<GraphEdge>, pub generated_at: String }
// views.rs (phase views)
pub enum RepositoryPhaseView { Pending, Initializing, Ready, Degraded, Failed, Unknown { raw: String } }
pub enum SnapshotPhaseView { Pending, Running, Succeeded, Failed, Deleting, Discovered, Unchanged, Unknown { raw: String } }
pub enum RestorePhaseView { Pending, Resolving, Restoring, Completed, Failed, Unknown { raw: String } }
pub enum ReplicationPhaseView { Pending, Replicating, Succeeded, Failed, Suspended, Unknown { raw: String } }
pub enum OriginView { Scheduled, Manual, Discovered, Adopted, Replicated }
pub struct Page<T> { pub items: Vec<T>, pub total: usize, pub offset: usize, pub limit: usize }
pub struct RepositorySummary { pub kind: String, pub name: String, pub namespace: Option<String>, pub phase: Option<RepositoryPhaseView>, pub health: Health, pub backend: Option<String>, pub mode: String, pub suspended: bool, pub snapshot_count: Option<i64>, pub total_size_bytes: Option<i64>, pub index_blob_count: Option<i64>, pub last_observed_at: Option<String>, pub server_endpoint: Option<String>, pub allowed_namespace_count: Option<i64> }
pub struct RepositoryDetail { pub summary: RepositorySummary, pub identity_cluster: Option<String>, pub catalog: Option<CatalogView>, pub health: Option<HealthProbeView>, pub server: Option<ServerView>, pub seed: Option<SeedView>, pub maintenance: Option<MaintenanceRow>, pub gates: Vec<GateHit>, pub conditions: Vec<ConditionView>, pub policies: Vec<PolicyRef>, pub replications_out: Vec<String>, pub replications_in: Vec<String>, pub sessions: Vec<SessionInfo> }
pub struct CatalogView { pub discovered_backup_count: Option<i64>, pub foreign_snapshot_count: Option<i64>, pub last_refresh_at: Option<String> }
pub struct HealthProbeView { pub last_probe_at: Option<String>, pub last_healthy_at: Option<String>, pub consecutive_probe_failures: Option<i64> }
pub struct ServerView { pub endpoint: Option<String>, pub read_only: Option<bool>, pub auth_mode: Option<String> }
pub struct SeedView { pub mode: Option<String>, pub source: Option<String>, pub seeded_at: Option<String>, pub snapshots_copied: Option<i64> }
pub struct ConditionView { pub r#type: String, pub status: String, pub reason: Option<String>, pub message: Option<String>, pub last_transition_time: Option<String> }
pub struct PolicyRef { pub namespace: String, pub name: String }
pub struct SnapshotRow { pub namespace: String, pub name: String, pub phase: Option<SnapshotPhaseView>, pub origin: Option<OriginView>, pub policy: Option<String>, pub repository: Option<String>, pub kopia_snapshot_id: Option<String>, pub identity: Option<String>, pub start_time: Option<String>, pub end_time: Option<String>, pub size_bytes: Option<i64>, pub bytes_new: Option<i64>, pub files_total: Option<i64>, pub files_failed: Option<i64>, pub pinned: bool, pub deletion_policy: Option<String>, pub copied_from: Option<String> }
pub struct SnapshotDetail { pub row: SnapshotRow, pub stats: Option<SnapshotStatsView>, pub duration_seconds: Option<i64>, pub sources: Vec<String>, pub lineage: Lineage, pub retention_preview: Option<RetentionPreview>, pub failure: Option<FailureView>, pub log_tail: Vec<String>, pub conditions: Vec<ConditionView>, pub gates: Vec<GateHit>, pub browsable: bool, pub browse_blocker: Option<String> }
pub struct SnapshotStatsView { pub size_bytes: Option<i64>, pub bytes_new: Option<i64>, pub files_new: Option<i64>, pub files_modified: Option<i64>, pub files_unchanged: Option<i64>, pub files_failed: Option<i64> }
pub struct Lineage { pub copied_from_repository: Option<String>, pub source_manifest_id: Option<String>, pub copies: Vec<SnapshotRefView> }
pub struct SnapshotRefView { pub namespace: String, pub name: String }
pub struct RetentionPreview { pub kept: bool, pub reasons: Vec<String>, pub computed_at: String }
pub struct FailureView { pub kopia_error_class: Option<String>, pub message: Option<String>, pub exit_code: Option<i32>, pub retry_recommended: Option<bool>, pub op: Option<String> }
pub struct PolicyRow { pub namespace: String, pub name: String, pub repositories: Vec<String>, pub multi_repo: bool, pub suspended: bool, pub last_successful_snapshot: Option<String>, pub last_verified: Option<String>, pub active_snapshot_count: Option<i64> }
pub struct PolicyDetail { pub row: PolicyRow, pub identity: Option<String>, pub sources: Vec<String>, pub retention: Option<RetentionView>, pub verification: Vec<RepoVerificationView>, pub schedules: Vec<ScheduleRow>, pub recent_snapshots: Vec<SnapshotRow>, pub gates: Vec<GateHit>, pub conditions: Vec<ConditionView> }
pub struct RetentionView { pub keep_latest: Option<i32>, pub keep_hourly: Option<i32>, pub keep_daily: Option<i32>, pub keep_weekly: Option<i32>, pub keep_monthly: Option<i32>, pub keep_annual: Option<i32> }
pub struct RepoVerificationView { pub repository: String, pub last_verified: Option<String> }
pub struct ScheduleRow { pub namespace: String, pub name: String, pub policy: Option<String>, pub policy_selector: Option<String>, pub cron: String, pub timezone: Option<String>, pub suspended: bool, pub last_fire: Option<String>, pub next_fire: Option<String>, pub last_snapshot: Option<String>, pub consecutive_failures: i64 }
pub struct RestoreRow { pub namespace: String, pub name: String, pub phase: Option<RestorePhaseView>, pub source_kind: Option<String>, pub target_kind: String, pub repository: Option<String>, pub kopia_snapshot_id: Option<String>, pub start_time: Option<String>, pub end_time: Option<String>, pub bytes_restored: Option<i64>, pub files_restored: Option<i64>, pub claims: Vec<RestoreClaimView> }
pub struct RestoreClaimView { pub pvc: String, pub phase: String, pub message: Option<String> }
pub struct MaintenanceRow { pub namespace: String, pub name: String, pub repository: String, pub owner: Option<String>, pub managed_by_repository: bool, pub quick: RunStatusView, pub full: RunStatusView, pub manual_run: Option<ManualRunView> }
pub struct RunStatusView { pub last_run_at: Option<String>, pub next_scheduled_at: Option<String>, pub consecutive_failures: i64, pub last_content_reclaimed_bytes: Option<i64> }
pub struct ManualRunView { pub requested_at: Option<String>, pub mode: Option<String>, pub phase: Option<String>, pub completed_at: Option<String> }
pub struct RepositoryReplicationRow { pub namespace: String, pub name: String, pub source: String, pub destination_backend: Option<String>, pub cron: String, pub suspended: bool, pub phase: Option<ReplicationPhaseView>, pub last_replicated: Option<String>, pub next_scheduled_at: Option<String>, pub last_replicated_bytes: Option<i64>, pub last_replicated_blobs: Option<i64> }
pub struct SnapshotReplicationRow { pub namespace: String, pub name: String, pub source: String, pub destination: String, pub cron: String, pub suspended: bool, pub phase: Option<ReplicationPhaseView>, pub last_replicated: Option<String>, pub identities_selected: Option<u32>, pub snapshots_copied: Option<u32>, pub already_present: Option<u32>, pub failed: Option<u32>, pub pruned: Option<u32> }
pub struct DoctorCheckView { pub check: String, pub title: String, pub outcome: String /* Pass|Warn|Fail */, pub what: Option<String>, pub why: Option<String>, pub fix: Option<String> }
pub struct DoctorReportView { pub checks: Vec<DoctorCheckView>, pub exit_code: u8, pub ran_at: String }
pub struct GateDescriptor { pub scope: String, pub condition: String, pub blocked_status: String, pub reason: String, pub severity: String }
pub struct EventRow { pub time: Option<String>, pub r#type: Option<String>, pub reason: Option<String>, pub message: Option<String>, pub regarding: String }
pub enum EntryKind { File, Dir, Symlink, Other { raw: String } }
pub struct DirEntryView { pub name: String, pub kind: EntryKind, pub size: Option<i64>, pub mtime: Option<String>, pub mode: Option<String> }
pub struct DirListing { pub path: String, pub entries: Vec<DirEntryView>, pub total: usize, pub offset: usize, pub limit: usize, pub session: SessionInfo }
pub struct SessionInfo { pub namespace: String, pub job: String, pub pod: Option<String>, pub reused: bool, pub expires_at: Option<String> }
pub struct ActionReceipt { pub kind: String, pub created: Vec<SnapshotRefView>, pub requested_at: Option<String>, pub note: Option<String> }
pub struct StatusOverview { pub report: serde_json::Value /* kopiur_ops StatusReport passthrough; TS type = unknown */, pub now: String }
// requests.rs — all #[serde(deny_unknown_fields)]
pub struct SnapshotNowBody { pub namespace: String, pub policy: String, pub name: Option<String>, pub tags: Vec<(String, String)>, pub pin: bool, pub description: Option<String>, pub repository: Option<String> }
pub struct RestoreBody { pub namespace: String, pub source: RestoreSourceBody, pub target: RestoreTargetBody, pub repository: Option<RepositoryRefBody>, pub name: Option<String>, pub overwrite: Option<bool>, pub source_path: Option<String> }
pub enum RestoreSourceBody { SnapshotRef { name: String, namespace: Option<String> }, FromPolicy { name: String, namespace: Option<String>, as_of: Option<String>, offset: Option<u32> }, Identity { username: String, hostname: String, source_path: Option<String>, snapshot_id: Option<String> } }
pub enum RestoreTargetBody { PvcRef { name: String }, Pvc { name: String, storage_class_name: Option<String>, size: String } }
pub struct RepositoryRefBody { pub kind: String, pub name: String, pub namespace: Option<String> }
pub struct SuspendBody { pub kind: String, pub namespace: Option<String>, pub name: String, pub suspend: bool }
pub struct MaintenanceRunBody { pub namespace: String, pub name: Option<String>, pub repository: Option<RepositoryRefBody>, pub mode: String /* quick|full */ }
pub struct ReplicationRunBody { pub namespace: String, pub name: String }
pub struct ScanCatalogBody { pub kind: String, pub namespace: Option<String>, pub name: String }
pub struct SessionCreateBody { pub ttl_seconds: Option<u64> }
```
`lib.rs`: `pub fn export_all(dir: &Path) -> Result<(), ts_rs::ExportError>` builds `ts_rs::Config::new()` with `export_dir = dir` and calls `T::export_all(&cfg)` for the root types (`Problem`, `Me`, `RepositoryGraph`, `StatusOverview`, `RepositorySummary`, `RepositoryDetail`, `Page<SnapshotRow>`, `SnapshotDetail`, `PolicyRow`, `PolicyDetail`, `ScheduleRow`, `RestoreRow`, `MaintenanceRow`, `RepositoryReplicationRow`, `SnapshotReplicationRow`, `DoctorReportView`, `GateDescriptor`, `EventRow`, `DirListing`, `SessionInfo`, `ActionReceipt`, and every request body). Externally-tagged enums (`RestoreSourceBody`, `RestoreTargetBody`) use `#[serde(rename_all = "camelCase")]` on variants too so the wire is `{ "snapshotRef": {...} }`.

- [ ] **Step 1: failing test** in `lib.rs`: `export_all(tempdir)` succeeds and the dir contains `Problem.ts`, `RepositoryGraph.ts`, `SnapshotPhaseView.ts`; `SnapshotPhaseView.ts` contains the string `"unknown"` (the Unknown variant with `raw`) and `RestoreSourceBody.ts` contains `snapshotRef`. Use `std::env::temp_dir().join(format!("kopiur-ui-model-{}", std::process::id()))`, clean up after.
- [ ] **Step 2:** implement all types and `export_all`. Serde checks: `Page<T>` needs `T: Serialize`; `StatusOverview.report` is `serde_json::Value` with `#[ts(type = "unknown")]`.
- [ ] **Step 3:** `cargo test -p kopiur-ui-model`, `mise run clippy`, `mise run fmt`; commit `feat(ui-model): wire types for the web UI with ts-rs export`.

---

### Task 2: `crates/ui` skeleton — config, main, ops listener, static files, metrics, module stubs

**Files:** create `crates/ui/Cargo.toml`, `build.rs`, `web/.gitkeep`, `src/{main,lib,config,metrics,ops_listener,static_files}.rs`, stubs for every module in File Structure (`auth/mod.rs` declares `identity, proxy_secret, impersonate, csrf, redact`; `cache/mod.rs` declares `stores, authz`; `api/mod.rs` declares all handler modules + `problem`; `browse/mod.rs` declares `session_pool, download`; `actions/mod.rs`). Modify workspace `Cargo.toml` (member, `kopiur-ui` dep, `tower-http`, `rust-embed`, `tokio-util`, `http-body-util`, `mime_guess`, `constant_time_eq`), `crates/xtask/src/phases.rs` + `wiring.rs` (add `"ui"`, `"ui-model"`), `.gitignore` (`!crates/ui/web/` is NOT needed; add `crates/ui/web/dist/` and `crates/ui/web/node_modules/` explicitly for clarity).

**Interfaces produced:**
```rust
// config.rs — consts (value): KOPIUR_UI_ADDR("[::]:8090"), KOPIUR_UI_OPS_ADDR("[::]:8091"), KOPIUR_UI_USER_HEADER, KOPIUR_UI_GROUPS_HEADER, KOPIUR_UI_GROUPS_SEPARATOR(","), KOPIUR_UI_EMAIL_HEADER, KOPIUR_UI_IMPERSONATE_EXTRA_KEYS (csv), KOPIUR_UI_ALLOWED_GROUPS (csv), KOPIUR_UI_ANONYMOUS_USER, KOPIUR_UI_ANONYMOUS_GROUPS (csv), KOPIUR_UI_ANONYMOUS_FALLBACK(false), KOPIUR_UI_PROXY_SECRET_FILE, KOPIUR_UI_ACKNOWLEDGE_NO_PROXY_SECRET(false), KOPIUR_NAMESPACE, KOPIUR_MOVER_IMAGE, KOPIUR_UI_CACHE(true), KOPIUR_UI_SESSION_TTL("15m"), KOPIUR_UI_SESSION_READY_TIMEOUT("300s"), KOPIUR_UI_MAX_SESSION_STARTS(4), KOPIUR_UI_MAX_EXEC_PER_IDENTITY(4), KOPIUR_UI_MAX_EXEC_GLOBAL(64), KOPIUR_UI_MAX_DOWNLOAD_BYTES(1073741824), KOPIUR_UI_MAX_MANIFEST_BYTES(67108864), KOPIUR_UI_SNAPSHOT_LIST_CAP(5000), KOPIUR_UI_CLIENT_CACHE_SIZE(256), KOPIUR_UI_CLIENT_CACHE_TTL("10m"), KOPIUR_UI_SAR_TTL("60s"), KOPIUR_UI_TLS_CERT, KOPIUR_UI_TLS_KEY, KOPIUR_UI_CORS_ORIGINS (csv, dev only). Re-export `kopiur_telemetry::env::*`.
pub struct UiArgs (clap; one field per const, `#[arg(long, env = X_ENV)]`)
pub struct UiConfig { pub addr: SocketAddr, pub ops_addr: SocketAddr, pub auth: AuthConfig, pub operator_namespace: Option<String>, pub mover_image: Option<String>, pub cache_enabled: bool, pub session: SessionLimits, pub download_max_bytes: u64, pub manifest_max_bytes: u64, pub snapshot_list_cap: usize, pub client_cache: CacheLimits, pub sar_ttl: Duration, pub tls: Option<TlsPaths>, pub cors_origins: Vec<String> }
pub struct AuthConfig { pub mode: AuthMode, pub groups_separator: String, pub email_header: Option<HeaderName>, pub extra_keys: Vec<String>, pub allowed_groups: Option<BTreeSet<String>>, pub proxy_secret: Option<Vec<u8>> }
pub enum AuthMode { Headers { user: HeaderName, groups: Option<HeaderName>, anonymous_fallback: Option<AnonymousIdentity> }, AnonymousOnly(AnonymousIdentity) }
pub struct AnonymousIdentity { pub user: String, pub groups: Vec<String> }
pub struct SessionLimits { pub ttl: Duration, pub ready_timeout: Duration, pub max_starts: usize, pub max_exec_per_identity: usize, pub max_exec_global: usize }
pub struct CacheLimits { pub size: usize, pub ttl: Duration }
pub struct TlsPaths { pub cert: PathBuf, pub key: PathBuf }
#[derive(Debug, thiserror::Error)] pub enum ConfigError { NoIdentitySource, ReservedHeaderName { header: String }, InvalidHeaderName { header: String, source: http::header::InvalidHeaderName }, ProxySecretRequired, ProxySecretUnreadable { path: PathBuf, source: std::io::Error }, AnonymousWithoutUser, InvalidDuration { name: &'static str, value: String }, InvalidAddr { name: &'static str, value: String, source: std::net::AddrParseError }, ForbiddenAnonymousGroup { group: String } }
impl UiArgs { pub fn resolve(self) -> Result<UiConfig, ConfigError>; }
pub const FIELD_MANAGER: &str = "kopiur-ui";
// metrics.rs
pub struct UiMetrics { … } // requests_total{route,status,identity_source}, identity_cache_size, sessions_started_total, download_incomplete_total, exec_inflight, cache_objects{kind}, sar_total{allowed}
impl UiMetrics { pub fn new(provider: Arc<MetricsProvider>) -> Self; pub fn gather(&self) -> Vec<u8>; }
// ops_listener.rs
pub struct Readiness { pub impersonation_ok: AtomicBool, pub cache_ready: AtomicBool, pub web_placeholder: bool }
pub async fn serve_ops(addr: SocketAddr, metrics: Arc<UiMetrics>, readiness: Arc<Readiness>) -> anyhow::Result<()>  // /metrics, /healthz (200), /readyz (503 with a plain-text reason when any flag is false)
// static_files.rs
#[derive(rust_embed::Embed)] #[folder = "$OUT_DIR/web"] pub struct Web;
pub fn is_placeholder() -> bool;                              // true when index.html carries the build.rs marker comment "<!-- kopiur-ui-placeholder -->"
pub async fn spa_fallback(uri: Uri) -> Response;              // exact asset → bytes + mime + Cache-Control (immutable for /assets/*, no-cache for index.html); else index.html
// lib.rs
pub struct AppState { pub cfg: Arc<UiConfig>, pub metrics: Arc<UiMetrics>, pub readiness: Arc<Readiness>, pub auth: Arc<auth::AuthState>, pub source: Arc<cache::Source>, pub sessions: Arc<browse::session_pool::SessionPool> }
pub fn app(state: AppState) -> axum::Router;   // T8 fills the routes; T2 leaves /api/v1/health-of-nothing: fallback → spa_fallback and `/api/*` → 404 problem
```
`resolve()` rules (fail closed): user header absent AND anonymous user absent → `NoIdentitySource`; user header named `impersonate-*`/`authorization`/`cookie` → `ReservedHeaderName`; header mode without proxy secret and without acknowledge → `ProxySecretRequired`; anonymous groups absent when anonymous user set is fine; anonymous user or any anonymous group starting with `system:` (other than `system:authenticated`) or equal to `system:masters` → `ForbiddenAnonymousGroup`; `KOPIUR_UI_ANONYMOUS_FALLBACK=true` without anonymous user → `AnonymousWithoutUser`. Durations parse `15m`, `300s`, `1h` (hand-rolled parser: integer + unit s/m/h; reject anything else). Every `ConfigError` message states what, why, fix (name the env var).

`build.rs`: `let dist = manifest_dir/web/dist; let out = OUT_DIR/web;` remove+recreate `out`; if `dist/index.html` exists copy the tree recursively; else if `env KOPIUR_UI_REQUIRE_WEB == "1"` panic with the message from the spec; else write `index.html` = `<!doctype html><!-- kopiur-ui-placeholder --><title>kopiur-ui</title><p>kopiur-ui was built without the web bundle. Run <code>mise run ui-build</code>.</p>`. `println!("cargo:rerun-if-changed=web/dist"); println!("cargo:rerun-if-env-changed=KOPIUR_UI_REQUIRE_WEB");`.

`main.rs`: `UiArgs::parse()` → `resolve()` (print the error and exit 2 on failure) → `let _telemetry = kopiur_telemetry::init_tracing("kopiur-ui")?;` → `rustls::crypto::ring::default_provider().install_default()` → `MetricsProvider::new("kopiur-ui")` → spawn `serve_ops` FIRST → build `AppState` (T8 wires auth/cache/sessions; T2 constructs with stubs) → `axum::serve` with graceful shutdown (`tokio::signal` SIGTERM/ctrl-c) or `axum_server::bind_rustls` when `tls` is set (copy `serve_tls`/`spawn_cert_reload`/`shutdown_signal` from `crates/webhook/src/main.rs`).

- [ ] **Step 1: tests first** (`config.rs`): table test over `resolve()` covering every `ConfigError` variant + the happy header mode + anonymous-only mode; duration parser table; each error message contains the env var name it points at (assert with `contains`).
- [ ] **Step 2:** implement config, metrics, ops listener, static files (+ `is_placeholder`), build.rs, main, lib with stubs. `cargo build -p kopiur-ui` with no `web/dist` embeds the placeholder; a unit test asserts `Web::get("index.html").is_some()` and `is_placeholder()` is true in that build.
- [ ] **Step 3:** `KOPIUR_UI_REQUIRE_WEB=1 cargo build -p kopiur-ui` fails with the spec message (run it, record the output in the report, then unset).
- [ ] **Step 4:** `mise run test clippy phase-check wiring-check fmt`; commit `feat(ui): kopiur-ui skeleton — config, ops listener, static embedding`.

---

### Task 3: `auth/` — identity, proxy secret, impersonating client cache, CSRF, redaction

**Files:** fill `crates/ui/src/auth/{mod,identity,proxy_secret,impersonate,csrf,redact}.rs`.

**Interfaces produced:**
```rust
// identity.rs
#[derive(Debug, Clone, PartialEq, Eq, Hash)] pub struct Identity { pub user: String, pub groups: Vec<String> /* sorted, deduped */, pub email: Option<String>, pub extra: BTreeMap<String, Vec<String>>, pub source: IdentitySource }
#[derive(Debug, thiserror::Error)] pub enum AuthError { NoIdentity { expected_header: Option<String> }, InvalidHeaderValue { header: String, reason: String }, ForbiddenPrincipal { principal: String, why: &'static str }, TooManyGroups { count: usize, max: usize }, ProxySecretMissing, ProxySecretMismatch }
pub fn extract_identity(headers: &HeaderMap, cfg: &AuthConfig) -> Result<Identity, AuthError>;
pub fn is_forbidden_principal(p: &str) -> bool;   // starts_with("system:") && p != "system:authenticated"
// proxy_secret.rs
pub const PROXY_TOKEN_HEADER: &str = "x-kopiur-proxy-token";
pub fn verify_proxy_secret(headers: &HeaderMap, expected: Option<&[u8]>) -> Result<(), AuthError>;  // constant_time_eq; None expected → Ok
// impersonate.rs
#[derive(Clone)] pub struct ImpersonateLayer { identity: Arc<Identity>, extra_keys: Arc<[String]> }
impl<S> tower::Layer<S> for ImpersonateLayer { type Service = Impersonate<S>; … }  // Impersonate<S>: removes every header whose name starts with "impersonate-", inserts Impersonate-User, one Impersonate-Group per group, Impersonate-Extra-<key> for configured keys present in identity.extra
pub struct ClientCache { … bounded LRU: Mutex<HashMap<IdentityKey, Entry{client, last_used}>>, size, ttl … }
impl ClientCache { pub fn new(base: kube::Config, limits: CacheLimits, extra_keys: Vec<String>) -> Self; pub fn client_for(&self, id: &Identity) -> Result<kube::Client, kube::Error>; pub fn len(&self) -> usize; }
// csrf.rs
pub const REQUEST_HEADER: &str = "x-kopiur-request";
#[derive(Debug, thiserror::Error)] pub enum CsrfError { MissingRequestHeader, WrongContentType { got: String }, CrossSite { sec_fetch_site: String }, OriginMismatch { origin: String, host: String } }
pub fn require_mutation_headers(headers: &HeaderMap, has_body: bool) -> Result<(), CsrfError>;
pub fn require_same_site_navigation(headers: &HeaderMap) -> Result<(), CsrfError>;  // for GET /file: Sec-Fetch-Site absent|same-origin|none
// redact.rs
pub const STRIPPED_HEADERS: &[&str] = &["cookie", "authorization", "x-forwarded-access-token", "x-forwarded-authorization"];
pub fn strip_sensitive(headers: &mut HeaderMap);
pub fn redact_text(s: &str) -> String;   // masks values following AWS_/KEY/TOKEN/PASSWORD/SECRET (case-insensitive) up to whitespace
// mod.rs
pub struct AuthState { pub cfg: AuthConfig, pub clients: ClientCache }
pub async fn identity_middleware(State(app): State<AppState>, mut req: Request, next: Next) -> Response;  // strip_sensitive → verify_proxy_secret (header mode) → extract_identity → insert Identity into extensions → metrics.requests_total; errors → Problem (401/400/403)
pub struct CurrentIdentity(pub Identity); impl FromRequestParts<AppState> for CurrentIdentity (reads the extension; 500 problem if absent — programming error)
pub async fn mutation_guard(req: Request, next: Next) -> Response;   // for POST/DELETE routers: require_mutation_headers
```
Identity rules (tests first, a table): no headers + header mode → `NoIdentity`; anonymous-only → Anonymous identity; header mode + fallback configured + header absent → Anonymous; header mode + fallback NOT configured + absent → `NoIdentity`; groups: every occurrence of the groups header split on separator, trimmed, empty dropped, deduped, sorted; `system:authenticated` appended if absent; `system:masters` or `system:*` (except authenticated) anywhere → `ForbiddenPrincipal`; `allowed_groups` set → groups intersected (a user with zero allowed groups still gets `system:authenticated`); non-visible-ASCII or >512 bytes → `InvalidHeaderValue`; >64 groups → `TooManyGroups`; inbound `Impersonate-User: system:admin` has no effect (test asserts identity.user is the configured header's value and the layer removes it).

`ImpersonateLayer` test: build `tower::service_fn` capturing the request; wrap; `oneshot` a request that already carries `Impersonate-User: evil` and `Impersonate-Group: system:masters`; assert outgoing headers are exactly `Impersonate-User: <user>` + N `Impersonate-Group` in sorted order + configured extras only. `ClientCache` tests: hit/miss counting via `len()`, evict at size, evict after ttl (inject a `now` fn), group order insensitivity (same key). `client_for` builds `kube::client::ClientBuilder::try_from(base.clone())?.with_layer(&layer).build()` — note kube's `ClientBuilder::try_from(Config)`; keep `base.auth_info.impersonate*` untouched (None).

- [ ] Steps: tests → implement → `cargo test -p kopiur-ui auth::` → clippy/fmt → commit `feat(ui): trusted-header identity, proxy secret, impersonating client cache, CSRF gate`.

---

### Task 4: `cache/` — reflector stores + SubjectAccessReview gate + `Source`

**Files:** fill `crates/ui/src/cache/{mod,stores,authz}.rs`.

**Interfaces produced:**
```rust
// stores.rs
pub struct Stores { pub repositories: Store<Repository>, pub cluster_repositories: Store<ClusterRepository>, pub policies: Store<SnapshotPolicy>, pub snapshots: Store<Snapshot>, pub schedules: Store<SnapshotSchedule>, pub restores: Store<Restore>, pub maintenances: Store<Maintenance>, pub repository_replications: Store<RepositoryReplication>, pub snapshot_replications: Store<SnapshotReplication> }
pub fn start(client: kube::Client, metrics: Arc<UiMetrics>) -> (Stores, Vec<JoinHandle<()>>);   // one reflector(writer, watcher(Api::all(client), Config::default()).default_backoff()) per kind, driven by `.for_each(|_| ready(()))`; cache_objects gauge updated from store.len() every event
pub async fn wait_ready(stores: &Stores) -> Result<(), WriterDropped>;   // joins every store.wait_until_ready()
// authz.rs
pub enum Visibility { All, Namespaces(BTreeSet<String>) }
pub struct SarCache { client: kube::Client /* UI SA */, ttl: Duration, inner: Mutex<HashMap<SarKey, (bool, Instant)>>, metrics: Arc<UiMetrics> }
pub struct SarKey { pub user: String, pub groups: Vec<String>, pub verb: &'static str, pub group: &'static str, pub resource: &'static str, pub namespace: Option<String>, pub name: Option<String> }
impl SarCache { pub fn new(client, ttl, metrics) -> Self; pub async fn allowed(&self, id: &Identity, verb: &'static str, resource: &'static str, namespace: Option<&str>, name: Option<&str>) -> Result<bool, OpsError>; pub async fn visible_namespaces(&self, id: &Identity, resource: &'static str, candidate_namespaces: &BTreeSet<String>) -> Result<Visibility, OpsError>; }
// group is always "kopiur.home-operations.com" for the 9 kinds; `allowed` posts a `SubjectAccessReview` with spec.user/groups/extra from the identity and `resource_attributes { verb, group, resource, namespace, name }`.
// mod.rs
pub enum Source { Cache { stores: Stores, sar: SarCache }, Impersonated }
pub struct Loaded<K> { pub items: Vec<Arc<K>> }
impl Source {
  pub async fn list<K: KopiurKind>(&self, id: &Identity, client: &kube::Client, namespace: Option<&str>) -> Result<Vec<Arc<K>>, OpsError>;  // Cache: state() filtered by Visibility (cluster-wide list SAR first; if denied, per-namespace SARs over namespaces present in the store); Impersonated: Api::<K>::all/namespaced(client).list()
  pub async fn get<K: KopiurKind>(&self, id: &Identity, client: &kube::Client, namespace: Option<&str>, name: &str) -> Result<Option<Arc<K>>, OpsError>;  // Cache: SAR get on (ns,name) then store.get; Impersonated: Api::get_opt
}
pub trait KopiurKind: kube::Resource<DynamicType = ()> + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static { const PLURAL: &'static str; fn store(stores: &Stores) -> &Store<Self>; }  // impl for the 9 kinds (exhaustive by construction — one impl each)
```
Tests first: `SarCache` with a fake client is not cheap — test the pure parts: `SarKey` canonicalization (group order), TTL expiry using an injected clock, `Visibility` filtering of a `Vec<Arc<K>>` by namespace (`filter_visible(items, &Visibility)`), and `KopiurKind::PLURAL` for all 9. `Source::list` for `Impersonated` is exercised in e2e (PR5). Note kube 4 `watcher::Config::default()` and `.default_backoff()`; do not enable streaming lists.

- [ ] Steps: tests → implement → `cargo test -p kopiur-ui cache::` → clippy/fmt → commit `feat(ui): reflector cache with SubjectAccessReview gating`.

---

### Task 5: `api/` — problem mapping + all read endpoints

**Files:** fill `crates/ui/src/api/{mod,problem,me,graph,status,repositories,snapshots,policies,schedules,restores,maintenance,replications,doctor,gates,events}.rs`. Every handler: `async fn handler(State(app), CurrentIdentity(id), Path/Query) -> Result<Json<T>, Problem>`; each module has `pub fn router() -> Router<AppState>` mounted by T8; IO in `load_*` (uses `app.auth.clients.client_for(&id)` + `app.source`), pure `view_*` fns tested with YAML fixtures via `kopiur_api::testutil::from_yaml` (enable feature `testutil` in ui dev-deps).

**Interfaces produced:**
```rust
// problem.rs
impl Problem /* from ui-model, via a local newtype `ApiError(Problem)` because of orphan rules */ 
pub struct ApiError(pub Problem);
impl IntoResponse for ApiError;  // status from problem.status, content-type application/problem+json, Cache-Control no-store, plus WWW-Authenticate: Kopiur-Proxy on 401
impl From<OpsError> for ApiError;  // exhaustive over OpsErrorKind: Forbidden→403 (fix: "ask a cluster admin to bind kopiur-ui-user, kopiur-ui-editor or kopiur-ui-viewer to your user or group"), NotFound→404, KindNotInstalled→503, Admission→422, Invalid→400, Conflict→409, Timeout→504, Upstream→502, Internal→500; type = "urn:kopiur:problem:<kind-kebab>"
impl From<AuthError> for ApiError;  // NoIdentity→401 "urn:kopiur:problem:no-identity" (fix names the expected header), InvalidHeaderValue→400, ForbiddenPrincipal→403, TooManyGroups→400, ProxySecretMissing/Mismatch→401 "urn:kopiur:problem:proxy-secret"
impl From<CsrfError> for ApiError;  // 403 "urn:kopiur:problem:csrf"
pub fn problem(status: u16, kind: &str, what: impl Into<String>, why: impl Into<String>, fix: impl Into<String>) -> ApiError;
// graph.rs (pure builder + handler)
pub struct GraphInputs<'a> { pub repositories: &'a [Arc<Repository>], pub cluster_repositories: &'a [Arc<ClusterRepository>], pub policies: &'a [Arc<SnapshotPolicy>], pub snapshot_replications: &'a [Arc<SnapshotReplication>], pub repository_replications: &'a [Arc<RepositoryReplication>], pub maintenances: &'a [Arc<Maintenance>], pub now: DateTime<Utc> }
pub fn build(inputs: &GraphInputs) -> RepositoryGraph;
pub fn repository_health(phase: Option<&RepositoryPhase>, suspended: bool, gates: &[GateHit]) -> Health;   // exhaustive over RepositoryPhase incl. Unknown(_) → Health::Unknown; suspended → Suspended; any gate with severity Fail → Failed; Degraded → Degraded; Ready → Healthy; Pending/Initializing → Pending
pub fn gate_hits(conditions: &[Condition], covers: fn(GateScope) -> bool) -> Vec<GateHit>;  // over kopiur_api::gates::STRUCTURAL_GATES
// views shared helpers (api/mod.rs)
pub fn repo_phase_view(p: &RepositoryPhase) -> RepositoryPhaseView; pub fn snapshot_phase_view(..); pub fn restore_phase_view(..); pub fn replication_phase_view(..) (each exhaustive; Unknown(s) → Unknown{raw:s})
pub fn origin_view(o: Origin) -> OriginView; pub fn condition_view(c: &Condition) -> ConditionView; pub fn repo_ref_display(r: &RepositoryRef, owner_ns: Option<&str>) -> String  // "Repository/ns/name" | "ClusterRepository/name" via kopiur_api::common::repo_key
```
Routes and their view logic:
- `GET /api/v1/me?namespace=` → `Me`; capabilities via `SarCache::allowed` (cache mode) or impersonated `SelfSubjectAccessReview` (impersonated mode) for: create snapshots, delete snapshots, create restores, patch snapshotpolicies, create pods/exec — in the given namespace (or cluster-wide when absent).
- `GET /api/v1/graph` → `graph::build`. Node ids `Repository/<ns>/<name>`, `ClusterRepository/<name>`, `Backend/<repl-ns>/<repl-name>` (label `Backend::kind_str()` + non-secret location: bucket/path/host only), `Policy/<ns>/<name>`, `Namespace/<name>`, `NamespaceSelector/<cr-name>`. Edges as spec; dangling refs → `missing: true` node with `Health::Failed`.
- `GET /api/v1/status?namespace=` → `StatusOverview { report: serde_json::to_value(kopiur_ops::status::build_report(&inputs, None)), now }` where inputs come from `Source::list` for the 7 kinds (snapshot_replications `Some(..)` — cache mode never 404s; impersonated mode maps a 404 list to `None` like the CLI).
- `GET /api/v1/repositories?namespace=` → `Vec<RepositorySummary>`; `GET /api/v1/repositories/{kind}/{name}?namespace=` (kind path enum `repository|cluster-repository`, exhaustive) → `RepositoryDetail` (policies via `kopiur_api::snapshot_policy::repository_refs` + `repo_key` equality; replications in/out by `repo_key`; maintenance via `kopiur_ops::maintenance::covers_repository`; sessions via `find_session_job` under the user's client).
- `GET /api/v1/snapshots?repository&repositoryKind&repositoryNamespace&policy&origin&phase&namespace&offset&limit` → `Page<SnapshotRow>`: filter (repository via `RepoFilter` — resolve the repo UID with the user's client through `kopiur_ops::snapshots::resolve_repo_filter_for`; policy via `CONFIG_LABEL` or `spec.policyRef.name`; origin exhaustive; phase exhaustive with `unknown` matching `Unknown(_)`), sort by `sort_key` desc, cap: if the filtered set exceeds `snapshot_list_cap` → 422 `urn:kopiur:problem:list-too-large` (fix: "filter by repository or policy"); paginate.
- `GET /api/v1/snapshots/{namespace}/{name}` → `SnapshotDetail`: lineage = other snapshots (same namespace) whose `status.copiedFrom.sourceManifestId == this.kopiaSnapshotID` (copies) and this one's own `copiedFrom`; retention preview via `kopiur_api::retention::select_kept` over the policy's succeeded snapshots (implement `SnapshotLike for &Snapshot` in this module: id = kopia id, time = `status.timing.endTime` parsed, pinned = `spec.pin`) when `spec.policyRef` resolves and the policy has `spec.retention`; `browsable` = phase Succeeded/Discovered/Adopted with a kopia id; `browse_blocker` text otherwise; `log_tail` and `failure.message` passed through `redact_text`.
- `GET /api/v1/policies?namespace=`, `/policies/{namespace}/{name}` → rows/detail (schedules = schedules whose `policyRef` names it, or whose `policySelector` matches its labels via `kopiur_api::expand::label_selector_string` only for display; recent_snapshots = last 10 by `sort_key`).
- `GET /api/v1/schedules`, `/restores` (+ `/{ns}/{name}`), `/maintenance`, `/replications` (`{ repository: [...], snapshot: [...] }`), `/doctor?stuckThreshold=&failureLookback=` (→ `kopiur_ops::doctor::run_all` with the user's `OpsCtx { client, namespace: operator_ns or "default", scope: All }`, mapped to `DoctorReportView`), `/gates` (static from `STRUCTURAL_GATES`), `/events?namespace=&kind=&name=` (`events.k8s.io/v1` list with field selector `regarding.kind=<kind>,regarding.name=<name>` under the user's client).

Tests first (fixtures in `crates/ui/tests/fixtures/*.yaml` loaded with `from_yaml`): graph fixture from the spec (2 Repositories, 1 ClusterRepository with `allowedNamespaces` List + a second with Selector + a third with All, 1 SnapshotReplication, 1 RepositoryReplication with a bare S3 destination, 1 seeded repo, 1 multi-repo policy, 1 policy with a dangling ref, one repo with `phase: Weird`) asserting exact node/edge id sets and healths; `repository_health` full table; phase view mappings exhaustive incl. `Unknown`; snapshot filter+paginate+cap; lineage; retention preview; `ApiError` mapping one case per `OpsErrorKind` (test builds each via an exhaustive `match` so a new kind fails to compile).

- [ ] Steps: tests → implement → `cargo test -p kopiur-ui api::` → clippy/phase-check/fmt → commit `feat(ui): read API — graph, status, resources, doctor, problem+json`.

---

### Task 6: `browse/` — session pool, tree, streaming download

**Files:** fill `crates/ui/src/browse/{mod,session_pool,download}.rs`.

**Interfaces produced:**
```rust
// session_pool.rs
pub struct SessionKey { pub namespace: String, pub kind: RepositoryKind, pub repo_namespace: Option<String>, pub repo_name: String }
pub struct SessionPool { starts: tokio::sync::Semaphore, inflight: Mutex<HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>>, exec_global: Arc<Semaphore>, exec_per_identity: Mutex<HashMap<String /*user*/, Arc<Semaphore>>>, limits: SessionLimits }
impl SessionPool { pub fn new(limits: SessionLimits) -> Self; pub async fn ensure(&self, ctx: &OpsCtx, target: &BrowseTarget, image: &MoverImageSource, progress: &dyn SessionProgress) -> Result<ExecSession, OpsError>  /* single-flight per key + starts permit */; pub async fn exec_permit(&self, user: &str) -> Result<ExecPermit, ApiError> /* 429 urn:kopiur:problem:too-many-requests when either semaphore is exhausted (try_acquire) */; }
pub struct ExecPermit { _global: OwnedSemaphorePermit, _user: OwnedSemaphorePermit }
pub fn session_info(job: &Job, ttl: Duration, reused: bool) -> SessionInfo;   // expires_at = creationTimestamp + ttl
// mod.rs — handlers (mounted by T8):
//   POST   /api/v1/snapshots/{ns}/{name}/session   (CSRF-gated) → 201 SessionInfo (ensure; reused=true when found non-terminal)
//   GET    /api/v1/snapshots/{ns}/{name}/session   → SessionInfo | 404 problem `session-required`
//   DELETE /api/v1/snapshots/{ns}/{name}/session   (CSRF-gated) → 204
//   DELETE /api/v1/repositories/{kind}/{name}/session?namespace=&sessionNamespace=  (CSRF-gated) → 204
//   GET    /api/v1/snapshots/{ns}/{name}/tree?path=&offset=&limit=   → DirListing; 409 `session-required` when no live session (never creates one)
//   GET    /api/v1/snapshots/{ns}/{name}/file?path=   → streamed bytes; requires require_same_site_navigation; 409 session-required; 413 `download-too-large` above download_max_bytes
pub struct CappedSink<W> { … }  // AsyncWrite wrapper: errors with OpsError::StreamIo("manifest exceeded KOPIUR_UI_MAX_MANIFEST_BYTES") past the cap — used for exec_capture of manifests (a `capture_capped(session, cmd, cap)` helper here, since ops' exec_capture is uncapped)
pub struct CachedAccess { session: ExecSession, manifest_cap: u64 }  impl SnapshotAccess for CachedAccess { type Error = OpsError; … list_dir uses capture_capped }
pub fn entry_view(e: &DirEntry) -> DirEntryView;  // kind: "f"→File, "d"→Dir, "s"→Symlink, other→Other{raw}
pub fn content_disposition(name: &str) -> HeaderValue;  // attachment; filename*=UTF-8''<percent-encoded basename>; strips '/', '"', control chars
// download.rs
pub fn stream_file(session: ExecSession, oid: ObjectId, size: u64, permit: ExecPermit, metrics: Arc<UiMetrics>) -> axum::body::Body;
// tokio::io::duplex(64 KiB); spawned task: exec_stream(ShowObject{oid}, &mut ExactSink{writer, expected: size}) — ExactSink errors `download-overrun` on the first byte past `size`; on task end, if written < size → metrics.download_incomplete_total += 1 and log; per-chunk timeout 60s inside the copy; body = Body::from_stream(ReaderStream::new(reader)) with Content-Length = size
```
Tests first: `entry_view` exhaustive; `content_disposition` cases (`a b.txt`, `../evil`, `q"uote`, unicode `ü.txt`); `CappedSink` stops at cap; `ExactSink` overrun/short accounting; single-flight (two concurrent `ensure` closures on one key run the inner future once — test with a counter and a fake `ensure_fn`); `exec_permit` 429 when exhausted; `session_info` expiry arithmetic.

- [ ] Steps: tests → implement → `cargo test -p kopiur-ui browse::` → clippy/fmt → commit `feat(ui): browse plane — sessions, directory listing, capped streaming download`.

---

### Task 7: `actions/` — mutations

**Files:** fill `crates/ui/src/actions/mod.rs`.

Routes (all under the CSRF `mutation_guard`, all impersonated, field manager `FIELD_MANAGER`):
- `POST /api/v1/actions/snapshot-now` `SnapshotNowBody` → `SnapshotNowRequest` (deletion_policy None, failure_policy None) → get the policy (impersonated `Api<SnapshotPolicy>::get`) → `plan_snapshots` → `create_snapshots` → 201 `ActionReceipt { kind: "Snapshot", created }`.
- `POST /api/v1/actions/restore` `RestoreBody` → `RestoreRequest` (map `RestoreSourceBody`/`RestoreTargetBody` exhaustively to `kopiur_api::{RestoreSource, RestoreTarget}`; `Pvc` → `PvcTemplate { name, storageClassName, size }` — read the exact field names from `crates/api/src/restore.rs`) → `build_restore` → `create_restore` → 201 receipt.
- `DELETE /api/v1/snapshots/{ns}/{name}` → impersonated `Api<Snapshot>::delete` (background propagation) → 202 receipt; `note` = "deletion is held by the repository's mass-deletion breaker" when the Snapshot carries condition `DELETION_HELD_CONDITION` true after the delete (re-get once).
- `POST /api/v1/actions/suspend` → `SuspendableKind` from `body.kind` (parse exhaustive: `policy|schedule|repository|cluster-repository|replication|snapshot-replication`, else 400) → `kopiur_ops::suspend::set_suspended` → 200 receipt.
- `POST /api/v1/actions/maintenance-run` → `MaintenanceTarget` (name xor repository; both/neither → 400) → `resolve` → `request_run(mode)` → 202 `{ requested_at }`.
- `POST /api/v1/actions/replication-run` → `detect_kind` → `request_run` → 202.
- `POST /api/v1/actions/scan-catalog` → `request_scan` → 202.
Bodies: `axum::Json<T>` with a custom rejection → 400 problem `urn:kopiur:problem:invalid-body` carrying serde's message; `deny_unknown_fields` is on the types.

Tests first: body parsing rejections (unknown field, wrong kind string, both/neither target); `RestoreBody` → `RestoreRequest` mapping for all source×target combinations (compare against the CLI's conversion tests semantics); receipts serialize with camelCase.

- [ ] Steps: tests → implement → `cargo test -p kopiur-ui actions::` → clippy/fmt → commit `feat(ui): action endpoints — snapshot now, restore, delete, suspend, run, scan`.

---

### Task 8: Wiring, router-level tests, security headers, readiness, smoke

**Files:** modify `crates/ui/src/lib.rs` (`app()` mounts every router under `/api/v1` with layers), `crates/ui/src/main.rs` (construct `AuthState`, `Source` per `cache_enabled`, `SessionPool`; start reflectors; readiness self-check), `crates/ui/tests/router.rs`.

`app()` layer order (outer→inner): `TraceLayer` (custom `make_span_with` recording method+path only), `SetResponseHeaderLayer` ×4 on `/api` (`Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Content-Security-Policy: default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'`), `Referrer-Policy: same-origin` on everything, `CompressionLayer` (predicate: skip when path ends with `/file`), `RequestBodyLimitLayer(64 KiB)`, `TimeoutLayer(30s)` on all `/api` routes except `/file` (mount `/file` on a separate un-timed router), `ConcurrencyLimitLayer(256)` on `/api`, `identity_middleware` on `/api`, optional `CorsLayer` when `cors_origins` non-empty. Fallback → `spa_fallback`; unknown `/api/*` → 404 problem.

Readiness: after building clients, run a `SelfSubjectAccessReview` (under the UI SA) for `impersonate users` and `impersonate groups`; set `readiness.impersonation_ok`; in cache mode await `wait_ready` (with a 2-minute timeout → log and keep `cache_ready=false`, `/readyz` says why); `web_placeholder = static_files::is_placeholder()` (readyz reports it but stays 200 — a placeholder page is a valid dev state; document in the response text).

Router tests (`tests/router.rs`, using `tower::ServiceExt::oneshot` against `app()` with a config in header mode + proxy secret; the kube client is a `kube::Client::new(tower::service_fn(...), "default")` that returns 200 `{}` for anything — enough for the identity/CSRF/header layers): `GET /api/v1/gates` without headers → 401 problem+json with `WWW-Authenticate: Kopiur-Proxy`; with user header but wrong proxy token → 401 `proxy-secret`; with `X-Forwarded-User: system:admin` → 403 `forbidden-principal`; `POST /api/v1/actions/suspend` without `X-Kopiur-Request` → 403 `csrf`; with `Sec-Fetch-Site: cross-site` → 403; `GET /unknown-path` → `text/html` index; `GET /api/v1/nope` → 404 problem; every `/api` response carries `Cache-Control: no-store`, `X-Frame-Options: DENY`, CSP with `frame-ancestors 'none'`.

Smoke (manual, record output in the report): against a local kind cluster with the operator installed, run `KOPIUR_UI_ANONYMOUS_USER=kubernetes-admin KOPIUR_UI_ANONYMOUS_GROUPS=kopiur:dev KOPIUR_UI_CACHE=false KOPIUR_NAMESPACE=kopiur-system cargo run -p kopiur-ui` with a ClusterRoleBinding of `cluster-admin` to group `kopiur:dev`, then `curl :8090/api/v1/graph` and `/api/v1/status`. If no kind cluster is available in this environment, say so in the report; PR5's e2e covers it.

- [ ] Steps: router tests (fail) → wiring → tests pass → `mise run ci` (`mise run test clippy build gen-check wiring-check phase-check fmt-check complexity-check`) → commit `feat(ui): wire the router, security headers, readiness`.

---

### Task 9: Close the API contract gaps the SPA needs

Added after Task 8 by a two-agent adversarial gap analysis of the SPA plan against this backend (reports: `.superpowers/sdd/2026-09-08-web-ui-pr3-spa-gap-api.md`, `…-gap-build.md`). Each item below is a verified mismatch between what the SPA must render and what the server can currently say. Fix them in code here rather than scaling the UI down — a limitation shipped into the wire contract becomes permanent.

**Files:** `crates/ui-model/src/{views,identity,graph}.rs`, `crates/ui-model/src/lib.rs` (`export_all` + its exact-count test), `crates/ui/src/api/{mod,me,doctor,snapshots,replications,repositories}.rs`, `crates/ui/src/browse/mod.rs`, `crates/api/src/retention.rs`.

**Interfaces:**
- Consumes: `kopiur_api::retention::{retention_view, retention_group_key, retention_buckets, select_kept}` (moved into `kopiur-api` in Task 5), `kopiur_api::gates::STRUCTURAL_GATES`.
- Produces: the wire additions below. Every new or changed wire type is exported by `export_all`, so the exact-count assertion in `crates/ui-model/src/lib.rs` moves with it — update the number AND the reason in its assertion message.

- [ ] **Item 1 — the retention preview must show the whole bucket, not one verdict.**
  `RetentionPreview` is `{ kept, reasons, computedAt }` for the single snapshot on the detail route. The SPA cannot draw "which snapshots this policy will keep and which it will prune", which is the one screen where a misreading costs a restore point.
  Add `GET /api/v1/snapshots/{namespace}/{name}/retention` returning a new `RetentionPlan` wire type:
  ```rust
  /// Every snapshot competing for the same retention buckets as one snapshot,
  /// with the verdict `select_kept` gives each. The bucket key is what the
  /// controller groups by, so a fan-out policy shows one group per source.
  pub struct RetentionPlan {
      /// Bucket key (the `retention_group_key` value) → its candidates, newest first.
      pub buckets: Vec<RetentionBucket>,
      /// The policy the plan was computed from, and when.
      pub policy: PolicyRef,
      pub computed_at: String,
      /// True when the policy sets no retention: nothing is pruned by GFS.
      pub unbounded: bool,
  }
  pub struct RetentionBucket {
      /// `retention_group_key` — the source path, plus the repository when the policy is multi-repo.
      pub key: String,
      pub candidates: Vec<RetentionCandidate>,
  }
  pub struct RetentionCandidate {
      pub namespace: String,
      pub name: String,
      pub end_time: Option<String>,
      pub kept: bool,
      /// Which GFS rule holds it, e.g. `keepDaily`. Empty when it is pruned.
      pub rules: Vec<String>,
      pub pinned: bool,
      /// True for the snapshot the request was about, so the SPA can highlight it.
      pub subject: bool,
  }
  ```
  It must use exactly the controller's population and grouping (`retention_view` + `retention_buckets` + `spec.pin`), so the preview cannot disagree with the prune. Test: the 7-PVC fan-out fixture yields 7 buckets, and the subject snapshot appears in exactly one.
  Also fix `RetentionPreview.reasons`: the field doc promises `"keepDaily slot 3"` and the code pushes bare rule names — emit the slot, or correct the doc. Say which you did and why.

- [ ] **Item 2 — `Capabilities` must cover every mutating endpoint.**
  It carries five booleans; there are seven mutating endpoints and four suspendable kinds. Add flags for `maintenance-run`, `replication-run`, `scan-catalog`, and suspend on each of `SnapshotPolicy`, `SnapshotSchedule`, `Repository`, `ClusterRepository` (the existing `patchPolicies` covers only the first). Each is a `SelfSubjectAccessReview` on the same verb+resource the handler actually performs — read the handler, do not guess. Document on the type that these are namespace-scoped and that the SPA must re-fetch `/me` per namespace. Test: a fixture where the review allows exactly one verb sets exactly one flag true.

- [ ] **Item 3 — one repository-kind vocabulary.**
  `/repositories/{kind}/{name}` takes kebab-case `repository`/`cluster-repository` case-sensitively; `DELETE /repositories/{kind}/{name}/session` takes `repository`/`clusterrepository` case-insensitively and rejects the hyphen. The same-looking segment means two things. Make both accept the same set (kebab canonical, `clusterrepository` and any case tolerated), share one parser, and test that every spelling resolves on both routes. `RepositorySummary`/`RepositoryDetail` gain `kindPath: String` carrying the canonical segment, so the SPA links without a local mapping table.

- [ ] **Item 4 — `/doctor` gains `namespace`, and both query types reject unknown parameters.**
  `DoctorQuery` has no `namespace` and no `deny_unknown_fields`, so `?namespace=x` is silently ignored — a filter that does nothing is worse than none. Add `namespace: Option<String>` (scoping the checks that can be scoped; document which stay cluster-wide) and `#[serde(deny_unknown_fields)]` on `DoctorQuery`, `NamespaceQuery`, and every other read query type that lacks it, so a typo is a 400 problem instead of silence. Test one ignored-parameter case per query type.

- [ ] **Item 5 — publish the download limits so the SPA can refuse before it navigates.**
  `download_size` answers 413 `download-too-large` / 422 `download-size-unknown` as `application/problem+json` on a top-level navigation, which the browser renders as raw JSON — the SPA never sees it. Add `downloadMaxBytes: i64` and `manifestMaxBytes: i64` to `SessionInfo` (the SPA already fetches it before browsing) so the file table can disable an oversized entry with the reason. Keep the server-side refusal exactly as it is; this is a second gate, not a replacement. Test: `SessionInfo` carries the configured values, not the defaults, when the config differs.

- [ ] **Item 6 — `ReplicationsView` moves into `kopiur-ui-model` and is exported.**
  It is declared in `crates/ui/src/api/replications.rs`, so `export_all` never emits it and the SPA would have to hand-write the one type the plan's own constraint forbids. Move it, export it, bump the count assertion.

- [ ] **Item 7 — the restore detail route returns a detail type.**
  `GET /restores/{namespace}/{name}` returns the same `RestoreRow` as the list. Add `RestoreDetail` carrying what the list omits: conditions, the claim, progress if the CR reports any, the source and target as resolved, and the failure view. If a field genuinely has no source in the CR, leave it out rather than adding a field that is always `None` — and say so in the report.

- [ ] **Item 8 — `deletionPolicy` and the delete receipt must not overstate.**
  `SnapshotRow.deletionPolicy` is `Option<String>`; document that an absent value means the CR does not set one, and that the SPA must not render a default. Confirm (with a test) that the delete receipt's `note` carries the mass-deletion-breaker text when the breaker holds, since the SPA will render it as the answer to "why is it still there".

- [ ] **Item 9 — document the enum wire shapes for the SPA.**
  ts-rs renders externally-tagged enums as heterogeneous `string | object` unions, and the fallback variant is not uniformly named: `Health::Unknown` is a unit variant, `EntryKind`'s fallback is `Other { raw }`, and `OriginView`/`GateSeverityView` have none. Add a module doc to `crates/ui-model/src/lib.rs` listing every exported enum, its variant shapes, and whether it has a fallback — the SPA's exhaustiveness strategy depends on it. No code change; this is the contract the next milestone reads.

- [ ] **Verify:** `cargo test --workspace --locked`, `mise run clippy fmt-check phase-check wiring-check gen-check complexity-check`. Every new endpoint gets a router-level test; every new wire field gets a test that it is populated from the source the doc claims. Commit as `feat(ui): close the API contract gaps the SPA needs`.
