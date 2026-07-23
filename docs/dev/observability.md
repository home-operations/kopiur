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
| `kopiur_resource_phase`                        | gauge (0/1) | `kind`, `namespace`, `name`, `phase`, plus `policy` on Snapshot series (omitted for discovered snapshots) | CR status; store-backed observable gauge, `1` for the **active** phase only — never a 0-valued series for inactive phases |
| `kopiur_snapshot_last_success_timestamp_seconds` | gauge       | `namespace`, `name`, `policy` (Snapshot only) | per-CR: mover-recorded `status.timing.endTime`, only while `phase == Succeeded`                                                                                                            |
| `kopiur_snapshot_consecutive_failures`           | gauge       | `namespace`, `name`                        | SnapshotPolicy reconcile (trailing Failed before the latest Succeeded); `name` here is the SnapshotPolicy name                                                                              |
| `kopiur_snapshot_size_bytes`                     | gauge       | `namespace`, `name`, `policy` (Snapshot only) | per-CR: Backup `status.stats.sizeBytes`                                                                                                                                                  |
| `kopiur_snapshot_files`                          | gauge       | `namespace`, `name`, `policy` (Snapshot only) | per-CR: Backup file counts (absent when unknown)                                                                                                                                         |
| `kopiur_snapshot_duration_seconds`               | gauge       | `namespace`, `name`, `policy` (Snapshot only) | per-CR: Backup `status.timing.durationSeconds`                                                                                                                                           |
| `kopiur_policy_last_backup_success_timestamp_seconds` | gauge  | `namespace`, `policy`                      | `endTime` (unix seconds) of the **latest Succeeded Snapshot per (namespace, policy)** — answers "latest per policy", which per-CR `max by` cannot (issue #172)                             |
| `kopiur_policy_last_backup_duration_seconds`     | gauge       | `namespace`, `policy`                      | duration of the latest Succeeded Snapshot per (namespace, policy)                                                                                                                          |
| `kopiur_policy_last_backup_size_bytes`           | gauge       | `namespace`, `policy`                      | logical size of the latest Succeeded Snapshot per (namespace, policy)                                                                                                                      |
| `kopiur_policy_last_backup_files`                | gauge       | `namespace`, `policy`                      | file count of the latest Succeeded Snapshot per (namespace, policy), only when known                                                                                                       |
| `kopiur_snapshotpolicy_last_backup_success`      | gauge (0/1) | `namespace`, `policy`                      | `1` if a policy's **most recent terminal backup** succeeded, `0` if it failed (absent until the first terminal run). `count(... == 1)` answers "how many of my N policies are currently healthy" — the per-policy STATE the run-counter below cannot (issue #196) |
| `kopiur_snapshots_completed_total`               | counter     | `result` (`succeeded`/`failed`), `namespace`, `policy` (omitted when the Snapshot has no `policyRef`) | incremented once per terminal transition (not per reconcile); durable across CR deletion, so it answers time-windowed "how many backups completed" (issue #175). NOT a fleet-health count — summing it over a window counts every run (policies × runs), so use `kopiur_snapshotpolicy_last_backup_success` for "how many policies are OK" |
| `kopiur_snapshot_verified_timestamp_seconds`     | gauge       | `namespace`, `name`                        | SnapshotPolicy verification (ADR-0005 §4) → `status.lastVerified` of the most recent successful verify (quick or deep); alert on staleness like `last_success`                            |
| `kopiur_orphaned_snapshots_total`              | counter     | `namespace`                                | Orphan policy / skip-cleanup escape hatch                                                                                                                                                  |
| `kopiur_snapshot_deletion_failures_total`      | counter     | `namespace`                                | finalizer snapshot-delete failures                                                                                                                                                         |
| `kopiur_snapshot_deletions_total`              | counter     | `namespace`, `outcome` (`deleted`/`retained`/`orphaned`/`cascade_retained`) | `Snapshot` finalizer RESOLUTIONS (mass-deletion protection). Distinct from `kopiur_snapshot_deletion_failures_total`, which counts kopia delete-call failures, not resolutions — never sum them |
| `kopiur_snapshots_cascade_retained_total`      | counter     | `namespace`                                | narrower, always-alongside view of `kopiur_snapshot_deletions_total{outcome="cascade_retained"}` — a `Snapshot` retained by the schedule-deletion cascade guard; both are bumped by one helper so they can't drift |
| `kopiur_snapshot_delete_batch_jobs_total`      | counter     | `outcome` (`succeeded`/`failed`)           | a mass-deletion batch-delete mover Job reached a terminal outcome                                                                                                                          |
| `kopiur_snapshot_delete_batch_members_total`   | counter     | `outcome` (`deleted`/`failed`)             | per-member batch-delete outcome. `deleted` is bumped once per member as it drains its own finalizer off a SUCCEEDED Job; `failed` is bumped once per member at the single point a FAILED Job is reaped (a failed member never drains its own finalizer, so it has no other emission site). Independent of the whole-Job `..._batch_jobs_total` outcome |
| `kopiur_snapshot_deletions_pending_external`   | gauge       | `repo_kind`, `repo_name` (unpinned → `"unknown"`/`"unknown"`) | store-backed observable gauge: Snapshots being deleted that still carry the cleanup finalizer, by resolved repository. A cheaper, coarser approximation of the mass-deletion breaker's own count — unlike the breaker it includes operator prunes and never re-runs `plan_deletion`, so it may read higher than the threshold-relevant count |
| `kopiur_snapshot_deletions_held`               | gauge       | `repo_kind`, `repo_name` (unpinned → `"unknown"`/`"unknown"`) | store-backed observable gauge: the subset of `kopiur_snapshot_deletions_pending_external` currently HELD by the mass-deletion breaker (`DeletionHeld=True`)                                |
| `kopiur_schedule_snapshots_created_total`        | counter     | `namespace`, `name`                        | SnapshotSchedule fires                                                                                                                                                                       |
| `kopiur_snapshot_refusals_total`                 | counter     | `namespace`, `name`, `reason`              | a backup refused by policy (`RepositoryReadOnly`, `PrivilegedMoverNotPermitted`); the reconcile itself returns Ok, so these never appear in `reconcile_errors`                              |
| `kopiur_secrets_projected_total`                 | counter     | `namespace`                                | credential projection copied a repository Secret into a mover namespace                                                                                                                    |
| `kopiur_work_spec_cms_swept_total`               | counter     | —                                          | legacy sweep deleted an orphaned per-run work-spec ConfigMap (#224)                                                                                                                        |
| `kopiur_projected_secrets_swept_total`           | counter     | —                                          | sweep deleted a per-run projected credential Secret left by pre-stable-naming versions (#231)                                                                                              |
| `kopiur_creds_secrets_reaped_total`              | counter     | `by`                                       | a projected credential copy reclaimed once no mover Job could still load it. `by=terminal` is the consuming CR's reconciler (the fast path), `by=sweep` the periodic backstop. **Never sum them** — in steady state the reconciler should get there first, so a sustained `by=sweep` rate is precisely the signal that its reap has stopped firing (#240). |
| `kopiur_projected_secrets_live`                  | gauge       | —                                          | projected credential copies alive right now, observed each sweep pass. The **population**, which is what a counter of projections cannot tell you: it rises identically whether or not copies are ever reclaimed, and that is why #240 ran for weeks unseen. `deriv(...[24h]) > 0` is the leak alarm.                                                      |
| `kopiur_snapshots_live`                          | gauge       | `namespace`, `name`                        | `Snapshot` CRs alive per SnapshotPolicy. Bounded by GFS retention when `spec.retention` is set; a policy **without** it never prunes (a deliberate safe default) and this gauge is the only thing that will tell you so.                                                                                                                                    |
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

- **Every reconcile failure also publishes a Warning Event** on the failing object (`error_policy` → `reconcile_failure_event`), for **every** CRD kind — so `kubectl get events`/`describe` shows the cause without reading controller logs. The Event `reason` is machine-readable (`MissingDependency`, `InvalidSpec`, `InvalidSchedule`, `KubeApiError`, … or the kopia error class for backend failures), the note says what failed / why / how to fix, and repeats of the same failure aggregate into one Event object with a climbing `series.count` instead of flooding the list.
- `kopiur_resource_phase` and every per-CR/per-policy Snapshot gauge (`kopiur_snapshot_*`, `kopiur_policy_last_backup_*`) are **store-backed observable gauges**: their callbacks are re-derived from the controllers' `kube::runtime::reflector::Store`s at each collection, not written imperatively. A resource's series exist only while the CR exists — deletion removes them from the next `/metrics` exposition, full stop, rather than leaving a `0`-valued series behind. (The Prometheus pull exporter is cumulative, but an observable-gauge callback is the sole source of truth for its metric on each collection: any attribute set the callback doesn't emit that cycle is simply absent from the collected point set, so a deleted CR's series drops out of the next exposition; a sync gauge could only ever overwrite a series' value, never drop it — the old zero-on-deletion approach this replaced.) This is why alert/dashboard queries no longer need an explicit `== 1` filter on `kopiur_resource_phase`: only the active phase is ever emitted, so `count(x)` and the old `count(x == 1)` are equivalent, but an absence-aware alert (see `KopiurBackupStale` in the PrometheusRule) still needs an explicit presence signal from a metric that *isn't* store-backed, since "the metric never existed" and "the metric was healthy" are otherwise indistinguishable in PromQL.
- The store-backed property is also what makes the two recovery-aware backup alerts (#280) *self-resolving*. Both key off `kopiur_snapshotpolicy_last_backup_success` (a store-backed `0`/`1` gauge per `(namespace, policy)`, `1` = the policy's most recent terminal backup succeeded). `KopiurLastBackupFailed` is simply `kopiur_snapshotpolicy_last_backup_success == 0`: a newer successful backup flips the series to `1` and the alert clears on its own, with no imperative reset. `KopiurSnapshotFailed` gates the per-`Snapshot` phase match with `unless on (namespace, policy) (kopiur_snapshotpolicy_last_backup_success == 1)`, so a Failed `Snapshot` CR — retained by design — stops firing the moment its policy records a newer success; a `Snapshot` with no `policyRef` carries no `policy` label, never joins the health series, and keeps the always-fire behavior. Because this gauge is store-backed, its series *vanishes* once the policy's terminal `Snapshot` CRs are all gone (pruned by retention, or never present) — precisely the pruned/never-succeeded gap that `KopiurBackupStale`'s second branch covers via the non-store-backed `kopiur_snapshot_consecutive_failures` liveness signal. `KopiurRestoreFailed` is deliberately **not** recovery-aware — a failed Restore has no "newer success" that supersedes it.
- Per-resource gauges are re-read from the freshest status on each collection, so they don't lag a cycle behind a phase transition.

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
