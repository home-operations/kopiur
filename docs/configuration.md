# Helm chart values

This page is a guided tour of the Kopiur chart's `values.yaml` — what every
setting does, which handful you actually change, and the trade-offs behind the
defaults. For _installing_ the chart (namespaces, webhook TLS modes, CRD
lifecycle, the quickstart), see [**Installation**](install.md); this page is the
reference for the knobs.

/// info | Single source

Every YAML block below is pulled directly from the chart's real
[`deploy/helm/kopiur/values.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/helm/kopiur/values.yaml)
at build time (MkDocs snippets), so the documented defaults can never drift from
the file Helm actually renders. The whole annotated file is also inlined at the
[bottom of this page](#the-complete-valuesyaml).

///

## The shape of the chart

Kopiur ships **three images** wired into **two Deployments** plus a per-Job image:

- **controller** — the operator (reconcilers); serves `/metrics`, `/healthz`, `/readyz`.
- **webhook** — a _separate_ axum admission Deployment (validating + mutating).
- **mover** — not a Deployment. The controller stamps this image into every
  `Snapshot` / `Restore` / `Maintenance` **Job** it creates.

So the values file is organized top-down as: images → install scope → the two
Deployments (`controller`, `webhook`) → cross-cutting concerns (metrics, OTLP,
logging, pod security). You configure the controller and webhook independently
because they have different resource profiles and lifecycles.

/// tip | The five values most people actually set

1. `installScope` — `namespaced` (default) or `cluster` (enables `ClusterRepository`).
2. `image.*.tag` / `image.mover.digest` — pin what runs (digest-pin the mover in prod).
3. `webhook.tls.mode` — `self` (default), `cert-manager`, or `manual`.
4. `metrics.serviceMonitor.enabled` / `grafanaDashboard.enabled` — wire up Prometheus + Grafana.
5. `observability.otlp.enabled` — add OTLP traces/logs/metrics-push on top of the pull endpoint.

Everything else has a sensible default. The sections below cover them all.

///

## Naming overrides

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:5:8"
```

Standard Helm escape hatches. `nameOverride` changes the chart-name component of
generated resource names; `fullnameOverride` replaces the whole `<release>-kopiur`
prefix. Leave both empty unless you're fitting Kopiur into an existing naming
scheme.

## Images

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:21:50"
```

All three images share a `registry` and (for controller/webhook) a `pullPolicy`,
each overridable per-image. Each image takes a `tag` (defaults to the chart's
`appVersion` when empty) or a `digest`.

/// warning | Digest-pin the mover in production

The mover image runs your data-protection Jobs. A floating `:latest` (or any
mutable tag) means a re-pull could silently change what runs during a backup or
restore. Set `image.mover.digest` to a `sha256:…` pin — when `digest` is set it
**wins over `tag`** — so a Job is always byte-for-byte reproducible.
The same advice applies to the controller and webhook, but the mover is the one
that touches your data.

///

`imagePullSecrets` is applied to the controller/webhook pods **and** the mover
Jobs, so a private registry only needs configuring once.

## Install scope & CRDs

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:56:73"
```

| `installScope` | RBAC | Manages | `ClusterRepository` |
| --- | --- | --- | --- |
| `namespaced` (default) | `Role` + `RoleBinding` | the release namespace only | **not** reconciled |
| `cluster` | `ClusterRole` + `ClusterRoleBinding` | cluster-wide | reconciled |

`namespaced` is the safer default. Switch to `cluster` when a
platform team runs one shared backup tier — a `ClusterRepository` that many
tenant namespaces reference. `ClusterRepository` is a cluster-scoped kind, so a
namespaced `Role` literally cannot reach it; that's why it's only reconciled in
`cluster` scope.

`installCRDs: true` renders the 8 CRDs as Helm **templates** (not via the special
`crds/` directory), so the flag is honored and `helm upgrade` re-applies schema
changes for the alpha API.

/// warning | Templated CRDs are deleted on `helm uninstall`

Because the CRDs are templates, `helm uninstall` removes them **and every
`kopiur.home-operations.com` object in the cluster** (Repositories, Snapshots, …).
For an alpha API this is the intended, predictable behavior. To decouple CRD
lifecycle from the release (GitOps), set `installCRDs: false` and apply
`deploy/crds/all-crds.yaml` out of band with `kubectl apply --server-side`. See
[Installation → CRD lifecycle](install.md#crd-lifecycle).

///

## Feature permissions

A couple of opt-in features need the operator to **write Secrets** in the
namespaces it manages, so each is gated behind a Helm flag — the chart does
**not** grant cluster-wide `secrets` write by default (least privilege). The flag
names match the CRD field that triggers them.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:76:102"
```

| CRD field you set… | …needs this Helm flag | Grants `secrets` |
| --- | --- | --- |
| `spec.credentialProjection` | `features.credentialProjection.enabled` | `create`, `patch` |
| `spec.server` (kopia web-UI) | `features.kopiaUi.enabled` | `create`, `patch`, `delete` |

/// warning | A real blast-radius trade-off

`create`/`delete` cannot be scoped to a Secret name, so enabling either flag lets
the operator write (and, for `kopiaUi`, delete) a Secret in any namespace it
manages. Leave them `false` to keep `secrets` RBAC read-only. If you enable the
feature in a CR but forget the flag, the resource's `.status` surfaces an
actionable `403` naming the exact flag to set. See
[Feature permissions](feature-permissions.md) for the full mapping and the
symptom→fix loop.

///

## ServiceAccount

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:121:127"
```

Set `serviceAccount.create: false` to bring your own. The `annotations` map is
where IRSA / GKE Workload Identity role bindings go, so the operator (and the
mover Jobs that inherit it) can authenticate to cloud object storage without a
static credential Secret.

## Controller Deployment

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:132:201"
```

The operator itself. The settings worth knowing:

- **`replicaCount` + `leaderElection`** — run more than one replica for HA. Only
  the elected leader reconciles; Kopiur's deterministic jitter (derived from
  `(scheduleUID, slot)`) keeps schedules identical across replicas and
  across failover, so HA never doubles or skews a scheduled backup.
- **`extraVolumes` / `extraVolumeMounts`** — the way to make a **filesystem
  backend** reachable in-process (hostPath / NFS / PVC), so the controller can
  run its short idempotent kopia ops. The e2e harness uses a hostPath
  here.
- **`resources`** — only **requests** are set by default; there are intentionally
  **no limits** (the `limits` block ships commented out). Uncomment and tune it to
  your own measured ceiling if you want them.
- **`listenAddr`** (env `KOPIUR_HTTP_ADDR`, default `0.0.0.0:8081`) — the
  address the controller's HTTP server (`/metrics`, `/healthz`, `/readyz`)
  binds to. You need this only on an **IPv6-only or dual-stack cluster**: the
  kubelet can't reach an IPv4-only `0.0.0.0` bind there, so the liveness/
  readiness probes never succeed and the pod never goes Ready — set
  `controller.listenAddr: "[::]:8081"` to fix it. The port must stay in sync
  with `probePort` below (the Service and the probes target that port, not
  whatever `listenAddr` happens to contain); an unparseable `listenAddr`
  fails the controller at startup with an actionable error instead of
  silently falling back to the default.

/// note | Why the controller ships with no memory limit

On (re)start the controller reconciles every existing resource at once, spawning
concurrent in-process `kopia` subprocesses (whose RSS counts against this
container's cgroup) to list/connect repositories that may hold many snapshots.
That makes RSS **burst** well above steady state (~120Mi). A memory limit that
doesn't cover the burst OOMKills the controller, which then crash-loops (OOM →
restart → re-reconcile burst → OOM) — so the chart sets no limit by default. If
you add one, size it for the burst, not steady state, and measure your own
ceiling first. See `crates/e2e/tests/lifecycle.rs`.

///

/// note | `controller.logLevel` is deprecated

Use the top-level [`logging.level`](#logging) instead — it applies to the
controller, the webhook, and the mover Jobs uniformly. `controller.logLevel` is
kept only as a fallback for existing values files, and `logging.level` wins when
both are set.

///

## Admission webhook

The webhook is a **separate** Deployment + Service; the Service maps
`443 → 8443`.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:208:246"
```

- **`enabled`** — when `false`, validation falls back to the controller's
  defensive checks only. Not recommended; the webhook is what makes invalid
  states unrepresentable at admission time.
- **`failurePolicy: Fail`** — fail-closed is the default and the right call for a
  backup operator: if the webhook is down, reject the write rather than
  silently admit an unvalidated `Snapshot`.
- **`serviceMonitor`** — the webhook serves `/metrics` on its TLS port; scraping
  it needs `insecureSkipVerify` (it serves a self-signed cert by default).

### Webhook TLS

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:247:277"
```

The webhook **always** serves TLS (Kubernetes requires HTTPS for admission);
`webhook.tls.mode` only chooses how the serving cert is provisioned:

| `mode` | What happens | Needs cert-manager? |
| --- | --- | --- |
| `self` (default) | Operator mints its own CA + cert, writes the Secret, injects the `caBundle`, auto-rotates. | No |
| `cert-manager` | cert-manager issues the cert; its `ca-injector` populates the `caBundle`. | Yes |
| `manual` | You pre-create the Secret and set `webhook.caBundle` (base64 PEM) yourself. | No |

The default `self` mode needs **zero** configuration and no external dependency.
Full walkthrough with `--set` commands for each mode: [Installation → Webhook
TLS](install.md#webhook-tls).

## Metrics & observability

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:282:338"
```

All metrics are under the `kopiur_` namespace and served via a Prometheus **pull**
endpoint on the controller's probe port. The chart can additionally wire up the
Prometheus Operator and Grafana:

- **`metrics.enabled`** (default `true`) — create the metrics `Service`.
- **`metrics.serviceMonitor.enabled`** — create a `ServiceMonitor` (needs the
  Prometheus-Operator CRDs); set `.labels` to match your `serviceMonitorSelector`.
- **`metrics.prometheusRule.enabled`** — ship the kopiur alert rules.
  `backupStaleAfterSeconds` (default 48h) is the age after which a
  `SnapshotPolicy`'s last success is considered stale.
- **`grafanaDashboard.enabled`** — ship the dashboard. By default it's a
  sidecar-discoverable `ConfigMap` (source: `deploy/dashboards/kopiur.json`); flip
  `grafanaDashboard.grafanaOperator.enabled` to render a grafana-operator
  `GrafanaDashboard` CR from the very same JSON instead.

## OpenTelemetry (OTLP)

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:348:361"
```

Off by default. Metrics are **always** available via the `/metrics` pull endpoint;
turning on OTLP _adds_ a push path plus **traces and logs**. When enabled, the
controller, webhook, and mover Jobs all export to the configured collector (the
controller forwards the same `OTEL_*` env to the movers it spawns). Only gRPC is
compiled in, so `endpoint` must point at the collector's gRPC port (4317).

`observability.otlp.strict` makes telemetry misconfiguration fail-fast instead of
degrading to fmt+pull — leave it `false` unless you want a broken collector to
block startup. See [Observability](dev/observability.md) for the full metric list
and a sample collector config.

## Logging

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:369:376"
```

Controls the stdout (`kubectl logs`) logging every component writes. The
controller passes `RUST_LOG` + `KOPIUR_LOG_FORMAT` through to mover Jobs, so a
mover honors the same level and format.

- **`level`** — `RUST_LOG`-style. Per-target works too: `"info,kopia=debug"`
  surfaces kopia's own progress in mover logs. When empty, falls back to the
  deprecated `controller.logLevel`.
- **`format`** — `text` (human-readable, default) or `json` (one structured
  object per line for Loki / ELK / Datadog).

## Pod security

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:382:395"
```

Shared defaults for the controller and webhook pods: non-root **uid/gid 65534
(nobody)**, `runAsNonRoot`, a `RuntimeDefault` seccomp profile, no privilege
escalation, a read-only root filesystem, and all capabilities dropped (the
images are `distroless:nonroot`). These harden the operator
itself.

/// note | This is the operator's security context, not the mover's

`podSecurityContext` / `securityContext` here govern the **controller and webhook
pods**. The UID/GID that a **mover** Job runs as — which has to match the
ownership of the data being backed up so it can read it — is configured
per-`SnapshotPolicy` / per-`Restore`, not here. See
[Permissions, UID & GID](permissions.md) and [Security context](security-context.md).

///

## A ready-made observability overlay

The repo ships an overlay that flips the whole metrics + dashboard surface on at
once. Pass it with `helm -f`:

```yaml
--8<-- "deploy/observability-values.yaml"
```

```bash
helm upgrade --install kopiur deploy/helm/kopiur -n kopiur-system \
  -f deploy/observability-values.yaml
```

## The complete `values.yaml`

The whole annotated file, exactly as the chart ships it:

/// details | Full `values.yaml` (click to expand)
    type: example

```yaml
--8<-- "deploy/helm/kopiur/values.yaml"
```

///

## See also

- [Installation](install.md) — quickstart, scope, webhook-TLS `--set` recipes, CRD lifecycle.
- [Movers, RBAC & credentials](movers.md) — what the mover Jobs need and how projection works.
- [Observability](dev/observability.md) — the full metric list, OTLP details, collector config.
- The chart's own [`README.md`](https://github.com/home-operations/kopiur/blob/main/deploy/helm/kopiur/README.md) — generated from the same values.
