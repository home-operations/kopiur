# Filesystem (PVC or inline NFS)

The filesystem backend stores the kopia repository on a **local path** that Kopiur mounts into the mover. Behind that path is either a `PersistentVolumeClaim` or an **inline NFS export**, which is `volume.nfs` and needs no PVC. Either way it is typically a NAS or NFS share. See [Inline NFS](#inline-nfs-no-pvc) below.

There are **no object-store credentials** here. The only secret is `KOPIA_PASSWORD`. What bites people on this backend is **ownership**, not authentication.

Reach for this when your off-site copy is an on-prem NAS, or any `ReadWriteMany` volume. For a remote server reached over SSH, see [SFTP](sftp.md).

## Provider prerequisites

- Storage the mover can mount **read-write**: either a **`PersistentVolumeClaim`** or an **NFS export**. For a PVC, use `ReadWriteMany`, which an NFS or NAS StorageClass provides. Backup, restore, _and_ maintenance movers may run as different Jobs at the same time, and they all need to mount it. The example bundles a PVC; the [inline-NFS variant](#inline-nfs-no-pvc) needs no PVC at all.
- The repository path must be **writable by the UID the mover runs as**, which defaults to `65532`. See Troubleshooting.

## The Secret shape

Filesystem backends need **only** the repository encryption password.

| Secret key       | Required | What it is                                                  |
| ---------------- | -------- | ----------------------------------------------------------- |
| `KOPIA_PASSWORD` | **yes**  | The repository encryption password. No backend `auth` keys. |

```yaml
stringData:
    KOPIA_PASSWORD: "choose-something-long-and-random"
```

/// warning | Lose the password, lose the backups

Even though the data sits on your own NAS, kopia still encrypts it with `KOPIA_PASSWORD`. Lose the password and the repository is unrecoverable. Store it outside the cluster and back up the Secret. See [Encryption](../repositories.md#encryption-and-repository-creation).

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/filesystem.yaml:repository"
```

## Fields reference (`backend.filesystem`)

| Field               | Required | Default | Example           | What it controls                                                                                                                |
| ------------------- | -------- | ------- | ----------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `path`              | yes      | —       | `/repo`           | **Mount path inside the mover pod** where kopia writes the repository. This is not a path on your NAS; the `volume` decides what's behind it. |
| `volume`            | no       | —       | —                 | What backs `path`: **exactly one of** `pvc` or `nfs`. Omit it entirely only if `path` already exists on the node or image, as a hostPath. |
| `volume.pvc.name`   | —        | —       | `nas-repo`        | The `PersistentVolumeClaim` mounted read-write at `path`. It must be `ReadWriteMany`, because movers overlap.                    |
| `volume.nfs.server` | —        | —       | `nas.lan`         | NFS server hostname or IP, for an inline NFS export with no PVC. See below.                                                      |
| `volume.nfs.path`   | —        | —       | `/export/kopia`   | The absolute export path **on the NFS server** (what `showmount -e nas.lan` lists).                                              |

/// note | `volume` is an "exactly one of" choice

`volume: { pvc: … }` and `volume: { nfs: … }` are externally-tagged variants. Set one, never both.

An empty or absent `volume` means "the path is already present in the mover", such as a `hostPath` or a mount baked into the image. That is mainly used by the e2e harness.

///

## Customization — the values you actually change

- **`volume.pvc.name`** names the PVC to mount. Its size and StorageClass live on the PVC, not here.
- **`volume.nfs`** points straight at an NFS export instead of a PVC. See [Inline NFS](#inline-nfs-no-pvc) below.
- **`path`** is the in-pod mount point. `/repo` is a fine default.
- **The mover `securityContext`** is where you set `runAsUser` and `fsGroup` on the consuming `SnapshotPolicy`, to match the share's ownership. See [Permissions](../permissions.md).
- **`create.enabled`** initializes the repository if it's missing.

### Sizing the PVC

The bundled example requests `500Gi` as a placeholder. Size yours to the **deduplicated, compressed** repository, not to the raw source data.

kopia content-addresses everything, so N daily snapshots of slowly-changing data cost roughly one full copy plus the churn, not N copies. A reasonable starting point is 1 to 1.5 times the source data. Watch actual usage after the first retention cycle and resize; most NAS-backed StorageClasses support volume expansion.

The failure mode to avoid is running the volume completely full, because kopia maintenance needs headroom to rewrite and compact blobs.

### Preparing the export (NFS-side ownership)

The mover runs as UID `65532` by default and is **not root**, so the classic `root_squash` setting on the export doesn't matter here. What matters is that UID `65532` can write the directory:

```console
# on the NAS / NFS server
$ mkdir -p /export/kopia
$ chown -R 65532:65532 /export/kopia
```

Your NAS may force all clients to one identity, through `all_squash` or a "map all users" setting. In that case point the mapping, meaning `anonuid` and `anongid`, at the directory owner. Or set the mover's `runAsUser` to whatever UID the NAS expects. The full decision table is in [Permissions, UID & GID](../permissions.md).

/// warning | `fsGroup` does **not** work here

The natural instinct is to set `moverDefaults.podSecurityContext.fsGroup` to the export's GID. That does nothing: **`fsGroup` has no effect on NFS**, because the kubelet doesn't chown in-tree NFS mounts.

Suppose the export is owned by a dedicated UID and GID while your apps run as other UIDs. Use a **shared supplemental group** instead. Make the export group-writable with `chown root:3001 … && chmod 2775 …`, then give that group to everything that writes to the backend, meaning the movers **and** the kopia-ui server:

```yaml
spec:
  moverDefaults:
    podSecurityContext:
      supplementalGroups: [3001]
  server: # only if the web UI is enabled
    podSecurityContext:
      supplementalGroups: [3001]
```

Per-policy source reads stay correct, because the mover reads the source as the app's UID and the group is additive.

The admission webhook **warns** when an NFS filesystem repository relies only on `fsGroup`. For the full recipe, see [Security context → NFS filesystem repositories](../security-context.md#nfs-filesystem-repositories).

## Inline NFS (no PVC) { #inline-nfs-no-pvc }

kopia has **no native NFS backend**. You reach NFS _through_ the filesystem backend, by mounting the export at `path`.

Instead of pre-creating a `ReadWriteMany` PVC, name an NFS export directly under `volume.nfs`. The operator then builds a Kubernetes inline `nfs` volume on every mover Job: bootstrap, backup, restore, and maintenance.

```yaml
--8<-- "deploy/examples/backends/nfs.yaml:repository"
```

This is the lowest-friction path to an on-prem NAS repository. No PVC, no StorageClass, no provisioner. Just a `server` and an absolute `path`. The same `volume.nfs` shape works on a `ClusterRepository`.

To back up an NFS export _as a source_, rather than as the repository, see [Example 10](../examples.md#example-10--nfs-source-no-pvc).

/// note | A volume-backed repo bootstraps in a mover Job

A bare-path filesystem repository is reachable from the controller, so it is connected or created in-process.

A PVC-backed or NFS-backed repository is **not** reachable from the controller. So the operator runs the connect-or-create in a short mover Job that mounts the volume, which is the same route object stores take. The Repository moves from `Initializing` to `Ready` as that Job completes.

///

## As a `ClusterRepository`

A `ClusterRepository` may also use a filesystem backend.

For a PVC, the claim must exist, and be `ReadWriteMany`, in whatever namespace the movers run in. See [Movers](../movers.md).

An [inline NFS export](#inline-nfs-no-pvc) is reachable from any mover namespace, because it is named rather than claimed. That can make it simpler than a cross-namespace PVC, though a cloud or object backend is usually the better fit for a shared platform repository.

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny smoke-test: a throwaway PVC, a `SnapshotPolicy`, and a `Snapshot`, all pointed at the `nas-primary` repository above. It takes you from "applied" to "a snapshot on my NAS" in one go.

/// warning | Two prerequisites for filesystem/NFS

- **The repository volume needs a ReadWriteMany StorageClass.** The bundled `nas-repo` PVC asks for `ReadWriteMany` so backup, restore, and maintenance movers can overlap. On a single-node test cluster you can substitute `ReadWriteOnce`.
- **The export or path must be writable by the mover UID `65532`.** `fsGroup` has no effect on NFS. Run `chown -R 65532:65532` on the path on the NAS, or use the shared supplemental group recipe in [Preparing the export](#preparing-the-export-nfs-side-ownership). Without this the `Repository` stops at `Failed` with a permission-denied event.

///

**1. Apply the bundle.** That is the `backups` namespace, the Secret, the repository PVC and Repository, and the smoke-test objects:

```console
$ kubectl apply -f deploy/examples/backends/filesystem.yaml
```

**2. Wait for the repository to be `Ready`.** Everything else waits on this. A volume-backed repository bootstraps in a short mover Job, as the note above explains, so this can take a little longer than an object-store repository:

```console
$ kubectl -n backups wait --for=condition=Ready repository/nas-primary --timeout=2m
repository.kopiur.home-operations.com/nas-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it rather than apply it. The namespace, Secret, Repository, PVCs, and policy already exist and report unchanged. The `Snapshot` is the one new object:

```console
$ kubectl create -f deploy/examples/backends/filesystem.yaml
snapshot.kopiur.home-operations.com/smoke-now-abc12 created
```

**4. Watch it succeed:**

```console
$ kubectl -n backups get snapshots -w
NAME              PHASE       ORIGIN   SNAPSHOT     AGE
smoke-now-abc12   Pending     manual                2s
smoke-now-abc12   Running     manual                7s
smoke-now-abc12   Succeeded   manual   k1f1ec0a8    38s
```

The output above is illustrative. The `Snapshot` has no fixed `Succeeded` *condition*, so to wait on it in a script, key on the phase:

```console
$ kubectl -n backups wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/smoke-now-abc12 --timeout=5m
```

**5. Prove the data really moved.** `status.stats` shows non-zero `bytesNew` and `filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia snapshot ID on your NAS:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

Both outputs are illustrative; sizes and the ID vary. Non-zero `bytesNew` proves the backup wrote real content to the repository path.

**6. Clean up** the smoke-test when you're done. This leaves the repository PVC in place; delete `nas-repo` too if you want the repository gone:

```console
$ kubectl -n backups delete snapshot --all       # finalizer also deletes the kopia snapshot
$ kubectl -n backups delete snapshotpolicy smoke
$ kubectl -n backups delete pvc smoke-data
```

/// warning | Deleting a Snapshot deletes its snapshot

A produced `Snapshot` defaults to `deletionPolicy: Delete`, so removing the CR runs `kopia snapshot delete` through a finalizer. Use `Retain` or `Orphan` to keep the data. See [Backups → deletionPolicy](../backups.md#deletionpolicy--what-happens-to-the-snapshot).

///

From here the rest of the lifecycle is the same on every backend. Only the `Repository` differs. Put it on a cron with a `SnapshotSchedule`, described in [Backups & schedules](../backups.md) and [Example 01](../examples.md#example-01--single-pvc-scheduled). Restore by picking a `Snapshot`, described in [Restores](../restores.md) and [Example 03](../examples.md#example-03--restore-by-picking-a-snapshot).

/// note | ReadWriteMany matters (for PVCs)

Backup, restore, and maintenance run as separate mover Jobs and may overlap. A `ReadWriteOnce` volume can only attach to one node at a time, so it blocks the others. Use `ReadWriteMany`.

An [inline NFS export](#inline-nfs-no-pvc) sidesteps this, because NFS is multi-mount by nature and concurrent movers all reach it.

///

## Troubleshooting

/// warning | It's ownership, not a credential

The most common filesystem failure is **permission denied on the repository path**. The path isn't writable by the mover's UID, which defaults to `65532`.

Kopiur's Warning Event names the exact UID and the `chown -R <uid> <path>` to run on the NAS. Either `chown` the path, or match the mover's UID and GID to the share owner through the mover `securityContext`. For the full story, see [Permissions, UID & GID](../permissions.md).

///

- **`permission denied` on create or connect.** Run `chown -R 65532 <path>`, or set the mover UID to the owner. If the export is owned by a dedicated UID and GID while your apps run as other UIDs, use a [shared supplemental group](#preparing-the-export-nfs-side-ownership) rather than `fsGroup`, which NFS ignores. See above.
- **Mover Job pending.** For a PVC, it isn't bound or isn't `ReadWriteMany`; check the PVC and StorageClass. For NFS, the pod can't mount the export; confirm the `server` and `path` are reachable from the cluster nodes and that the export permits them.

## See also

- [Permissions, UID & GID](../permissions.md): the ownership story this backend lives and dies by.
- [Repositories & backends](../repositories.md): the concepts, meaning scope, encryption, and creation.
- [Movers, RBAC & credentials](../movers.md): mover Jobs and volume mounts.
- Sibling backend: [SFTP](sftp.md), the same NAS reached over SSH instead.
