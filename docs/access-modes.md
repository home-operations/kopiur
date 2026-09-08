# PVC access modes (RWO, RWX, RWOP)

Kopiur's movers are ordinary pods. A backup mover must **mount** the volume it reads, and a restore mover must mount the volume it writes.

Whether Kubernetes lets a *second* pod do that while your application is running is governed by the PVC's **access mode**. So the access mode, together with the [copy method](copy-methods.md), decides how a backup or restore can run alongside your app, and whether it can run at all.

Everything on this page is automatic. There is nothing to install or enable. The behavior comes from `moverDefaults.sourceColocation`, which defaults to [`Auto`](repositories.md#sourcecolocation-avoid-the-rwo-multi-attach-error), and you only touch that knob to opt out.

## Compatibility at a glance

| Access mode | `Direct` backup (mounts the **live** PVC) | `Snapshot` / `Clone` backup (mounts a **staged copy**) | Restore **into** the PVC |
| --- | --- | --- | --- |
| `ReadWriteMany` / `ReadOnlyMany` | ✅ mover schedules freely | ✅ | ✅ |
| `ReadWriteOnce` (RWO) | ✅ mover **co-locates** onto the attach node automatically | ✅ | ✅ co-locates automatically |
| `ReadWriteOncePod` (RWOP) | ⚠️ only while **no pod holds** the volume; a held volume fails fast with guidance | ✅ **works with no downtime** — recommended | ⚠️ only while no pod holds the volume |

## Try it end-to-end

Prove the headline RWOP claim, which is that you can back up a held `ReadWriteOncePod` volume with zero downtime.

It is one self-contained bundle, [`deploy/examples/tryit/access-modes.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/access-modes.yaml). It contains a namespace, a filesystem `Repository` on a PVC, an RWOP `app-data` PVC, a long-running `holder` Deployment that mounts it and writes a marker before sleeping, a `copyMethod: Snapshot` policy, and a fixed-name `Snapshot`.

Two pieces make the point. First, the source PVC is `ReadWriteOncePod`, so it is exclusive to a single pod cluster-wide:

```yaml
--8<-- "deploy/examples/tryit/access-modes.yaml:app-data"
```

Second, the policy uses `copyMethod: Snapshot`, so the mover reads a staged copy and never contends for that single-pod mount:

```yaml
--8<-- "deploy/examples/tryit/access-modes.yaml:policy"
```

/// warning | Prerequisite: CSI + the snapshot stack

RWOP is CSI-only, and `copyMethod: Snapshot` needs the [external-snapshotter](https://kubernetes-csi.github.io/docs/snapshot-controller.html) plus a `VolumeSnapshotClass` for your driver. A driver new enough for RWOP almost always ships snapshots too.

Fill in **both** `REPLACE_ME` values: `storageClassName`, which must be a CSI class, and `KOPIA_PASSWORD`.

///

**1. Apply and wait.** Note that you do *not* scale the holder down.

```console
$ kubectl apply -f deploy/examples/tryit/access-modes.yaml
$ kubectl -n kopiur-tryit rollout status deploy/holder --timeout=2m
$ kubectl -n kopiur-tryit wait --for=condition=Ready repository/primary --timeout=2m
```

**2. Confirm the holder owns the live volume before the backup.** It should be `Running` with `0` restarts:

```console
$ kubectl -n kopiur-tryit get pods -l app=holder
NAME                      READY   STATUS    RESTARTS   AGE
holder-7d9c8b6f4c-x2k9p   1/1     Running   0          30s
```

**3. Back it up without touching the holder, and read `status.staged` (deep).** The mover reads a staged copy, so the RWOP exclusivity is never violated:

```console
$ kubectl -n kopiur-tryit wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/app-data-snapshot --timeout=5m
$ kubectl -n kopiur-tryit get snapshot app-data-snapshot \
    -o jsonpath='{.status.staged.pvcName}'
app-data-snapshot-src
```

The backup mounted `app-data-snapshot-src`, the staged copy, and never the live `app-data`.

**4. Confirm the holder never flinched.** Same pod, still `Running`, still `0` restarts:

```console
$ kubectl -n kopiur-tryit get pods -l app=holder
NAME                      READY   STATUS    RESTARTS   AGE
holder-7d9c8b6f4c-x2k9p   1/1     Running   0          6m
```

/// note | Contrast: `Direct` fails fast on a held RWOP volume

Change the policy to `copyMethod: Direct` and re-run. The backup **fails immediately**, because a second pod, the mover, cannot mount an RWOP volume even on the same node. It fails with the actionable message shown in [Backing up an RWOP volume with `Direct`](#backing-up-an-rwop-volume-with-direct--only-while-nothing-holds-it) below.

That fast failure is the point: Kopiur will not leave a mover stuck `Pending` forever.

///

To tear down, run `kubectl delete namespace kopiur-tryit`.

## `ReadWriteMany` / `ReadOnlyMany` — nothing to think about

The volume can be attached to many nodes and mounted by many pods at once. The mover schedules wherever the cluster likes, alongside your running app. There is no pinning and no restriction.

## `ReadWriteOnce` — handled automatically

An RWO volume attaches to **one node at a time**, but any number of pods *on that node* may mount it.

Kopiur detects the node your app holds the volume on and pins the mover there, so backups and restores of in-use RWO PVCs just work. This is the default `sourceColocation.mode: Auto` behavior, and it avoids the Kubernetes Multi-Attach error. For the full detail, meaning the discovery order, the `Required` and `Disabled` modes, and the RBAC it needs, see [Repositories → `sourceColocation`](repositories.md#sourcecolocation-avoid-the-rwo-multi-attach-error).

## `ReadWriteOncePod` — exclusive to one pod, so pick the right copy method

`ReadWriteOncePod` (RWOP), which has been GA since Kubernetes 1.29 and works on CSI volumes only, hardens RWO's guarantee: the volume can be mounted by **a single pod cluster-wide**.

That single-pod exclusivity is exactly what makes it attractive for databases, and exactly what a backup tool has to plan around, because the mover *is* a second pod. Unlike RWO, **co-locating the mover on the same node cannot help**: the kubelet refuses the second mount even there.

Here is what Kopiur does about it, per situation.

### Backing up an RWOP volume with `Snapshot` or `Clone` — no downtime (recommended)

With [`copyMethod: Snapshot` or `Clone`](copy-methods.md), the mover **never mounts your live volume**.

Kopiur takes a CSI VolumeSnapshot, or a CSI clone, of the source. That is a storage-layer operation, which the RWOP mount exclusivity does not restrict. It then provisions a temporary staged PVC from it and runs kopia against that stage.

By default the staged PVC inherits your source's access modes, RWOP included, but the mover is its **only** pod, so the exclusivity is satisfied. Your app keeps running, untouched. `spec.staging.accessModes` can override the staged PVC's modes, for example to `[ReadOnlyMany]` for a snapshot-backed read-only class such as CephFS `backingSnapshot`; see [Copy methods → staging overrides](copy-methods.md#staging-overrides).

This is the recommended way to back up RWOP volumes, and you almost certainly already have what it needs: RWOP itself requires a CSI driver, and most CSI drivers that ship RWOP support also ship snapshots.

```yaml
--8<-- "deploy/examples/24-rwop-snapshot-backup.yaml"
```

/// note | `Clone` of an in-use volume is driver-dependent

CSI **snapshots** of attached volumes are universally supported. CSI **clones** of an attached volume are up to the driver, and some refuse and leave the staged PVC `Pending`. If that happens, prefer `copyMethod: Snapshot`.

///

### Backing up an RWOP volume with `Direct` — only while nothing holds it

`copyMethod: Direct` mounts the live PVC into the mover, so it can only work when **no pod currently holds the volume**. Then the mover is the sole pod, which RWOP permits, and Kopiur schedules it freely.

If a running pod *does* hold the volume, Kopiur does not leave a mover stuck `Pending` forever. The backup **fails immediately** with an actionable message:

```text
PVC `ns/data` is ReadWriteOncePod and is currently held by a running pod; a second
pod (the backup mover) cannot mount it even on the same node — scale the workload
down before backing it up, switch the PVC to ReadWriteMany, or set
moverDefaults.sourceColocation.mode=Disabled
```

Your options, best first:

1. **Switch to `copyMethod: Snapshot`**, described above. No downtime, point-in-time, and decoupled from the app.
2. **Scale the workload down** for the backup window, with `kubectl scale deploy/<app> --replicas=0`. With the volume released, `Direct` works. Scale back up afterwards.
3. **Change the PVC's access mode** to `ReadWriteOnce` if you do not actually need single-*pod* exclusivity. RWO still guarantees single-*node* attachment, and Kopiur co-locates the mover automatically.

### Restoring into an RWOP volume

A restore mover **writes into** the target PVC, so the same rule applies: the target must not be held by a running pod.

Restoring into a **freshly created** `target.pvc` always works, because the mover is the sole pod. Restoring into an **existing** RWOP PVC, meaning `target.pvcRef`, requires scaling the workload down first, which you generally want during a restore anyway so the app does not read or write data mid-rewrite. A held RWOP target fails fast with the same actionable message as above.

### Escape hatch: `sourceColocation.mode: Disabled`

Setting `moverDefaults.sourceColocation.mode: Disabled` skips the access-mode checks entirely, along with RWO node pinning, and schedules the mover with only your explicit `nodeSelector`, `affinity` and `tolerations`.

Use it only when you manage placement and volume hand-offs yourself, for example with an external system that releases the volume right before the backup window. If a pod still holds the RWOP volume when the mover starts, the mover pod sits `Pending` on a mount conflict instead of failing with guidance.

/// warning | RWOP failures are structural, not transient

A held-RWOP failure is reported as a validation failure, with the message above on the run's condition and on an Event, and it recurs on every run until **you** change something: scale the holder down, switch the copy method, or change the access mode. Kopiur will not retry its way out of it. See [Troubleshooting](troubleshooting.md#mover-pod-stuck-with-multi-attach-error-rwo-pvc).

///
