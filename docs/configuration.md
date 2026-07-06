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

The **controller is the chart's primary component**, so its knobs live at the
**root** (unprefixed): `replicaCount`, `resources`, `nodeSelector`,
`podSecurityContext`, `image`, and so on all configure the controller. The two
auxiliary components get their own named blocks: `webhook:` (a full Deployment
surface) and `mover:` (the per-Job image only). So a root-level workload key
applies to the controller alone. You configure the controller and webhook
independently because they have different resource profiles and lifecycles.

/// tip | The five values most people actually set

1. `installScope` — `cluster` (default) or `namespaced` (least-privilege opt-down; disables `ClusterRepository`).
2. `image.tag` (controller) / `mover.image.digest` — pin what runs (digest-pin the mover in prod).
3. `webhook.tls.mode` — `self` (default), `cert-manager`, or `manual`.
4. `monitoring.serviceMonitor.enabled` / `monitoring.dashboards.enabled` — wire up Prometheus + Grafana.
5. `observability.otlp.enabled` — add OTLP traces/logs/metrics-push on top of the pull endpoint.

Everything else has a sensible default. The sections below cover them all.

///

## Naming overrides

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:14:17"
```

Standard Helm escape hatches. `nameOverride` changes the chart-name component of
generated resource names; `fullnameOverride` replaces the whole `<release>-kopiur`
prefix. Leave both empty unless you're fitting Kopiur into an existing naming
scheme.

## Images

Each of the three images is configured next to the component it belongs to, and
each `repository` is a **full registry + path** string:

- **controller** — the root `image` block (`image.repository`, `image.tag`,
  `image.digest`, `image.pullPolicy`).
- **webhook** — `webhook.image.*` (its own `webhook.image.pullPolicy`).
- **mover** — `mover.image.*` (its own `mover.image.pullPolicy`).

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:71:83"
```

Each image takes a `tag` (defaults to the chart's `appVersion` when empty) or a
`digest`. `mover.image.pullPolicy` sets the `imagePullPolicy` on every mover
**Job** pod the controller creates (`Always` / `IfNotPresent` / `Never`); when
unset, the controller infers `IfNotPresent` whenever an explicit mover image is
configured (so a pinned, e.g. locally-loaded, image is never re-pulled) and
otherwise leaves the cluster default in charge.

/// warning | Digest-pin the mover in production

The mover image runs your data-protection Jobs. A floating `:latest` (or any
mutable tag) means a re-pull could silently change what runs during a backup or
restore. Set `mover.image.digest` to a `sha256:…` pin — when `digest` is set it
**wins over `tag`** — so a Job is always byte-for-byte reproducible.
The same advice applies to the controller and webhook, but the mover is the one
that touches your data.

///

`imagePullSecrets` (at the root, concatenated with `global.imagePullSecrets`) is
applied to the controller/webhook pods **and** the mover Jobs, so a private
registry only needs configuring once.

## Install scope & CRDs

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:45:60"
```

| `installScope` | RBAC | Manages | `ClusterRepository` |
| --- | --- | --- | --- |
| `cluster` (default) | `ClusterRole` + `ClusterRoleBinding` | cluster-wide | reconciled |
| `namespaced` | `Role` + `RoleBinding` | the release namespace only | **not** reconciled |

`cluster` is the default because a namespace-scoped `Role` silently disables the
`ClusterRepository` kind — a cluster-scoped resource is out of a `Role`'s reach,
so a namespaced install turns Kopiur into a shared-backup-tier operator that
can't do shared backup tiers. Choose `namespaced` as the explicit
least-privilege opt-down for a single-team install where the reduced blast radius
is worth losing cluster-scoped repositories.

/// info | How the CRDs are installed

The 8 CRDs ship in the chart's special `crds/` directory: `helm install`
installs them, but `helm upgrade` **never** touches them (a Helm rule for the
`crds/` directory). For a **helm-CLI upgrade** that carries a schema change you
must apply the new CRDs yourself:

```console
$ kubectl apply -f deploy/crds/
```

A GitOps flow (`CreateReplace` sync) applies them automatically. There is no
install-time toggle anymore — anyone managing CRDs out of band just applies
`deploy/crds/` and Helm skips the ones that already exist. See
[Installation → CRD lifecycle](install.md#crd-lifecycle).

///

## Feature permissions

A couple of opt-in features need the operator to **write Secrets** in the
namespaces it manages, so each is gated behind a Helm flag — the chart does
**not** grant cluster-wide `secrets` write by default (least privilege). The flag
names match the CRD field that triggers them.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:239:253"
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
--8<-- "deploy/helm/kopiur/values.yaml:216:224"
```

Set `serviceAccount.create: false` to bring your own. The `annotations` map is
where IRSA / GKE Workload Identity role bindings go, so the operator (and the
mover Jobs that inherit it) can authenticate to cloud object storage without a
static credential Secret. `serviceAccount.automount` (default `true`) controls
whether the token is mounted into the controller/webhook pods.

## Controller Deployment

The controller's knobs live at the **root** of the values file (no `controller.`
prefix): a root-level workload key applies to the controller alone.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:89:162"
```

The operator itself. The settings worth knowing:

- **`replicaCount` + `leaderElection`** — run more than one replica for HA.
  With `leaderElection.enabled` (the default) the replicas elect a leader via a
  `coordination.k8s.io/v1` Lease in the release namespace (named after the
  release): only the Lease holder runs reconcilers, while standby replicas stay
  Ready (probes and `/metrics` are served by every replica) and take over
  within ~15s (the lease duration) if the leader dies — or **immediately** on a
  graceful shutdown, where the outgoing leader releases the Lease so rolling
  upgrades don't stall reconciliation. A leader that loses its Lease exits and
  re-enters the election on restart — fail-fast beats a split-brain
  double-reconcile. If the leases RBAC is missing (e.g. a new image under an
  old chart), the controller logs a loud error and runs **without** election
  rather than crash-looping. Kopiur's deterministic jitter (derived from
  `(scheduleUID, slot)`) keeps schedules identical across replicas and across
  failover, so HA never doubles or skews a scheduled backup.

/// warning | Don't run replicas > 1 with leaderElection disabled

`leaderElection.enabled: false` removes the Lease RBAC and the election
entirely — every replica then reconciles concurrently, duplicating mover Jobs
and racing status writes. Only disable it at `replicaCount: 1`.

///
- **`streamingLists`** — use the Kubernetes WatchList streaming-list API for the
  controller's cluster-wide watches, lowering peak memory during the initial
  resync by streaming pages instead of buffering them (the startup burst the
  memory note below warns about). **Default `true`**: WatchList is beta in
  Kubernetes 1.32/1.33 and GA in 1.34, and the chart's `kubeVersion` floor is
  `>=1.32.0-0`. The controller gates it on the server version at startup and
  falls back to paged lists below 1.32 either way, so set `false` only if your
  apiserver has the WatchList feature gate disabled (turning it off just skips
  the feature probe).
- **`workerThreads`** — Tokio worker threads for the controller runtime
  (default `2`). The controller is I/O-bound, so a small pool is ample; raise
  only for a reconcile-heavy deployment.
- **`extraVolumes` / `extraVolumeMounts`** — the way to make a **filesystem
  backend** reachable in-process (hostPath / NFS / PVC), so the controller can
  run its short idempotent kopia ops. The e2e harness uses a hostPath here.
- **`resources`** — only **requests** are set by default; there are intentionally
  **no limits** (the `limits` block ships commented out). Uncomment and tune it to
  your own measured ceiling if you want them.
- **`podDisruptionBudget` / `topologySpreadConstraints`** — pair these with
  `replicaCount > 1` to make HA genuine: the PDB keeps a voluntary disruption
  (node drain, cluster upgrade) from taking the controller to zero, and the
  spread constraints keep both replicas off the same node/zone. Both fall back
  to their `global.*` counterparts when left empty.

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

### Controller port & probes

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:186:211"
```

The controller has a single operational port, `metrics.port` (default `8081`),
which co-hosts `/metrics`, `/healthz`, and `/readyz`. The chart renders the
**dual-stack wildcard** bind address `[::]:<port>` into the `KOPIUR_HTTP_ADDR`
env, which serves both IPv4 and IPv6 kubelets (a wildcard IPv6 bind also accepts
IPv4 on Linux when `net.ipv6.bindv6only=0`, the default), so probes work on
either. The metrics `Service` and the probes both target this port.

/// note | Forcing an IPv4-only bind

On a host where **IPv6 is disabled in the pod network namespace** a `[::]` bind
fails outright. There is no `listenAddr` value anymore — override the bind
address directly by adding `KOPIUR_HTTP_ADDR` (e.g. `0.0.0.0:8081`) through the
controller's `extraEnv`. An unparseable address fails the controller at startup
with an actionable error instead of silently falling back to the default.

///

`livenessProbe` / `readinessProbe` are passed through with `toYaml`, so you can
retune timings/thresholds, swap the scheme, or set either to `{}` to drop the
probe.

## Admission webhook

The webhook is a **separate** Deployment + Service; the Service maps
`443 → 8443`.

The webhook is a **full component** with its own `webhook:` block: its own
`webhook.image`, `webhook.port` (the container port, default `8443`; the chart
renders `[::]:<port>` into `KOPIUR_WEBHOOK_ADDR` and the Service maps `443 →`
it), `webhook.replicaCount`, scheduling (`webhook.nodeSelector` /
`tolerations` / `affinity` / `topologySpreadConstraints`), its own
`webhook.podDisruptionBudget`, and its own security context (see [Pod
security](#pod-security)). A root-level workload key never touches it.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:381:427"
```

- **`enabled`** — when `false`, validation falls back to the controller's
  defensive checks only. Not recommended; the webhook is what makes invalid
  states unrepresentable at admission time.
- **`failurePolicy: Fail`** — fail-closed is the default and the right call for a
  backup operator: if the webhook is down, reject the write rather than
  silently admit an unvalidated `Snapshot`. That makes `webhook.podDisruptionBudget`
  the most important PDB to enable in HA.
- **`webhook.serviceMonitor`** — the webhook serves `/metrics` on its TLS port;
  scraping it needs `insecureSkipVerify` (it serves a self-signed cert by
  default). This stays under `webhook:` (not `monitoring:`) because it's an HTTPS
  scrape of the webhook's own port.

### Webhook TLS

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:480:510"
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

## Monitoring (Prometheus & Grafana)

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:321:373"
```

All metrics are under the `kopiur_` namespace and served via a Prometheus **pull**
endpoint on the controller's port (`metrics.port`). The controller's metrics
`Service` is **always** created — that listener co-hosts `/metrics` with
`/healthz` + `/readyz`, so there's nothing to disable. The `monitoring:` block
additionally wires up the Prometheus Operator and Grafana:

- **`monitoring.serviceMonitor.enabled`** — create a `ServiceMonitor` scraping
  the controller's `/metrics` (plain HTTP; needs the Prometheus-Operator CRDs);
  set `.labels` to match your `serviceMonitorSelector`.
- **`monitoring.prometheusRule.enabled`** — ship the kopiur alert rules.
  `backupStaleAfterSeconds` (default 48h) is the age after which a
  `SnapshotPolicy`'s last success is considered stale.
- **`monitoring.dashboards.enabled`** — ship the dashboard. By default it's a
  sidecar-discoverable `ConfigMap` (source: `deploy/dashboards/kopiur.json`); flip
  `monitoring.dashboards.grafanaOperator.enabled` to render a grafana-operator
  `GrafanaDashboard` CR from the very same JSON instead.

The webhook's own HTTPS scrape lives separately under
[`webhook.serviceMonitor`](#admission-webhook).

## OpenTelemetry (OTLP)

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:297:310"
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
--8<-- "deploy/helm/kopiur/values.yaml:279:286"
```

Controls the stdout (`kubectl logs`) logging every component writes. The
controller passes `RUST_LOG` + `KOPIUR_LOG_FORMAT` through to mover Jobs, so a
mover honors the same level and format.

- **`level`** — `RUST_LOG`-style, default `info`. Per-target works too:
  `"info,kopia=debug"` surfaces kopia's own progress in mover logs.
- **`format`** — `text` (human-readable, default) or `json` (one structured
  object per line for Loki / ELK / Datadog).

## Flags & environment variables

You normally configure Kopiur through the Helm values above — the chart turns
them into environment variables on the controller and webhook Deployments.
Under the hood, every one of those knobs is also a **command-line flag** on the
binary, with the env var as its fallback (**flag > env var > built-in
default**). Run any binary with `--help` for the full, self-documenting list:

```console
$ kopiur-controller --help   # every knob, its KOPIUR_* env var, and its default
$ kopiur-webhook --help
$ kopiur-mover --help        # ready / serve subcommands + run-once mode
```

The flags matter in two situations:

- **Running a binary outside the chart** (local development, a custom
  deployment): `kopiur-controller --mover-image ghcr.io/… --http-addr '[::]:8081'`
  beats exporting env vars by hand.
- **`extraArgs`** (the controller's) — extra flags are now actually parsed. That
  cuts both ways: a valid flag works, and an unknown or malformed one fails the
  container at startup with an actionable usage error (previously extra args
  were silently ignored).

/// warning | Malformed values now fail loudly at startup

A typo'd configuration value used to be silently swallowed: a garbage
`KOPIUR_WORKER_THREADS` fell back to the default, and a misspelled boolean
(e.g. `KOPIUR_STREAMING_LISTS=ture`) silently meant "off". Every value is now
validated at startup — an unparseable number, boolean, socket address,
role kind, or pull policy stops the process with a message naming the variable,
the accepted values, and the fix. Chart-rendered values are unaffected (the
chart only emits valid ones); hand-set `extraEnv` / `extraArgs` values get the
loud failure instead of a silent mis-configuration.

///

## Pod security

Security contexts are now **per-component**. The root `podSecurityContext` /
`securityContext` are the **controller's**; the webhook carries its own
`webhook.podSecurityContext` / `webhook.securityContext` with the same restricted
defaults, so relaxing the controller (e.g. `runAsUser: 1000` + `fsGroup` to read
a filesystem/NFS-backed repository for in-process kopia ops) never loosens the
webhook.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:164:184"
```

Defaults for both: non-root **uid/gid 65534 (nobody)**, `runAsNonRoot`, a
`RuntimeDefault` seccomp profile, no privilege escalation, a read-only root
filesystem, and all capabilities dropped (the images are `distroless:nonroot`).
The webhook's own block:

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:428:444"
```

/// note | This is the operator's security context, not the mover's

These `podSecurityContext` / `securityContext` blocks govern the **controller and
webhook pods**. The UID/GID that a **mover** Job runs as — which has to match the
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
