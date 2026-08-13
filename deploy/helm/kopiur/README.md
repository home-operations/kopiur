# kopiur

![Version](https://img.shields.io/static/v1?label=Version&message=0.10.2&color=informational&style=flat-square) <!-- x-release-please-version -->
![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square)
![AppVersion](https://img.shields.io/static/v1?label=AppVersion&message=0.10.2&color=informational&style=flat-square) <!-- x-release-please-version -->

Kopiur — a Kopia-native Kubernetes backup operator written in Rust.
Installs the controller, admission webhook, the 8 kopiur.home-operations.com/v1alpha1 CRDs,
and the RBAC required to run them.

**Homepage:** <https://github.com/home-operations/kopiur>

Requires Kubernetes **>= 1.32** — the floor matches the default
`streamingLists: true` (the WatchList API is beta from 1.32, GA in 1.34). Set
`streamingLists: false` on an apiserver with the feature gate disabled.

## TL;DR

```bash
# cluster install (default): manages kopiur objects cluster-wide and reconciles
# ClusterRepository. The webhook cert is self-managed by default — no
# cert-manager, no manual steps. Installing from the published OCI chart is the
# preferred path — its images are digest-pinned to the release and it is
# cosign-signed (add --version x.y.z to pin a release):
helm install kopiur oci://ghcr.io/home-operations/charts/kopiur \
  --namespace kopiur-system --create-namespace
```

Working from a checkout instead? Swap the OCI ref for the local chart path
`deploy/helm/kopiur` — the dev copy floats image tags (empty digests).

See [`docs/install.md`](../../../docs/install.md) for the full quickstart and prerequisites.

## Install modes

### Scope: `cluster` (default) vs `namespaced`

| `installScope` | RBAC | What it manages | `ClusterRepository` |
|---|---|---|---|
| `cluster` | `ClusterRole` + `ClusterRoleBinding` | `kopiur.home-operations.com` objects **cluster-wide** | reconciled |
| `namespaced` | `Role` + `RoleBinding` | `kopiur.home-operations.com` objects in the **release namespace** only | not reconciled |

`cluster` is the default: a namespace-scoped `Role` can't reach the cluster-scoped `ClusterRepository` kind, so a namespaced install silently can't run a shared backup tier. Choose `namespaced` as the explicit least-privilege opt-down for a single-team install.

```bash
helm install kopiur oci://ghcr.io/home-operations/charts/kopiur --set installScope=namespaced ...
```

The RBAC rules are **synced from `cargo xtask gen-rbac`** (the checked-in `deploy/rbac/operator-*.yaml`), which derives the `kopiur.home-operations.com` permissions from the kube-rs `Resource` traits. The xtask is the source of truth; the chart templates carry a header comment to that effect and own only the names/labels.

### Webhook TLS: `webhook.tls.mode`

The admission webhook always serves TLS. `webhook.tls.mode` picks how the serving certificate is provisioned and trusted:

- **`self`** (default) — the operator mints its own CA + serving cert, writes the `Secret` (`webhook.tls.secretName`), and injects the `caBundle` into both webhook configurations itself. **No cert-manager, no manual steps.** The leaf is auto-rotated before expiry and the webhook hot-reloads it with zero downtime. (The webhook pod waits in `ContainerCreating` until the controller mints the Secret — a few seconds after the controller is ready.)
- **`cert-manager`** — the chart provisions a cert-manager `Certificate` (+ a self-signed `Issuer`, unless you point `webhook.certManager.issuerRef` at your own) and lets cert-manager's `ca-injector` populate the `caBundle`. Requires cert-manager installed.
- **`manual`** — you supply the serving cert yourself: create the `Secret` named by `webhook.tls.secretName` (type `kubernetes.io/tls`) and set `webhook.caBundle` (base64 PEM) so the API server trusts the webhook.

In `self` mode the operator's ServiceAccount is granted the minimal extra RBAC to write that one Secret and `patch` the `caBundle` of its two webhook configurations (resourceName-scoped); a namespaced install also gets a tiny ClusterRole for the cluster-scoped webhook-config patch.

Disable the webhook entirely with `webhook.enabled=false` (validation then falls back to the controller's defensive checks only — not recommended).

### CRDs

The 9 CRDs ship in the chart's special `crds/` directory. Helm installs them on `helm install`, and — because Helm never touches `crds/` on `helm upgrade` — an accidental `helm uninstall` leaves them (and every `kopiur.home-operations.com` object) untouched, exactly what you want for a backup operator. The flip side: a plain helm-CLI **upgrade** does not update the CRD schema as the `v1alpha1` API evolves. For a helm-CLI upgrade, apply the new schema yourself with `kubectl apply -f deploy/crds/`; GitOps tooling with a `CreateReplace` CRD policy (e.g. Flux) upgrades the `crds/`-shipped CRDs automatically. **Upgrading from 0.5.x to 0.6.0 is a one-time special case** — the CRDs moved out of Helm-templated resources into this `crds/` directory, so the crossing removes and re-installs them (cascade-deleting your CRs) unless you pin them first; see [`docs/upgrade.md`](../../../docs/upgrade.md) before upgrading.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| affinity | object | `{}` |  |
| extraArgs | list | `[]` | Extra CLI args appended to the controller container. |
| extraEnv | list | `[]` | Extra environment variables for the controller container. |
| extraVolumeMounts | list | `[]` | Extra volume mounts on the controller container (pairs with extraVolumes). |
| extraVolumes | list | `[]` | Extra volumes on the controller pod. Use this to make a filesystem-backend repository reachable in-process (hostPath/NFS/PVC) so the controller can run short idempotent kopia ops directly. The e2e harness uses a hostPath here. |
| features.credentialProjection.enabled | bool | `false` | Grant the operator `secrets` create+patch+delete so `spec.credentialProjection` works: the operator copies a repository's credential Secret(s) into each mover Job's namespace (the chief win is a shared ClusterRepository whose Secret is pinned to one namespace) and cleans its copies up again (legacy-copy sweep, reap-on-shrink, reap-on-disable). SECURITY TRADE-OFF: `create` cannot be scoped to a Secret name, so the operator can write a Secret in any namespace it manages. |
| features.kopiaUi.enabled | bool | `false` | Grant the operator `secrets` create+patch+delete so `spec.server` (the kopia web-UI) works: it creates a generated-auth Secret, mirrors a ClusterRepository's credentials into the server namespace, and deletes both on teardown. SECURITY TRADE-OFF: grants unscoped create/delete on Secrets in any namespace the operator manages. |
| fullnameOverride | string | `""` | Override the full release-qualified name (defaults to "<release>-kopiur"). |
| global.affinity | object | `{}` |  |
| global.commonLabels | object | `{}` | Labels stamped on every rendered object (fleet-wide labelling). |
| global.imagePullSecrets | list | `[]` | Concatenated with the top-level imagePullSecrets; reaches controller, webhook, and mover Jobs. |
| global.nodeSelector | object | `{}` | Scheduling defaults every kopiur pod inherits unless the component (root = controller, webhook.*) sets its own. |
| global.tolerations | list | `[]` |  |
| global.topologySpreadConstraints | list | `[]` |  |
| image.digest | string | `""` | Pin by digest (e.g. "sha256:..."); takes precedence over tag. |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy for the controller. |
| image.repository | string | `"ghcr.io/home-operations/kopiur-controller"` | Full controller image repository (registry + path). |
| image.tag | string | `""` | Defaults to .Chart.AppVersion when empty. |
| imagePullSecrets | list | `[]` | Image pull secrets for the controller pod (concatenated with global.imagePullSecrets; also reaches the webhook pod and mover Jobs). |
| installScope | string | `"cluster"` | "cluster" (default) or "namespaced".   cluster    — RBAC is a ClusterRole/ClusterRoleBinding. The operator manages     kopiur.home-operations.com resources cluster-wide AND reconciles     ClusterRepository. This is the default because a namespace-scoped Role     silently disables the ClusterRepository kind (a cluster-scoped resource is     out of a Role's reach), turning kopiur into a shared-backup-tier operator     that can't do shared backup tiers.   namespaced — RBAC is a namespace-scoped Role/RoleBinding: the operator     manages resources only in its own namespace and ClusterRepository is NOT     reconciled. Choose this as the explicit least-privilege opt-down for a     single-team install where the reduced blast radius is worth losing     cluster-scoped repositories. |
| leaderElection | object | `{"enabled":true,"flowSchema":{"enabled":true,"matchingPrecedence":200},"timings":{}}` | Enable leader election. Required when replicaCount > 1; harmless at 1. |
| leaderElection.flowSchema.matchingPrecedence | int | `200` | Ordering against other FlowSchemas; lower wins. Must stay above the built-in `system-leader-election` (100) so kube's own controllers keep priority, and below `service-accounts` (9000) or this has no effect. |
| leaderElection.timings | object | `{}` | Leader-election protocol timings, in seconds. Leave unset to use the client-go defaults (lease 15 / renewDeadline 10 / renewPeriod 2 / retry 2). `renewDeadline` is a budget for a whole ROUND of renew attempts, not one attempt's timeout, and `renewPeriod + renewDeadline` must stay strictly under `leaseDuration` — the operator rejects any other combination at startup rather than risk two replicas both believing they lead. Widen these only if your control plane genuinely cannot answer a lease write inside the default window; prefer leaving `flowSchema` enabled instead. |
| livenessProbe | object | `{"httpGet":{"path":"/healthz","port":"metrics"},"initialDelaySeconds":10,"periodSeconds":15}` | Liveness probe for the controller container. The whole probe is passed through with `toYaml`, so you can retune timings/thresholds or swap the scheme entirely; set to `{}` to drop the probe. `port: metrics` is the named container port (metrics.port above). |
| logging.format | string | `"text"` | Console format: "text" (human-readable, default) or "json" (one structured object per line for Loki/ELK/Datadog). Unknown values degrade to text. |
| logging.level | string | `"info"` | Log level / filter directive (RUST_LOG style: error|warn|info|debug|trace; per-target works too, e.g. "info,kopia=debug" to see kopia's own progress in mover logs). |
| maxConcurrentDeleteJobs | int | `0` | 0 = uncapped (default); set to bound concurrent snapshot-delete batch Jobs across repositories (mass-deletion protection). Batching itself is the primary protection — one Job per repository per accumulation window, not one per Snapshot — so this cap is an opt-in backstop for a resource-constrained cluster, not a load-bearing safety mechanism: a small cap risks head-of-line-blocking every OTHER repository's deletions behind one slow/failing one. It does NOT bound how many Snapshots one batch Job deletes, nor does it gate whether a deletion is allowed at all — that's deletionProtection.threshold on the Repository/ClusterRepository. |
| metrics.port | int | `8081` | The controller's single operational port. Rendered into KOPIUR_HTTP_ADDR as "[::]:<port>" (dual-stack wildcard), co-hosting /metrics, /healthz and /readyz. The metrics Service and the probes both target this port. |
| monitoring.dashboards | object | `{"annotations":{},"enabled":false,"folder":"","folderAnnotation":"","grafanaOperator":{"allowCrossNamespaceImport":true,"enabled":false,"folder":"","matchLabels":{},"resyncPeriod":"10m"},"label":"grafana_dashboard","labelValue":"1","labels":{},"namespace":""}` | Grafana dashboard(s) for the kopiur fleet. The same JSON lives in deploy/dashboards/kopiur.json (the single source of truth, copied into the chart by `cargo xtask gen-all`) for manual import. By default it ships as a ConfigMap labeled for the Grafana sidecar to auto-discover; flip grafanaOperator.enabled to render a grafana-operator GrafanaDashboard CR from the very same JSON instead. |
| monitoring.dashboards.annotations | object | `{}` | Extra annotations added to the dashboard object (ConfigMap or CR). |
| monitoring.dashboards.enabled | bool | `false` | Create the dashboard (a sidecar ConfigMap by default). |
| monitoring.dashboards.folderAnnotation | string | `""` | Annotation setting the Grafana folder for the sidecar ConfigMap (optional). |
| monitoring.dashboards.grafanaOperator.allowCrossNamespaceImport | bool | `true` | Allow a Grafana in any namespace to import this GrafanaDashboard. |
| monitoring.dashboards.grafanaOperator.enabled | bool | `false` | Render a grafana-operator GrafanaDashboard CR instead of the sidecar ConfigMap. |
| monitoring.dashboards.grafanaOperator.folder | string | `""` | Folder to create the dashboard in (Grafana folder name). |
| monitoring.dashboards.grafanaOperator.matchLabels | object | `{}` | spec.instanceSelector.matchLabels — selects which Grafana instance(s) load this dashboard. |
| monitoring.dashboards.grafanaOperator.resyncPeriod | string | `"10m"` | How often grafana-operator re-checks the dashboard for updates. |
| monitoring.dashboards.label | string | `"grafana_dashboard"` | Label the Grafana sidecar watches for (key: value). Adjust to your stack. |
| monitoring.dashboards.labels | object | `{}` | Extra labels added to the dashboard object (ConfigMap or CR). |
| monitoring.dashboards.namespace | string | `""` | Namespace for the dashboard object; defaults to the release namespace. |
| monitoring.prometheusRule.backupStaleAfterSeconds | int | `172800` | Age (seconds) after which a SnapshotPolicy's last success is "stale". |
| monitoring.prometheusRule.enabled | bool | `false` | Create a Prometheus-Operator PrometheusRule with kopiur alerts. |
| monitoring.prometheusRule.labels | object | `{}` | Extra labels (e.g. to match your Prometheus ruleSelector). |
| monitoring.serviceMonitor.enabled | bool | `false` | Create a Prometheus-Operator ServiceMonitor scraping the controller's /metrics (plain HTTP). Requires the ServiceMonitor CRD to exist. |
| monitoring.serviceMonitor.interval | string | `"30s"` | Scrape interval. |
| monitoring.serviceMonitor.labels | object | `{}` | Extra labels (e.g. to match your Prometheus serviceMonitorSelector). |
| monitoring.serviceMonitor.metricRelabelings | list | `[]` |  |
| monitoring.serviceMonitor.relabelings | list | `[]` | Relabelings / metricRelabelings passed through verbatim. |
| monitoring.serviceMonitor.scrapeTimeout | string | `"10s"` | Scrape timeout. |
| mover.image.digest | string | `""` | Pin the mover image by digest so a re-pulled tag can never change what runs in a data-protection Job. STRONGLY RECOMMENDED in production. |
| mover.image.pullPolicy | string | `"IfNotPresent"` | Pull policy used on the mover Job pods. |
| mover.image.repository | string | `"ghcr.io/home-operations/kopiur-mover"` | Full mover image repository (registry + path). |
| mover.image.tag | string | `""` | Defaults to .Chart.AppVersion when empty. |
| nameOverride | string | `""` | Override the chart name used in resource names (defaults to .Chart.Name = "kopiur"). |
| nodeSelector | object | `{}` | Scheduling controls (fall back to global.* when left empty). |
| observability.otlp.enabled | bool | `false` | Enable OTLP export (sets OTEL_EXPORTER_OTLP_ENDPOINT on all components). |
| observability.otlp.endpoint | string | `"http://otel-collector.observability.svc:4317"` | Collector gRPC endpoint. Required when enabled. Only gRPC is compiled in. |
| observability.otlp.extraEnv | list | `[]` | Extra raw env (e.g. OTEL_TRACES_SAMPLER) added to every component. |
| observability.otlp.headers | string | `""` | OTEL_EXPORTER_OTLP_HEADERS, e.g. "authorization=Bearer xyz". Empty to omit. |
| observability.otlp.protocol | string | `"grpc"` | OTEL_EXPORTER_OTLP_PROTOCOL (only "grpc" is supported by this build). |
| observability.otlp.strict | bool | `false` | Fail-fast on telemetry misconfiguration instead of degrading to fmt+pull. |
| podAnnotations | object | `{}` |  |
| podDisruptionBudget | object | `{"enabled":false,"minAvailable":1}` | PodDisruptionBudget for the controller. Keeps a voluntary disruption (node drain, cluster upgrade) from taking the controller to zero replicas. Only useful with replicaCount > 1. |
| podLabels | object | `{}` | Extra pod labels / annotations. |
| podSecurityContext.fsGroup | int | `65534` |  |
| podSecurityContext.runAsGroup | int | `65534` |  |
| podSecurityContext.runAsNonRoot | bool | `true` |  |
| podSecurityContext.runAsUser | int | `65534` |  |
| podSecurityContext.seccompProfile.type | string | `"RuntimeDefault"` |  |
| priorityClassName | string | `""` | Pod-level priority class. |
| rbac.browseRole | bool | `false` | Render an OPT-IN ClusterRole "<release>-browse" carrying exactly what a human needs to run the `kubectl kopiur ls/cat/download/browse` data-plane: read snapshots/repositories, create/delete the session Job + ConfigMap, and exec into the session pod. It deliberately grants NO access to Secrets — the session pod loads the repository credentials itself, so a browsing user never reads them (`--local` is the exception and additionally needs `get secrets`, granted separately). The chart only renders the ClusterRole; bind it to your users/groups with your own (Cluster)RoleBinding. |
| readinessProbe | object | `{"httpGet":{"path":"/readyz","port":"metrics"},"initialDelaySeconds":5,"periodSeconds":10}` | Readiness probe for the controller container (same passthrough as livenessProbe; set to `{}` to drop it). |
| reconcileConcurrency | int | `8` | Per-controller cap on concurrently running reconciles (the operator runs 8 controllers, so the process-wide worst case is 8x this). Bounds API-server load and file descriptors during re-list storms and API-server outages — unbounded reconcile concurrency is what let an apiserver flap exhaust the controller's fd table (EMFILE) within seconds. The default 8 clears a few-hundred-object re-list in seconds while keeping slow reconciles (hooks, in-process kopia ops) from starving a controller. 0 = unbounded (the pre-fix behavior; not recommended). |
| replicaCount | int | `1` | Number of controller replicas. >1 enables HA via leader election; only the elected leader reconciles, so deterministic jitter keeps schedules identical across replicas and across failover. Pair >1 with podDisruptionBudget and topologySpreadConstraints below to make it genuinely highly-available. |
| resources | object | `{"requests":{"cpu":"50m","memory":"128Mi"}}` | Resource requests for the controller pod. No limits by default. No CPU limit (CPU throttling on an operator only adds reconcile latency; the request reserves a fair share). No memory limit either: the controller's RSS can *burst* on startup/restart, not just steady state (~120Mi) — on (re)start it reconciles every existing resource at once, spawning concurrent in-process `kopia` subprocesses (whose RSS counts against this container's cgroup) to list/connect a repository that may hold many snapshots. A memory limit that doesn't cover that burst OOMKills the controller, which then crash-loops (OOM -> restart -> re-reconcile burst -> OOM). Set a limit only if you've measured your own ceiling. See crates/e2e/tests/lifecycle.rs. |
| securityContext.allowPrivilegeEscalation | bool | `false` |  |
| securityContext.capabilities.drop[0] | string | `"ALL"` |  |
| securityContext.readOnlyRootFilesystem | bool | `true` |  |
| serviceAccount.annotations | object | `{}` | Extra annotations (e.g. IRSA / Workload Identity role bindings). |
| serviceAccount.automount | bool | `true` | Mount the ServiceAccount token into the controller/webhook pods. |
| serviceAccount.create | bool | `true` | Create the ServiceAccount. Disable to bring your own. |
| serviceAccount.name | string | `""` | Name to use; defaults to the chart fullname when empty. |
| streamingLists | bool | `true` | Use the Kubernetes WatchList streaming-list API for the controller's cluster-wide watches, lowering peak memory during the initial resync by streaming pages instead of buffering them (the startup burst the resources note below warns about). On by default: WatchList is GA in Kubernetes 1.34 (beta 1.32/1.33) and the chart's kubeVersion floor is 1.32. Set `false` if your apiserver has the WatchList feature gate disabled — the watches degrade to paged lists either way, but turning it off skips the feature probe. |
| tolerations | list | `[]` |  |
| topologySpreadConstraints | list | `[]` | Spread controller replicas across nodes/zones. Only meaningful with replicaCount > 1; pairs with podDisruptionBudget so a drain can't collapse both replicas onto one node and then evict them together. |
| webhook.affinity | object | `{}` |  |
| webhook.caBundle | string | `""` | Base64-encoded PEM CA bundle injected into the webhook configurations. Only used when tls.mode is manual; required there so the API server trusts the serving cert. Ignored in self and cert-manager modes (caBundle is populated by the operator or cert-manager's ca-injector respectively). |
| webhook.certManager.issuerRef | object | `{"kind":"Issuer","name":""}` | Use an existing Issuer/ClusterIssuer instead of the self-signed Issuer this chart creates. Only used when tls.mode is cert-manager. Leave name empty to use the chart-managed self-signed Issuer. |
| webhook.enabled | bool | `true` | Deploy the webhook (Deployment + Service + Validating/Mutating configs). When false, validation falls back to the controller's defensive checks only. |
| webhook.extraEnv | list | `[]` | Extra environment variables for the webhook container (list of `{name, value}` / `{name, valueFrom}` entries), appended after the operator-managed env. Mirrors the root `extraEnv`. |
| webhook.failurePolicy | string | `"Fail"` | failurePolicy for both webhook configurations: Fail (fail-closed, recommended for a backup operator) or Ignore. Fail means a webhook outage blocks kopiur CR writes — see podDisruptionBudget below for why HA matters. |
| webhook.image.digest | string | `""` | Pin by digest (e.g. "sha256:..."); takes precedence over tag. |
| webhook.image.pullPolicy | string | `"IfNotPresent"` | Image pull policy for the webhook. |
| webhook.image.repository | string | `"ghcr.io/home-operations/kopiur-webhook"` | Full webhook image repository (registry + path). |
| webhook.image.tag | string | `""` | Defaults to .Chart.AppVersion when empty. |
| webhook.livenessProbe | object | `{"httpGet":{"path":"/healthz","port":"https","scheme":"HTTPS"},"initialDelaySeconds":5,"periodSeconds":15}` | Liveness probe for the webhook container. Passed through with `toYaml` (retune timings/thresholds or swap the probe; `{}` drops it). The webhook only serves HTTPS, so `scheme: HTTPS` on the named `https` port. |
| webhook.nodeSelector | object | `{}` | Scheduling controls (fall back to global.* when left empty). |
| webhook.podAnnotations | object | `{}` |  |
| webhook.podDisruptionBudget | object | `{"enabled":false,"minAvailable":1}` | PodDisruptionBudget for the webhook — the most important one to enable in HA. With failurePolicy: Fail, a node drain that evicts the only webhook replica blocks every kopiur CR write until it reschedules; a PDB with replicaCount > 1 keeps one replica serving through the drain. |
| webhook.podLabels | object | `{}` |  |
| webhook.podSecurityContext | object | `{"fsGroup":65534,"runAsGroup":65534,"runAsNonRoot":true,"runAsUser":65534,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod security context for the webhook pod. Kept at the most locked-down posture (the webhook is pure admission and never touches a repository), so relaxing the controller's context never loosens the webhook. |
| webhook.port | int | `8443` | The webhook's container port. Rendered into KOPIUR_WEBHOOK_ADDR as "[::]:<port>" (dual-stack wildcard); the Service maps 443 -> this port. |
| webhook.priorityClassName | string | `""` |  |
| webhook.readinessProbe | object | `{"httpGet":{"path":"/readyz","port":"https","scheme":"HTTPS"},"initialDelaySeconds":5,"periodSeconds":10}` | Readiness probe for the webhook container (same passthrough as livenessProbe; set to `{}` to drop it). |
| webhook.replicaCount | int | `1` |  |
| webhook.resources.requests.cpu | string | `"25m"` |  |
| webhook.resources.requests.memory | string | `"64Mi"` |  |
| webhook.securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container security context for the webhook. |
| webhook.serviceMonitor.enabled | bool | `false` | Create a ServiceMonitor scraping the webhook's /metrics over HTTPS. |
| webhook.serviceMonitor.insecureSkipVerify | bool | `true` | The webhook serves a self-signed cert, so skip verification by default. |
| webhook.serviceMonitor.interval | string | `"30s"` |  |
| webhook.serviceMonitor.labels | object | `{}` |  |
| webhook.serviceMonitor.scrapeTimeout | string | `"10s"` |  |
| webhook.timeoutSeconds | int | `10` | timeoutSeconds for admission requests (1..30). |
| webhook.tls.mode | string | `"self"` | How the webhook serving certificate is provisioned and trusted. One of:   self         — the operator mints its own CA + serving cert, writes the                  Secret, and injects caBundle into the webhook                  configurations itself. No cert-manager, no manual steps,                  and the leaf is auto-rotated before expiry. (default)   cert-manager — cert-manager issues the serving cert and its ca-injector                  populates caBundle (requires cert-manager installed;                  configure certManager.issuerRef below).   manual       — you pre-create the tls.secretName Secret (kubernetes.io/tls)                  and set webhook.caBundle (base64 PEM) yourself. |
| webhook.tls.secretName | string | `"kopiur-webhook-tls"` | Name of the Secret holding tls.crt / tls.key (and, in self mode, ca.crt). In self mode the operator creates and owns it; in cert-manager mode cert-manager writes it; in manual mode YOU create it before install. |
| webhook.tolerations | list | `[]` |  |
| webhook.topologySpreadConstraints | list | `[]` | Spread webhook replicas across nodes/zones (pairs with podDisruptionBudget). |
| workerThreads | int | `2` | Tokio worker threads for the controller runtime. The controller is I/O-bound, so a small pool is ample; the runtime default sizes to the host core count (ignoring the cgroup CPU quota), over-allocating worker threads — each a stack plus a malloc arena — on large nodes, inflating RSS for no throughput gain. Raise only for a reconcile-heavy deployment. |

### Observability

Metrics are always available on the controller's `/metrics` (also `/healthz`, `/readyz`); enable `metrics.serviceMonitor` to scrape them. Turning on `observability.otlp` additionally exports **traces, logs, and a metrics push** over OTLP from the controller, webhook, and mover Jobs (the controller passes the `OTEL_*` env through to the Jobs it creates) — set `observability.otlp.endpoint` to your collector's gRPC port. All metrics are under the `kopiur_` namespace; see [`docs/dev/observability.md`](../../../docs/dev/observability.md) for the full metric list, env vars, and a sample collector config. A ready-made values overlay that turns everything on is at `deploy/observability-values.yaml`. The dashboard JSON also lives at `deploy/dashboards/kopiur.json` for manual Grafana import.

## Verify a render locally

```bash
helm lint deploy/helm/kopiur
helm template kopiur deploy/helm/kopiur --set installScope=cluster --set webhook.tls.mode=cert-manager
```

## Maintainers

| Name | Email | Url |
| ---- | ------ | --- |
| kopiur maintainers |  |  |

## Source Code

* <https://github.com/home-operations/kopiur>

## Requirements

Kubernetes: `>=1.32.0-0`

---

_This README is generated by [helm-docs](https://github.com/norwoodj/helm-docs) from `Chart.yaml` and `values.yaml`. Edit those (or `README.md.gotmpl`) and run `mise run helm-docs`._
