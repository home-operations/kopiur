# Installing Kopiur

Kopiur is a Kopia-native Kubernetes backup operator (Rust / kube-rs). This guide covers installing the operator with the Helm chart and verifying it.

The chart is published as an OCI artifact at
`oci://ghcr.io/home-operations/charts/kopiur` — **that is the preferred way to
install**. Every published version is cosign-signed and ships with all three
component images (controller, webhook, mover) pinned by digest to the exact
release build. The in-repo chart (`deploy/helm/kopiur`) is the development
copy: its image digests are empty and its tags float, so use it only when
working from a checkout.

> Status: **alpha** — API group `kopiur.home-operations.com`, version `v1alpha1`. The CRD surface may still change between releases.

## Prerequisites

- **Kubernetes >= 1.24.** The deploy-or-restore volume-populator path (`Restore` + `PVC.spec.dataSourceRef`) relies on the `AnyVolumeDataSource` feature, available from 1.24.
- **Helm 3 or 4.**
- A **kopia repository backend** you can reach: S3/MinIO, Azure Blob, GCS, B2, filesystem (PVC), SFTP, WebDAV, or rclone.
- A **CSI snapshot stack** (a `snapshot-controller` plus a `VolumeSnapshotClass` for your driver) for the **default** `copyMethod: Snapshot`. Many distributions bundle it (EKS, GKE, AKS, Talos, k3s add-ons); where yours does not, install the home-operations [`snapshot-controller`](https://github.com/home-operations/helm-charts) chart (`oci://ghcr.io/home-operations/charts/snapshot-controller`). Not needed if every `SnapshotPolicy` sets `copyMethod: Direct`. See [Copy methods → What it requires](copy-methods.md#what-it-requires).
- _(Optional)_ **cert-manager** — only if you prefer it to manage the admission webhook's certificate. **It is not required**: by default the operator manages the webhook cert itself (see [Webhook TLS](#webhook-tls)).
- _(Optional)_ **volume-data-source-validator** — recommended alongside CSI populators so a malformed `dataSourceRef` is surfaced as an event rather than a silently-stuck PVC.
- _(Optional)_ **Prometheus Operator** — if you want the chart's `ServiceMonitor`.

## Quickstart

```bash
# 1. Install the published chart, creating the operator namespace. No extra
#    flags needed — the webhook cert is self-managed by default (no
#    cert-manager required). Add --version x.y.z to pin a release.
helm install kopiur oci://ghcr.io/home-operations/charts/kopiur \
  --namespace kopiur-system --create-namespace

# 2. Wait for rollout. (The webhook pod stays in ContainerCreating until the
#    controller mints its serving Secret — a few seconds after the controller
#    becomes ready.)
kubectl -n kopiur-system rollout status deploy/kopiur-controller
kubectl -n kopiur-system rollout status deploy/kopiur-webhook

# 3. Confirm the 9 CRDs are registered.
kubectl get crd | grep kopiur.home-operations.com
```

## GitOps install (Flux / Argo)

Point your GitOps tool at the same OCI chart — **never at the in-repo
`deploy/helm/kopiur` path**, whose image digests are empty and whose tags float
(a git-sourced install renders image tags from the chart's `appVersion`, so a
checkout can pull a tag that was never published). The published chart pins all
three images by digest.

/// tab | Flux

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: kopiur-system
---
apiVersion: source.toolkit.fluxcd.io/v1
kind: OCIRepository
metadata:
  name: kopiur
  namespace: kopiur-system
spec:
  interval: 1h
  url: oci://ghcr.io/home-operations/charts/kopiur
  ref:
    tag: 0.8.0 # pin the chart version
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: kopiur
  namespace: kopiur-system
spec:
  interval: 1h
  chartRef:
    kind: OCIRepository
    name: kopiur
  # values: {}   # override chart values here
```

The `OCIRepository` and `HelmRelease` are namespaced, so `kopiur-system` must
exist before they apply; that is why the `Namespace` is included above (Flux's
`install.createNamespace` only creates the release's *target* namespace, not
the one the `HelmRelease` object itself lives in). In a real Flux repo the
namespace usually comes from the parent Kustomization; keep it here so the
snippet applies stand-alone.

///

/// tab | Argo CD

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: kopiur
  namespace: argocd
spec:
  project: default
  source:
    repoURL: ghcr.io/home-operations/charts # no oci:// prefix here
    chart: kopiur
    targetRevision: 0.8.0
    # helm:
    #   valuesObject: {}   # override chart values here
  destination:
    server: https://kubernetes.default.svc
    namespace: kopiur-system
  syncPolicy:
    syncOptions:
      - CreateNamespace=true
```

///

/// warning | `helm upgrade` and GitOps reconciles never update the CRDs

The 9 CRDs ship in the chart's special `crds/` directory, which Helm installs
once and never touches on upgrade (see [CRD lifecycle](#crd-lifecycle) below).
A GitOps tool with a `CreateReplace` / server-side-apply CRD policy handles
schema changes automatically; otherwise apply `deploy/crds/` yourself.

///

## Webhook TLS

The admission webhook always serves TLS, and the API server must trust it. `webhook.tls.mode` chooses how the serving certificate is provisioned:

| `webhook.tls.mode` | What happens | Needs cert-manager? |
| ------------------ | ------------ | ------------------- |
| `self` (default)   | The operator mints its own CA + serving cert, writes the Secret, injects the `caBundle`, and auto-rotates the cert (zero-downtime hot-reload). | No |
| `cert-manager`     | cert-manager issues the cert; its `ca-injector` populates the `caBundle`. | Yes |
| `manual`           | You pre-create the Secret and supply `webhook.caBundle`. | No |

### `self` (default)

Nothing to configure — this is the `helm install` above. The operator grants itself the minimal extra RBAC to write its one serving Secret and `patch` the `caBundle` of its two webhook configurations (resourceName-scoped).

### `cert-manager`

```bash
helm install kopiur oci://ghcr.io/home-operations/charts/kopiur \
  --namespace kopiur-system \
  --set webhook.tls.mode=cert-manager
# Optionally point at your own Issuer/ClusterIssuer instead of the chart's
# self-signed one: --set webhook.certManager.issuerRef.name=my-issuer
```

### `manual`

```bash
# create a kubernetes.io/tls Secret named per webhook.tls.secretName, then:
helm install kopiur oci://ghcr.io/home-operations/charts/kopiur \
  --namespace kopiur-system \
  --set webhook.tls.mode=manual \
  --set webhook.tls.secretName=kopiur-webhook-tls \
  --set webhook.caBundle="$(base64 -w0 ca.crt)"
```

Or disable the webhook entirely (validation then relies on the controller's defensive checks only — not recommended):

```bash
helm install kopiur oci://ghcr.io/home-operations/charts/kopiur -n kopiur-system --set webhook.enabled=false
```

## Install scope

| Mode               | `--set installScope=` | RBAC        | Manages                | `ClusterRepository` |
| ------------------ | --------------------- | ----------- | ---------------------- | ------------------- |
| Cluster (default)  | `cluster`             | ClusterRole | cluster-wide           | reconciled          |
| Namespaced         | `namespaced`          | Role        | release namespace only | not reconciled      |

**Cluster** is the default so a shared platform repository (`ClusterRepository`) referenced by many tenant namespaces works out of the box — a namespace-scoped `Role` silently disables the cluster-scoped `ClusterRepository` kind. See `deploy/examples/02-cluster-repository.yaml`. Choose **namespaced** as the explicit least-privilege opt-down for a single-team install.

In namespaced scope the controller's watches are narrowed to the release
namespace to match the Role-only RBAC (the chart passes
`--namespace={{ .Release.Namespace }}`), and cluster-scoped kinds are skipped
entirely: `ClusterRepository` is not reconciled, and the
[privileged-movers namespace opt-in](permissions.md) check fails **open**
(the operator is already confined to admin-chosen namespaces there).

/// warning | Features that need cluster RBAC are refused in namespaced scope

Two features read or write cluster-scoped objects (PersistentVolumes,
StorageClasses, VolumeSnapshotClasses) that a Role can never grant, so a
namespaced install refuses them **up front with an actionable message**
instead of retrying forever:

- **`target.populator` restores** — use `target.pvc`/`target.pvcRef`, or
  install with `installScope: cluster`.
- **`copyMethod: Snapshot`/`Clone` backups** (CSI staging) — set
  `copyMethod: Direct` on the `SnapshotPolicy`. Note `copyMethod`
  **defaults to `Snapshot`**, so an untouched policy hits this refusal; the
  message spells out the fix.

///

/// note | RBAC for the optional web UI

If you use the [web UI](server.md) (`spec.server` on a `Repository`/`ClusterRepository`), the controller manages `Deployments`, `Services`, `ConfigMaps`, and `Secrets` for the kopia server pod. That RBAC ships with the chart in both scopes — nothing extra to grant. The feature is off until you add a `spec.server` block.

///

## Runtime tuning

The chart's defaults are sized for a typical homelab-to-mid-size cluster; a handful of
top-level values tune the controller's runtime footprint and API-server load
(full rationale in the
[developer docs](dev/watch-and-reconcile.md#memory-footprint)):

| Value | Env var | Default | What it does |
|---|---|---|---|
| `workerThreads` | `KOPIUR_WORKER_THREADS` | `2` | Tokio worker threads. The controller is I/O-bound; raise only for a reconcile-heavy deployment. |
| `streamingLists` | `KOPIUR_STREAMING_LISTS` | `true` | Stream cluster-wide re-lists via the WatchList API (lower apiserver + controller memory). Auto-downgrades to paged lists when the apiserver *answers* with a pre-1.32 version; a startup probe that fails to answer keeps the configured value, since a transport failure is not evidence about the server's version. |
| `reconcileConcurrency` | `KOPIUR_RECONCILE_CONCURRENCY` | `8` | Per-controller cap on concurrent reconciles. Bounds API-server load and file descriptors during re-list storms and API-server outages. `0` = unbounded (not recommended). |
| `maxConcurrentDeleteJobs` | `KOPIUR_MAX_CONCURRENT_DELETE_JOBS` | `0` (uncapped) | Opt-in backstop on concurrent snapshot-delete batch Jobs; batching per repository is the primary protection. |
| `maxConcurrentJobs` | `KOPIUR_MAX_CONCURRENT_JOBS` | `0` (uncapped) | Opt-in cluster-wide backstop on concurrent **pooled** mover Jobs — backups, restores, and the source side of either replication — across all repositories. The per-repository [`spec.concurrency.maxConcurrentJobs`](repositories.md#concurrency--cap-the-mover-jobs-one-repository-runs-at-once) is the primary knob; a run must satisfy both. |
| `leaderElection.flowSchema.enabled` | — | `true` | Give the controller's leader-election Lease its own API Priority and Fairness lane. See below. |

/// note | Which of the two job caps to reach for

`maxConcurrentJobs` here is the **cluster-operator's** backstop: set it when the node pool cannot host N movers no matter which repositories they belong to. Set the per-repository `spec.concurrency.maxConcurrentJobs` when one **backend** is the thing that must not be saturated — that is the knob most people want, and it is the one whose numbers appear in a queued run's status message. Leaving this one at `0` costs nothing: with no cap set anywhere the operator makes no extra API calls at all.

Restores are always admitted and never queued (though a running restore still occupies a slot); maintenance, verification, pin and snapshot-delete Jobs are outside the pool entirely. See [Backups → limiting concurrent jobs per repository](backups.md#limiting-concurrent-jobs-per-repository).

///

/// note | Why the chart ships a FlowSchema

Kubernetes' built-in `system-leader-election` FlowSchema routes lease renewals
into a guaranteed priority level whose concurrency is never lent away — but it
matches only `kube-controller-manager`, `kube-scheduler`, and ServiceAccounts in
`kube-system`. An operator in its own namespace falls through to
`service-accounts` → `workload-low`, where its lease renewals queue behind every
other ServiceAccount's bulk traffic in the cluster. On one production cluster the
p99 queue wait differed by ~360× between the two lanes, and the controller was
restarting roughly fifteen times a day as a result.

The chart therefore ships a `FlowSchema` scoping **only** the operator's
`leases` get/create/update calls into the built-in `leader-election` level.
Set `leaderElection.flowSchema.enabled: false` if APF is disabled on your
cluster, or if you cannot create cluster-scoped
`flowcontrol.apiserver.k8s.io` objects. Kopiur runs correctly without it — the
renew loop tolerates a congested lane, it just has less headroom.

///

/// note | Why reconcileConcurrency is bounded by default

Unbounded reconcile concurrency let an API-server outage exhaust the
controller's file descriptors within seconds (every re-listed object
reconciling — and failing — at once). The default of 8 per controller clears a
few-hundred-object re-list in seconds while keeping the controller a good
citizen toward a struggling control plane.

///

## CRD lifecycle

The 9 CRDs ship in the chart's special `crds/` directory. Helm treats that directory specially: **`helm install` installs the CRDs, but `helm upgrade` never touches them.** There is no toggle for this.

> Caution: because `helm upgrade` skips the `crds/` directory, a **helm-CLI upgrade that carries a schema change does not apply it** — you must apply the new CRDs yourself:
>
> ```bash
> # Server-side apply is required: the SnapshotPolicy CRD embeds a full JobSpec
> # (runJob hook) and is too large for the client-side last-applied annotation.
> kubectl apply --server-side -f deploy/crds/
> ```
>
> A GitOps flow (Flux/Argo with a `CreateReplace` sync policy) applies CRD changes automatically. Anyone managing CRDs entirely out of band just applies `deploy/crds/` themselves — `helm install` skips the ones that already exist.
>
> Skipping this is usually **silent**, not loud: when a release adds a new spec field, an apiserver still running the old schema **prunes** that field out of every object you apply. The object admits cleanly, `kubectl get -o yaml` shows no trace of the field, and the feature you configured simply never happens. If a newly-documented field seems to be ignored, check the live CRD before anything else (`kubectl get crd <plural>.kopiur.home-operations.com -o yaml | grep <field>`); `kubectl kopiur doctor` flags known-stale schemas too.

The CRDs and RBAC shipped by the chart are **generated** by `cargo xtask gen-crds` / `cargo xtask gen-rbac` and checked in under `deploy/crds/` and `deploy/rbac/`. Those xtasks are the source of truth.

Upgrading **from 0.5.x to 0.6.0** is a special case: the CRDs used to be release-owned templates and moved into this `crds/` directory, so the crossing removes and re-installs them (cascade-deleting your CRs) unless you pin them first. See [Upgrading → 0.5.x → 0.6.0](upgrade.md#upgrading-05x--060-one-time-crd-migration) before you upgrade.

## First backup

After install, create a repository and start backing up a PVC. The smallest end-to-end example is `deploy/examples/01-single-pvc-scheduled.yaml`:

```bash
kubectl apply -f deploy/examples/01-single-pvc-scheduled.yaml
kubectl get repositories,snapshotpolicies,snapshotschedules -n billing
```

Eight runnable walkthroughs live in `deploy/examples/`:

| File                               | Pattern                                             |
| ---------------------------------- | --------------------------------------------------- |
| `01-single-pvc-scheduled.yaml`     | Single PVC, scheduled daily                         |
| `02-cluster-repository.yaml`       | Shared platform `ClusterRepository` (cluster scope) |
| `03-restore-by-backup.yaml`        | Restore by picking a `Snapshot`                       |
| `04-multi-pvc-selector.yaml`       | Multi-PVC label selector + group snapshot           |
| `05-deploy-or-restore-gitops.yaml` | Deploy-or-restore (PVC `dataSourceRef`)             |
| `06-manual-backup.yaml`            | Manual one-shot `Snapshot`                            |
| `07-restore-discovered.yaml`       | Restore a discovered / foreign snapshot             |
| `08-maintenance.yaml`              | `kopia maintenance` schedule + ownership lease      |

## Observability

- The controller serves `/metrics`, `/healthz`, and `/readyz` on its single port (`metrics.port`, default `:8081`); the webhook serves `/metrics` on its TLS port (`webhook.port`, default `:8443`). All metrics are under the `kopiur_` namespace.
- The controller and webhook default to the **dual-stack wildcard bind** (`[::]:8081` / `[::]:8443`), which serves both IPv4 and IPv6 kubelets/API servers, so probes work out of the box on either. The chart renders `[::]:<port>` into `KOPIUR_HTTP_ADDR` (controller) and `KOPIUR_WEBHOOK_ADDR` (webhook). On a host where **IPv6 is disabled in the pod network namespace** a `[::]` bind fails outright — force an IPv4-only bind by adding `KOPIUR_HTTP_ADDR=0.0.0.0:8081` through the controller's `extraEnv`. See [Helm chart values → Controller Deployment](configuration.md#controller-deployment) for the full detail.
- The controller's metrics `Service` is **always** created (the `:8081` listener co-hosts `/metrics` with the probes, so there's nothing to disable).
- `monitoring.serviceMonitor.enabled=true` creates a Prometheus-Operator `ServiceMonitor` (requires the Prometheus-Operator CRDs); `monitoring.prometheusRule.enabled=true` ships the kopiur alert rules.
- `monitoring.dashboards.enabled=true` ships the Grafana dashboard as a sidecar-discoverable `ConfigMap` (source: `deploy/dashboards/kopiur.json`). Set `monitoring.dashboards.grafanaOperator.enabled=true` to instead render a [grafana-operator](https://grafana.github.io/grafana-operator/) `GrafanaDashboard` CR from the same JSON (use `monitoring.dashboards.grafanaOperator.matchLabels` to select the Grafana instance).
- `observability.otlp.enabled=true` (with `observability.otlp.endpoint`) additionally exports OTLP **traces, logs, and a metrics push** from the controller, webhook, and mover Jobs. Off by default.

Turn it all on with the ready-made overlay:

```bash
helm upgrade kopiur oci://ghcr.io/home-operations/charts/kopiur -n kopiur-system \
  -f deploy/observability-values.yaml
```

See [`docs/dev/observability.md`](dev/observability.md) for the full metric list, OTLP details, and a sample collector config.

## Upgrade / uninstall

```bash
helm upgrade kopiur oci://ghcr.io/home-operations/charts/kopiur -n kopiur-system   # does NOT touch crds/ — apply schema changes yourself (see CRD lifecycle)
helm uninstall kopiur -n kopiur-system                                             # leaves the crds/ CRDs (and your CRs) in place
```

Upgrading **from 0.5.x** needs a one-time pre-step so the CRD move doesn't delete your resources — see [Upgrading](upgrade.md).

## See also

- Every chart value, walked through: [Helm chart values](configuration.md)
- Chart values & modes: [`deploy/helm/kopiur/README.md`](https://github.com/home-operations/kopiur/blob/main/deploy/helm/kopiur/README.md)
