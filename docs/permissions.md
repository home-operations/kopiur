# Permissions, UID & GID

The most common reason a backup runs but reads **nothing**, or a restore writes files the app then cannot open, is a **UID or GID mismatch**. This page shows how to find the right numbers, how to set them, and how to check it worked, with no guesswork.

/// tip | The mental model: the mover is a separate pod

A backup does not run inside your app's pod. Kopiur launches a short-lived **mover** Job that mounts your PVC and runs kopia. Linux file permissions do not care that it is "your" data. They only see the **UID and GID the mover process runs as**. So the rule is simply:

- **Backup**: the mover's UID and GID must be able to **read** every file in the source PVC.
- **Restore**: the mover's UID and GID must be able to **write** into the target PVC.

Get the numbers to line up and permissions stop being a problem.

///

/// info | Looking for the field reference?

This page is the **task** guide: how to find the owning UID and make a stuck backup or restore work.

For the full `securityContext` reference, meaning every field, the hardened default, inheriting from a workload, root and privileged movers, and the awkward cases such as RWX volumes and preserving ownership on restore, see [**The mover security context**](security-context.md).

///

## What the mover runs as by default

Out of the box the mover runs **unprivileged**, as the mover image's user, which is **UID `65532`**, the distroless `nonroot` user. It runs with a hardened security context: `runAsNonRoot: true`, `allowPrivilegeEscalation: false`, all Linux capabilities dropped, and seccomp `RuntimeDefault`.

That default reads data that is **world-readable** or **owned by `65532`**. If your app writes files `0600` or `0640` owned by some other UID, which is very common because most images run as `1000`, `1001` or `999`, an unprivileged mover at `65532` gets **permission denied** on those files.

You then have three options, best first:

1. Run the mover as the **same UID and GID** that owns the data.
2. Run the mover with a **GID** that matches a group the files are readable by.
3. Run the mover as **root**, which reads anything but is elevated and needs an admin opt-in. This is the last resort.

The rest of this page is how to do options 1 and 2 reliably, and when to reach for option 3.

## Step 1 — Find the UID/GID that owns your data

You want the **numeric** owner of the files in the PVC. Numeric, not names: the mover image has no `/etc/passwd` entry for your app's user, so `ls -l` showing a name is misleading. Use `-n` for numeric.

**If the workload is running**, read it straight from the app pod:

```console
$ kubectl exec -n app deploy/myapp -- id
uid=1000(app) gid=1000(app) groups=1000(app)

$ kubectl exec -n app deploy/myapp -- ls -ln /data
drwxr-xr-x 2 1000 1000 4096 Jun  6 12:00 .
-rw------- 1 1000 1000  512 Jun  6 12:00 secret.key   # 0600, owner-only
```

Here the data is owned by `1000:1000` and some files are owner-only at `0600`, so the mover **must** run as UID `1000`. Matching the group is not enough for `0600` files.

**If nothing is mounting the PVC**, for example a fresh restore target or a scaled-down app, spin up a throwaway pod that mounts it read-only and inspect it:

```console
$ kubectl run pvc-inspect -n app --rm -it --restart=Never \
    --image=busybox --overrides='
{
  "spec": {
    "containers": [{
      "name": "x", "image": "busybox", "command": ["sh"], "stdin": true, "tty": true,
      "volumeMounts": [{"name": "d", "mountPath": "/data", "readOnly": true}]
    }],
    "volumes": [{"name": "d", "persistentVolumeClaim": {"claimName": "app-data"}}]
  }
}' -- sh

/ # stat -c '%u %g %a %n' /data /data/*    # numeric uid, gid, mode, name
1000 1000 755 /data
1000 1000 600 /data/secret.key
```

Look for the strictest file. If any file you need is `0600` owned by `1000`, the mover has to be UID `1000`. If everything is at least group-readable, so `0640` or `0750`, and shares a GID, matching the **GID** is enough.

## Step 2 — Set the mover's UID/GID in the `SnapshotPolicy`

Set it per recipe under `spec.mover.securityContext`, which is a standard Kubernetes container `SecurityContext`. Match what you found in Step 1:

```yaml
spec:
    mover:
        securityContext:
            runAsUser: 1000 # the UID that owns the data
            runAsGroup: 1000 # the GID that owns the data
            runAsNonRoot: true # keep the unprivileged guarantee
            allowPrivilegeEscalation: false
            capabilities:
                drop: ["ALL"]
            seccompProfile:
                type: RuntimeDefault
```

A complete, apply-ready example, with a Repository and a SnapshotPolicy carrying this block plus the root-mover variant commented out, is [Example 09](examples.md#example-09--mover-uidgid--permissions):

/// tip | `fsGroup` lives on `mover.podSecurityContext`

`runAsUser` and `runAsGroup` above are **container**-level, under `spec.mover.securityContext`. `fsGroup` is **pod**-level, so it has its own sibling field, `spec.mover.podSecurityContext.fsGroup`.

`fsGroup` is the right tool when an unprivileged mover must **write a freshly provisioned restore volume**, because the kubelet makes the mount group-writable by that GID. For *reading* source data, prefer matching the owning `runAsUser` and `runAsGroup`. See [The mover security context → fsGroup](security-context.md).

///

/// warning | `fsGroup` does nothing on NFS

`fsGroup` relies on the kubelet chowning the volume, and the kubelet **does not do that for in-tree NFS mounts**. So `fsGroup` cannot grant write access to an NFS source or an NFS-backed filesystem repository.

Use one of these instead: `podSecurityContext.supplementalGroups` against a group-writable export, since supplemental GIDs *are* honored over NFS; `securityContext.runAsUser` matching the export owner; or a server-side remap such as TrueNAS Mapall or `all_squash`. See [Security context → NFS filesystem repositories](security-context.md#nfs-filesystem-repositories).

///

## Step 3 — Verify it worked

Re-run the backup and confirm it actually read files, rather than quietly snapshotting an empty or partial tree:

```console
# the mover Job's exact name lives on the Snapshot (it's named after the Snapshot CR):
$ kubectl get snapshot <snapshot-name> -n app -o jsonpath='{.status.job.name}'
app-data-manual-abc12

# its pod (by the standard Job-managed pod label), then the container's effective UID:
$ kubectl get pods -n app --selector=job-name=app-data-manual-abc12
$ kubectl get pod <mover-pod> -n app \
    -o jsonpath='{.spec.containers[0].securityContext.runAsUser}{"\n"}'
1000

# permission errors, if any, surface in the mover log and on the Snapshot status:
$ kubectl logs <mover-pod> -n app | grep -i "permission denied"
$ kubectl get snapshot <snapshot-name> -n app -o jsonpath='{.status.conditions}'
```

A healthy backup ends `Succeeded` with non-zero files and bytes in `status`. A backup that "succeeded" but shows **zero files** is the classic sign the mover could not read the data. Recheck the UID.

## When you can't match the UID: the root mover

If the data is owned by **assorted UIDs you cannot match**, such as a `lost+found`, a multi-user volume, or an app that writes as root, a **root mover** reads everything. Set:

```yaml
spec:
    mover:
        securityContext:
            runAsUser: 0
            runAsNonRoot: false
        privilegedMode: true # also preserves UID/GID ownership on RESTORE
```

A root, or otherwise elevated, mover is a **privileged mover**, and granting it is a per-namespace admin decision. If the namespace has not opted in, the `Snapshot` is refused with a clear `MoverPermitted=False` condition.

Opt the namespace in by applying a `Namespace` carrying the opt-in annotation:

```yaml
--8<-- "deploy/examples/privileged-mover-namespace.yaml"
```

```console
$ kubectl apply -f privileged-mover-namespace.yaml
```

Or do it imperatively with `kubectl annotate namespace <ns> kopiur.home-operations.com/privileged-movers=true`.

Anything that trips the privileged detector needs that opt-in: `runAsUser: 0`, `privileged: true`, `allowPrivilegeEscalation: true`, added Linux capabilities, `runAsNonRoot: false`, or `privilegedMode: true`. The full detail and the revoke path are in [Movers → Privileged movers](movers.md#privileged-movers).

/// tip | Prefer matching the UID over going root

A root mover widens the blast radius of the mover ServiceAccount Kopiur mints. Reach for it only when you genuinely cannot match the owning UID or GID. Most single-app PVCs back up fine as their app's UID.

///

## Filesystem repositories: the _other_ permission

The UID and GID story above is about reading **source data**. A [filesystem repository](backends/filesystem.md), backed by a PVC or NFS, adds a second surface: the **repository path itself must be writable** by the operator and mover UID.

When create or connect cannot write the repository path, Kopiur does not hang. It emits a Warning Event, and a `Bootstrapped=False` condition, naming the **actual** UID it runs as and the fix:

```console
$ kubectl describe repository nas-primary -n backups
...
Warning  PermissionDenied  the repository path is not writable by the operator's UID (65532) —
  fix its ownership/mode (e.g. `chown -R 65532 /repo`) and reconcile again.
```

The UID in that message is the operator's real effective UID, which varies with the chart's `podSecurityContext.runAsUser`, so the `chown` it prints is always correct for your install. Run it on the NAS or host backing the PVC, then reconcile.

For an **NFS-backed** repository whose export is owned by a dedicated UID and GID while your apps run as other UIDs, do not reach for `fsGroup`, which does nothing on NFS. Make the export group-writable instead, and give every backend writer the shared group: movers through `moverDefaults.podSecurityContext.supplementalGroups`, and the kopia UI server through `server.podSecurityContext.supplementalGroups`. That keeps per-policy source reads running as the app's UID while the group grants repository writes. The full recipe and an apply-ready example are in [Security context → NFS filesystem repositories](security-context.md#nfs-filesystem-repositories).

## Restore-side permissions

A restore writes files into the **target** PVC, so the same rules apply in reverse. A `Restore` has the **same `spec.mover`** surface a `SnapshotPolicy` does:

- **`Restore.spec.mover.securityContext`** sets the UID and GID the restore mover writes as, so the restored files land owned correctly and the mover can write the target. Before this field existed the restore mover always ran as UID `65532`. For a freshly created target PVC, meaning `target.pvc`, the default is usually fine. For an existing PVC, meaning `target.pvcRef`, match the UID that owns it.
- **`Restore.spec.mover.inheritSecurityContextFrom`** copies the `securityContext` from a live workload pod by label selector, instead of hard-coding it. It combines with `securityContext`, which overrides it field by field, and it needs the workload to pin `runAsUser`. It is handy for "restore as whatever the app runs as", and is covered fully in [Security context → Inherit it from the workload](security-context.md#2-inherit-it-from-the-workload).
- **Preserving original ownership.** Kopia restores files with the UID and GID they had when snapshotted. Reproducing that ownership requires a privileged root mover with `privilegedMode: true`; an unprivileged mover writes files owned by its own UID instead. An elevated restore mover, whether root, `privilegedMode`, or inherited from a root pod, is gated by the same `privileged-movers` namespace opt-in a backup uses.
- **`spec.options.ignorePermissionErrors`**, default `true`, lets a restore complete and *report* permission problems through a condition rather than failing hard. Set it `false` to fail closed when exact permissions matter.

See [Restores → Mover, cache & failure policy](restores.md#mover-cache--failure-policy) for the full restore mover surface, and [example 12](examples.md#example-12--restore-mover-cache--failure-policy).

## Try it end-to-end

See the UID-match fix work, and watch the classic anti-pattern fail, from a clean slate.

The setup is a PVC seeded with files owned `1000:1000`, mode `0600`, so readable **only** by UID 1000, backed up by a mover that runs as `runAsUser: 1000`. The backup reads the files, so `status.stats.filesNew` is non-zero. The *default* mover, at UID 65532, would "succeed" while reading **zero**.

It is one apply-ready bundle, [`deploy/examples/tryit/permissions-uid.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/permissions-uid.yaml): the `app` `Namespace`, a PVC, a seed Job, an S3 `Repository`, a UID-matched `SnapshotPolicy`, and a manual `Snapshot`.

The seed writes owner-only files that only UID 1000 can read:

```yaml
--8<-- "deploy/examples/tryit/permissions-uid.yaml:seed"
```

The policy matches the mover to that owner:

```yaml
--8<-- "deploy/examples/tryit/permissions-uid.yaml:policy"
```

**1. Fill in the credentials**, meaning `AWS_*` and `KOPIA_PASSWORD`, in the `secret` section, then apply the bundle:

```console
$ kubectl apply -f deploy/examples/tryit/permissions-uid.yaml
$ kubectl -n app wait --for=condition=Ready repository/app-primary --timeout=2m
$ kubectl -n app wait --for=condition=complete job/seed-app-data --timeout=2m
```

**2. Take the backup.** The `Snapshot` uses `generateName`, so `create` it:

```console
$ kubectl create -f deploy/examples/tryit/permissions-uid.yaml
snapshot.kopiur.home-operations.com/app-data-manual-abc12 created

$ kubectl -n app wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/app-data-manual-abc12 --timeout=5m
```

**3. Prove it read real data (deep).** The matched UID means non-zero `filesNew`, and the mover pod ran as 1000:

```console
# illustrative stats — non-zero filesNew/bytesNew is the proof:
$ kubectl -n app get snapshot app-data-manual-abc12 -o jsonpath='{.status.stats}{"\n"}'
{"sizeBytes":2097640,"bytesNew":2097640,"filesNew":2, ...}

# the mover Job is named after the Snapshot CR (no -snap suffix):
$ kubectl -n app get snapshot app-data-manual-abc12 -o jsonpath='{.status.job.name}{"\n"}'
app-data-manual-abc12

# its pod's effective UID — 1000, matching the data owner:
$ kubectl -n app get pods --selector=job-name=app-data-manual-abc12
$ kubectl -n app get pod <mover-pod> \
    -o jsonpath='{.spec.containers[0].securityContext.runAsUser}{"\n"}'
1000
```

/// warning | The anti-pattern: the default UID reads nothing

Drop the `mover` block, or set `runAsUser: 65532`, and re-run. The backup still ends **`Succeeded`**, but `status.stats.filesNew` reads **0**: the unprivileged mover got permission denied on every `0600` file and snapshotted an empty tree without saying so.

A `Succeeded` snapshot with **zero files** is the canonical sign of a UID mismatch. Match the owning UID as described above, or, if the data is owned by UIDs you cannot match, reach for a [root mover](#when-you-cant-match-the-uid-the-root-mover).

///

/// note | Illustrative output

The `status.stats` numbers and the `app-data-manual-abc12` and `<mover-pod>` names stand in for what your run produces. The shape, and the **non-zero `filesNew`**, are the point.

///

## Troubleshooting

| Symptom                                        | Where it shows                 | Cause                                                      | Fix                                                                                                                |
| ---------------------------------------------- | ------------------------------ | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| Backup `Succeeded` but **0 files / 0 bytes**   | `Snapshot` `.status`             | Mover UID can't read the source files.                     | Match `spec.mover.securityContext.runAsUser/Group` to the data owner (Steps 1–2).                                  |
| Mover log: `permission denied` reading source  | Mover pod logs                 | Same as above: partial read.                              | Same as above; or a root mover if UIDs can't be matched.                                                           |
| Backup stuck `Pending`, `MoverPermitted=False` | `Snapshot` condition / Event     | Mover requests privilege; namespace not opted in.          | `kubectl annotate namespace <ns> kopiur.home-operations.com/privileged-movers=true`, or drop the elevated context. |
| `Repository` `Failed`, `PermissionDenied`      | `Repository` Event / condition | Filesystem repo path not writable by the operator UID.     | `chown -R <uid> <path>` (the Event names the UID), then reconcile.                                                 |
| Restored files unreadable by the app           | After restore                  | Files restored as the mover's UID, not the original owner. | Set `Restore.spec.mover.securityContext.runAsUser/Group` to the app's UID (or `inheritSecurityContextFrom`); use a root mover with `privilegedMode: true` to preserve the original ownership exactly. |
| Restore stuck `Pending`, `MoverPermitted=False` | `Restore` condition / Event   | Restore mover requests privilege; namespace not opted in.  | `kubectl annotate namespace <ns> kopiur.home-operations.com/privileged-movers=true`, or drop the elevated context. |

## Quick reference

| Thing                            | Value                                                                               |
| -------------------------------- | ----------------------------------------------------------------------------------- |
| Default mover UID                | `65532` (distroless `nonroot`), `runAsNonRoot: true`; pod `fsGroup: 65532` so the kopia cache is writable on PVC-backed storage |
| Set the mover UID/GID            | `SnapshotPolicy.spec.mover.securityContext.runAsUser` / `runAsGroup` (same on `Restore.spec.mover` / `Maintenance.spec.mover`) |
| Inherit UID/GID from a workload  | `spec.mover.inheritSecurityContextFrom`: `pvcConsumer: {}` (backup: auto-derive from the PVC's consumer) or `workloadSelector` (by label; required on a Restore). Needs the workload to pin `runAsUser`; combines with `securityContext` (explicit wins). See [Security context](security-context.md#2-inherit-it-from-the-workload) |
| Mover cache size / warm cache    | `spec.mover.cache` (`capacity`, `storageClassName`, `mode: Ephemeral`/`Persistent`, `content`/`metadataCacheSizeMb`); inherits `Repository.spec.moverDefaults.cache` |
| `fsGroup`                        | `spec.mover.podSecurityContext.fsGroup`, which **defaults to `65532`** (keeps the cache writable); override to make a fresh restore volume writable by a mover running as a different UID |
| Root / preserve-ownership        | `runAsUser: 0` + `privilegedMode: true` (needs the namespace opt-in)                |
| Privileged-mover opt-in          | `kubectl annotate namespace <ns> kopiur.home-operations.com/privileged-movers=true` |
| Filesystem repo not writable     | Event prints `chown -R <uid> <path>` with the real operator UID                     |
| Restore ignore/permission errors | `Restore.spec.options.ignorePermissionErrors` (default `true`)                      |

## See also

- [Movers, RBAC & credentials](movers.md) covers privileged movers, the minted ServiceAccount, and credential placement.
- [Backend configuration](backends/index.md) covers the filesystem and SFTP backends, where ownership matters most.
- [Restores](restores.md) covers restore targets and options.
