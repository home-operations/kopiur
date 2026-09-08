# Getting started

This is the **end-to-end walkthrough**: from a cluster with nothing installed to a verified backup *and* a verified restore. It holds your hand at every step and shows you exactly what to look for, so you know each one worked. Budget about 15 minutes.

If you only want the install reference, meaning every Helm value, the scopes, and the certificate options, see [Installation](install.md). This page is the guided first run.

Want the long version, with the reasoning behind every value, a NAS track, and the `kubectl kopiur` plugin woven in? That is the [Complete walkthrough](walkthrough.md).

/// tip | The mental model, read this first

Kopiur splits one job into three resources so each can change independently:

- a **`Repository`** is *where* snapshots are stored, so your S3 bucket, NAS or B2;
- a **`SnapshotPolicy`** is the **recipe**, meaning *what* to back up. It is idempotent and **runs nothing on its own**;
- a **`Snapshot`** is an **invocation**, meaning one snapshot as a Kubernetes object. It is the universal trigger, created by a schedule, by `kubectl`, or by automation;
- a **`SnapshotSchedule`** is the **cron**, meaning *when* the recipe runs. It creates `Snapshot` objects for you.

A `Restore` reads a snapshot back into a PVC. That is the whole model. Everything below is just those pieces in order.

For the full picture, so how Kopia dedups, the `username@hostname:path` identity model, and why these are separate resources, see [Concepts](concepts/how-kopia-works.md).

///

## What you need

- A Kubernetes cluster, **1.24 or later**, and `kubectl` pointed at it.
- **Helm 3 or 4.**
- A storage backend kopia can reach. This guide uses **S3 or an S3-compatible store**, such as AWS S3, MinIO, RustFS or Ceph RGW. Any of the [eight backends](repositories.md) works the same way; only the `Repository` changes.
- A **PersistentVolumeClaim** with some data in it to back up. The walkthrough assumes one named `app-data` in a namespace called `demo`.

/// note | One bundle, applied once

Every manifest on this page is a section of a single apply-ready file, [`deploy/examples/getting-started.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/getting-started.yaml). Each step below shows its section so you understand what it does, but you do not apply them one at a time.

Fill in the `REPLACE_ME` values, meaning your backend keys and a generated `KOPIA_PASSWORD`, then apply the whole thing once:

```console
$ kubectl apply -f deploy/examples/getting-started.yaml
```

Re-applying is idempotent, and the operator resolves the ordering for you. The `Snapshot` simply stays `Pending` until the `Repository` is `Ready`, then proceeds. So apply once now, then walk each step below to watch the pieces reconcile in turn.

The one exception is the manual `Snapshot` in Step 5. It uses `generateName`, so it needs `kubectl create` rather than `apply`, and that step calls it out.

///

/// note | No spare PVC?

These two sections, at the top of the same bundle, create the `demo` namespace and a throwaway PVC to follow along:

```yaml
--8<-- "deploy/examples/getting-started.yaml:namespace"
--8<-- "deploy/examples/getting-started.yaml:pvc"
```

You do not need a separate apply for them. They are applied along with everything else when you run `kubectl apply -f deploy/examples/getting-started.yaml` in the credentials step below. Mount the PVC in a throwaway pod and write a file if you want to see real data move.

///

## Step 1 — Install the operator

Install the chart into its own namespace. By default the operator manages the webhook's serving certificate itself, so **no cert-manager is required**. If you prefer cert-manager or a hand-supplied certificate, see [Installation → Webhook TLS](install.md#webhook-tls).

```console
$ helm install kopiur oci://ghcr.io/home-operations/charts/kopiur \
    --namespace kopiur-system --create-namespace
```

**Verify** the operator is up and the 9 CRDs are registered:

```console
$ kubectl -n kopiur-system rollout status deploy/kopiur-controller
$ kubectl -n kopiur-system rollout status deploy/kopiur-webhook
$ kubectl get crd | grep kopiur.home-operations.com
snapshotpolicies.kopiur.home-operations.com           ...
snapshots.kopiur.home-operations.com                  ...
snapshotschedules.kopiur.home-operations.com          ...
clusterrepositories.kopiur.home-operations.com        ...
maintenances.kopiur.home-operations.com               ...
repositories.kopiur.home-operations.com               ...
repositoryreplications.kopiur.home-operations.com     ...
snapshotreplications.kopiur.home-operations.com       ...
restores.kopiur.home-operations.com                   ...
```

Nine CRDs and two ready Deployments means the operator is live.

## Step 2 — Give it credentials

The mover Job that runs kopia reads two things from a Secret: your **backend access keys** and the **repository encryption password**.

That Secret must live in the **same namespace as the data you back up**, so `demo` here, because the mover loads it with `envFrom`, which only reads the local namespace. See [Movers, RBAC & credentials](movers.md) for the full reasoning.

Fill in your backend keys and a generated `KOPIA_PASSWORD` in this section of the bundle:

```yaml
--8<-- "deploy/examples/getting-started.yaml:secret"
```

This is the moment to apply the bundle. Once the `REPLACE_ME` values are filled in, `kubectl apply -f deploy/examples/getting-started.yaml` creates the namespace, PVC, Secret and every custom resource in one shot. The rest of the steps just watch each piece come up.

/// note | Prefer to create it imperatively?

```console
$ kubectl -n demo create secret generic repo-creds \
    --from-literal=AWS_ACCESS_KEY_ID='REPLACE_ME' \
    --from-literal=AWS_SECRET_ACCESS_KEY='REPLACE_ME' \
    --from-literal=KOPIA_PASSWORD="$(openssl rand -base64 24)"
```

///

/// warning | Save the KOPIA_PASSWORD

The `KOPIA_PASSWORD` encrypts the repository. **If you lose it, the backups are unrecoverable**, because kopia cannot decrypt without it. Store it in your password manager or secret store, not just in the cluster.

The backend keys, the `AWS_*` values, are your object-store credentials. The well-known key names per backend are in the [Repositories reference](repositories.md#credential-secret-keys-by-backend).

///

## Step 3 — Create the Repository

Tell Kopiur where to store snapshots. `create.enabled: true` lets the operator *initialize* a brand-new kopia repository in the bucket. Drop it, or set `false`, to require that one already exists.

```yaml
--8<-- "deploy/examples/getting-started.yaml:repository"
```

Once the bundle is applied, **wait for `Ready`**. This is the gate everything else waits on:

```console
$ kubectl -n demo get repository primary -w
NAME      PHASE          BACKEND   AGE
primary   Initializing   S3        5s
primary   Ready          S3        12s
```

If it sticks in `Pending` or `Failed`, read the reason. Kopiur tells you exactly what is wrong:

```console
$ kubectl -n demo describe repository primary    # see Conditions + Events
```

The common causes are wrong keys, an unreachable endpoint, or a bucket that does not exist while `create.enabled: false`. See [Troubleshooting](troubleshooting.md).

## Step 4 — Write the recipe (SnapshotPolicy)

Now describe *what* to back up and *how long to keep it*. Retention is **GFS**, meaning grandfather-father-son, and it is the only thing that prunes successful backups.

```yaml
--8<-- "deploy/examples/getting-started.yaml:policy"
```

It was applied with the bundle. Confirm it is registered:

```console
$ kubectl -n demo get snapshotpolicy
NAME       REPOSITORY   AGE
app-data   primary      3s
```

A `SnapshotPolicy` runs nothing yet, because it is the recipe. Next we invoke it.

## Step 5 — Take your first backup (and watch it work)

Trigger one snapshot by creating a `Snapshot` that references the recipe:

```yaml
--8<-- "deploy/examples/getting-started.yaml:snapshot"
```

This is the one resource you `create` rather than `apply`. It uses `generateName`, so every invocation gets a fresh, unique name such as `app-data-manual-abc12`, and `kubectl apply` cannot track a server-named object.

Run `create` against the bundle. The namespace, Secret and other resources already exist, so `kubectl` reports them unchanged, and the `Snapshot` is the one new object it mints:

```console
$ kubectl create -f deploy/examples/getting-started.yaml
snapshot.kopiur.home-operations.com/app-data-manual-abc12 created

$ kubectl -n demo get snapshots -w
NAME                    PHASE       ORIGIN   SNAPSHOT     AGE
app-data-manual-abc12   Pending     manual                2s
app-data-manual-abc12   Running     manual                6s
app-data-manual-abc12   Succeeded   manual   k8f3c1a90    41s
```

`Succeeded` with a `SNAPSHOT` ID means the data is in your repository. Inspect the details:

```console
$ kubectl -n demo get snapshot app-data-manual-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":...,"bytesNew":...,"filesNew":...}
```

If it stays `Pending` with no Job, the mover is blocked on a precondition, usually credentials. The cause is on the `Snapshot`'s conditions and as an Event. See [Movers → Troubleshooting](movers.md#troubleshooting).

## Step 6 — Restore it (the half people forget to test)

A backup you have never restored is a hope, not a backup. Restore the snapshot you just made into a **new** PVC, so you can compare it without touching the original.

This `Restore` section was applied with the bundle, but its `source.snapshotRef.name` is a placeholder until you point it at the `Snapshot` name from Step 5, here `app-data-manual-abc12`:

```yaml
--8<-- "deploy/examples/getting-started.yaml:restore"
```

Set that name, re-apply the bundle, which is idempotent so only the changed `Restore` updates, then watch it come up:

```console
$ kubectl apply -f deploy/examples/getting-started.yaml
$ kubectl -n demo get restore -w
NAME              PHASE        AGE
app-data-verify   Resolving    2s
app-data-verify   Restoring    8s
app-data-verify   Completed    37s
```

`Completed` means the data landed in `app-data-restored`. Mount that PVC in a pod and confirm your files are there. That is the real proof the round trip works.

## Step 7 — Put it on a schedule

Manual backups prove the pipeline; a `SnapshotSchedule` makes it routine. It creates `Snapshot` objects on a cron, with deterministic jitter so replicas agree and load spreads.

```yaml
--8<-- "deploy/examples/getting-started.yaml:schedule"
```

It was applied with the bundle. Confirm it is registered, and check the firing it pinned:

```console
$ kubectl -n demo get snapshotschedule
NAME               CONFIG     SCHEDULE    SUSPENDED   AGE
app-data-nightly   app-data   H 2 * * *   false       3s

# the controller pins the next firing into status:
$ kubectl -n demo get snapshotschedule app-data-nightly \
    -o jsonpath='{.status.nextSchedule.at}'
2026-06-07T02:17:00Z
```

That is a complete, recurring, restore-tested backup. 🎉

## What just happened (and where to go next)

You created a **Repository** (where), a **SnapshotPolicy** (what), invoked it with a **Snapshot** (one snapshot), proved it with a **Restore**, and automated it with a **SnapshotSchedule** (when).

Maintenance, the periodic `kopia maintenance` that reclaims space, was set up for you automatically the moment the repository existed. You do not have to do anything for it.

/// tip | Wait for `Ready` (kstatus / GitOps)

Every reconciled CRD exposes standard `metav1.Condition` values, so `Ready` plus `Reconciling` and `Stalled`, and `status.observedGeneration`. Flux `wait` and `healthChecks`, Argo CD health, and plain `kubectl wait` therefore work natively:

```console
$ kubectl -n demo wait --for=condition=Ready repository/primary --timeout=2m
$ kubectl -n demo wait --for=condition=Ready snapshotpolicy/app-data --timeout=2m
```

///

From here:

- **[Concepts](concepts/how-kopia-works.md)** is the *why* behind what you just did: dedup, the identity model, the three-resource split, and one-shared-repository guidance.
- **[Repositories & backends](repositories.md)** points Kopiur at Azure, GCS, B2, a NAS through filesystem, SFTP or WebDAV, or rclone, and shares one repository across namespaces with `ClusterRepository`.
- **[Backups & schedules](backups.md)** covers multi-PVC selectors, hooks such as quiescing a database before snapshotting, retention tuning, and `deletionPolicy`.
- **[Restores](restores.md)** covers point-in-time restore, deploy-or-restore for GitOps, and restoring snapshots Kopiur did not create.
- **[Maintenance](maintenance.md)** covers what runs, when, and how shared repositories coordinate.
- **[Examples](examples.md)** is the complete, apply-ready manifest ladder covering the patterns above.
- **[Troubleshooting](troubleshooting.md)** is for when a step above does not go green.

## Tearing down the walkthrough

```console
$ kubectl -n demo delete snapshotschedule app-data-nightly
$ kubectl -n demo delete restore app-data-verify
$ kubectl -n demo delete snapshot --all         # finalizer also deletes the snapshots
$ kubectl -n demo delete snapshotpolicy app-data
$ kubectl -n demo delete repository primary
```

/// warning | Deleting a Snapshot deletes its snapshot

A scheduled or manual `Snapshot` defaults to `deletionPolicy: Delete`, so removing the object runs `kopia snapshot delete` through a finalizer. Use `Retain`, or `Orphan`, if you want the object gone but the snapshot kept. See [Backups → deletionPolicy](backups.md#deletionpolicy--what-happens-to-the-snapshot).

///
