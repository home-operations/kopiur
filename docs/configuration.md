# Helm chart values

This page is a guided tour of the Kopiur chart's `values.yaml`. It covers what every setting does, which handful you actually change, and the trade-offs behind the defaults. For _installing_ the chart, meaning namespaces, webhook TLS modes, CustomResourceDefinition (CRD) lifecycle and the quickstart, see [**Installation**](install.md). This page is the reference for the knobs.

/// info | Single source

Every YAML block below is pulled directly from the chart's real
[`deploy/helm/kopiur/values.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/helm/kopiur/values.yaml)
at build time, using MkDocs snippets. So the documented defaults can never drift from the file Helm actually renders. The whole annotated file is also inlined at the [bottom of this page](#the-complete-valuesyaml).

///

## The shape of the chart

Kopiur ships **three images** wired into **two Deployments**, plus a per-Job image:

- **controller**: the operator, meaning the reconcilers. It serves `/metrics`, `/healthz`, and `/readyz`.
- **webhook**: a _separate_ axum admission Deployment, both validating and mutating.
- **mover**: not a Deployment. The controller stamps this image into every
  `Snapshot`, `Restore` and `Maintenance` **Job** it creates.

The **controller is the chart's primary component**, so its knobs live at the **root**, with no prefix. `replicaCount`, `resources`, `nodeSelector`, `podSecurityContext`, `image` and so on all configure the controller. The two auxiliary components get their own named blocks: `webhook:` gets a full Deployment surface, and `mover:` gets the per-Job image only. So a root-level workload key applies to the controller alone. You configure the controller and webhook independently because they have different resource profiles and lifecycles.

/// tip | The five values most people actually set

1. `installScope`: `cluster` (default) or `namespaced` (least-privilege opt-down; disables `ClusterRepository`).
2. `image.tag` (controller) / `mover.image.digest`: pin what runs (digest-pin the mover in prod).
3. `webhook.tls.mode`: `self` (default), `cert-manager`, or `manual`.
4. `monitoring.serviceMonitor.enabled` / `monitoring.dashboards.enabled`: wire up Prometheus + Grafana.
5. `observability.otlp.enabled`: add OTLP traces/logs/metrics-push on top of the pull endpoint.

Everything else has a sensible default. The sections below cover them all.

///

## Naming overrides

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:14:17"
```

These are the standard Helm escape hatches. `nameOverride` changes the chart-name component of generated resource names. `fullnameOverride` replaces the whole `<release>-kopiur` prefix. Leave both empty unless you're fitting Kopiur into an existing naming scheme.

## Images

Each of the three images is configured next to the component it belongs to, and each `repository` is a **full registry plus path** string:

- **controller**: the root `image` block is `image.repository`, `image.tag`,
  `image.digest`, `image.pullPolicy`.
- **webhook**: `webhook.image.*`, with its own `webhook.image.pullPolicy`.
- **mover**: `mover.image.*`, with its own `mover.image.pullPolicy`.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:62:86"
```

Each image takes a `tag`, which defaults to the chart's `appVersion` when empty, or a `digest`. `mover.image.pullPolicy` sets the `imagePullPolicy` on every mover **Job** pod the controller creates, using `Always`, `IfNotPresent` or `Never`. When it is unset, the controller infers `IfNotPresent` whenever an explicit mover image is configured, so a pinned image, for example one loaded locally, is never re-pulled. Otherwise it leaves the cluster default in charge.

/// warning | Digest-pin the mover in production

The mover image runs your data-protection Jobs. A floating `:latest`, or any mutable tag, means a re-pull could silently change what runs during a backup or restore. Set `mover.image.digest` to a `sha256:…` pin, and when `digest` is set it **wins over `tag`**, so a Job always runs exactly the same image. The same advice applies to the controller and webhook, but the mover is the one that touches your data.

///

`imagePullSecrets`, at the root and concatenated with `global.imagePullSecrets`, is applied to the controller and webhook pods **and** to the mover Jobs. So a private registry only needs configuring once.

## Install scope & CRDs

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:45:60"
```

| `installScope` | RBAC | Manages | `ClusterRepository` |
| --- | --- | --- | --- |
| `cluster` (default) | `ClusterRole` + `ClusterRoleBinding` | cluster-wide | reconciled |
| `namespaced` | `Role` + `RoleBinding` | the release namespace only | **not** reconciled |

`cluster` is the default because a namespace-scoped `Role` silently disables the `ClusterRepository` kind. A cluster-scoped resource is out of a `Role`'s reach, so a namespaced install turns Kopiur into a shared-backup-tier operator that can't do shared backup tiers. Choose `namespaced` as the explicit least-privilege opt-down for a single-team install, where the reduced blast radius is worth losing cluster-scoped repositories.

/// info | How the CRDs are installed

The 9 CRDs ship in the chart's special `crds/` directory. `helm install` installs them, but `helm upgrade` **never** touches them, which is a Helm rule for the `crds/` directory. For a **helm-CLI upgrade** that carries a schema change you must apply the new CRDs yourself:

```console
$ kubectl apply --server-side -f deploy/crds/
```

A GitOps flow with a `CreateReplace` sync applies them automatically. There is no install-time toggle anymore: anyone managing CRDs out of band just applies `deploy/crds/`, and Helm skips the ones that already exist. See [Installation → CRD lifecycle](install.md#crd-lifecycle).

///

## Feature permissions

A couple of opt-in features need the operator to **write Secrets** in the namespaces it manages. Each is gated behind a Helm flag, because the chart does **not** grant cluster-wide `secrets` write by default. That is least privilege. The flag names match the CRD field that triggers them.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:303:333"
```

| CRD field you set… | …needs this Helm flag | Grants `secrets` |
| --- | --- | --- |
| `spec.credentialProjection` | `features.credentialProjection.enabled` | `create`, `patch` |
| `spec.server` (kopia web-UI) | `features.kopiaUi.enabled` | `create`, `patch`, `delete` |

/// warning | A real blast-radius trade-off

`create` and `delete` cannot be scoped to a Secret name, so enabling either flag lets the operator write, and for `kopiaUi` delete, a Secret in any namespace it manages. Leave them `false` to keep `secrets` RBAC read-only. If you enable the feature in a CR but forget the flag, the resource's `.status` surfaces an actionable `403` naming the exact flag to set. See [Feature permissions](feature-permissions.md) for the full mapping and the symptom-to-fix loop.

///

## ServiceAccount

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:290:301"
```

Set `serviceAccount.create: false` to bring your own. The `annotations` map is where IRSA and GKE Workload Identity role bindings go, so the operator, and the mover Jobs that inherit it, can authenticate to cloud object storage without a static credential Secret. `serviceAccount.automount`, default `true`, controls whether the token is mounted into the controller and webhook pods.

## Controller Deployment

The controller's knobs live at the **root** of the values file, with no `controller.` prefix. A root-level workload key applies to the controller alone.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:89:239"
```

This is the operator itself. The settings worth knowing:

- **`replicaCount` + `leaderElection`**: run more than one replica for high availability.
  With `leaderElection.enabled`, the default, the replicas elect a leader through a
  `coordination.k8s.io/v1` Lease in the release namespace, named after the
  release. Only the Lease holder runs reconcilers. Standby replicas stay
  Ready, because probes and `/metrics` are served by every replica, and they take over
  within about 15s, the lease duration, if the leader dies. On a graceful shutdown the
  takeover is **immediate**, because the outgoing leader releases the Lease so rolling
  upgrades don't stall reconciliation. A leader that loses its Lease exits and
  re-enters the election on restart: failing fast beats a split-brain
  double-reconcile. If the leases RBAC is missing, for example a new image under an
  old chart, the controller logs a loud error and runs **without** election
  rather than crash-looping. Kopiur's deterministic jitter, derived from
  `(scheduleUID, slot)`, keeps schedules identical across replicas and across
  failover, so high availability never doubles or skews a scheduled backup.

/// warning | Don't run replicas > 1 with leaderElection disabled

`leaderElection.enabled: false` removes the Lease RBAC and the election entirely. Every replica then reconciles concurrently, duplicating mover Jobs and racing status writes. Only disable it at `replicaCount: 1`.

///
- **`streamingLists`**: use the Kubernetes WatchList streaming-list API for the
  controller's cluster-wide watches. It lowers peak memory during the initial
  resync by streaming pages instead of buffering them, which is the startup burst the
  memory note below warns about. **Default `true`**: WatchList is beta in
  Kubernetes 1.32 and 1.33 and generally available in 1.34, and the chart's `kubeVersion` floor is
  `>=1.32.0-0`. The controller gates it on the server version at startup and
  falls back to paged lists below 1.32 either way. So set `false` only if your
  apiserver has the WatchList feature gate disabled; turning it off just skips
  the feature probe.
- **`workerThreads`**: Tokio worker threads for the controller runtime,
  default `2`. The controller is I/O-bound, so a small pool is ample. Raise it
  only for a reconcile-heavy deployment.
- **`reconcileConcurrency`**: per-controller cap on concurrently running
  reconciles, default `8`. The operator runs 8 controllers, so that is at most 64 process-wide.
  This is the one cap on this page that is **bounded by default**, because
  unbounded reconcile concurrency is what let an apiserver flap exhaust the
  controller's file descriptors within seconds. `0` means unbounded, which is not recommended.
- **`maxConcurrentJobs`**: cluster-wide cap on **pooled mover Jobs**, counted across
  all repositories, default `0` which means uncapped. Pooled Jobs are backup
  snapshots, restores, and the source side of either replication. This is the cluster-operator's
  *backstop*; the primary knob is each repository's own
  [`spec.concurrency.maxConcurrentJobs`](repositories.md#concurrency--cap-the-mover-jobs-one-repository-runs-at-once).
  A run must satisfy both, and whichever cap it meets first parks it. Reach for
  this one when the **node pool** cannot host N movers regardless of which
  repositories they belong to. Reach for the per-repository field when one
  **backend** is the thing that must not be saturated. Restores are always
  admitted and never queued, though a running restore still occupies a slot.
  Maintenance, verification, pin and snapshot-delete Jobs are outside the pool
  entirely. Leaving it at `0` costs nothing: with no cap set anywhere the
  operator performs no extra API calls at all.
- **`maxConcurrentDeleteJobs`**: cluster-wide cap on concurrent `snapdel-*`
  batch-delete Jobs, default `0` which means uncapped. This is a *separate* pool from
  `maxConcurrentJobs`, because a deletion reduces repository load and is already batched
  one Job per repository. Queuing it behind backups would grow the backlog it
  exists to drain. It does not gate whether a deletion is *allowed*; that is
  `deletionProtection.threshold` on the repository.
- **`extraVolumes` / `extraVolumeMounts`**: the way to make a **filesystem
  backend** reachable in-process, through hostPath, NFS or a PVC, so the controller can
  run its short idempotent kopia ops. The e2e harness uses a hostPath here.
- **`resources`**: only **requests** are set by default. There are intentionally
  **no limits**; the `limits` block ships commented out. Uncomment and tune it to
  your own measured ceiling if you want them.
- **`podDisruptionBudget` / `topologySpreadConstraints`**: pair these with
  `replicaCount > 1` to make high availability genuine. The PodDisruptionBudget keeps a voluntary
  disruption, such as a node drain or cluster upgrade, from taking the controller to zero, and the
  spread constraints keep both replicas off the same node or zone. Both fall back
  to their `global.*` counterparts when left empty.

/// note | Why the controller ships with no memory limit

On start or restart the controller reconciles every existing resource at once, spawning concurrent in-process `kopia` subprocesses to list and connect repositories that may hold many snapshots. Those subprocesses' RSS counts against this container's cgroup. That makes RSS **burst** well above steady state, which is around 120Mi. A memory limit that doesn't cover the burst OOMKills the controller, which then crash-loops: OOM, restart, re-reconcile burst, OOM. So the chart sets no limit by default. If you add one, size it for the burst, not steady state, and measure your own ceiling first. See `crates/e2e/tests/lifecycle.rs`.

///

### Controller port & probes

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:263:288"
```

The controller has a single operational port, `metrics.port`, default `8081`, which co-hosts `/metrics`, `/healthz`, and `/readyz`. The chart renders the **dual-stack wildcard** bind address `[::]:<port>` into the `KOPIUR_HTTP_ADDR` env, which serves both IPv4 and IPv6 kubelets. A wildcard IPv6 bind also accepts IPv4 on Linux when `net.ipv6.bindv6only=0`, the default, so probes work on either. The metrics `Service` and the probes both target this port.

/// note | Forcing an IPv4-only bind

On a host where **IPv6 is disabled in the pod network namespace**, a `[::]` bind fails outright. There is no `listenAddr` value anymore. Override the bind address directly by adding `KOPIUR_HTTP_ADDR`, for example `0.0.0.0:8081`, through the controller's `extraEnv`. An unparseable address fails the controller at startup with an actionable error instead of silently falling back to the default.

///

`livenessProbe` and `readinessProbe` are passed through with `toYaml`, so you can retune timings and thresholds, swap the scheme, or set either to `{}` to drop the probe.

## Admission webhook

The webhook is a **separate** Deployment plus Service, and the Service maps `443 → 8443`.

The webhook is a **full component** with its own `webhook:` block. That covers its own `webhook.image`; `webhook.port`, the container port, default `8443`, where the chart renders `[::]:<port>` into `KOPIUR_WEBHOOK_ADDR` and the Service maps `443` to it; `webhook.replicaCount`; scheduling through `webhook.nodeSelector`, `tolerations`, `affinity` and `topologySpreadConstraints`; its own `webhook.podDisruptionBudget`; and its own security context, see [Pod security](#pod-security). A root-level workload key never touches it.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:455:510"
```

- **`enabled`**: when `false`, validation falls back to the controller's
  defensive checks only. Not recommended: the webhook is what makes invalid
  states unrepresentable at admission time.
- **`failurePolicy: Fail`**: failing closed is the default and the right call for a
  backup operator. If the webhook is down, reject the write rather than
  silently admit an unvalidated `Snapshot`. That makes `webhook.podDisruptionBudget`
  the most important PodDisruptionBudget to enable in a high-availability setup.
- **`webhook.serviceMonitor`**: the webhook serves `/metrics` on its TLS port,
  and scraping it needs `insecureSkipVerify`, because it serves a self-signed cert by
  default. This stays under `webhook:` rather than `monitoring:` because it's an HTTPS
  scrape of the webhook's own port.

### Webhook TLS

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:563:593"
```

The webhook **always** serves TLS, because Kubernetes requires HTTPS for admission. `webhook.tls.mode` only chooses how the serving cert is provisioned:

| `mode` | What happens | Needs cert-manager? |
| --- | --- | --- |
| `self` (default) | Operator mints its own CA + cert, writes the Secret, injects the `caBundle`, auto-rotates. | No |
| `cert-manager` | cert-manager issues the cert; its `ca-injector` populates the `caBundle`. | Yes |
| `manual` | You pre-create the Secret and set `webhook.caBundle` (base64 PEM) yourself. | No |

The default `self` mode needs **zero** configuration and no external dependency. For a full walkthrough with `--set` commands for each mode, see [Installation → Webhook TLS](install.md#webhook-tls).

## Monitoring (Prometheus & Grafana)

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:392:453"
```

All metrics are under the `kopiur_` namespace and served through a Prometheus **pull** endpoint on the controller's port, `metrics.port`. The controller's metrics `Service` is **always** created, because that listener co-hosts `/metrics` with `/healthz` and `/readyz`, so there's nothing to disable. The `monitoring:` block additionally connects the Prometheus Operator and Grafana:

- **`monitoring.serviceMonitor.enabled`**: create a `ServiceMonitor` scraping
  the controller's `/metrics` over plain HTTP. It needs the Prometheus-Operator CRDs.
  Set `.labels` to match your `serviceMonitorSelector`. The `ServiceMonitor` sets
  `honorLabels: true`, so each `kopiur_*` series keeps its own `namespace` label,
  which is the namespace of the CR the metric describes, instead of having it overwritten
  by the controller's namespace. Prometheus would otherwise move the CR's namespace aside
  to `exported_namespace`.
- **`monitoring.prometheusRule.enabled`**: ship the kopiur alert rules.
  `backupStaleAfterSeconds`, default 48h, is the age after which a
  `SnapshotPolicy`'s last success is considered stale. The backup-failure alerts
  are **recovery-aware**. `KopiurLastBackupFailed` fires while a `SnapshotPolicy`'s
  most recent completed backup failed, and resolves automatically the moment a
  newer backup succeeds. `KopiurSnapshotFailed`, which is per-`Snapshot`, stops firing for a
  retained Failed `Snapshot` once its policy's latest backup succeeds; a
  `Snapshot` with no policy reference keeps the always-fire behavior instead.
  `KopiurBackupStale` still covers the policy that has never succeeded, or whose
  successful `Snapshot` CRs were all pruned. Both recovery-aware alerts wait
  `for: 10m` at `severity: warning`.
- **`monitoring.dashboards.enabled`**: ship the dashboard. By default it's a
  sidecar-discoverable `ConfigMap`, sourced from `deploy/dashboards/kopiur.json`. Flip
  `monitoring.dashboards.grafanaOperator.enabled` to render a grafana-operator
  `GrafanaDashboard` CR from the very same JSON instead.

/// warning | `honorLabels: true` changes the `namespace` label

With `honorLabels: true`, the `namespace` label on every scraped `kopiur_*` series is now the **CR's** namespace rather than the controller's. If you keyed external dashboards or queries on `exported_namespace`, where the CR namespace previously landed, update them to `namespace`. The dashboard this chart ships already assumed CR-namespace semantics, so this fixes its `$namespace` dropdown rather than breaking it.

///

The webhook's own HTTPS scrape lives separately under
[`webhook.serviceMonitor`](#admission-webhook).

## OpenTelemetry (OTLP)

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:368:390"
```

This is off by default. Metrics are **always** available through the `/metrics` pull endpoint. Turning on OpenTelemetry Protocol (OTLP) _adds_ a push path plus **traces and logs**. When enabled, the controller, webhook, and mover Jobs all export to the configured collector, because the controller forwards the same `OTEL_*` env to the movers it spawns. Only gRPC is compiled in, so `endpoint` must point at the collector's gRPC port, 4317.

`observability.otlp.strict` makes telemetry misconfiguration fail fast instead of degrading to fmt plus pull. Leave it `false` unless you want a broken collector to block startup. See [Observability](dev/observability.md) for the full metric list and a sample collector config.

## Logging

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:349:366"
```

This controls the stdout logging, what `kubectl logs` shows, that every component writes. The controller passes `RUST_LOG` and `KOPIUR_LOG_FORMAT` through to mover Jobs, so a mover honors the same level and format.

- **`level`**: `RUST_LOG`-style, default `info`. Per-target works too:
  `"info,kopia=debug"` surfaces kopia's own progress in mover logs.
- **`format`**: `text`, which is human-readable and the default, or `json`, one structured
  object per line for Loki, ELK or Datadog.

## Flags & environment variables

You normally configure Kopiur through the Helm values above, and the chart turns them into environment variables on the controller and webhook Deployments. Underneath, every one of those knobs is also a **command-line flag** on the binary, with the env var as its fallback. The precedence is flag, then env var, then built-in default. Run any binary with `--help` for the full, self-documenting list:

```console
$ kopiur-controller --help   # every knob, its KOPIUR_* env var, and its default
$ kopiur-webhook --help
$ kopiur-mover --help        # ready / serve subcommands + run-once mode
```

The flags matter in two situations:

- **Running a binary outside the chart**, for local development or a custom
  deployment. `kopiur-controller --mover-image ghcr.io/… --http-addr '[::]:8081'`
  beats exporting env vars by hand.
- **`extraArgs`**, the controller's. Extra flags are now actually parsed. That
  cuts both ways: a valid flag works, and an unknown or malformed one fails the
  container at startup with an actionable usage error. Previously extra args
  were silently ignored.

/// warning | Malformed values now fail loudly at startup

A typo'd configuration value used to be silently swallowed. A garbage `KOPIUR_WORKER_THREADS` fell back to the default, and a misspelled boolean such as `KOPIUR_STREAMING_LISTS=ture` silently meant "off". Every value is now validated at startup. An unparseable number, boolean, socket address, role kind, or pull policy stops the process with a message naming the variable, the accepted values, and the fix. Chart-rendered values are unaffected, because the chart only emits valid ones. Hand-set `extraEnv` and `extraArgs` values get the loud failure instead of a silent misconfiguration.

///

## Pod security

Security contexts are now **per-component**. The root `podSecurityContext` and `securityContext` are the **controller's**. The webhook carries its own `webhook.podSecurityContext` and `webhook.securityContext` with the same restricted defaults, so relaxing the controller never loosens the webhook. You might relax the controller with, say, `runAsUser: 1000` plus an `fsGroup` to read a filesystem or NFS-backed repository for in-process kopia ops.

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:241:261"
```

The defaults for both: non-root **uid/gid 65534 (nobody)**, `runAsNonRoot`, a `RuntimeDefault` seccomp profile, no privilege escalation, a read-only root filesystem, and all capabilities dropped. The images are `distroless:nonroot`. The webhook's own block:

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:511:527"
```

/// note | This is the operator's security context, not the mover's

These `podSecurityContext` and `securityContext` blocks govern the **controller and webhook pods**. The UID and GID that a **mover** Job runs as, which has to match the ownership of the data being backed up so it can read it, is configured per-`SnapshotPolicy` and per-`Restore`, not here. See [Permissions, UID & GID](permissions.md) and [Security context](security-context.md).

///

## A ready-made observability overlay

The repo ships an overlay that turns the whole metrics and dashboard surface on at once. Pass it with `helm -f`:

```yaml
--8<-- "deploy/observability-values.yaml"
```

```bash
helm upgrade --install kopiur oci://ghcr.io/home-operations/charts/kopiur -n kopiur-system \
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

- [Installation](install.md): quickstart, scope, webhook-TLS `--set` recipes, CRD lifecycle.
- [Movers, RBAC & credentials](movers.md): what the mover Jobs need and how projection works.
- [Observability](dev/observability.md): the full metric list, OTLP details, collector config.
- The chart's own [`README.md`](https://github.com/home-operations/kopiur/blob/main/deploy/helm/kopiur/README.md), generated from the same values.
