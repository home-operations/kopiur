# Filesystem (PVC or inline NFS)

The filesystem backend stores the kopia repository on a **local path** Kopiur
mounts into the mover. That path is backed by either a `PersistentVolumeClaim` or
an **inline NFS export** (`volume.nfs` — no PVC; see [below](#inline-nfs-no-pvc)),
typically a NAS/NFS share. There are **no object-store credentials**: the only
secret is `KOPIA_PASSWORD`. The thing that bites people here is **ownership**, not
auth.

Reach for this when your "off-site" is an on-prem NAS or any `ReadWriteMany`
volume. For a remote server reached over SSH, see [SFTP](sftp.md).

## Provider prerequisites

- Storage the mover can mount **read-write**: either a **`PersistentVolumeClaim`**
  or an **NFS export**. For a PVC, use `ReadWriteMany` (NFS/NAS) so that backup,
  restore, _and_ maintenance movers — which may run as different Jobs at the same
  time — can all mount it. The example bundles a PVC; the
  [inline-NFS variant](#inline-nfs-no-pvc) needs no PVC at all.
- The repository path must be **writable by the UID the mover runs as** (default
  `65532`). See Troubleshooting.

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

Even though the data sits on your own NAS, kopia still encrypts it with
`KOPIA_PASSWORD`. Lose the password and the repository is unrecoverable. Store it
outside the cluster and back up the Secret. See [Encryption](../repositories.md#encryption-and-repository-creation).

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/filesystem.yaml:repository"
```

## Fields reference (`backend.filesystem`)

| Field               | Required | Default | Example           | What it controls                                                                                                                |
| ------------------- | -------- | ------- | ----------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `path`              | yes      | —       | `/repo`           | **Mount path inside the mover pod** where kopia writes the repository. Not a path on your NAS — the `volume` decides what's behind it. |
| `volume`            | no       | —       | —                 | What backs `path`: **exactly one of** `pvc` or `nfs`. Omit entirely only if `path` already exists on the node/image (hostPath).  |
| `volume.pvc.name`   | —        | —       | `nas-repo`        | The `PersistentVolumeClaim` mounted read-write at `path`. Must be `ReadWriteMany` (movers overlap).                              |
| `volume.nfs.server` | —        | —       | `nas.lan`         | NFS server hostname or IP — for an inline NFS export with no PVC (see below).                                                    |
| `volume.nfs.path`   | —        | —       | `/export/kopia`   | The absolute export path **on the NFS server** (what `showmount -e nas.lan` lists).                                              |

/// note | `volume` is an "exactly one of" choice

`volume: { pvc: … }` and `volume: { nfs: … }` are externally-tagged variants — set
one, never both. An empty/absent `volume` means "the path is already present in
the mover" (a `hostPath`/baked-in mount; mainly the e2e harness).

///

## Customization — the values you actually change

- **`volume.pvc.name`** — the PVC to mount (its size/StorageClass live on the PVC, not here).
- **`volume.nfs`** — point straight at an NFS export instead of a PVC ([below](#inline-nfs-no-pvc)).
- **`path`** — the in-pod mount point; `/repo` is a fine default.
- **mover `securityContext`** — set `runAsUser`/`fsGroup` on the consuming
  `SnapshotPolicy` to match the share's ownership; see [Permissions](../permissions.md).
- **`create.enabled`** — initialize the repository if missing.

### Sizing the PVC

The bundled example requests `500Gi` as a placeholder — size yours to the
**deduplicated, compressed** repository, not the raw source data. kopia
content-addresses everything, so N daily snapshots of slowly-changing data cost
roughly one full copy plus the churn, not N copies. A reasonable starting point
is 1–1.5× the source data; watch actual usage after the first retention cycle
and resize (most NAS-backed StorageClasses support volume expansion). Running
the volume completely full is the failure mode to avoid — kopia maintenance
needs headroom to rewrite and compact blobs.

### Preparing the export (NFS-side ownership)

The mover runs as UID `65532` by default and is **not root**, so classic
`root_squash` on the export is irrelevant — what matters is that UID `65532`
can write the directory:

```console
# on the NAS / NFS server
$ mkdir -p /export/kopia
$ chown -R 65532:65532 /export/kopia
```

If your NAS forces all clients to one identity (`all_squash` /
"map all users"), point that mapping (`anonuid`/`anongid`) at the directory
owner instead — or set the mover's `runAsUser` to whatever UID the NAS expects.
The full decision table is in [Permissions, UID & GID](../permissions.md).

/// warning | `fsGroup` does **not** work here

A natural instinct is to set `moverDefaults.podSecurityContext.fsGroup` to the
export's GID — but **`fsGroup` is a no-op on NFS** (the kubelet doesn't chown
in-tree NFS mounts). For an export owned by a dedicated UID/GID while your apps
run as other UIDs, use a **shared supplemental group** instead: make the export
group-writable (`chown root:3001 … && chmod 2775 …`) and give every
backend-writer (movers **and** the kopia-ui server) that group:

```yaml
spec:
  moverDefaults:
    podSecurityContext:
      supplementalGroups: [3001]
  server: # only if the web UI is enabled
    podSecurityContext:
      supplementalGroups: [3001]
```

Per-policy source reads stay correct (the mover reads the source as the app's
UID; the group is additive). The admission webhook **warns** when an NFS
filesystem repo relies only on `fsGroup`. Full recipe:
[Security context → NFS filesystem repositories](../security-context.md#nfs-filesystem-repositories).

## Inline NFS (no PVC) { #inline-nfs-no-pvc }

kopia has **no native NFS backend** — NFS is reached _through_ the filesystem
backend by mounting the export at `path`. Instead of pre-creating a
`ReadWriteMany` PVC, name an NFS export directly under `volume.nfs` and the
operator synthesizes a Kubernetes inline `nfs` volume on every mover Job
(bootstrap, backup, restore, maintenance):

```yaml
--8<-- "deploy/examples/backends/nfs.yaml:repository"
```

This is the lowest-friction path to an on-prem NAS repository: no PVC, no
StorageClass, no provisioner — just a `server` and an absolute `path`. The same
`volume.nfs` shape works on a `ClusterRepository`. To back up an NFS export
_as a source_ (rather than as the repository), see
[Example 10](../examples.md#example-10--nfs-source-no-pvc).

/// note | A volume-backed repo bootstraps in a mover Job

A bare-path filesystem repo is reachable from the controller and is
connected/created in-process. A PVC- or NFS-backed repo is **not** reachable from
the controller, so the operator runs the connect/create in a short mover Job that
mounts the volume — the same path object stores use. The Repository moves
`Initializing` → `Ready` as that Job completes.

///

## As a `ClusterRepository`

A `ClusterRepository` may also use a filesystem backend. For a PVC, the claim and
its `ReadWriteMany` reach must be available in whatever namespace the movers run
in — see [Movers](../movers.md). An [inline NFS export](#inline-nfs-no-pvc) sees
the same reach from any mover namespace (it's named, not claimed), which can make
it simpler than a cross-namespace PVC — though a cloud/object backend is usually
the better fit for a shared platform repository.

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny
smoke-test (a throwaway PVC + a `SnapshotPolicy` + a `Snapshot`) that targets the
`nas-primary` repository above, so you can go from "applied" to "a snapshot on my
NAS" in one arc.

/// warning | Two prerequisites for filesystem/NFS

- **The repository volume needs an RWX StorageClass.** The bundled `nas-repo` PVC
  asks for `ReadWriteMany` so backup, restore, and maintenance movers can overlap.
  On a single-node test cluster you can substitute `ReadWriteOnce`.
- **The export/path must be writable by the mover UID `65532`.** `fsGroup` is a
  no-op on NFS — `chown -R 65532:65532` the path on the NAS, or use the shared
  supplemental group recipe in
  [Preparing the export](#preparing-the-export-nfs-side-ownership). Without this
  the `Repository` stops at `Failed` with a permission-denied event.

///

**1. Apply the bundle** (namespace `backups`, Secret, the repo PVC + Repository,
and the smoke-test objects):

```console
$ kubectl apply -f deploy/examples/backends/filesystem.yaml
```

**2. Wait for the repository to be `Ready`** — the gate everything else waits on.
A volume-backed repo bootstraps in a short mover Job (see the note above), so
this can take a touch longer than an object-store repo:

```console
$ kubectl -n backups wait --for=condition=Ready repository/nas-primary --timeout=2m
repository.kopiur.home-operations.com/nas-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it
(the namespace, Secret, Repository, PVCs, and policy already exist and report
unchanged — the `Snapshot` is the one new object):

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

(Output illustrative.) The `Snapshot` has no fixed `Succeeded` *condition*; to
wait on it in a script, key on the phase:

```console
$ kubectl -n backups wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/smoke-now-abc12 --timeout=5m
```

**5. Deep proof — the data really moved.** `status.stats` shows non-zero
`bytesNew`/`filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia
snapshot ID on your NAS:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

(Both outputs illustrative — sizes and the ID vary.) Non-zero `bytesNew` is the
proof the backup wrote real content to the repository path.

**6. Clean up** the smoke-test when you're done (this leaves the repo PVC; delete
`nas-repo` too if you want the repository gone):

```console
$ kubectl -n backups delete snapshot --all       # finalizer also deletes the kopia snapshot
$ kubectl -n backups delete snapshotpolicy smoke
$ kubectl -n backups delete pvc smoke-data
```

/// warning | Deleting a Snapshot deletes its snapshot

A produced `Snapshot` defaults to `deletionPolicy: Delete`, so removing the CR
runs `kopia snapshot delete` via a finalizer. Use `Retain` (or `Orphan`) to keep
the data — see [Backups → deletionPolicy](../backups.md#deletionpolicy--what-happens-to-the-snapshot).

///

From here the full lifecycle is backend-independent — only the `Repository`
differs. Put it on a cron with a `SnapshotSchedule`
([Backups & schedules](../backups.md),
[Example 01](../examples.md#example-01--single-pvc-scheduled)) and restore by
picking a `Snapshot` ([Restores](../restores.md),
[Example 03](../examples.md#example-03--restore-by-picking-a-snapshot)).

/// note | ReadWriteMany matters (for PVCs)

Backup, restore, and maintenance run as separate mover Jobs and may overlap. A
`ReadWriteOnce` volume can only attach to one node at a time and will block the
others; use `ReadWriteMany`. An [inline NFS export](#inline-nfs-no-pvc) sidesteps
this — NFS is inherently multi-mount, so concurrent movers all reach it.

///

## Troubleshooting

/// warning | It's ownership, not a credential

The most common filesystem failure is **permission denied on the repository
path** — the path isn't writable by the mover's UID (default `65532`). Kopiur's
Warning Event names the exact UID and the `chown -R <uid> <path>` to run on the
NAS. Either `chown` the path or match the mover's UID/GID to the share owner via
the mover `securityContext`. Full story: [Permissions, UID & GID](../permissions.md).

///

- **`permission denied`** on create/connect — `chown -R 65532 <path>` (or set the
  mover UID to the owner). If the export is owned by a dedicated UID/GID and your
  apps run as other UIDs, use a [shared supplemental group](#preparing-the-export-nfs-side-ownership)
  rather than `fsGroup` (which NFS ignores). See above.
- **Mover Job pending** — for a PVC, it isn't bound or isn't `ReadWriteMany`; check
  the PVC and StorageClass. For NFS, the pod can't mount the export — confirm the
  `server`/`path` are reachable from the cluster nodes and the export permits them.

## See also

- [Permissions, UID & GID](../permissions.md) — the ownership story this backend lives and dies by.
- [Repositories & backends](../repositories.md) — concepts: scope, encryption, creation.
- [Movers, RBAC & credentials](../movers.md) — mover Jobs and volume mounts.
- Sibling backend: [SFTP](sftp.md) — same NAS, reached over SSH instead.
