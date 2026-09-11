# Web UI (kopia server)

Kopia ships a built-in **web UI**, an HTML view of a repository's snapshots, policies, sources, and tasks.

Kopiur exposes it declaratively. Set `spec.server` on a `Repository`, or on a `ClusterRepository`, and the operator runs `kopia server start` in a `Deployment` and puts a `Service` in front of it.

There is **no `enabled` field**. The presence of the `spec.server` block is what turns the UI on, and removing the block tears everything back down.

Kopiur creates the workload and the `Service` only. Routing that Service to the outside world, with an `Ingress` or an `HTTPRoute`, is your job. See [Exposing the Service](#exposing-the-service).

/// info | Not the same thing as Kopiur's own web console

This page is kopia's UI: it serves **one repository** to kopia clients and shows
that repository's snapshots, policies and sources. It knows nothing about
Kopiur's CRDs.

Kopiur's own console, [`kopiur-ui`](ui.md), is the other way round: it shows your
`Repository`, `SnapshotPolicy`, `Snapshot`, `Restore` and `Maintenance` objects
across the fleet, and acts on them as the signed-in user through Kubernetes RBAC.
The two solve different problems and can run side by side.

///

## When would you use this?

The UI is an **interactive** surface for a human. Reach for it when you want to:

- **Browse and verify** snapshots, policies, and sources visually, without the [kubectl plugin](cli/index.md).
- **Restore one-off files** through the UI: pick a snapshot, mount it, pull a file.
- Give an operator a point-and-click view of a repository's contents. Ideally make it [read-only](#read-only-ui), so browsing can't accidentally delete a backup.

You do **not** need it for normal operation. Scheduled backups, restores, and maintenance all run headless in short-lived mover Jobs, and the UI is never involved.

The server is a **long-lived pod that holds the repository decryption key**, as the warning below explains. So only run it where you actually want interactive access, and tear it down when you're done.

/// warning | The UI holds the decryption key

By default the UI has full **read, write, and delete** access. And the server pod **always** holds the repository **decryption key**, even in [read-only mode](#read-only-ui).

Setting [`readOnly`](#read-only-ui) blocks *changes*, but not *reading*. Anyone who can reach the UI can still read and restore every backup.

So treat exposing the UI exactly like exposing the repository itself. Keep the Service `ClusterIP`, which is the default, put authentication in front of it, and restrict who can reach it with a `NetworkPolicy`.

///

## The `spec.server` surface

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `auth` | externally-tagged [enum](#authentication) (`generate` \| `secretRef` \| `insecure`) | `generate` | UI login. Leave it out and the operator generates credentials. It **never** defaults to no login. |
| `readOnly` | bool | `false` | [Read-only UI](#read-only-ui). Connects the repository read-only, so the UI cannot create, delete, or alter backups. It can browse and restore only. This is forced on when the `Repository` has `spec.mode: ReadOnly`. |
| `service.type` | enum(**`ClusterIP`**\|`NodePort`\|`LoadBalancer`) | `ClusterIP` | How the `Service` is exposed. Routing outside the cluster is your job. |
| `service.port` | int | `51515` | Listen + `Service` port. |
| `service.annotations` | map | — | Applied to the `Service`. This is where you feed your ingress or load-balancer controller. |
| `resources` | [ResourceRequirements](field-reference.md) | — | Requests/limits for the server pod. |
| `securityContext` | [SecurityContext](security-context.md) | hardened default | Override the default hardened container security context. |
| `namespace` | string | — | **Required, and only for `ClusterRepository`.** It says which namespace the server objects land in, because a cluster-scoped owner has no namespace of its own. |

There is no `enabled` field. **The presence of `spec.server` means on**, and its absence means off. See the [full field reference](field-reference.md#repository-spec-server).

## How to deploy it

`spec.server` is just a field on a `Repository`, so it deploys like any other CRD edit: `kubectl apply`, or through GitOps with [Flux or Argo](gitops.md).

The smallest form adds the block to a repository and takes the safe defaults, which are operator-minted credentials and a `ClusterIP` Service:

```yaml
--8<-- "deploy/examples/repository-server-ui-minimal.yaml"
```

Apply it like any other manifest, with `kubectl apply -f` or through GitOps.

### What the operator creates for you

Once the repository is `Ready`, the controller creates the objects below. They are all named `<repo>-kopia-ui`, and all labeled `app.kubernetes.io/name=kopiur-server` and `app.kubernetes.io/instance=<repo>`:

| Object | Name | Purpose |
| --- | --- | --- |
| `Deployment` | `<repo>-kopia-ui` | Runs `kopia server start`. One replica, the `Recreate` strategy, the mover image, and TCP readiness and liveness probes. |
| `Service` | `<repo>-kopia-ui` | Fronts the Deployment on the configured port. |
| `ConfigMap` | `<repo>-kopia-ui` | The server's work spec (which repo, port, auth mode). |
| `Secret` | `<repo>-kopia-ui-auth` | **`generate` mode only.** Holds the minted UI credentials, under the keys `username` and `password`. |

```console
$ kubectl get deploy,svc,cm,secret -n apps \
    -l app.kubernetes.io/name=kopiur-server,app.kubernetes.io/instance=nas-primary
```

The controller manages `Deployments`, `Services`, `ConfigMaps`, and `Secrets` for this feature.

For a namespaced `Repository`, the objects carry an `ownerReference` back to the repository. A `ClusterRepository` cleans them up with a finalizer instead. See [ClusterRepository server](#clusterrepository-server).

### How the server gets the repository credentials

The server pod receives **every** credential `Secret` the repository references as environment variables, through `envFrom`, exactly like a mover Job. That is the encryption-password Secret holding `KOPIA_PASSWORD`, plus the backend's `auth.secretRef` Secret when it is a different one.

Keeping the password and the backend keys in separate Secrets is fully supported. The Deployment carries one `envFrom` entry per distinct Secret.

For a backend using `auth.workloadIdentity`, which S3, Azure, and GCS support, there is no backend Secret. The server pod runs **as the workload-identity ServiceAccount** instead.

That ServiceAccount must exist, with its cloud-federation annotations, in the namespace the server runs in. That is the repository's namespace, or `spec.server.namespace` for a `ClusterRepository`.

Kopiur never creates that ServiceAccount. A missing one surfaces as an actionable error on the repository's `.status`. Azure pods get the `azure.workload.identity/use: "true"` opt-in label automatically.

/// warning | The server needs `features.kopiaUi.enabled` in the chart

Writing the generated-auth `Secret` needs cluster-wide `create`, `patch`, and `delete` on `secrets`. So do the cross-namespace credential copies a `ClusterRepository` makes. That permission is **off by default**, for least privilege.

Set `features.kopiaUi.enabled: true` in the Helm chart. Without it, the repository's `.status` surfaces an actionable `403` naming the flag. See [Feature permissions](feature-permissions.md).

///

/// info | The server runs without in-pod TLS

The operator starts kopia with `--insecure`, meaning plain HTTP inside the pod.

That is deliberate. TLS termination belongs at your ingress or load balancer, not in the server pod. The credentials still protect the UI. Just don't expose the raw `Service` to an untrusted network without TLS in front of it.

///

## Try it end-to-end

Turn on the UI from a clean slate and prove it answers, without leaving the cluster. You should get `200` with credentials and `401` without them.

It takes one apply-ready bundle, [`deploy/examples/tryit/server-ui.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/server-ui.yaml). It contains the `apps` `Namespace`, the backend Secret, and a `Repository` named `nas-primary` with the minimal `spec.server` block.

```yaml
--8<-- "deploy/examples/tryit/server-ui.yaml:repository"
```

**1. Fill in the credentials**, meaning the `AWS_*` values and `KOPIA_PASSWORD`, in the `secret` section. Then apply the bundle and wait for the repository to be `Ready`. The server objects are created only once it is:

```console
$ kubectl apply -f deploy/examples/tryit/server-ui.yaml
$ kubectl -n apps wait --for=condition=Ready repository/nas-primary --timeout=2m
```

**2. Confirm the operator created the server objects.** They are all named `nas-primary-kopia-ui*` and labeled for the instance:

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

A `1/1` Deployment, a Service on `51515`, and the `nas-primary-kopia-ui-auth` Secret, with its `username` and `password` keys, together mean the UI is up.

**3. Read the minted credentials** from that Secret:

```console
$ kubectl -n apps get secret nas-primary-kopia-ui-auth \
    -o jsonpath='{.data.username}' | base64 -d; echo
kopia
$ kubectl -n apps get secret nas-primary-kopia-ui-auth \
    -o jsonpath='{.data.password}' | base64 -d; echo
<illustrative — your minted password>
```

**4. Prove the UI answers.** Port-forward the Service and curl it. You get `200` with the credentials and `401` without:

```console
$ kubectl -n apps port-forward svc/nas-primary-kopia-ui 51515:51515 &

# with the credentials → 200:
$ curl -su 'kopia:<password>' http://localhost:51515/ -o /dev/null -w '%{http_code}\n'
200

# without them → 401 (the UI never defaults to no-auth):
$ curl -s http://localhost:51515/ -o /dev/null -w '%{http_code}\n'
401
```

The server speaks plain HTTP **inside the pod**, because the operator starts kopia with `--insecure`. TLS belongs at your ingress or load balancer, never on the raw `Service`.

/// note | Illustrative output

The `CLUSTER-IP`, the minted password, and the `AGE` values vary from run to run. What actually matters is the `1/1` `nas-primary-kopia-ui` Deployment, port `51515`, the `nas-primary-kopia-ui-auth` Secret, and the `200` and `401` responses.

///

**Tear it down** by removing the `spec.server` block and re-applying. The operator then deletes the Deployment, the Service, the ConfigMap, and the generated Secret it owns:

```console
$ kubectl -n apps patch repository nas-primary --type merge -p '{"spec":{"server":null}}'
```

## Authentication

`spec.server.auth` is an externally-tagged enum, so you set exactly one of three keys. It defaults to `generate`, and never to "no login".

| Mode | Shape | When to use |
| --- | --- | --- |
| **`generate`** _(default)_ | `generate: { username? }` | Let the operator mint a random password. The simplest safe choice. |
| **`secretRef`** | `secretRef: { name, usernameKey, passwordKey }` | You manage the UI credentials yourself (e.g. a shared/SSO-fronted password). |
| **`insecure`** | `insecure: { acknowledgeInsecure: true }` | **No login at all.** Dangerous. For throwaway or lab use only. |

### `generate` — operator-minted credentials (recommended)

The operator creates a `Secret` named `<repo>-kopia-ui-auth` once, with the keys `username` and `password`. It pins the reference to it in `status.server.generatedSecretRef`, and **never rotates it** on later reconciles.

The username defaults to `kopia`. Set `generate: { username: alice }` to change it. Read the password with:

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

This disables the UI login entirely. It demands an explicit acknowledgement, so you can't end up here by accident:

```yaml
server:
    auth:
        insecure: { acknowledgeInsecure: true } # required — the webhook rejects it otherwise
```

/// danger | `insecure` exposes the whole repository with no login

With `insecure`, anyone who can reach the `Service` has full read, write, and **delete** access to every backup.

The admission webhook rejects the mode unless you set `acknowledgeInsecure: true`. Only use it on an isolated network you fully trust, and pair it with a `NetworkPolicy`.

///

## Read-only UI { #read-only-ui }

Set `spec.server.readOnly: true` and the operator connects the server's repository **read-only**, using `kopia repository connect --readonly`, before starting the UI.

Every operation on that connection is then **unable to change the repository**, and so is everything the UI does. Creating, deleting, or altering snapshots, policies, and maintenance is rejected.

This is the right setting for a point-and-click *browse* and *restore* surface, where you never want a stray click to delete a backup.

```yaml
server:
    readOnly: true # the UI cannot mutate the repository (browse + restore only)
    auth: { generate: {} }
```

The UI is read-only when `spec.mode: ReadOnly` is set **or** `spec.server.readOnly: true` is set:

- A `Repository` with [`spec.mode: ReadOnly`](repositories.md) already serves restores only. Its UI is forced read-only, so you don't need the field. Setting an explicit `readOnly: false` on such a repository is **rejected by the webhook**, because a read-only repository can't serve a writable UI.
- A normal `ReadWrite` repository, still taking backups through movers, gets a read-only *UI* by opting in with `readOnly: true`.

The reconciler pins the resolved value to `status.server.readOnly`. Here is a complete, apply-ready example: a ReadWrite repository with a read-only UI.

```yaml
--8<-- "deploy/examples/26-repository-server-ui-readonly.yaml"
```

/// warning | Read-only blocks mutation, not reading

`readOnly` stops the UI from **changing** backups. It does **not** make the UI confidential.

The server pod still holds the repository **decryption key**, so anyone who can reach the UI can still **read and restore** every backup. Keep the login on and the `Service` `ClusterIP` regardless.

Note too that kopia's UI does not grey out the write and delete buttons, even though they no longer work. The actions simply fail at the backend.

///

/// info | Why a connection-level flag (not a server flag)

kopia 0.23, which is the version Kopiur ships, has no `kopia server start --readonly` flag. That landed later upstream.

Kopiur gets the same guarantee from the read-only *connection*, whose read-only setting every later operation inherits.

One side effect: kopia may log occasional errors if its internal scheduler probes for maintenance on a read-only connection. They are harmless, because nothing can be written.

///

## Exposing the Service

`spec.server.service` controls the `Service`; `port` defaults to `51515`.

| `service.type` | Reach it from | Notes |
| --- | --- | --- |
| **`ClusterIP`** _(default)_ | inside the cluster | Use `kubectl port-forward` or your own ingress. This is the safe default. |
| `NodePort` | each node's IP | A static high port on every node. |
| `LoadBalancer` | an external IP | Provisioned by your cloud/LB controller. |

**Kopiur creates the `Service` only. It never creates an `Ingress` or an `HTTPRoute`.**

Point your own router at the `Service` named `<repo>-kopia-ui`, on the configured port, and put TLS there, ideally with an extra authentication layer.

The [full example](#full-example) carries commented `HTTPRoute` and `NetworkPolicy` templates you can adapt. Use `service.annotations` to feed your ingress or load-balancer controller, for instance an `external-dns` hostname or a load-balancer class.

## Accessing the UI

For a quick look, port-forward the `Service` and open it locally:

```console
$ kubectl port-forward -n apps svc/nas-primary-kopia-ui 51515:51515
# then browse http://localhost:51515 and log in with the credentials above
```

For ongoing access, route an `Ingress` or `HTTPRoute` to the `Service`, with TLS. Strongly consider a `NetworkPolicy` restricting who may reach it.

## ClusterRepository server { #clusterrepository-server }

A `ClusterRepository` is cluster-scoped and has no namespace of its own, so its `spec.server` block **requires** a `namespace`. The other fields are identical:

```yaml
--8<-- "deploy/examples/clusterrepository-server-ui.yaml"
```

A cluster-scoped object can't own namespaced children through an `ownerReference`. So the controller tracks and cleans up the server objects with a **finalizer plus labels** instead.

`envFrom` can't cross namespaces, so any credential Secret living in a different namespace than the server is copied next to the server pod. That means the password Secret **and** the backend `auth` Secret, when they are separate.

Each reference's source namespace is its own explicit `namespace`, falling back to the operator's namespace. That is the same rule the repository uses for bootstrap, so "absent" always means one thing.

Changing `server.namespace` moves the server. The operator deletes the objects in the old namespace, copies included, and recreates them in the new one. It tracks the last-applied namespace in `status.server.namespace`.

## Filesystem backends require ReadWriteMany

For an **object-store** backend, such as S3, Azure, GCS, or B2, the server connects over the network, so there is no volume constraint.

For a **filesystem** backend the server pod must mount the repository volume, and that pod is long-lived:

/// warning | A filesystem-backed server needs a ReadWriteMany repo PVC

A long-lived server holding a `ReadWriteOnce` repository PVC would block every backup, restore, and maintenance mover that needs the same volume.

So the operator **requires the repository PVC to be `ReadWriteMany`** when `spec.server` is set on a filesystem `Repository`, and it rejects the reconcile otherwise.

Use a StorageClass that supports `ReadWriteMany`, or an inline NFS export, for the repository volume. Or keep the UI on an object-store repository.

///

### Server permissions on an NFS-backed repo

Like a mover, the server pod mounts the filesystem repository **read-write** and writes to the backend. So it must be able to write the export.

The server gets the same hardened pod defaults as movers, including `fsGroup: 65532`. But **`fsGroup` has no effect on NFS**.

If the export is owned by a dedicated UID and GID, give the server the shared group through `spec.server.podSecurityContext`, which mirrors `moverDefaults.podSecurityContext`:

```yaml
spec:
  server:
    podSecurityContext:
      supplementalGroups: [3001] # the export's group; matches moverDefaults
  moverDefaults:
    podSecurityContext:
      supplementalGroups: [3001]
```

Without it, the server crash-loops on startup, because it can't read or write the repository. See [Security context → NFS filesystem repositories](security-context.md#nfs-filesystem-repositories).

The container-level `spec.server.securityContext` overrides the hardened **container** context, for instance `runAsUser`, independently of this.

## Inspecting status

The reconciler pins a `status.server` block (it never stores a password):

```console
$ kubectl get repository nas-primary -n apps -o jsonpath='{.status.server}' | jq
```

| Field | Meaning |
| --- | --- |
| `endpoint` | In-cluster address, `<service>.<namespace>.svc:<port>`. |
| `namespace` | The namespace the server objects were last applied to. The operator uses it to detect a `namespace` change. |
| `authMode` | The resolved auth mode: `Generate`, `SecretRef`, or `Insecure`. |
| `readOnly` | The effective read-only state. It is `true` when either `spec.mode: ReadOnly` or `spec.server.readOnly: true` is set. |
| `generatedSecretRef` | **`generate` mode only.** The operator-owned Secret holding the UI credentials. |

When the server is disabled, `status.server` is cleared to null.

## Disabling it

Delete the `spec.server` block from the `Repository` manifest and re-apply it. The operator then deletes the Deployment, the Service, the ConfigMap, and any generated Secret it owns:

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

A complete, apply-ready `Repository` with `spec.server`. It uses an S3 backend and `generate` auth, and it carries commented `HTTPRoute` and `NetworkPolicy` templates:

```yaml
--8<-- "deploy/examples/25-repository-server-ui.yaml"
```

## See also

- [Repositories & backends](repositories.md): the `Repository` and `ClusterRepository` surface this feature sits on.
- [Security context](security-context.md): the hardened default the server pod runs under, and how to override it.
- [Installation](install.md): install scope, and the RBAC the controller needs to manage the server objects.
- [GitOps (Flux / Argo)](gitops.md): deploying the field through a GitOps pipeline.
- [`deploy/examples/25-repository-server-ui.yaml`](#full-example): the apply-ready example above.
- [`deploy/examples/26-repository-server-ui-readonly.yaml`](#read-only-ui): the read-only-UI variant.
</content>
