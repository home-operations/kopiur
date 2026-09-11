# kopiur

![Version](https://img.shields.io/static/v1?label=Version&message=0.10.8&color=informational&style=flat-square) <!-- x-release-please-version -->
![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square)
![AppVersion](https://img.shields.io/static/v1?label=AppVersion&message=0.10.8&color=informational&style=flat-square) <!-- x-release-please-version -->

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

The 9 CRDs ship in the chart's special `crds/` directory. Helm installs them on `helm install`, and — because Helm never touches `crds/` on `helm upgrade` — an accidental `helm uninstall` leaves them (and every `kopiur.home-operations.com` object) untouched, exactly what you want for a backup operator. The flip side: a plain helm-CLI **upgrade** does not update the CRD schema as the `v1alpha1` API evolves. For a helm-CLI upgrade, apply the new schema yourself with `kubectl apply --server-side -f deploy/crds/`; GitOps tooling with a `CreateReplace` CRD policy (e.g. Flux) upgrades the `crds/`-shipped CRDs automatically. **Upgrading from 0.5.x to 0.6.0 is a one-time special case** — the CRDs moved out of Helm-templated resources into this `crds/` directory, so the crossing removes and re-installs them (cascade-deleting your CRs) unless you pin them first; see [`docs/upgrade.md`](../../../docs/upgrade.md) before upgrading.

### Web console (`ui.enabled`)

Off by default. `ui.enabled: true` deploys `kopiur-ui`, an in-cluster web console that reads and acts on the kopiur CRDs **as the signed-in user**: it takes the user's identity from a header your authenticating proxy sets, and impersonates them on every apiserver call, so Kubernetes RBAC — not the console — decides what each person may see and do.

Three things are required, and the chart `fail`s at render time (naming the value to set) rather than shipping a console that quietly lets the wrong person act as someone else:

- **`installScope: cluster`** — impersonation and the console's cluster-wide reads cannot be expressed in a namespaced `Role`.
- **An identity source** — `ui.auth.userHeader` (header mode) or `ui.auth.anonymous.*` (one fixed identity). With neither, every request would run as the console's own ServiceAccount, which can impersonate anyone.
- **A proxy shared secret in header mode** — `ui.auth.proxySecret.existingSecret`, a token your proxy sends as `X-Kopiur-Proxy-Token`. Without it, any pod that can reach the Service (or anyone with `services/proxy`) can send `X-Forwarded-User` and be authorized as that person. `ui.auth.acknowledgeNoProxySecret: true` overrides it, and is refused outright alongside a non-`ClusterIP` `ui.service.type`.

The chart renders **no Ingress**, in any mode, and creates **no role bindings**. Point your own proxy at the `ClusterIP` Service, put your edge in front of the proxy, and bind `<release>-ui-user` to your people — until you do, a freshly-enabled console reports `Forbidden` on every screen, which is the correct first experience rather than a broken install.

`ui.rbac.browseRole` is separate and off by default for a reason: **granting the in-snapshot file browser in a namespace grants that namespace's repository credentials**, because the browse session pod loads them from its environment and `pods/exec` cannot be narrowed to one pod by RBAC. Bind it with a namespaced `RoleBinding`, never cluster-wide.

Full documentation — trust model, the four roles, browse sessions, caps, and troubleshooting by problem type — is in [`docs/ui.md`](../../../docs/ui.md), with a worked oauth2-proxy deployment in [`deploy/examples/45-web-ui.yaml`](../../examples/45-web-ui.yaml).

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| affinity | object | `{}` |  |
| extraArgs | list | `[]` | Extra CLI args appended to the controller container. |
| extraEnv | list | `[]` | Extra environment variables for the controller container. |
| extraVolumeMounts | list | `[]` | Extra volume mounts on the controller container (pairs with extraVolumes). |
| extraVolumes | list | `[]` | Extra volumes on the controller pod. Use this to make a filesystem-backend repository reachable in-process (hostPath/NFS/PVC) so the controller can run short idempotent kopia ops directly. The e2e harness uses a hostPath here. |
| features.credentialProjection.enabled | bool | `false` | Enable `spec.credentialProjection` (copy a repository's credential Secret(s) into each mover Job's namespace) by granting the operator `secrets` create+patch+delete. The chief win is a shared ClusterRepository whose Secret is pinned to one namespace; the operator also cleans its copies up again (legacy-copy sweep, reap-on-shrink, reap-on-disable). SECURITY TRADE-OFF: `create` cannot be scoped to a Secret name, so the operator can write a Secret in any namespace it manages. |
| features.kopiaUi.enabled | bool | `false` | Enable `spec.server` (the kopia web-UI) by granting the operator `secrets` create+patch+delete. The server needs a generated-auth Secret, a mirror of each of a ClusterRepository's credential Secrets in the server namespace, and deletion of all of them on teardown. SECURITY TRADE-OFF: grants unscoped create/delete on Secrets in any namespace the operator manages. |
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
| leaderElection.flowSchema | object | `{"enabled":true,"matchingPrecedence":200}` | API Priority and Fairness lane for the election Lease. Kubernetes' own `system-leader-election` FlowSchema covers only kube-system ServiceAccounts, so an operator in its own namespace has its lease renewals queued behind all other ServiceAccount traffic in `workload-low` — the congestion behind the controller restart storms in #319. This puts Kopiur's `leases` calls (and only those) into the built-in guaranteed `leader-election` priority level. Turn off if APF is disabled on the cluster or you cannot create cluster-scoped flowcontrol.apiserver.k8s.io objects. |
| leaderElection.flowSchema.matchingPrecedence | int | `200` | Ordering against other FlowSchemas; lower wins. Must stay above the built-in `system-leader-election` (100) so kube's own controllers keep priority, and below `service-accounts` (9000) or this has no effect. |
| leaderElection.timings | object | `{}` | Leader-election protocol timings, in seconds. Leave unset to use the client-go defaults (lease 15 / renewDeadline 10 / renewPeriod 2 / retry 2). `renewDeadline` is a budget for a whole ROUND of renew attempts, not one attempt's timeout, and `renewPeriod + renewDeadline` must stay strictly under `leaseDuration` — the operator rejects any other combination at startup rather than risk two replicas both believing they lead. Widen these only if your control plane genuinely cannot answer a lease write inside the default window; prefer leaving `flowSchema` enabled instead. |
| livenessProbe | object | `{"httpGet":{"path":"/healthz","port":"metrics"},"initialDelaySeconds":10,"periodSeconds":15}` | Liveness probe for the controller container. The whole probe is passed through with `toYaml`, so you can retune timings/thresholds or swap the scheme entirely; set to `{}` to drop the probe. `port: metrics` is the named container port (metrics.port above). |
| logging.format | string | `"text"` | Console format: "text" (human-readable, default) or "json" (one structured object per line for Loki/ELK/Datadog). Unknown values degrade to text. |
| logging.level | string | `"info"` | Log level / filter directive (RUST_LOG style: error|warn|info|debug|trace; per-target works too, e.g. "info,kopia=debug" to see kopia's own progress in mover logs). |
| maxConcurrentDeleteJobs | int | `0` | 0 = uncapped (default); set to bound concurrent snapshot-delete batch Jobs across repositories (mass-deletion protection). Batching itself is the primary protection — one Job per repository per accumulation window, not one per Snapshot — so this cap is an opt-in backstop for a resource-constrained cluster, not a load-bearing safety mechanism: a small cap risks head-of-line-blocking every OTHER repository's deletions behind one slow/failing one. It does NOT bound how many Snapshots one batch Job deletes, nor does it gate whether a deletion is allowed at all — that's deletionProtection.threshold on the Repository/ClusterRepository. |
| maxConcurrentJobs | int | `0` | 0 = uncapped (default); set to bound how many POOLED mover Jobs — backup snapshots, restores, and the source side of either replication — may run at once across ALL repositories. This is the cluster-operator BACKSTOP beneath each repository's own spec.concurrency.maxConcurrentJobs, which stays the primary knob: a run must satisfy both, and whichever cap it meets first holds it. Set this when the node pool cannot host N movers no matter which repositories they belong to; set the per-repository field when one BACKEND is the thing that must not be saturated. Restores are ALWAYS admitted and never queued (a recovery in progress must not wait behind routine backups), though a running restore still occupies a slot and so displaces backups. Maintenance, verification, pin and snapshot-delete Jobs are outside the pool entirely. Leaving this at 0 costs nothing: with no cap set anywhere the operator performs no extra API calls at all. |
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
| ui.affinity | object | `{}` |  |
| ui.auth.acknowledgeNoProxySecret | bool | `false` | Deploy header mode WITHOUT a proxy shared secret. Mirrors the repositories' `acknowledgeInsecure`: the unsafe posture stays reachable, but only for someone who wrote down that they meant it — and it is greppable in your values afterwards. Only set this when something else (a service mesh with mTLS, a NetworkPolicy) genuinely prevents anything but your proxy from reaching the Service. |
| ui.auth.allowedGroups | list | `[]` | Allow-list of groups the console may impersonate. Empty means "whatever groups the proxy asserts". Set, it is enforced TWICE: the console filters inbound groups, and the same list is pinned as `resourceNames` on the impersonate rule so the apiserver enforces it even if the console did not. |
| ui.auth.anonymous.enabled | bool | `false` | Run every request as one fixed identity instead of reading a header. Sensible only where an outer layer has already authenticated everyone identically, or for a read-only demo. The console's impersonate rules are then pinned by NAME to this identity, so it cannot become anyone else even if the header parsing were fooled. |
| ui.auth.anonymous.fallback | bool | `false` | In HEADER mode, serve a request whose identity header is absent as the anonymous identity instead of rejecting it with 401. Off by default and never implicit: a silent downgrade from "the proxy said who you are" to "everyone is this fixed user" is exactly the failure this flag exists to make deliberate. Requires anonymous.enabled and anonymous.user. |
| ui.auth.anonymous.groups | list | `[]` | Groups paired with that user. |
| ui.auth.anonymous.user | string | `""` | The username to impersonate. Required when anonymous.enabled. May not be a `system:` principal (`system:masters` least of all) — the console refuses to start on one, and so does this chart's RBAC. |
| ui.auth.emailHeader | string | `""` | Header carrying the caller's email. Display only: never impersonated, never used for authorization. |
| ui.auth.groupsHeader | string | `""` | Header carrying the caller's groups (e.g. X-Forwarded-Groups). Optional — an identity with no groups is valid, it just matches fewer RoleBindings. |
| ui.auth.groupsSeparator | string | `","` | Separator the groups header uses. oauth2-proxy emits ",". |
| ui.auth.impersonateExtraKeys | list | `[]` | `userextras` keys the console may impersonate, as a list. Each one becomes its OWN `userextras/<key>` rule in the console's ClusterRole — Kubernetes RBAC has no wildcard there, so a `userextras/*` would authorize nothing and fail at the first request instead of at install. |
| ui.auth.proxySecret.existingSecret | string | `""` | Name of an existing Secret holding the shared token the proxy must send in `X-Kopiur-Proxy-Token`. YOU create it (and configure the proxy to send it); the chart only mounts it. This is what makes the identity headers trustworthy, so without it — and without the acknowledgement below — the chart refuses to render. |
| ui.auth.proxySecret.key | string | `"token"` | Key within that Secret holding the token. |
| ui.auth.userHeader | string | `""` | Header the proxy puts the authenticated username in (e.g. X-Forwarded-User). Setting it selects header mode; a request without it gets a 401 unless auth.anonymous.fallback is on. |
| ui.cache.enabled | bool | `true` | Serve reads from watch-fed stores under the console's own ServiceAccount, filtered per user with SubjectAccessReviews, instead of one impersonated LIST per request. This is the ONLY reason the console reads a Kopiur object as itself, so turning it off REMOVES those grants from its ClusterRole entirely. Leave it on unless you want every read impersonated. |
| ui.download.chunkTimeout | string | `"60s"` | How long a download may make NO progress before it is abandoned. Not a total-transfer budget (a real restore outlives any fixed deadline) — it bounds the silence, so one stalled transfer cannot hold an exec slot and a kopia process forever. |
| ui.download.maxBytes | int | `1073741824` | Largest single file the console will stream, in bytes (default 1 GiB). Checked before streaming starts, so an oversized request is a 413 rather than a half-gigabyte of wasted egress. |
| ui.enabled | bool | `false` | Deploy the web console. Requires installScope: cluster — impersonation and the console's cluster-wide reads cannot be expressed in a namespaced Role, so the chart refuses a namespaced install rather than ship a console that 403s on its first request. |
| ui.extraEnv | list | `[]` | Extra environment variables for the console container, appended after the chart-managed env. Anything in `crates/ui/src/config.rs` can be set here. |
| ui.image.digest | string | `""` | Pin by digest (e.g. "sha256:..."); takes precedence over tag. |
| ui.image.pullPolicy | string | `"IfNotPresent"` | Image pull policy for the console. |
| ui.image.repository | string | `"ghcr.io/home-operations/kopiur-ui"` | Full web UI image repository (registry + path). |
| ui.image.tag | string | `""` | Defaults to .Chart.AppVersion when empty. |
| ui.limits.clientCacheSize | int | `256` | How many per-identity impersonating clients to keep. Each holds a connection pool, so this is a socket and memory bound. |
| ui.limits.clientCacheTtl | string | `"10m"` | How long an idle cached client survives. |
| ui.limits.maxManifestBytes | int | `67108864` | Largest kopia JSON manifest the console will buffer from an exec, in bytes (default 64 MiB). A directory with a million entries must fail as a 422, not as an OOM kill. |
| ui.limits.sarCacheSize | int | `4096` | How many SubjectAccessReview answers to retain. |
| ui.limits.sarTtl | string | `"60s"` | How long a SubjectAccessReview answer is reused. Short by design: a revoked RoleBinding must stop hiding behind the cache quickly. |
| ui.limits.snapshotListCap | int | `5000` | Largest `limit` the snapshot list endpoint accepts. |
| ui.livenessProbe | object | `{"httpGet":{"path":"/healthz","port":"ops"},"initialDelaySeconds":5,"periodSeconds":15}` | Liveness probe. On the OPS port, never the app port: the app port is firewalled to the proxy, and a probe there would either fail or force a hole. |
| ui.networkPolicy.enabled | bool | `false` | Render a NetworkPolicy admitting the app port only from your proxy and the ops port only from your monitoring stack. STRONGLY RECOMMENDED in header mode: it is what makes "only the proxy can set identity headers" true at the network layer rather than only on paper. |
| ui.networkPolicy.monitoringSelector | object | `{}` | Ingress peer allowed to reach the ops port (/metrics, probes). Leave empty to admit the ops port from anywhere in the cluster, which is the usual posture for a metrics endpoint that exposes no user data. |
| ui.networkPolicy.proxySelector | object | `{}` | Ingress peer allowed to reach the app port, as a NetworkPolicy peer (`namespaceSelector` and/or `podSelector`). Required when enabled — a policy with no peer would admit nothing, or (written loosely) everything. |
| ui.nodeSelector | object | `{}` | Scheduling controls (fall back to global.* when left empty). |
| ui.opsPort | int | `8091` | Container port serving /metrics, /healthz and /readyz. A SECOND listener on purpose: probes and Prometheus must stay reachable from the monitoring namespace while the app port is restricted to the proxy, and /metrics must never sit behind the identity middleware. |
| ui.podAnnotations | object | `{}` |  |
| ui.podDisruptionBudget.enabled | bool | `false` | PodDisruptionBudget for the console. Pair with replicaCount > 1. |
| ui.podDisruptionBudget.minAvailable | int | `1` |  |
| ui.podLabels | object | `{}` |  |
| ui.podSecurityContext | object | `{"fsGroup":65532,"runAsGroup":65532,"runAsNonRoot":true,"runAsUser":65532,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod security context for the console pod. Same locked-down posture as the webhook: it runs no kopia, writes no files, and needs no privileges. |
| ui.port | int | `8090` | Container port serving the SPA and /api. Rendered into KOPIUR_UI_ADDR as "[::]:<port>" (dual-stack wildcard). |
| ui.priorityClassName | string | `""` |  |
| ui.rbac.browseRole | bool | `false` | Render `kopiur-ui-browse`, the role behind the in-snapshot file browser.  READ THIS BEFORE ENABLING. Browsing runs a session pod and `pods/exec`s kopia into it, and that pod loads the repository credentials from its own environment — so anyone who can exec in that namespace can print them with `env`. `pods/exec create` cannot be narrowed to one pod by RBAC. Granting browse in a namespace therefore GRANTS THAT NAMESPACE'S REPOSITORY CREDENTIALS. It is deliberately NOT aggregated into `kopiur-ui-user`: bind it on purpose, ideally with a namespaced RoleBinding, and see execPolicy below for the enforcement RBAC cannot express. |
| ui.rbac.execPolicy.enabled | bool | `false` | Render a ValidatingAdmissionPolicy that narrows browse `pods/exec` to `kopiur-browse-*` pods running `/usr/local/bin/kopia`, turning the console's closed command set into one the APISERVER enforces. This is the only real mitigation for what browseRole grants; it does NOT stop someone from exec'ing kopia commands, only from exec'ing `env` or a shell. |
| ui.rbac.execPolicy.extraPodPrefixes | list | `[]` | Extra pod-name prefixes the policy also accepts, for a non-default session naming scheme. The built-in `kopiur-browse-` is always allowed. |
| ui.rbac.execPolicy.subjects | list | `[]` | Subjects the policy applies to — the same users/groups you bound `kopiur-ui-browse` to, as `{kind: User|Group|ServiceAccount, name, namespace}`. Required when enabled: a policy matching nobody enforces nothing, so the chart refuses to render one. |
| ui.rbac.userRoles | bool | `true` | Render the human roles: `kopiur-ui-viewer` (read-only) and `kopiur-ui-editor` (the console's action buttons), which aggregate into `kopiur-ui-user`, plus the namespaced `kopiur-ui-doctor` Role. Bind `kopiur-ui-user` to your people — the chart creates no bindings, because who may operate your backups is not a chart decision.  These render only while `ui.enabled` is also true, so an install that does not run the console gains no ClusterRoles for a component it does not have. The consequence: setting `ui.enabled: false` DELETES these roles, and any (Cluster)RoleBinding you made to them is left with a dangling roleRef and grants nothing until you re-enable the console (bindings survive, so access returns by itself — nothing needs re-creating). That is only ever true while there is no console to use, which is why the roles stay gated rather than appearing in every install that has never heard of the UI. |
| ui.readinessProbe | object | `{"httpGet":{"path":"/readyz","port":"ops"},"initialDelaySeconds":5,"periodSeconds":10}` | Readiness probe (same passthrough as livenessProbe; `{}` drops it). /readyz reports the cache sync, so a console whose stores have not filled does not take traffic and answer with an empty fleet. |
| ui.replicaCount | int | `1` | Number of console replicas. Stateless: browse sessions live in the cluster as Jobs, not in the process, so any replica can serve any request. |
| ui.resources.requests.cpu | string | `"25m"` |  |
| ui.resources.requests.memory | string | `"64Mi"` |  |
| ui.securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container security context for the console. |
| ui.service.annotations | object | `{}` | Extra annotations on the console Service. |
| ui.service.labels | object | `{}` | Extra labels on the console Service. |
| ui.service.type | string | `"ClusterIP"` | Service type. Keep ClusterIP: this Service is for your proxy to reach, not for the internet — a LoadBalancer or NodePort here publishes a console that trusts identity headers to whoever can send them.  A non-ClusterIP type is allowed when the proxy really does live outside the cluster, but ONLY alongside auth.proxySecret: publishing the console AND setting auth.acknowledgeNoProxySecret is refused, because the pair is an unauthenticated console on the network. |
| ui.serviceAccount.annotations | object | `{}` | Extra annotations (e.g. IRSA / Workload Identity role bindings). |
| ui.serviceAccount.automount | bool | `true` | Mount the ServiceAccount token into the console pod. Required — the token IS how the console authenticates to impersonate. |
| ui.serviceAccount.create | bool | `true` | Create the console's own ServiceAccount. It is deliberately NOT the operator's: the operator reads Secrets, and the console must never be able to (it impersonates instead, so every read is a user's read). |
| ui.serviceAccount.name | string | `""` | Name to use; defaults to "<fullname>-ui" when empty. |
| ui.serviceMonitor.enabled | bool | `false` | Create a Prometheus-Operator ServiceMonitor scraping the console's /metrics on the ops port (plain HTTP). |
| ui.serviceMonitor.interval | string | `"30s"` |  |
| ui.serviceMonitor.labels | object | `{}` |  |
| ui.serviceMonitor.metricRelabelings | list | `[]` |  |
| ui.serviceMonitor.relabelings | list | `[]` |  |
| ui.serviceMonitor.scrapeTimeout | string | `"10s"` |  |
| ui.session.maxExecGlobal | int | `64` | Process-wide cap on concurrent pods/exec calls. |
| ui.session.maxExecPerIdentity | int | `4` | Cap on concurrent pods/exec calls ONE identity may have in flight. |
| ui.session.maxStarts | int | `4` | Cap on concurrent session-pod CREATIONS. |
| ui.session.readyTimeout | string | `"300s"` | How long `POST …/session` waits for the session pod to become ready. Generous: the pod may have to pull the mover image. |
| ui.session.ttl | string | `"15m"` | How long an idle browse-session pod lives before it is reaped. |
| ui.tls.existingSecret | string | `""` | Serve the app port over HTTPS from an existing kubernetes.io/tls Secret. Usually unnecessary: the proxy terminates TLS and this hop is in-cluster. |
| ui.tolerations | list | `[]` |  |
| ui.topologySpreadConstraints | list | `[]` |  |
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
