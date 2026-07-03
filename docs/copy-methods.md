# Copy methods: `Snapshot`, `Clone`, `Direct`

`SnapshotPolicy.spec.copyMethod` chooses **how Kopiur captures your data before kopia reads it**. Kopia always backs up files from a mounted volume — the copy method only decides *which* volume the backup mover mounts:

| Method | What the mover reads | Point-in-time? | Decoupled from the app's node? | Requires |
| --- | --- | --- | --- | --- |
| **`Snapshot`** _(default)_ | A temporary PVC restored from a CSI **VolumeSnapshot** of your source | ✅ yes | ✅ yes | CSI snapshot stack + a `VolumeSnapshotClass` |
| **`Clone`** | A temporary **CSI clone** of your source PVC | ✅ yes (at clone time) | ✅ yes | CSI driver with volume-clone support |
| **`Direct`** | Your **live** source PVC, read-only | ❌ no (crash-consistent live read) | ❌ no (co-located with the app) | Nothing — works on any storage |

## Which should I use?

```text
Do you have (or can you install) the CSI snapshot stack for this source?
│
├─ Yes  ─────────────────────────────────────────────►  Snapshot   (default, preferred)
│         (crash-consistent, point-in-time, decoupled from the app's node)
│
│         Only volume cloning, not snapshots?  ────►  Clone
│
└─ No — no CSI snapshot support, or a static/hostPath/non-CSI source  ─►  Direct
          (config, media, file shares; simplest; works everywhere; set explicitly)
```

- **Start with `Snapshot`** (the default) — crash-consistent, point-in-time, and decoupled from the node your app runs on. Best for databases and anything you don't want tied to app placement.
- **Set `copyMethod: Direct` explicitly** if you don't have (or don't want to maintain) the CSI snapshot stack, or the source is a static/non-CSI volume (hostPath, some NFS setups) — it works on any storage, no CSI required.
- **Use `Clone`** only if your driver does cloning but not snapshots (uncommon).

!!! note "`Snapshot` is the default"
    `copyMethod` defaults to `Snapshot` because it is **crash-consistent**: kopia reads a frozen point-in-time capture instead of a live, possibly-mid-write PVC — the difference matters most for databases and other stateful apps. It requires the CSI snapshot stack (external-snapshotter + a `VolumeSnapshotClass` for your source's driver). If your cluster doesn't have it, or the source is a static/non-CSI volume, set `copyMethod: Direct` explicitly. When the stack/class is missing and `copyMethod` was left at its default, the backup fails with a clear condition telling you exactly what to install or which field to set — it never silently falls back to a live read.

!!! warning "Upgrading? Two hazards when `copyMethod` is left implicit"
    `copyMethod` began defaulting to `Snapshot` as of this release (previously `Direct`). Check any `SnapshotPolicy` that never sets `copyMethod` explicitly:

    1. **No CSI snapshot stack for that source?** It now fails instead of silently reading the live PVC — pin `copyMethod: Direct` on that policy.
    2. **Server-side re-defaulting**: a server-defaulted field has no field owner under server-side apply. Re-applying an *existing* manifest that omits `copyMethod` can silently flip a stored `Direct` value to `Snapshot` once the CRD is upgraded — pin `copyMethod` explicitly on every `SnapshotPolicy` you manage, especially ones reconciled by GitOps. This also applies to manifests previously produced by `kopiur-migrate`: translations run before this release omit `copyMethod` when the source VolSync object had none set, so they're exposed to this hazard too — re-run the migration (now emits an explicit value) or add `copyMethod: Direct` by hand before re-applying.

---

## Try it end-to-end

See `copyMethod: Snapshot` actually stage a CSI copy: the bundle [`deploy/examples/tryit/copy-methods.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/copy-methods.yaml) is self-contained — namespace, a filesystem `Repository` on a PVC, a CSI-provisioned `app-data` PVC, a seed Job, a `copyMethod: Snapshot` policy, and a **fixed-name** `Snapshot` (`app-data-snapshot`) so the staged PVC is deterministically `app-data-snapshot-src`.

The load-bearing line is `copyMethod: Snapshot` on the policy — it tells the mover to read a staged CSI copy instead of the live volume:

```yaml
--8<-- "deploy/examples/tryit/copy-methods.yaml:policy"
```

/// warning | Prerequisite: this bundle needs the CSI snapshot stack

`copyMethod: Snapshot` snapshots the source PVC, so the source must be **CSI-provisioned** and the cluster must have the [external-snapshotter](https://kubernetes-csi.github.io/docs/snapshot-controller.html) (the `snapshot-controller` + `VolumeSnapshot`/`VolumeSnapshotContent`/`VolumeSnapshotClass` CRDs) and a `VolumeSnapshotClass` whose `driver` matches your source's `StorageClass` provisioner. Fill in **both** `REPLACE_ME` values: `storageClassName` (a CSI class that supports snapshots) and `KOPIA_PASSWORD`. Leave `volumeSnapshotClassName` unset to auto-pick the driver's default class.

///

**1. Apply and wait for the prerequisites.**

```console
$ kubectl apply -f deploy/examples/tryit/copy-methods.yaml
$ kubectl -n kopiur-tryit wait --for=condition=complete job/seed-data --timeout=2m
$ kubectl -n kopiur-tryit wait --for=condition=Ready repository/primary --timeout=2m
```

**2. While the backup is `Running`, catch the staged PVC.** The mover reads `app-data-snapshot-src`, *not* the live `app-data`. Fetch it **by name** (not a label) while the snapshot is in flight:

```console
$ kubectl -n kopiur-tryit get snapshot app-data-snapshot -w &
$ kubectl -n kopiur-tryit get pvc app-data-snapshot-src
NAME                    STATUS   VOLUME   CAPACITY   ACCESS MODES   AGE
app-data-snapshot-src   Bound    pvc-..   1Gi        RWO            6s
```

**3. After success, read `status.staged` (deep).** Wait for the terminal phase, then confirm the run staged a CSI copy and which PVC the mover mounted:

```console
$ kubectl -n kopiur-tryit wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/app-data-snapshot --timeout=5m
$ kubectl -n kopiur-tryit get snapshot app-data-snapshot \
    -o jsonpath='{.status.staged}'
{"copyMethod":"Snapshot","volumeSnapshotName":"app-data-snapshot-...","pvcName":"app-data-snapshot-src","ready":true}
```

*(Illustrative: `volumeSnapshotName` is generated; the rest is exact.)*

**4. Confirm the stage was reaped and the live PVC was never mounted.** Kopiur cleans up the staged objects on completion:

```console
$ kubectl -n kopiur-tryit get pvc app-data-snapshot-src
Error from server (NotFound): persistentvolumeclaims "app-data-snapshot-src" not found
```

The live `app-data` PVC was never mounted by the mover — only the staged copy was. To tear down: `kubectl delete namespace kopiur-tryit`.

---

## `Snapshot` — point-in-time CSI snapshot (default)

When a backup runs, Kopiur:

1. Creates a CSI **`VolumeSnapshot`** of your source PVC (after any `beforeSnapshot` hooks, so a quiesced app yields a consistent capture).
2. Waits for the snapshot to become `readyToUse`.
3. Provisions a temporary **staged PVC** from the snapshot.
4. Runs the kopia mover against the **staged PVC** — never the live volume.
5. **Cleans everything up** (staged PVC + VolumeSnapshot) when the backup finishes.

The staged PVC is brand-new and unheld, so the backup mover **schedules freely** — it is fully decoupled from the node your application runs on (unlike `Direct`, which must co-locate).

### What it requires

`Snapshot` needs the cluster's **CSI snapshot stack**, which your cluster administrator installs once:

- The **external-snapshotter** — the `snapshot-controller` Deployment **and** the `VolumeSnapshot`/`VolumeSnapshotContent`/`VolumeSnapshotClass` CRDs (see the [kubernetes-csi external-snapshotter docs](https://kubernetes-csi.github.io/docs/snapshot-controller.html)). Many managed distributions (EKS, GKE, AKS, Talos, k3s add-ons) ship or offer this.
- A **`VolumeSnapshotClass`** whose `driver` matches the CSI provisioner of your source PVC's `StorageClass`.

If any of this is missing, the backup **fails with a clear condition** telling you exactly what to do — Kopiur never silently downgrades a `Snapshot` backup to a live read. See [Troubleshooting](#troubleshooting) below.

### Choosing the `VolumeSnapshotClass`

```yaml
spec:
    copyMethod: Snapshot
    # Optional. Leave unset to auto-select your driver's DEFAULT class.
    volumeSnapshotClassName: csi-rbd-snapclass
```

- **Set it explicitly** to pin a specific class.
- **Leave it unset** and Kopiur picks the **default `VolumeSnapshotClass` for your source's driver** (the one annotated `snapshot.storage.kubernetes.io/is-default-class: "true"`). If exactly one class exists for the driver it's used even without the annotation.
- If **no** class matches your driver, or **several** match with no single default, the backup fails asking you to create/annotate a class or name one explicitly.

```yaml
--8<-- "deploy/examples/21-copy-method-snapshot.yaml"
```

---

## `Clone` — CSI volume clone

`Clone` provisions the staged PVC directly from your source PVC (`dataSource: PersistentVolumeClaim`) — a CSI **volume clone** — with no intermediate VolumeSnapshot. Like `Snapshot`, the mover reads the clone and the clone is cleaned up afterward.

Use it when your CSI driver supports cloning (`CLONE_VOLUME`) but not snapshots. It needs no `VolumeSnapshotClass`.

```yaml
--8<-- "deploy/examples/22-copy-method-clone.yaml"
```

!!! warning "Clone requires driver support"
    If your driver can't clone the volume, the staged PVC stays `Pending` and the backup never starts. If you see that, check the staged PVC's events (`kubectl describe pvc <snapshot-name>-src`) and use `Snapshot` or `Direct` instead.

---

## `Direct` — read the live volume (opt-in)

`Direct` mounts your **live** source PVC into the mover, read-only, and kopia reads it in place. No snapshot, no clone, no extra storage — it works on **any** storage, including `local-path`/hostPath that has no snapshot support. Set `copyMethod: Direct` explicitly on the `SnapshotPolicy` to opt in — it is no longer the default.

Because the live volume is mounted, Kopiur **co-locates** the mover on the node already holding the PVC (for `ReadWriteOnce` volumes), avoiding the Kubernetes *Multi-Attach error*. See [Repositories → `sourceColocation`](repositories.md#sourcecolocation-avoid-the-rwo-multi-attach-error). A `ReadWriteOncePod` source is stricter: it can't be co-mounted by the mover **at all** while your app holds it, so use `Snapshot` (or `Clone`) for those — see [PVC access modes & RWOP](access-modes.md).

```yaml
--8<-- "deploy/examples/23-copy-method-direct.yaml"
```

`Direct` reads a **live filesystem**, so the backup is *crash-consistent* — fine for most file data, but for a busy database prefer `Snapshot`, or quiesce the app with hooks (below).

---

## Consistency: what each method guarantees

- `Snapshot` / `Clone` capture a **point-in-time** image at the block level — *crash-consistent* (like a power-cut: the filesystem is intact, in-flight writes may not be flushed).
- `Direct` reads files **while the app may be writing** — also crash-consistent, but spread across the read rather than a single instant.

For **application consistency** (a database flushed and quiesced), use `SnapshotPolicy.spec.hooks` to quiesce before the capture and resume after. With `Snapshot`, the VolumeSnapshot is taken **after** your `beforeSnapshot` hooks, so a `FLUSH`/`fsfreeze` hook yields a consistent snapshot. See [Backups → hooks](backups.md).

## Cleanup & cost

Kopiur reaps the staged PVC and VolumeSnapshot when the backup reaches a terminal state (and again if you delete the `Snapshot`). To avoid the well-known leak where a **`Retain`** StorageClass leaves the staged PV (and its backend volume) behind, Kopiur flips a bound staged PV's reclaim policy to `Delete` before removing it. A `Retain` **`VolumeSnapshotClass`** keeps the *underlying* storage snapshot after the VolumeSnapshot object is deleted — prefer a `Delete` deletion policy for the class you point Kopiur at, unless you want to keep raw storage snapshots yourself.

`status.staged` on the `Snapshot` records what was created (the VolumeSnapshot + staged PVC names) for visibility.

## Troubleshooting

If a `SnapshotPolicy` never sets `copyMethod` and the cluster has no CSI snapshot stack, the backup fails **immediately** on the (default) `Snapshot` attempt — most often with `SnapshotStackMissing` below. The fix in every row is the same shape: install what's missing, **or** pin `copyMethod: Direct` on the policy to opt out of CSI staging.

| Condition / symptom | Cause | Fix |
| --- | --- | --- |
| `SourceStaged=False`, reason **`SnapshotStackMissing`** | No `VolumeSnapshotClass` API — the external-snapshotter isn't installed. | Install the [snapshot-controller + CRDs](https://kubernetes-csi.github.io/docs/snapshot-controller.html) and a `VolumeSnapshotClass`, or set `copyMethod: Direct`. |
| `SourceStaged=False`, reason **`NoVolumeSnapshotClass`** | No class matches your source PVC's driver (or several do with no single default). | Create/annotate a `VolumeSnapshotClass` for the driver, set `volumeSnapshotClassName` explicitly, or use `Direct`. |
| `SourceStaged=False`, reason **`VolumeSnapshotFailed`** | The CSI driver reported an error creating the snapshot. | Read the message (it includes the driver's error); check the class/driver, then re-create the `Snapshot`. |
| `SourceStaged=False`, reason **`SourceNotCSIProvisioned`** | The source PVC has no `StorageClass` (a static/hostPath volume) — nothing to snapshot. | Use a CSI-provisioned PVC, or `copyMethod: Direct`. |
| Backup stuck `Pending`, staged PVC `Pending` | `WaitForFirstConsumer` (normal — binds when the mover starts) **or** the driver can't clone (for `Clone`). | If it never binds, `kubectl describe pvc <name>-src` for the driver event; switch method if cloning is unsupported. |

See also [Troubleshooting → Multi-Attach](troubleshooting.md) for the `Direct`-mode co-location path.
