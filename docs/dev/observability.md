# Observability

Kopiur exposes Prometheus metrics, and can additionally export OpenTelemetry (OTLP) traces, logs, and metrics. The implementation lives in the **`kopiur-telemetry`** crate, shared by the controller, webhook, and mover.

## The one idea: instrument once, two readers

Metrics are instrumented **once** against the OpenTelemetry metrics API. A single `SdkMeterProvider` fans out to two readers:

1. an **`opentelemetry-prometheus` exporter** that populates a `prometheus::Registry` behind the always-on `/metrics` pull endpoint (so a `ServiceMonitor` scrapes the pods directly — no collector required), and
2. an **OTLP `PeriodicReader`** that _pushes_ the same measurements to a collector — added only when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.

Recording a value updates both; there is no double instrumentation. Traces (the controller's `#[instrument]` reconcile spans) and logs (bridged from `tracing` events) export over OTLP via `tracing-opentelemetry` and `opentelemetry-appender-tracing`.

**OTLP is env-gated and off by default.** With no endpoint configured the behavior is identical to fmt-only logging + the Prometheus pull, so the hermetic test suite stays offline. A misconfiguration is logged with an actionable error and **degrades** to fmt-logging + the Prometheus pull rather than crashing a backup operator — unless `KOPIUR_OTEL_STRICT=true`, which makes it fail fast.

## Logging (stdout / `kubectl logs`)

Every component writes structured `tracing` events to **stdout** via an fmt layer installed by `kopiur_telemetry::init_tracing`. No collector is needed — this is the always-on path that `kubectl logs` shows. Reconcilers carry a `#[instrument]` span with `kind`, `namespace`, and `name`, so each line is attributable to the resource being reconciled.

**Level** — the standard `RUST_LOG` filter (default `info`). Per-target directives work: `RUST_LOG=info,kopia=debug` keeps the operator at `info` while surfacing **kopia's own progress and log output** (emitted line-by-line under the `kopia` target) in mover and controller logs. Without it, kopia's output is captured for the failure tail but not printed.

**Format** — `KOPIUR_LOG_FORMAT` selects `text` (human-readable, default) or `json` (one structured object per line for Loki/ELK/Datadog). An unrecognized value degrades to `text` with a warning. In `text` mode ANSI color is suppressed when stdout is not a TTY (i.e. in a container), so `kubectl logs` stays clean.

**Movers inherit the controller's config.** The controller forwards both `RUST_LOG` and `KOPIUR_LOG_FORMAT` (alongside the OTLP vars) onto every mover `Job`, so a backup/restore Job logs at the same level and format — set it once on the controller.

Helm knobs (`logging.*`, applied to controller + webhook, and through to movers):

| Key              | Default | Effect                                    |
| ---------------- | ------- | ----------------------------------------- |
| `logging.level`  | `info`  | sets `RUST_LOG` (e.g. `info,kopia=debug`) |
| `logging.format` | `text`  | sets `KOPIUR_LOG_FORMAT` (`text`/`json`)  |

```bash
# JSON logs everywhere, and show kopia's progress in mover logs:
helm upgrade --install kopiur oci://ghcr.io/home-operations/charts/kopiur -n kopiur-system \
  --set logging.format=json --set logging.level='info,kopia=debug'
```

## HTTP endpoints

| Component  | Endpoint                                                | Notes                                   |
| ---------- | ------------------------------------------------------- | --------------------------------------- |
| Controller | `GET /metrics`, `/healthz`, `/readyz` on `:8081` (axum) | probes hit the real health routes       |
| Webhook    | `GET /metrics` on its TLS port (8443)                   | plus `/healthz`, `/readyz`              |
| Mover      | none (short-lived Job)                                  | OTLP **push** only; flushed before exit |

## Metrics

All metrics are under the `kopiur_` namespace. The Prometheus exporter applies the OTel→Prometheus conventions, so a counter instrument named `kopiur_x` is exported as `kopiur_x_total`.

| Metric                                         | Type        | Labels                                     | Source                                                                                                                                                                                     |
| ---------------------------------------------- | ----------- | ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `kopiur_controller_reconciliations_total`      | counter     | `kind`                                     | every reconcile                                                                                                                                                                            |
| `kopiur_controller_reconcile_errors_total`     | counter     | `kind`, `class` (`transient`/`structural`/`terminal`) | `error_policy` (which also publishes a Warning Event on the failing object — see below)                                                                                          |
| `kopiur_controller_reconcile_duration_seconds` | histogram   | `kind`                                     | every reconcile                                                                                                                                                                            |
| `kopiur_kube_client_requests_total`            | counter     | `verb` (`get`/`list`/`watch`/`create`/`update`/`patch`/`apply`/`delete`/`deletecollection`/`other`), `group` (`""` = core), `kind` (resource plural, plus `/subresource`, or `other`), `client` (`main`/`exec`/`election`) | **every HTTP request the controller's kube clients send to the apiserver**, counted client-side by a tower layer under `ClientBuilder` (issue #382) — see [Kube API request attribution](#kube-api-request-attribution-issue-382) |
| `kopiur_watcher_restarts_total`                | counter     | `kind`                                     | a watch error on a stream kopiur drives itself (the referent metadata trigger streams + the shared Maintenance informer); each error backs the watcher off and restarts its watch          |
| `kopiur_resource_phase`                        | gauge (0/1) | `kind`, `namespace`, `name`, `phase`, plus `policy` on Snapshot series (omitted for discovered snapshots) | CR status; store-backed observable gauge, `1` for the **active** phase only — never a 0-valued series for inactive phases |
| `kopiur_snapshot_last_success_timestamp_seconds` | gauge       | `namespace`, `name`, `policy` (Snapshot only) | per-CR: mover-recorded `status.timing.endTime`, only while `phase == Succeeded`                                                                                                            |
| `kopiur_snapshot_consecutive_failures`           | gauge       | `namespace`, `name` (+ `repository`¹)      | trailing Failed before the latest Succeeded, per policy; `name` here is the SnapshotPolicy name. Since #345 M6 a **store-backed observable** derived from the Snapshot store (`spec.policyRef` attribution): the series exists iff the policy currently has ≥1 Snapshot CR, and keeps updating even when the policy reconcile is not running |
| `kopiur_snapshot_size_bytes`                     | gauge       | `namespace`, `name`, `policy` (Snapshot only) | per-CR: Backup `status.stats.sizeBytes`                                                                                                                                                  |
| `kopiur_snapshot_files`                          | gauge       | `namespace`, `name`, `policy` (Snapshot only) | per-CR: Backup file counts (absent when unknown)                                                                                                                                         |
| `kopiur_snapshot_duration_seconds`               | gauge       | `namespace`, `name`, `policy` (Snapshot only) | per-CR: Backup `status.timing.durationSeconds`                                                                                                                                           |
| `kopiur_policy_last_backup_success_timestamp_seconds` | gauge  | `namespace`, `policy` (+ `repository`¹)    | `endTime` (unix seconds) of the **latest Succeeded Snapshot per (namespace, policy)** — answers "latest per policy", which per-CR `max by` cannot (issue #172)                             |
| `kopiur_policy_last_backup_duration_seconds`     | gauge       | `namespace`, `policy` (+ `repository`¹)    | duration of the latest Succeeded Snapshot per (namespace, policy)                                                                                                                          |
| `kopiur_policy_last_backup_size_bytes`           | gauge       | `namespace`, `policy` (+ `repository`¹)    | logical size of the latest Succeeded Snapshot per (namespace, policy)                                                                                                                      |
| `kopiur_policy_last_backup_files`                | gauge       | `namespace`, `policy` (+ `repository`¹)    | file count of the latest Succeeded Snapshot per (namespace, policy), only when known                                                                                                       |
| `kopiur_snapshotpolicy_last_backup_success`      | gauge (0/1) | `namespace`, `policy` (+ `repository`¹)    | `1` if a policy's **most recent terminal backup** succeeded, `0` if it failed (absent until the first terminal run). `count(... == 1)` answers "how many of my N policies are currently healthy" — the per-policy STATE the run-counter below cannot (issue #196) |
| `kopiur_snapshots_completed_total`               | counter     | `result` (`succeeded`/`failed`/`unchanged`), `namespace`, `policy` (omitted when the Snapshot has no `policyRef`) | incremented once per terminal transition (not per reconcile); durable across CR deletion, so it answers time-windowed "how many backups completed" (issue #175). NOT a fleet-health count — summing it over a window counts every run (policies × runs), so use `kopiur_snapshotpolicy_last_backup_success` for "how many policies are OK" |
| `kopiur_snapshot_verified_timestamp_seconds`     | gauge       | `namespace`, `name`                        | SnapshotPolicy verification (ADR-0005 §4) → `status.lastVerified` of the most recent successful verify (quick or deep); alert on staleness like `last_success`                            |
| `kopiur_orphaned_snapshots_total`              | counter     | `namespace`                                | Orphan policy / skip-cleanup escape hatch                                                                                                                                                  |
| `kopiur_snapshot_deletion_failures_total`      | counter     | `namespace`                                | finalizer snapshot-delete failures                                                                                                                                                         |
| `kopiur_snapshot_deletions_total`              | counter     | `namespace`, `outcome` (`deleted`/`retained`/`orphaned`/`cascade_retained`) | `Snapshot` finalizer RESOLUTIONS (mass-deletion protection). Distinct from `kopiur_snapshot_deletion_failures_total`, which counts kopia delete-call failures, not resolutions — never sum them |
| `kopiur_snapshots_cascade_retained_total`      | counter     | `namespace`                                | narrower, always-alongside view of `kopiur_snapshot_deletions_total{outcome="cascade_retained"}` — a `Snapshot` retained by the schedule-deletion cascade guard; both are bumped by one helper so they can't drift |
| `kopiur_snapshot_delete_batch_jobs_total`      | counter     | `outcome` (`succeeded`/`failed`)           | a mass-deletion batch-delete mover Job reached a terminal outcome                                                                                                                          |
| `kopiur_snapshot_delete_batch_members_total`   | counter     | `outcome` (`deleted`/`failed`)             | per-member batch-delete outcome. `deleted` is bumped once per member as it drains its own finalizer off a SUCCEEDED Job; `failed` is bumped once per member at the single point a FAILED Job is reaped (a failed member never drains its own finalizer, so it has no other emission site). Independent of the whole-Job `..._batch_jobs_total` outcome |
| `kopiur_snapshot_deletions_pending_external`   | gauge       | `repo_kind`, `repo_name` (unpinned → `"unknown"`/`"unknown"`) | store-backed observable gauge: Snapshots being deleted that still carry the cleanup finalizer, by resolved repository. A cheaper, coarser approximation of the mass-deletion breaker's own count — unlike the breaker it includes operator prunes and never re-runs `plan_deletion`, so it may read higher than the threshold-relevant count |
| `kopiur_snapshot_deletions_held`               | gauge       | `repo_kind`, `repo_name` (unpinned → `"unknown"`/`"unknown"`) | store-backed observable gauge: the subset of `kopiur_snapshot_deletions_pending_external` currently HELD by the mass-deletion breaker (`DeletionHeld=True`)                                |
| `kopiur_replication_runs_total`                  | counter     | `kind` (`RepositoryReplication`/`SnapshotReplication`), `trigger` (`cron`/`manual`), `outcome` (`succeeded`/`failed`) | a replication run reached a terminal mover-Job outcome — the only business metric either replication kind emits. Counted ONCE per run: the reconcile's Job-outcome arms are reached zero-to-many times for a single run (a cron success is usually observed via the idle arm, a cron failure re-observed every retry pass), so the increment is keyed on a durable `run-counted` annotation stamped on the Job itself. `trigger=manual` is a `run-requested` run; a Job from a pre-#380 operator carries no trigger annotation and counts as `cron` |
| `kopiur_schedule_snapshots_created_total`        | counter     | `namespace`, `name`                        | SnapshotSchedule fires                                                                                                                                                                       |
| `kopiur_snapshot_refusals_total`                 | counter     | `namespace`, `name`, `reason`              | a backup refused by policy (`RepositoryReadOnly`, `PrivilegedMoverNotPermitted`); the reconcile itself returns Ok, so these never appear in `reconcile_errors`                              |
| `kopiur_secrets_projected_total`                 | counter     | `namespace`                                | credential projection copied a repository Secret into a mover namespace                                                                                                                    |
| `kopiur_work_spec_cms_swept_total`               | counter     | —                                          | legacy sweep deleted an orphaned per-run work-spec ConfigMap (#224)                                                                                                                        |
| `kopiur_projected_secrets_swept_total`           | counter     | —                                          | sweep deleted a per-run projected credential Secret left by pre-stable-naming versions (#231)                                                                                              |
| `kopiur_creds_secrets_reaped_total`              | counter     | `by`                                       | a projected credential copy reclaimed once no mover Job could still load it. `by=terminal` is the consuming CR's reconciler (the fast path), `by=sweep` the periodic backstop. **Never sum them** — in steady state the reconciler should get there first, so a sustained `by=sweep` rate is precisely the signal that its reap has stopped firing (#240). |
| `kopiur_projected_secrets_live`                  | gauge       | —                                          | projected credential copies alive right now, observed each sweep pass. The **population**, which is what a counter of projections cannot tell you: it rises identically whether or not copies are ever reclaimed, and that is why #240 ran for weeks unseen. `deriv(...[24h]) > 0` is the leak alarm.                                                      |
| `kopiur_snapshots_live`                          | gauge       | `namespace`, `name` (+ `repository`¹)      | `Snapshot` CRs alive per SnapshotPolicy (`name` = the policy name). Bounded by GFS retention when `spec.retention` is set; a policy **without** it never prunes (a deliberate safe default) and this gauge is the only thing that will tell you so. Since #345 M6 a **store-backed observable**: the series exists iff the policy currently has ≥1 Snapshot CR |
| `kopiur_snapshot_gated`                          | gauge       | `namespace`, `policy` (omitted when the Snapshot has no `policyRef`) | store-backed observable: Snapshots parked `Pending` behind a not-Ready repository (`Ready` reason `RepositoryNotReady`) — deferrals, not refusals; they launch automatically on recovery and the series drains to absence |
| `kopiur_repository_health_probe_failures_total`  | counter     | `kind`, `namespace`, `name`, `outcome` (`vanished`/`unreachable`/`timed_out`) | backend health-probe alerts raised after the consecutive-failure debounce                                                                                                                  |
| `kopiur_repository_breaker_trips_total`          | counter     | `kind`, `namespace`, `name`, `probe_kind` (`vanished`/`unreachable`/`timed_out`) | circuit-breaker openings (#345): the probe exceeded `failureThreshold` under `onFailure: Degrade`, moving the repository to `Degraded`; counted on the transition only                     |
| `kopiur_repository_seed_total`                   | counter     | `kind`, `namespace`, `name`, `mode` (`blob`/`migrate`), `outcome` (`seeded`/`already_initialized`/`failed`) | `spec.seed` outcomes on a repository bootstrap (#380). `seeded` = data was copied in; `already_initialized` = the standing no-op on a repository that was already there (the steady state of a `spec.seed` left in a GitOps manifest, so a fleet of these is healthy — one on a FRESH repository is not); `failed` = the seeding bootstrap failed and the repository is **not** Ready. Counted on the status TRANSITION only. That holds for the retry loop too: a relaunch deliberately does NOT overwrite a recorded seed-failure reason with `Seeding`, so a dead or empty seed source — retried every ~2 minutes — writes one status change and so counts once, not once per cycle |
| `kopiur_repository_consecutive_backend_failures` | gauge       | `kind`, `namespace`, `name` (`namespace` empty for `ClusterRepository`) | store-backed observable: `status.health.consecutiveProbeFailures` — every failed backend connect (probe + strict retries). Emitted whenever health status exists (a 0 after recovery is informative); absent when the probe never ran; dies with the CR |
| `kopiur_repository_breaker_open_since_timestamp_seconds` | gauge | `kind`, `namespace`, `name`               | store-backed observable, emitted **only while the breaker is open** (phase `Degraded` + `BackendReachable=False`): `status.health.firstFailureAt` as unix seconds, so `time() - metric` is the open duration; the series disappears when the breaker closes |
| `kopiur_repository_breaker_open`                 | gauge       | `kind`, `namespace`, `name`, `reason` (`unreachable`/`vanished`/`timed_out`/`other`) | store-backed observable, 1 for the same open window, labeled by the cause (#413/#414): the hard causes pause backups/replication/maintenance, while `timed_out` keeps maintenance running and escalates the deadline (usually self-healing); drives the split `KopiurRepositoryBreakerOpen`/`KopiurRepositoryConnectSlow` alerts |
| `kopiur_repo_size_bytes`                       | gauge       | `namespace`, `name`                        | logical bytes under management (newest snapshot per source)                                                                                                                                |
| `kopiur_repo_snapshot_count`                   | gauge       | `namespace`, `name`                        | repository catalog scan                                                                                                                                                                    |
| `kopiur_repo_discovered_snapshots`               | gauge       | `namespace`, `name`                        | repository catalog scan                                                                                                                                                                    |
| `kopiur_repository_maintenance_configured`     | gauge (0/1) | `kind`, `namespace`, `name`                | Repository/ClusterRepository reconcile once Ready; 1 = a `Maintenance` references it, 0 = none (also emits a `MaintenanceNotConfigured` Warning event + `MaintenanceConfigured` condition) |
| `kopiur_restore_duration_seconds`              | gauge       | `namespace`, `name`                        | restore Job completion − start                                                                                                                                                             |
| `kopiur_maintenance_last_reclaimed_bytes`      | gauge       | `namespace`, `name`                        | full maintenance run                                                                                                                                                                       |
| `kopiur_webhook_admission_total`               | counter     | `kind`, `decision` (`allowed`/`denied`)    | admission webhook                                                                                                                                                                          |
| `kopiur_mover_operations_total`                | counter     | `operation`, `result`                      | mover Job (OTLP push)                                                                                                                                                                      |
| `kopiur_mover_operation_duration_seconds`      | histogram   | `operation`, `result`                      | mover Job (OTLP push)                                                                                                                                                                      |

Notes:

- **¹ The `repository` label exists only on multi-repository children.** A per-policy series (`kopiur_policy_last_backup_*`, `kopiur_snapshotpolicy_last_backup_success`, `kopiur_snapshots_live`, `kopiur_snapshot_consecutive_failures`) gains a `repository` label — the normalized repo key, e.g. `Repository/billing/nas` — **only** when its Snapshot rows carry the mint-time `spec.repository` pin, which only [multi-repository fan-out](../backups.md#repositories--one-recipe-several-repositories-fan-out) children do. Single-repo policies and all pre-feature rows emit their exact legacy label sets, so existing series identities (and recording rules keyed on them) are continuous by construction. The point of the dimension is per-repo health: with one flat series per policy, interleaved per-repo results would reset the failure streak and flap `last_backup_success`, so a permanently failing repository B behind a healthy repository A would never alert. The shipped PrometheusRule aggregates `min by (namespace, policy, repository)` — legacy series have an empty `repository` and group exactly as before, while a multi-repo policy alerts **per repository** (`KopiurLastBackupFailed` fires for the one broken repo; `KopiurBackupStale`'s `unless` join carries `repository` too, so a fresh success in repo A never masks staleness in repo B).
- **Every reconcile failure also publishes a Warning Event** on the failing object (`error_policy` → `reconcile_failure_event`), for **every** CRD kind — so `kubectl get events`/`describe` shows the cause without reading controller logs. The Event `reason` is machine-readable (`MissingDependency`, `InvalidSpec`, `InvalidSchedule`, `KubeApiError`, … or the kopia error class for backend failures), the note says what failed / why / how to fix, and repeats of the same failure aggregate into one Event object with a climbing `series.count` instead of flooding the list.
- `kopiur_resource_phase` and every per-CR/per-policy Snapshot gauge (`kopiur_snapshot_*`, `kopiur_policy_last_backup_*`) are **store-backed observable gauges**: their callbacks are re-derived from the controllers' `kube::runtime::reflector::Store`s at each collection, not written imperatively. A resource's series exist only while the CR exists — deletion removes them from the next `/metrics` exposition, full stop, rather than leaving a `0`-valued series behind. (The Prometheus pull exporter is cumulative, but an observable-gauge callback is the sole source of truth for its metric on each collection: any attribute set the callback doesn't emit that cycle is simply absent from the collected point set, so a deleted CR's series drops out of the next exposition; a sync gauge could only ever overwrite a series' value, never drop it — the old zero-on-deletion approach this replaced.) This is why alert/dashboard queries no longer need an explicit `== 1` filter on `kopiur_resource_phase`: only the active phase is ever emitted, so `count(x)` and the old `count(x == 1)` are equivalent, but an absence-aware alert (see `KopiurBackupStale` in the PrometheusRule) still needs an explicit presence signal from a metric that *isn't* store-backed, since "the metric never existed" and "the metric was healthy" are otherwise indistinguishable in PromQL.
- The store-backed property is also what makes the two recovery-aware backup alerts (#280) *self-resolving*. Both key off `kopiur_snapshotpolicy_last_backup_success` (a store-backed `0`/`1` gauge per `(namespace, policy)`, `1` = the policy's most recent terminal backup succeeded). `KopiurLastBackupFailed` is simply `kopiur_snapshotpolicy_last_backup_success == 0`: a newer successful backup flips the series to `1` and the alert clears on its own, with no imperative reset. `KopiurSnapshotFailed` gates the per-`Snapshot` phase match with `unless on (namespace, policy) (kopiur_snapshotpolicy_last_backup_success == 1)`, so a Failed `Snapshot` CR — retained by design — stops firing the moment its policy records a newer success; a `Snapshot` with no `policyRef` carries no `policy` label, never joins the health series, and keeps the always-fire behavior. Because this gauge is store-backed, its series *vanishes* once the policy's terminal `Snapshot` CRs are all gone (pruned by retention, or never present) — precisely the pruned/never-succeeded gap that `KopiurBackupStale`'s second branch covers via the `kopiur_snapshot_consecutive_failures` liveness signal (since #345 M6 also store-backed — its series now vanishes with the policy's last `Snapshot` CR — but the branch's coverage is unchanged: a streak > 0 was always computed from retained Failed `Snapshot` CRs, so whenever the alert's `> 0` filter could match, the series exists). `KopiurRestoreFailed` is deliberately **not** recovery-aware — a failed Restore has no "newer success" that supersedes it.
- Per-resource gauges are re-read from the freshest status on each collection, so they don't lag a cycle behind a phase transition.

## Kube API request attribution (issue #382)

`kopiur_kube_client_requests_total` is the controller's **self-reported apiserver footprint**. During issue #382 (a control-plane OOM under sustained Snapshot LIST load) the request rates had to be reconstructed from the apiserver's own `apiserver_request_total` — the controller had no client-side view at all. Now every one of the controller's three kube clients is built through `kube::client::ClientBuilder` with a tower layer (`crates/controller/src/kube_metrics.rs`) that classifies each outgoing request before it leaves the process:

- **`verb`** — from the HTTP method + path shape, with `watch=true` in the query → `watch` and a server-side-apply `Content-Type` refining `patch` → `apply`. The classification itself is the pure `classify_kube_request(method, path, query)` mirroring the apiserver's request-info resolver, so it is table-tested without HTTP machinery.
- **`group`/`kind`** — the API group (`""` for the core group, deliberately matching `apiserver_request_total` so the two can be joined in one panel) and the **resource plural** from the path (`snapshots`, `secrets`, …), with a `/subresource` suffix (`snapshots/status`). Object names and namespaces are never labels; unrecognizable paths (discovery, `/version`) fold into a single `other` bucket, so cardinality is bounded by construction.
- **`client`** — which connection pool: `main` (every watch and reconcile), `exec` (`workloadExec` hook attaches), `election` (the leader Lease; its renewals are real steady-state load and used to be invisible).

**The attribution split with `kopiur_watcher_restarts_total`:** kube-rs `Controller`s drive their own internal trigger streams (the primary reflector, `.owns()`, `.watches()`), and those expose **no error or event hook** — they cannot be individually instrumented. The two metrics divide the job:

- `kopiur_watcher_restarts_total{kind}` counts watch **errors** (→ backoff + watch restart, i.e. re-LIST load) on the streams kopiur drives itself: the metadata-only referent trigger streams (`controllers.rs::referent_meta` — Secrets/ConfigMaps/ServiceAccounts/Namespaces) and the standalone Maintenance informer (`startup.rs`). Controller-internal trigger streams are **not** in this counter.
- `kopiur_kube_client_requests_total` sits **below** every stream at the HTTP layer, so the traffic of the uninterceptable Controller-internal streams is still fully attributed there — a watcher stuck in a restart loop shows up as a climbing `verb="list"`/`verb="watch"` rate on its `group`/`kind` even though no restart counter exists for it.

Steady-state expectations: `verb="watch"` recycles at ~1 per stream per ~290s (the server-side watch timeout); `verb="list"` should stay near the catalog/sweep cadence — a sustained LIST rate on `kind="snapshots"` is exactly the #382 signature.

## Enabling everything (Helm)

```bash
helm upgrade --install kopiur oci://ghcr.io/home-operations/charts/kopiur -n kopiur-system \
  --set monitoring.serviceMonitor.enabled=true \
  --set monitoring.prometheusRule.enabled=true \
  --set monitoring.dashboards.enabled=true \
  --set webhook.serviceMonitor.enabled=true \
  --set observability.otlp.enabled=true \
  --set observability.otlp.endpoint=http://otel-collector.observability.svc:4317
```

A ready-to-use values overlay is at [`deploy/observability-values.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/observability-values.yaml):

```bash
helm upgrade --install kopiur oci://ghcr.io/home-operations/charts/kopiur -n kopiur-system \
  -f deploy/observability-values.yaml
```

Keys (see `deploy/helm/kopiur/values.yaml` for the full set):

| Key                              | Default  | Effect                                          |
| -------------------------------- | -------- | ----------------------------------------------- |
| `monitoring.serviceMonitor.enabled` | `false`  | scrape the controller `/metrics`             |
| `monitoring.prometheusRule.enabled` | `false`  | install the kopiur alert rules               |
| `monitoring.dashboards.enabled`     | `false`  | ship the dashboard as a sidecar ConfigMap    |
| `monitoring.dashboards.grafanaOperator.enabled` | `false` | render a grafana-operator `GrafanaDashboard` CR instead of the ConfigMap |
| `webhook.serviceMonitor.enabled` | `false`  | scrape the webhook `/metrics` (HTTPS)           |
| `observability.otlp.enabled`     | `false`  | export OTLP from all components                 |
| `observability.otlp.endpoint`    | `…:4317` | collector gRPC endpoint (required when enabled) |
| `observability.otlp.protocol`    | `grpc`   | only gRPC is compiled in                        |
| `observability.otlp.headers`     | `""`     | e.g. `authorization=Bearer …`                   |
| `observability.otlp.strict`      | `false`  | fail-fast on telemetry misconfig                |

When OTLP is enabled the controller passes the same `OTEL_EXPORTER_OTLP_*` env to every mover `Job` it creates, so mover traces/logs/metrics reach the same collector.

## Environment variables

The env var **names** are centralized in `crates/telemetry/src/env.rs` (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_HEADERS`, `KOPIUR_OTEL_STRICT`, plus the logging vars `RUST_LOG` and `KOPIUR_LOG_FORMAT`); the Helm `observability.otlp` and `logging` blocks set them. Only gRPC is compiled in — point the endpoint at the collector's gRPC port (4317). Setting `OTEL_EXPORTER_OTLP_PROTOCOL` to anything other than `grpc` is rejected with an actionable error.

`OTLP_PASSTHROUGH` and `LOG_PASSTHROUGH` (same module) list the vars the controller forwards onto mover `Job`s: OTLP only when a collector is configured, logging whenever set.

## Dashboard

`deploy/dashboards/kopiur.json` is the source of truth (import it into Grafana directly). The chart copy under `deploy/helm/kopiur/files/dashboards/kopiur.json` is **generated** from it by `cargo xtask gen-all` and guarded by `cargo xtask gen-all --check`, so the two can never drift. Edit the source, then regenerate.

Both Helm render paths read that one generated copy: `monitoring.dashboards.enabled` emits a sidecar `ConfigMap`, and `monitoring.dashboards.grafanaOperator.enabled` emits a grafana-operator `GrafanaDashboard` CR (with the JSON inline under `spec.json`) *instead of* the ConfigMap. So a dashboard change is a single-file edit (`deploy/dashboards/kopiur.json`) + `mise run gen`, regardless of how it's delivered.

## Grafana via the OTLP path

If you run OTLP-only and don't scrape the pods, point Prometheus at the collector instead. A minimal OpenTelemetry Collector that ingests OTLP and re-exposes a Prometheus scrape target:

```yaml
# otel-collector config (configmap data)
receivers:
    otlp:
        protocols:
            grpc:
                endpoint: 0.0.0.0:4317
exporters:
    prometheus:
        endpoint: 0.0.0.0:8889 # scrape this with Prometheus
    # debug:                        # uncomment to see traces/logs in the collector log
service:
    pipelines:
        metrics: { receivers: [otlp], exporters: [prometheus] }
        traces: { receivers: [otlp], exporters: [debug] }
        logs: { receivers: [otlp], exporters: [debug] }
```

For most users the direct-scrape `ServiceMonitor` path is simpler; OTLP is for shops that already run a collector and want traces + logs alongside metrics.
