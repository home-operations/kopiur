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

/// note | `Snapshot` is the default

`copyMethod` defaults to `Snapshot` because it is **crash-consistent**: kopia reads a frozen point-in-time capture instead of a live, possibly-mid-write PVC — the difference matters most for databases and other stateful apps. It requires the CSI snapshot stack (external-snapshotter + a `VolumeSnapshotClass` for your source's driver). If your cluster doesn't have it, or the source is a static/non-CSI volume, set `copyMethod: Direct` explicitly. When the stack/class is missing and `copyMethod` was left at its default, the backup fails with a clear condition telling you exactly what to install or which field to set — it never silently falls back to a live read.

///

/// warning | Upgrading? Two hazards when `copyMethod` is left implicit

`copyMethod` began defaulting to `Snapshot` as of this release (previously `Direct`). Check any `SnapshotPolicy` that never sets `copyMethod` explicitly:

1. **No CSI snapshot stack for that source?** It now fails instead of silently reading the live PVC — pin `copyMethod: Direct` on that policy.
2. **Server-side re-defaulting**: a server-defaulted field has no field owner under server-side apply. Re-applying an *existing* manifest that omits `copyMethod` can silently flip a stored `Direct` value to `Snapshot` once the CRD is upgraded — pin `copyMethod` explicitly on every `SnapshotPolicy` you manage, especially ones reconciled by GitOps. This also applies to manifests previously produced by `kopiur-migrate`: translations run before this release omit `copyMethod` when the source VolSync object had none set, so they're exposed to this hazard too — re-run the migration (now emits an explicit value) or add `copyMethod: Direct` by hand before re-applying.

///

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
{"copyMethod":"Snapshot","volumeSnapshotName":"app-data-snapshot-...","pvcName":"app-data-snapshot-src","ready":true,"storageClassName":"...","stagingTimeoutSeconds":600}
```

*(Illustrative: `volumeSnapshotName` is generated and `storageClassName` is your class; the rest is exact.)*

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
2. Waits for the snapshot to become `readyToUse` — bounded by the [staging deadline](#how-long-staging-may-wait-specstagingtimeout) (`spec.staging.timeout`, default `10m`).
3. Provisions a temporary **staged PVC** from the snapshot.
4. Runs the kopia mover against the **staged PVC** — never the live volume.
5. **Cleans everything up** (staged PVC + VolumeSnapshot) when the backup finishes.

The staged PVC is brand-new and unheld, so the backup mover **schedules freely** — it is fully decoupled from the node your application runs on (unlike `Direct`, which must co-locate).

### What it requires

`Snapshot` needs the cluster's **CSI snapshot stack**, which your cluster administrator installs once:

- The **external-snapshotter** — the `snapshot-controller` Deployment **and** the `VolumeSnapshot`/`VolumeSnapshotContent`/`VolumeSnapshotClass` CRDs (see the [kubernetes-csi external-snapshotter docs](https://kubernetes-csi.github.io/docs/snapshot-controller.html)). Many managed distributions (EKS, GKE, AKS, Talos, k3s add-ons) ship or offer this.
- A **`VolumeSnapshotClass`** whose `driver` matches the CSI provisioner of your source PVC's `StorageClass`.

If your distribution does **not** bundle a `snapshot-controller`, the home-operations [`snapshot-controller`](https://github.com/home-operations/helm-charts) chart installs the controller and the snapshot CRDs (vendored byte-for-byte from the [upstream external-snapshotter](https://github.com/kubernetes-csi/external-snapshotter) release):

```bash
helm install snapshot-controller oci://ghcr.io/home-operations/charts/snapshot-controller \
  --namespace kube-system
```

The same chart can create the `VolumeSnapshotClass` for you: add an entry under its `volumeSnapshotClasses` value naming the `driver` for your source's storage (optionally annotated `snapshot.storage.kubernetes.io/is-default-class: "true"`), so the whole prerequisite lands in one install. Skip the chart on a distribution that already runs a controller (EKS, GKE, AKS, Talos, k3s add-ons); a second one just contends for the same CRs.

/// warning | Helm never upgrades the snapshot CRDs

Like Kopiur's own chart, this one ships the CRDs in its `crds/` directory: `helm install` creates them, but `helm upgrade` leaves them untouched. After bumping the chart across an appVersion, reapply the matching CRDs yourself:

```bash
helm show crds oci://ghcr.io/home-operations/charts/snapshot-controller | kubectl apply --server-side -f -
```

///

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

/// tip | Templating this field with Flux or Kustomize?

An **empty** value counts as unset, exactly like omitting the field. That makes the field safe to template unconditionally — a Kustomize component can always emit the line and let a Flux post-build substitution fill it in:

```yaml
volumeSnapshotClassName: ${KOPIUR_SNAPSHOTCLASS:=}
```

With the variable undefined this renders empty and Kopiur auto-selects the driver's default class; with it set, the named class is used. You do not need conditional templating to keep the field optional.

///

### How long staging may wait (`spec.staging.timeout`)

Staging has a **deadline budget** that bounds each of its phases:

1. The `VolumeSnapshot` becoming `readyToUse` — measured from the VolumeSnapshot's creation.
2. The **staged PVC binding** — a *fresh* budget measured from the staged PVC's creation. On an `Immediate`-binding StorageClass the CSI restore/clone runs at provision time and Kopiur waits for `Bound` **before** creating the mover Job (so a slow restore can never strand an unschedulable mover, and the VolumeSnapshot is never torn down under an in-flight restore). On a `WaitForFirstConsumer` class the bind happens when the mover pod schedules — Kopiur then keeps watching the staged PVC *while the Job runs* and fails the backup with the same reason if the bind blows the budget.

```yaml
spec:
    copyMethod: Snapshot
    staging:
        # Go-style duration; default 10m. "0" waits indefinitely.
        timeout: 30m
```

- **Default `10m`** — plenty for drivers that cut snapshots in seconds (most local/on-cluster CSI), and bounded so a broken driver can't hold a `Snapshot` `Pending` forever and silently starve a `concurrencyPolicy: Forbid` schedule.
- **Raise it** for backends whose snapshots or restores take long — e.g. cloud snapshots of large volumes (the first EBS snapshot of a big volume can take well over 10 minutes), or a **CephFS full-clone restore of a small-file-heavy volume** (see [staging overrides](#staging-overrides) for the shallow-clone alternative that makes it near-instant instead).
- **`timeout: "0"`** waits indefinitely (never fails on the deadline).

Only this deadline fails staging. If it expires the backup goes `Failed` with reason `VolumeSnapshotFailed` (the snapshot was still reporting an error), `StagingTimedOut` (no error — the driver/snapshot-controller is stuck), or `StagedPvcBindTimeout` (the snapshot was fine but the staged PVC never bound — the restore/clone is still provisioning or can't provision), and the message names this field. A `Failed` backup is terminal; the next scheduled run (or a new `Snapshot`) retries — and Kopiur reaps whatever staging objects the failed run already created.

/// note | A `VolumeSnapshot` error during the wait is NOT a failure

The snapshot-controller routinely reports **transient** errors on a perfectly healthy `VolumeSnapshot` — most commonly a benign `409 Conflict` (`"the object has been modified; please apply your changes to the latest version and try again"`) while it adds finalizers, which its own retry clears a moment later. Kopiur surfaces such errors on the `SourceStaged` condition for visibility but keeps waiting; it declares `VolumeSnapshotFailed` only if the snapshot is still not `readyToUse` when the staging deadline passes.

///

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

/// warning | Clone requires driver support

If your driver can't clone the volume, the staged PVC stays `Pending` and the backup fails with `StagedPvcBindTimeout` once the [staging deadline](#how-long-staging-may-wait-specstagingtimeout) passes. If you see that, check the staged PVC's events (`kubectl describe pvc <snapshot-name>-src`) and use `Snapshot` or `Direct` instead.

///

---

## Staging overrides

By default the staged PVC copies its `storageClassName` and `accessModes` **from the source PVC**. Two optional `spec.staging` fields override that — for the staged PVC only; your application's PVC is never touched:

```yaml
spec:
    copyMethod: Snapshot   # overrides apply to Snapshot AND Clone
    staging:
        storageClassName: cephfs-backingsnapshot  # class for the STAGED PVC
        accessModes: [ReadOnlyMany]                # modes for the STAGED PVC
```

- **`storageClassName`** — stage on a different class of the **same CSI driver**, typically one with different *restore parameters*. Kopiur verifies the driver matches up front and fails fast with `StagedClassMismatch` if it doesn't (a foreign driver can never provision from your source's snapshot — without the check you'd get an opaque bind timeout instead).
- **`accessModes`** — request different modes for the stage (e.g. `[ReadOnlyMany]` for a snapshot-backed read-only class). The mover mounts the staged source read-only unless the source sets [`readOnly: false`](#making-fsgroup-apply-to-the-source) — and `[ReadOnlyMany]` is rejected together with that, since a read-only stage cannot be mounted read-write.
- Both are meaningless without a staged PVC, so they're **rejected at admission** for `copyMethod: Direct`, NFS sources, and `pvcSelector` sources.

### The flagship use: CephFS shallow snapshots

On CephFS, restoring a VolumeSnapshot into a staged PVC is a **full subvolume clone** — an MDS-metadata-bound copy of every file. For a volume with many small files (a git server's loose objects, maildirs), that clone can take *many minutes* even at ~100 MB, blowing the staging budget on setup. ceph-csi's answer is a StorageClass with `backingSnapshot: "true"`: restore-from-snapshot then mounts the snapshot **shallowly** — metadata-only, read-only, ready in seconds. Point `spec.staging.storageClassName` at such a class and the whole problem disappears:

```yaml
--8<-- "deploy/examples/30-cephfs-shallow-snapshot.yaml"
```

/// note | Same driver, shallow-capable versions, delete order

The shallow class must use the **same provisioner** as your source's class (kopiur enforces this). CephFS shallow volumes need ceph-csi ≥ 3.7 and are read-only by design — request them `ReadOnlyMany`. ceph-csi reference-tracks the backing snapshot, and Kopiur always deletes the staged PVC **before** the VolumeSnapshot, so the cleanup order is safe for shallow mounts.

///

---

## `Direct` — read the live volume (opt-in)

`Direct` mounts your **live** source PVC into the mover, read-only, and kopia reads it in place. No snapshot, no clone, no extra storage — it works on **any** storage, including `local-path`/hostPath that has no snapshot support. Set `copyMethod: Direct` explicitly on the `SnapshotPolicy` to opt in — it is no longer the default.

Because the live volume is mounted, Kopiur **co-locates** the mover on the node already holding the PVC (for `ReadWriteOnce` volumes), avoiding the Kubernetes *Multi-Attach error*. See [Repositories → `sourceColocation`](repositories.md#sourcecolocation-avoid-the-rwo-multi-attach-error). A `ReadWriteOncePod` source is stricter: it can't be co-mounted by the mover **at all** while your app holds it, so use `Snapshot` (or `Clone`) for those — see [PVC access modes & RWOP](access-modes.md).

```yaml
--8<-- "deploy/examples/23-copy-method-direct.yaml"
```

`Direct` reads a **live filesystem**, so the backup is *crash-consistent* — fine for most file data, but for a busy database prefer `Snapshot`, or quiesce the app with hooks (below).

---

## Making `fsGroup` apply to the source

By default the mover mounts the source **read-only** — kopia only ever reads it. That default has one surprising consequence: **`fsGroup` does nothing on the source.**

`fsGroup` is not a passive grant. The kubelet implements it by *walking the volume and rewriting it*: `chgrp` every file to the group, `chmod g+rw`, setgid on directories. That rewrite is the whole mechanism — and **the kubelet skips it entirely on a read-only mount**. So a mover `podSecurityContext.fsGroup` (or `fsGroupChangePolicy`) has no effect on a backup source, no matter what you set it to.

That matters when the mover must run as a *specific* uid:gid for reasons of its own — a non-ID-squashed NFS repository export, say — and the source PVC's files are owned by someone else. Set `readOnly: false` on the source and the kubelet does the walk:

```yaml
spec:
  copyMethod: Snapshot        # the stage is what gets rewritten — see below
  mover:
    podSecurityContext:
      fsGroup: 1000
      fsGroupChangePolicy: OnRootMismatch
  sources:
    - pvc: { name: app-data }
      readOnly: false
```

**What gets rewritten depends entirely on `copyMethod`:**

| `copyMethod` | The mover mounts | `readOnly: false` rewrites |
| --- | --- | --- |
| `Snapshot` / `Clone` | a temporary **staged PVC** | the throwaway stage, deleted when the run ends. Your volume is never touched. |
| `Direct` | your **live PVC** | your production data — permanently, while the app runs. |

So under `Snapshot`/`Clone` this is free, and it is the combination to reach for:

```yaml
--8<-- "deploy/examples/31-source-fsgroup-normalize.yaml"
```

/// danger | `Direct` + `readOnly: false` rewrites your live data

With `Direct` there is no stage: the kubelet chgrp's **your running application's files** to the mover's `fsGroup` and makes them group-writable — permanently, on the next backup. Postgres and Redis both refuse to start on an over-permissive data directory; anything asserting on group ownership can break the same way. There is no undo.

Because that is not inferable from intent — you set one flag to fix a permission error, not to re-own your data — Kopiur rejects the combination at admission unless you say so explicitly with `acknowledgeLiveMutation: true`.

Prefer `copyMethod: Snapshot`/`Clone` if your storage supports it. `acknowledgeLiveMutation` is ignored anywhere it is not needed, so it is safe to leave in place if you switch back.

///

The acknowledged form, for storage with no CSI snapshot support:

```yaml
--8<-- "deploy/examples/32-source-writable-direct.yaml"
```

Two more rejections you may hit, both at admission:

- **`nfs` sources.** The kubelet does not apply `fsGroup` to in-tree NFS volumes *at all*, so `readOnly: false` cannot achieve anything there and only makes the export writable. Use `mover.podSecurityContext.supplementalGroups` / `mover.securityContext.runAsUser` matching the export's ownership, or remap IDs server-side. See [Security context](security-context.md).
- **`staging.accessModes: [ReadOnlyMany]`** (and read-only staged classes generally, like CephFS `backingSnapshot`). A read-only stage cannot be mounted read-write — the kubelet would fail the mount at backup time.

Even with all this correct, whether the kubelet *actually* performs the walk still depends on your CSI driver: `fsGroupPolicy: None` skips it, and the default `ReadWriteOnceWithFSType` skips RWX volumes. Kopiur therefore never claims `SecurityContextCompatible=True` on an `fsGroup` basis — it reports `Unknown` and lets the mover's readability preflight be the arbiter at runtime.

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
| `SourceStaged=False`, reason **`SnapshotStackMissing`** | No `VolumeSnapshotClass` API — the external-snapshotter isn't installed. | Install the snapshot-controller and a `VolumeSnapshotClass` ([What it requires](#what-it-requires) has the `snapshot-controller` chart command), or set `copyMethod: Direct`. |
| `SourceStaged=False`, reason **`NoVolumeSnapshotClass`** | No class matches your source PVC's driver, or several do with no single default. (An **empty** `volumeSnapshotClassName` is not a cause — it counts as unset.) | Create/annotate a `VolumeSnapshotClass` for the driver, set `volumeSnapshotClassName` explicitly, or use `Direct`. |
| `SourceStaged=False`, reason **`VolumeSnapshotFailed`** | The VolumeSnapshot was **still reporting an error when the staging deadline passed** (`spec.staging.timeout`, default `10m`) — transient errors during the wait are retried, never fatal on their own. | Read the message (it includes the driver's last error); fix the class/driver, or raise `spec.staging.timeout` if the backend is just slow. The next scheduled run (or a new `Snapshot`) retries. |
| `SourceStaged=False`, reason **`StagingTimedOut`** | The VolumeSnapshot never became `readyToUse` within the staging deadline and reported **no error** — the CSI driver / snapshot-controller is stuck or very slow. | Check the driver and the snapshot-controller; raise `spec.staging.timeout` (or set it to `"0"` to wait indefinitely) if the backend is just slow. |
| `SourceStaged=False`, reason **`SourceNotCSIProvisioned`** | The source PVC has no `StorageClass` (a static/hostPath volume) — nothing to snapshot. | Use a CSI-provisioned PVC, or `copyMethod: Direct`. |
| `SourceStaged=False`, reason **`StagedClassNotFound`** | `spec.staging.storageClassName` names a StorageClass that doesn't exist. | Create the class, point the override at an existing one, or remove the override to stage on the source's class. |
| `SourceStaged=False`, reason **`StagedClassMismatch`** | `spec.staging.storageClassName` is on a **different CSI driver** than the source — its provisioner can never restore/clone from your source, so the staged PVC would never bind. | Point the override at a class of the source's driver (the message names both), or remove it. |
| `SourceStaged=False`, reason **`WaitingForStagedPvcBind`** (`Pending`, transient) | The staged PVC is still binding — the CSI restore/clone from the source is provisioning. Normal for slow restores; bounded by the [staging deadline](#how-long-staging-may-wait-specstagingtimeout). | Nothing, usually — it either binds or fails at the deadline with the row below. |
| `SourceStaged=False` / phase `Failed`, reason **`StagedPvcBindTimeout`** | The staged PVC never reached `Bound` within `spec.staging.timeout` — the restore/clone is still provisioning (e.g. a CephFS **full clone** of a small-file-heavy volume) or the class can't provision it. | `kubectl describe pvc <name>-src` + the CSI provisioner logs; raise `spec.staging.timeout` if the copy is just slow, or (CephFS) stage on a [`backingSnapshot: "true"` shallow class](#the-flagship-use-cephfs-shallow-snapshots). |
| Phase `Failed`, reason **`StagedPvcLost`** | The staged PVC reports `Lost` — its bound PV disappeared mid-stage. | Check the CSI driver / PV lifecycle; the next scheduled run retries. |
| Backup stuck `Pending`, staged PVC `Pending` | `WaitForFirstConsumer` (normal — binds when the mover starts) **or** the driver can't clone (for `Clone`). | If it never binds, the backup fails at the staging deadline with `StagedPvcBindTimeout`; `kubectl describe pvc <name>-src` for the driver event; switch method if cloning is unsupported. |

See also [Troubleshooting → Multi-Attach](troubleshooting.md) for the `Direct`-mode co-location path.
