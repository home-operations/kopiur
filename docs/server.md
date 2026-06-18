# Web UI (kopia server)

Kopia ships a built-in **web UI** — an HTML view of a repository's snapshots,
policies, sources, and tasks. Kopiur exposes it declaratively: set `spec.server`
on a `Repository` (or `ClusterRepository`) and the operator runs `kopia server
start` in a `Deployment` and puts a `Service` in front of it. There is **no
`enabled` bool** — the presence of the `spec.server` block is what turns it on,
and removing the block tears everything back down.

Kopiur creates the workload and the `Service` only. Routing the Service to the
outside world (an `Ingress`/`HTTPRoute`) is yours to wire — see
[Exposing the Service](#exposing-the-service).

## When would you use this?

The UI is an **interactive** surface for a human. Reach for it when you want to:

- **Browse and verify** snapshots, policies, and sources visually, without the
  [kubectl plugin](cli/index.md).
- **Restore ad hoc** through the UI — pick a snapshot, mount it, pull a file.
- Give an operator a point-and-click view of a repository's contents — ideally
  [read-only](#read-only-ui), so browsing can't accidentally delete a backup.

You do **not** need it for normal operation. Scheduled backups, restores, and
maintenance all run headless in short-lived mover Jobs — the UI is never on that
path. Because it is a **long-lived pod that holds the repository decryption key**
(see the warning below), only run it where you actually want interactive access,
and tear it down when you're done.

/// warning | The UI holds the decryption key

By default the UI is full **read/write/delete**, and the server pod **always**
holds the repository **decryption key** — even in [read-only mode](#read-only-ui).
Setting [`readOnly`](#read-only-ui) blocks *mutation* but not *reading*: anyone who
can reach the UI can still read and restore every backup. So treat exposing the UI
exactly like exposing the repository itself: keep it `ClusterIP` (the default),
put authentication in front of it, and restrict who can reach the `Service` with a
`NetworkPolicy`.

///

## The `spec.server` surface

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `auth` | externally-tagged [enum](#authentication) (`generate` \| `secretRef` \| `insecure`) | `generate` | UI login. Omitted ⇒ operator-generated credentials. **Never** defaults to no-auth. |
| `readOnly` | bool | `false` | [Read-only UI](#read-only-ui) — connect the repository read-only so the UI cannot create/delete/alter backups (browse + restore only). Forced on when the `Repository` has `spec.mode: ReadOnly`. |
| `service.type` | enum(**`ClusterIP`**\|`NodePort`\|`LoadBalancer`) | `ClusterIP` | How the `Service` is exposed. Routing outside the cluster is your job. |
| `service.port` | int | `51515` | Listen + `Service` port. |
| `service.annotations` | map | — | Applied to the `Service` — the seam for your ingress/LB controller. |
| `resources` | [ResourceRequirements](field-reference.md) | — | Requests/limits for the server pod. |
| `securityContext` | [SecurityContext](security-context.md) | hardened default | Override the default hardened container security context. |
| `namespace` | string | — | **`ClusterRepository` only, required** — which namespace the server objects land in (a cluster-scoped owner has no implicit namespace). |

There is no `enabled` field: **presence of `spec.server` is "on"**, absence is
"off". See the [full field reference](field-reference.md#serverspec).

## How to deploy it

`spec.server` is just a field on a `Repository`, so it deploys like any other CRD
edit — `kubectl apply`, or through GitOps ([Flux/Argo](gitops.md)). The smallest
form adds the block to a repository and takes the safe defaults (operator-minted
credentials, `ClusterIP`):

```yaml
--8<-- "deploy/examples/repository-server-ui-minimal.yaml"
```

Apply it like any other manifest — `kubectl apply -f`, or through GitOps.

### What the operator creates for you

Once the repository is `Ready`, the controller materializes — all named
`<repo>-kopia-ui` and labeled `app.kubernetes.io/name=kopiur-server`,
`app.kubernetes.io/instance=<repo>`:

| Object | Name | Purpose |
| --- | --- | --- |
| `Deployment` | `<repo>-kopia-ui` | Runs `kopia server start` (1 replica, `Recreate` strategy, mover image, TCP readiness/liveness probes). |
| `Service` | `<repo>-kopia-ui` | Fronts the Deployment on the configured port. |
| `ConfigMap` | `<repo>-kopia-ui` | The server's work spec (which repo, port, auth mode). |
| `Secret` | `<repo>-kopia-ui-auth` | **`generate` mode only** — the minted UI credentials (`username`/`password`). |

```console
$ kubectl get deploy,svc,cm,secret -n apps \
    -l app.kubernetes.io/name=kopiur-server,app.kubernetes.io/instance=nas-primary
```

The controller manages `Deployments`/`Services`/`ConfigMaps`/`Secrets` for this
feature; the RBAC for it ships with the chart (see
[Installation](install.md#install-scope)). For a namespaced `Repository` the
objects carry an `ownerReference` to the repository; a `ClusterRepository` cleans
them up via a finalizer instead (see [ClusterRepository](#clusterrepository-server)).

/// info | The server runs without in-pod TLS

The operator starts kopia with `--insecure` — i.e. plain HTTP inside the pod.
That is deliberate: TLS termination belongs at your ingress/load balancer, not in
the server pod. The credentials still protect the UI; just don't expose the raw
`Service` to an untrusted network without TLS in front.

///

## Try it end-to-end

Turn on the UI from a clean slate and prove it answers — `200` with auth, `401` without — without leaving the cluster. One apply-ready bundle, [`deploy/examples/tryit/server-ui.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/server-ui.yaml): the `apps` `Namespace`, the backend Secret, and a `Repository` `nas-primary` with the minimal `spec.server` block.

```yaml
--8<-- "deploy/examples/tryit/server-ui.yaml:repository"
```

**1. Fill in the credentials** (`AWS_*` + `KOPIA_PASSWORD`) in the `secret` section, then apply the bundle and wait for the repository to be `Ready` — the server objects materialize only once it is:

```console
$ kubectl apply -f deploy/examples/tryit/server-ui.yaml
$ kubectl -n apps wait --for=condition=Ready repository/nas-primary --timeout=2m
```

**2. Confirm the operator created the server objects (deep).** All named `nas-primary-kopia-ui*` and labeled for the instance:

```console
$ kubectl -n apps get deploy,svc,secret \
    -l app.kubernetes.io/name=kopiur-server,app.kubernetes.io/instance=nas-primary
NAME                                  READY   UP-TO-DATE   AVAILABLE   AGE
deployment.apps/nas-primary-kopia-ui  1/1     1            1           40s

NAME                          TYPE        CLUSTER-IP     PORT(S)     AGE
service/nas-primary-kopia-ui  ClusterIP   10.96.12.34    51515/TCP   40s

NAME                              TYPE     DATA   AGE
secret/nas-primary-kopia-ui-auth  Opaque   2      40s
```

A `1/1` Deployment, a Service on `51515`, and the `nas-primary-kopia-ui-auth` Secret (keys `username`/`password`) means the UI is up.

**3. Read the minted credentials** from that Secret:

```console
$ kubectl -n apps get secret nas-primary-kopia-ui-auth \
    -o jsonpath='{.data.username}' | base64 -d; echo
kopia
$ kubectl -n apps get secret nas-primary-kopia-ui-auth \
    -o jsonpath='{.data.password}' | base64 -d; echo
<illustrative — your minted password>
```

**4. Prove the UI answers (deep).** Port-forward the Service and curl it — `200` with the credentials, `401` without:

```console
$ kubectl -n apps port-forward svc/nas-primary-kopia-ui 51515:51515 &

# with the credentials → 200:
$ curl -su 'kopia:<password>' http://localhost:51515/ -o /dev/null -w '%{http_code}\n'
200

# without them → 401 (the UI never defaults to no-auth):
$ curl -s http://localhost:51515/ -o /dev/null -w '%{http_code}\n'
401
```

The server speaks plain HTTP **inside the pod** (the operator starts kopia with `--insecure`); TLS belongs at your ingress/LB, never the raw `Service`.

/// note | Illustrative output

The `CLUSTER-IP`, the minted password, and the `AGE`s vary per run — the load-bearing facts are the `1/1` `nas-primary-kopia-ui` Deployment, port `51515`, the `nas-primary-kopia-ui-auth` Secret, and `200`/`401`.

///

**Tear it down** by removing the `spec.server` block and re-applying — the operator deletes the Deployment, Service, ConfigMap, and the generated Secret it owns:

```console
$ kubectl -n apps patch repository nas-primary --type merge -p '{"spec":{"server":null}}'
```

## Authentication

`spec.server.auth` is an externally-tagged enum — you set exactly one of three
keys. It defaults to `generate` (never to no-auth).

| Mode | Shape | When to use |
| --- | --- | --- |
| **`generate`** _(default)_ | `generate: { username? }` | Let the operator mint a random password. The simplest safe choice. |
| **`secretRef`** | `secretRef: { name, usernameKey, passwordKey }` | You manage the UI credentials yourself (e.g. a shared/SSO-fronted password). |
| **`insecure`** | `insecure: { acknowledgeInsecure: true }` | **No login at all.** A footgun; for throwaway/lab use only. |

### `generate` — operator-minted credentials (recommended)

The operator creates a `Secret` `<repo>-kopia-ui-auth` once (keys `username`,
`password`), pins its reference to `status.server.generatedSecretRef`, and
**never rotates it** on later reconciles. The username defaults to `kopia`; set
`generate: { username: alice }` to change it. Read the password with:

```console
$ kubectl get secret nas-primary-kopia-ui-auth -n apps \
    -o jsonpath='{.data.password}' | base64 -d; echo
```

### `secretRef` — bring your own credentials

Point at a `Secret` you own; all three keys are required:

```yaml
server:
    auth:
        secretRef: { name: my-ui-creds, usernameKey: username, passwordKey: password }
```

### `insecure` — no authentication

Disables the UI login entirely. It demands an explicit acknowledgement, so you
can't reach it by accident:

```yaml
server:
    auth:
        insecure: { acknowledgeInsecure: true } # required — the webhook rejects it otherwise
```

/// danger | `insecure` exposes the whole repository with no login

With `insecure`, anyone who can reach the `Service` has full read/write/**delete**
of every backup. The admission webhook rejects the mode unless you set
`acknowledgeInsecure: true`. Only use it on an isolated network you fully trust,
and pair it with a `NetworkPolicy`.

///

## Read-only UI { #read-only-ui }

Set `spec.server.readOnly: true` and the operator connects the server's repository
**read-only** (`kopia repository connect --readonly`) before starting the UI. Every
operation on that connection — and so everything the UI does — is then **unable to
mutate the repository**: creating, deleting, or altering snapshots/policies/
maintenance is rejected. It's the right default for a point-and-click *browse* and
*restore* surface where you never want a stray click to delete a backup.

```yaml
server:
    readOnly: true # the UI cannot mutate the repository (browse + restore only)
    auth: { generate: {} }
```

The **effective** read-only state is `spec.mode: ReadOnly` **OR**
`spec.server.readOnly: true`:

- A `Repository` with [`spec.mode: ReadOnly`](repositories.md) already serves
  restores only — its UI is forced read-only and you don't need the field. Setting
  an explicit `readOnly: false` on such a repository is **rejected by the webhook**
  (a read-only repository can't serve a writable UI).
- A normal `ReadWrite` repository (still taking backups via movers) gets a
  read-only *UI* by opting in with `readOnly: true`.

The reconciler pins the resolved value to `status.server.readOnly`. A complete,
apply-ready read-only `Repository` (a ReadWrite repo with a read-only UI):

```yaml
--8<-- "deploy/examples/26-repository-server-ui-readonly.yaml"
```

/// warning | Read-only blocks mutation, not reading

`readOnly` stops the UI from **changing** backups; it does **not** make the UI
confidential. The server pod still holds the repository **decryption key**, so
anyone who can reach the UI can still **read and restore** every backup. Keep auth
on and the `Service` `ClusterIP` regardless. Note too that kopia's UI does not grey
out the (now non-functional) write/delete buttons — the actions simply fail at the
backend.

///

/// info | Why a connection-level flag (not a server flag)

kopia 0.23 — the version Kopiur ships — has no `kopia server start --readonly`
flag; that landed later upstream. Kopiur achieves the same guarantee with the
read-only *connection*, whose read-only bit every later operation inherits. One
side effect: kopia may log occasional errors if its internal scheduler probes for
maintenance on a read-only connection. They're harmless (nothing can be written).

///

## Exposing the Service

`spec.server.service` controls the `Service`; `port` defaults to `51515`.

| `service.type` | Reach it from | Notes |
| --- | --- | --- |
| **`ClusterIP`** _(default)_ | inside the cluster | Use `kubectl port-forward` or your own ingress. The safe default. |
| `NodePort` | each node's IP | A static high port on every node. |
| `LoadBalancer` | an external IP | Provisioned by your cloud/LB controller. |

**Kopiur creates the `Service` only — it never creates an `Ingress` or
`HTTPRoute`.** Point your own router at `Service` `<repo>-kopia-ui` on the
configured port, and put TLS + (ideally) an additional auth layer there. The
[full example](#full-example) carries commented `HTTPRoute` and `NetworkPolicy`
templates you can adapt. Use `service.annotations` to feed your ingress/LB
controller (e.g. an `external-dns` hostname or an LB class).

## Accessing the UI

For a quick look, port-forward the `Service` and open it locally:

```console
$ kubectl port-forward -n apps svc/nas-primary-kopia-ui 51515:51515
# then browse http://localhost:51515 and log in with the credentials above
```

For ongoing access, route an `Ingress`/`HTTPRoute` to the `Service` (with TLS),
and strongly consider a `NetworkPolicy` restricting who may reach it.

## ClusterRepository server { #clusterrepository-server }

A `ClusterRepository` is cluster-scoped and has no implicit namespace, so its
`spec.server` block **requires** a `namespace` (the fields are otherwise
identical, flattened in):

```yaml
--8<-- "deploy/examples/clusterrepository-server-ui.yaml"
```

Because a cluster-scoped object can't own namespaced children via an
`ownerReference`, the controller tracks and cleans up the server objects with a
**finalizer + labels** instead. If the repository credentials Secret lives in a
different namespace than the server, the operator mirrors it next to the server
pod (`envFrom` can't cross namespaces). Changing `server.namespace` moves the
server: the operator deletes the objects in the old namespace and recreates them
in the new one (it tracks the last-applied namespace in `status.server.namespace`).

## Filesystem backends require ReadWriteMany

For an **object-store** backend (S3, Azure, GCS, B2, …) the server connects over
the network — no volume constraint. For a **filesystem** backend the server pod
must mount the repository volume, and it is long-lived:

/// warning | A filesystem-backed server needs a ReadWriteMany repo PVC

A long-lived server holding a `ReadWriteOnce` repo PVC would block every
backup/restore/maintenance mover that needs the same volume. The operator
therefore **requires the repository PVC to be `ReadWriteMany`** when `spec.server`
is set on a filesystem `Repository`, and rejects the reconcile otherwise. Use an
RWX-capable StorageClass (or an inline NFS export) for the repository volume, or
keep the UI on an object-store repository.

///

### Server permissions on an NFS-backed repo

Like a mover, the server pod mounts the filesystem repo **read-write** and writes
to the backend — so it must be able to write the export. The server gets the same
hardened pod defaults as movers (`fsGroup: 65532`), but **`fsGroup` is a no-op on
NFS**. If the export is owned by a dedicated UID/GID, give the server the shared
group via `spec.server.podSecurityContext` (mirrors `moverDefaults.podSecurityContext`):

```yaml
spec:
  server:
    podSecurityContext:
      supplementalGroups: [3001] # the export's group; matches moverDefaults
  moverDefaults:
    podSecurityContext:
      supplementalGroups: [3001]
```

Without it, the server CrashLoops on startup (it can't read/write the repo). See
[Security context → NFS filesystem repositories](security-context.md#nfs-filesystem-repositories).
The container-level `spec.server.securityContext` overrides the hardened
**container** context (e.g. `runAsUser`) independently.

## Inspecting status

The reconciler pins a `status.server` block (it never stores a password):

```console
$ kubectl get repository nas-primary -n apps -o jsonpath='{.status.server}' | jq
```

| Field | Meaning |
| --- | --- |
| `endpoint` | In-cluster address, `<service>.<namespace>.svc:<port>`. |
| `namespace` | Namespace the server objects were last applied to (used to detect a `namespace` change). |
| `authMode` | Resolved auth discriminant — `Generate` / `SecretRef` / `Insecure`. |
| `readOnly` | Effective read-only state — `true` when `spec.mode: ReadOnly` or `spec.server.readOnly: true`. |
| `generatedSecretRef` | **`generate` mode only** — the operator-owned Secret holding the UI credentials. |

When the server is disabled, `status.server` is cleared to null.

## Disabling it

Delete the `spec.server` block from the `Repository` manifest and re-apply it.
The operator deletes the Deployment, Service, ConfigMap, and any generated Secret
it owns:

```console
$ kubectl apply -f your-repository.yaml   # the manifest with the spec.server block removed
```

/// note | Quick imperative form

To tear it down without editing the manifest, patch `spec.server` to null directly:

```console
$ kubectl patch repository nas-primary -n apps --type merge -p '{"spec":{"server":null}}'
```

Re-apply your manifest afterward so your source of truth (especially under GitOps) doesn't put it back.

///

## Full example

A complete, apply-ready `Repository` with `spec.server` (S3 backend, `generate`
auth, plus commented `HTTPRoute` + `NetworkPolicy` templates):

```yaml
--8<-- "deploy/examples/25-repository-server-ui.yaml"
```

## See also

- [Repositories & backends](repositories.md) — the `Repository`/`ClusterRepository` surface this feature sits on.
- [Security context](security-context.md) — the hardened default the server pod runs under, and how to override it.
- [Installation](install.md) — install scope and the RBAC the controller needs to manage the server objects.
- [GitOps (Flux / Argo)](gitops.md) — deploying the field through a GitOps pipeline.
- [`deploy/examples/25-repository-server-ui.yaml`](#full-example) — the apply-ready example above.
- [`deploy/examples/26-repository-server-ui-readonly.yaml`](#read-only-ui) — the read-only-UI variant.
</content>
