# RBAC reference

Everything Kopiur is allowed to do in your cluster, in one place. Use it for security review, for scoping a namespaced install, and for debugging `Forbidden` errors.

Kopiur runs as **three principals**:

| ServiceAccount | Who uses it | Bound to |
| --- | --- | --- |
| `kopiur-controller` | The controller Deployment **and** the admission webhook Deployment (they share one ServiceAccount) | `kopiur-controller` ClusterRole (cluster scope) or Role (namespaced scope) |
| `kopiur-mover` | Every ordinary mover `Job` (snapshot, restore, bootstrap, maintenance, verify, replicate, pin, delete) | `kopiur-mover` ClusterRole/Role |
| `kopiur-snapshot-replication-mover` | Only `SnapshotReplication` mover Jobs | `kopiur-snapshot-replication-mover` ClusterRole/Role |

The authoritative definitions are **generated** by `cargo xtask gen-rbac` into `deploy/rbac/`, as `operator-clusterrole.yaml`, `operator-role.yaml`, `mover-clusterrole.yaml` and `mover-role.yaml`.

The Helm chart templates, under `deploy/helm/kopiur/templates/`, are hand-synced copies of those files. If this page and the chart ever disagree, `deploy/rbac/` wins. Regenerate with `mise run gen`, and check for drift with `mise run gen-check`.

## The controller / webhook (`kopiur-controller`)

What each rule is **for**, grouped by purpose:

| API group → resources | Verbs | Why |
| --- | --- | --- |
| `kopiur.home-operations.com` → all 9 CRDs (`repositories`, `snapshotpolicies`, `snapshots`, `snapshotschedules`, `restores`, `maintenances`, `repositoryreplications`, `snapshotreplications`, `clusterrepositories`†) | get, list, watch, create, update, patch, delete | Reconcile every kind; schedules **create** `Snapshot` CRs; repositories **create** owned `Maintenance` CRs; retention **deletes** pruned `Snapshot` CRs. |
| same group → each CRD's `/status` and `/finalizers` | get, update, patch | Status is written via server-side apply (**a PATCH, so `patch` is required, not just `update`**); finalizers gate snapshot deletion. |
| core → `pods`, `persistentvolumeclaims`, `configmaps` | get, list, watch, create, update, patch, delete | Resolve workload pods for `inheritSecurityContextFrom` and hooks; create restore-target / cache PVCs; write the bootstrap result ConfigMap and sweep legacy work-spec ConfigMaps. |
| core → `pods/exec` | create, get | Run `hooks.beforeSnapshot/afterSnapshot` `workloadExec` commands inside the workload pod (quiesce/resume). |
| core → `events` **and** `events.k8s.io` → `events` | create, patch | The kube `Recorder` writes `events.k8s.io/v1` Events; **both** groups are needed (a common gotcha: without `events.k8s.io` every event write 403s). |
| core → `secrets` | get, list, watch, create, patch, delete | Read repository credential Secrets (and re-reconcile when they change); **create/patch/delete** is the credential-projection feature (copying a repo's Secret into a consumer namespace, plus reaping the copies: legacy-copy sweep, reap-on-shrink, reap-on-disable) and the self-managed webhook TLS Secret. |
| `batch` → `jobs` | get, list, watch, create, update, patch, delete | Create and track the mover Jobs; reap them per `failedJobsHistoryLimit`. |
| `snapshot.storage.k8s.io` → `volumesnapshots`; `groupsnapshot.storage.k8s.io` → `volumegroupsnapshots` | get, list, watch, create, **patch**, delete | CSI snapshot / group-snapshot copy methods (`SnapshotPolicy.spec.copyMethod`). |
| core → `serviceaccounts`; `rbac.authorization.k8s.io` → `rolebindings` | get, **list, watch** (SAs), create, update, patch | Mint the per-namespace mover ServiceAccount + RoleBinding on demand (see below). `list`/`watch` on ServiceAccounts re-reconciles a repository the moment its `auth.workloadIdentity` SA is created. |
| core → `namespaces` | get, list, watch | Read the `kopiur.home-operations.com/privileged-movers` annotation (the elevated-mover opt-in) and drive `pvcSelector` namespace selection. *(Cluster scope only.)* |
| `groupsnapshot.storage.k8s.io` → `volumegroupsnapshotclasses` | get, list, watch | Resolve the group-snapshot class for `groupBy: VolumeGroupSnapshot`, the same way `volumesnapshotclasses` is resolved for per-volume staging. *(Cluster scope only, which is why group staging needs `installScope: cluster`.)* |
| `admissionregistration.k8s.io` → `validatingwebhookconfigurations`, `mutatingwebhookconfigurations` (names `kopiur-validating` / `kopiur-mutating` only) | get, patch | Inject the self-managed CA bundle into the webhook configurations (`webhook.tls.mode: self`). *(Cluster scope only.)* |
| core → `secrets` (name `kopiur-webhook-tls` only) | update, patch | Rotate the self-managed webhook serving certificate. |

† In a **namespaced install**, meaning `installScope: namespaced`, the Role drops three things: `clusterrepositories`, because it is a cluster-scoped kind, the webhook-configuration rule, and the `namespaces` rule. Dropping `namespaces` is also why the privileged-mover gate fails *open* there: the operator cannot read namespace annotations, and the install is already confined to admin-chosen namespaces.

## The mover (`kopiur-mover`)

The mover is deliberately tiny. It can only report its result and read back what a prior attempt reported:

| API group → resources | Verbs | Why |
| --- | --- | --- |
| `kopiur.home-operations.com` → every CRD's `/status` | get, patch | `patch`: progress and the terminal result (snapshot id, stats, timing, `logTail`, `failure`) onto the CR that owns the Job. `get`: a restore mover re-reads the CR's pinned `status.resolved` through the status subresource, so a retried Job pod restores the same snapshot a prior attempt chose instead of re-resolving "latest". |
| core → `configmaps` | get, patch | Write bootstrap results back to the result ConfigMap (the work spec itself rides the Job env). |

It cannot read Secrets, because credentials arrive through `envFrom` on the Job. It cannot create or delete anything, and it cannot touch other namespaces.

### The snapshot-replication mover (`kopiur-snapshot-replication-mover`)

A [`SnapshotReplication`](snapshot-replication.md) run materializes each copied snapshot as a `Snapshot` object, and reaps discovered duplicates. Its mover therefore needs verbs no other mover may have.

That is exactly why it is a **separate** principal rather than a widening of `kopiur-mover`: a compromised backup mover pod must not be able to delete every `Snapshot` object in the namespace.

| API group → resources | Verbs | Why |
| --- | --- | --- |
| `kopiur.home-operations.com` → `snapshots` | get, list, create, patch, delete | Create the copy `Snapshot` CRs (`origin: replicated`), stamp the `pruned-by` annotation before a `retention`-mode prune, delete pruned copies and race-window duplicates. |
| same group → `snapshots/status`, `snapshotreplications/status` | get, patch | Write each copy's status atomically; report the run's terminal result and `lastRun` counters. |
| core → `configmaps` | get, patch | Same result-reporting channel as the ordinary mover. |

Like `kopiur-mover`, its ServiceAccount and RoleBinding are minted per namespace on demand, and it cannot read Secrets.

### The runtime-minted per-namespace mover identity

Mover Jobs run in the **workload's** namespace, so before creating a Job there the controller mints two objects, idempotently, with server-side apply:

1. a `kopiur-mover` ServiceAccount in that namespace, and
2. a RoleBinding from it to the `kopiur-mover` ClusterRole, or Role.

That is what the `serviceaccounts` and `rolebindings` rules above are for.

A tenant in that namespace could create pods that *use* the mover ServiceAccount. That is exactly why an **elevated** mover, meaning root UID, `privilegedMode`, or added capabilities, additionally requires the namespace to opt in with the `kopiur.home-operations.com/privileged-movers: "true"` annotation. See [Movers, RBAC & credentials](movers.md).

With **workload identity**, meaning `auth.workloadIdentity` on a cloud backend, the Job runs as the *user's* federated ServiceAccount instead. The controller never creates or modifies that ServiceAccount. It only `get`s it as a preflight check, and applies a RoleBinding called `kopiur-mover-wi-<sa>` to the same `kopiur-mover` role, so the mover can still patch its own `.status`.

/// note | Auditing tip
`kubectl auth can-i --list --as=system:serviceaccount:kopiur-system:kopiur-controller` shows the effective permissions on a live cluster. Diff it against `deploy/rbac/operator-clusterrole.yaml` if something looks off.
///

## Browsing snapshots (`rbac.browseRole`)

The `kubectl kopiur ls`, `cat`, `download` and `browse` commands read data as the **human's** kubeconfig identity, not as a ServiceAccount.

The chart can render an opt-in ClusterRole carrying exactly what browsing needs. Set `rbac.browseRole: true` to get `<release>-browse`, and bind it yourself. A namespaced RoleBinding limits a user to browsing snapshots in that one namespace.

| API group → resources | Verbs | Why |
| --- | --- | --- |
| `kopiur.home-operations.com` → `snapshots`, `repositories` | get, list | Resolve the Snapshot → repository chain. |
| `kopiur.home-operations.com` → `clusterrepositories` | get | Same chain for cluster-scoped repositories. |
| `apps` → `deployments` | get, list | Resolve the mover image from the controller Deployment (sessions run exactly what the operator runs). |
| `batch` → `jobs` | create, get, list, delete | Find-or-create the read-only session Job; `session end`. |
| core → `configmaps` | delete | `session end` cleans up a legacy session's work-spec ConfigMap (the spec rides the Job env today). |
| core → `pods` | get, list, watch | Wait for the session pod to become Ready. |
| core → `pods/log` | get | Surface the pod's logs when the session fails to start. |
| core → `pods/exec` | **create, get** | The read path: exec the closed kopia read-command set. Both verbs: an exec over WebSocket is an HTTP **upgrade**, which is a `GET`, so the apiserver authorizes it as `get pods/exec`; `create` alone covers only the older SPDY `POST` and leaves the user refused with a bare "cannot get resource pods/exec". |

There is deliberately **no `secrets` access**. The session pod loads the repository credentials itself, so a browsing user never reads them.

Be honest about the binding's blast radius, though: `pods/exec` and `jobs delete` are namespace-wide once bound. RBAC cannot scope exec to session pods only, so bind this role only where that is acceptable.

The `--local` flag is the exception. It copies the credentials to the user's machine and therefore also needs `get` on `secrets`. Grant that separately and deliberately.

## The web console (`ui.enabled`)

The optional [web console](ui.md) adds a **fourth principal** and **four human roles**. It is off by default; everything below renders only while `ui.enabled` is true.

Role names carry the release name, like everything else the chart renders: at release `kopiur` they are `kopiur-ui-viewer` and so on, and at release `backups` they are `backups-ui-viewer`. The names below use the default.

### The console's own ServiceAccount (`kopiur-ui`)

Deliberately **not** the operator's. The operator reads Secrets on every reconcile; the console must never be able to.

| API group → resources | Verbs | Why |
| --- | --- | --- |
| core → `users`, `groups` | impersonate | The whole design. Every apiserver call the console makes on your behalf is made **as you**, so Kubernetes RBAC — not the console — decides the answer. In anonymous-only mode both rules are pinned by `resourceNames` to that one identity; with `ui.auth.allowedGroups` set, the groups rule is pinned to that list, so the apiserver enforces the allow-list even if the console's own parsing were fooled. |
| `authentication.k8s.io` → `userextras/<key>` | impersonate | One rule **per** configured `ui.auth.impersonateExtraKeys` entry. Never `userextras/*` — RBAC has no wildcard there, so the wildcard form renders happily and authorizes nothing, failing at the first request instead of at install. |
| `kopiur.home-operations.com` → all 9 CRDs | get, list, watch | **Only when `ui.cache.enabled`** (the default): the watch-fed reflector stores behind the console's reads. Their contents are then filtered per caller with SubjectAccessReviews, so an object from a namespace you cannot list is never serialized to you. Setting `ui.cache.enabled: false` removes these rules entirely and makes every read an impersonated `LIST`. |
| `authorization.k8s.io` → `subjectaccessreviews` | create | The per-identity filter above, and the capability flags `/api/v1/me` reports. Also only with the cache on. |

There is **no `secrets` rule and no `pods/exec` rule**, in any mode. The console never reads credentials, and the browse exec is performed as the *browsing user* under `kopiur-ui-browse`, never as the console.

### The four human roles

You bind these; the chart creates no bindings, because who may operate your backups is not a chart decision. Until you write one, every console screen reports `Forbidden` — which is correct, not a broken install.

| Role | Rules | Bind it as |
| --- | --- | --- |
| `kopiur-ui-viewer` | `get`/`list`/`watch` on the 9 CRDs; `get`/`list` on `customresourcedefinitions`; `list` on `events.k8s.io` events. | ClusterRoleBinding |
| `kopiur-ui-editor` | `create`/`delete` `snapshots`; `create` `restores`; `patch` on the 7 patched kinds (policies, schedules, repositories, clusterrepositories, maintenances, and both replication kinds); `list` `persistentvolumeclaims`. | ClusterRoleBinding |
| `kopiur-ui-user` | Both of the above, by **aggregation**. The one to bind for an operator. | ClusterRoleBinding |
| `kopiur-ui-browse` | `get`/`list` `snapshots`+`repositories`, `get` `clusterrepositories`; `create`/`get`/`list`/`delete` `jobs`; `get`/`delete` `configmaps`; `get`/`list`/`watch` `pods`; `get` `pods/log`; **`create`+`get` `pods/exec`**. | **RoleBinding, one namespace** |

Bind the first three cluster-wide: the console is a fleet view, and a viewer scoped to one namespace would report a healthy cluster while another namespace burned.

`kopiur-ui-browse` carries no `apps/deployments` read, unlike the CLI's `<release>-browse` role. The console is told its mover image through `KOPIUR_MOVER_IMAGE` instead, so a browsing user never needs to read the controller Deployment.

The chart also renders a namespaced `kopiur-ui-doctor` Role in the operator's own namespace, so doctor's "controller running" check can list the operator Deployment there. Without it that one check degrades to a warning and nothing else changes.

### Granting `kopiur-ui-browse` grants that namespace's repository credentials

A browse session runs a mover pod that **loads the repository credentials from its own environment**, and RBAC cannot narrow `pods/exec create` to one pod. So anyone bound to `kopiur-ui-browse` in a namespace can exec into that session pod and print those credentials with `env`.

Bind it per namespace, with a `RoleBinding`, to people you would hand those credentials to anyway — never with a `ClusterRoleBinding`, which is every namespace's credentials at once. That is why the role is shipped separately instead of aggregating into `kopiur-ui-user`.

/// note | Why `pods/exec` carries two verbs

Kubernetes authorizes a subresource request by its HTTP method. An exec over
WebSocket is an HTTP **upgrade**, which is a `GET`, so the apiserver asks the
authorizer for `get pods/exec`. The older SPDY exec is a `POST`, which maps to
`create`. Both roles therefore grant **both** verbs; with only `create`, every
browse is refused with `cannot get resource "pods/exec"`, which reads like a
missing binding rather than a missing verb.

///

`ui.rbac.execPolicy.enabled` renders a `ValidatingAdmissionPolicy` narrowing those subjects' `pods/exec` to `kopiur-browse-*` pods running `/usr/local/bin/kopia`. It stops `env` and a shell; it does not un-grant anything above. See [the web console page](ui.md#the-one-mitigation-rbac-cannot-express) for its current caveats.
