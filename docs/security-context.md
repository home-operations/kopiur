# The mover security context

Every backup and restore in Kopiur runs in a short-lived **mover** pod, and that pod's **security context** decides which files it can read and write. This page explains what the security context is, the fields that matter, the three ways to set it, how to work out the right values, and how to handle the awkward cases (mixed ownership, RWX volumes, preserving ownership on restore, restricted namespaces).

/// tip | The mental model: the mover is a separate pod

A backup or restore does **not** run inside your app's pod. Kopiur launches a short-lived **mover** Job that mounts the PVC and runs kopia. Linux file permissions don't care that it's "your" data — they only see the **UID/GID the mover process runs as**. The security context is how you control that identity.

- **Backup** — the mover must be able to **read** every file in the source.
- **Restore** — the mover must be able to **write** into the target (and, ideally, land files owned correctly for the app).

///

## What "security context" means here

A Kubernetes [`SecurityContext`](https://kubernetes.io/docs/tasks/configure-pod-container/security-context/) is the block that sets a container's Linux identity and privileges: the UID/GID it runs as, whether it may escalate, which capabilities it holds, its seccomp profile, and so on. Kopiur exposes the standard, unmodified `core/v1` `SecurityContext` on every kind that runs a mover:

| Kind | Field |
| --- | --- |
| `SnapshotPolicy` | `spec.mover.securityContext` |
| `Restore` | `spec.mover.securityContext` |
| `Maintenance` | `spec.mover.securityContext` |

`spec.mover.securityContext` is applied at the **container** level (on the mover container). For pod-level settings — most importantly **`fsGroup`** — there's a separate sibling field:

| Kind | Container-level | Pod-level |
| --- | --- | --- |
| `SnapshotPolicy` | `spec.mover.securityContext` | `spec.mover.podSecurityContext` |
| `Restore` | `spec.mover.securityContext` | `spec.mover.podSecurityContext` |
| `Maintenance` | `spec.mover.securityContext` | `spec.mover.podSecurityContext` |

/// tip | `fsGroup` for a freshly-provisioned restore volume

`fsGroup` is a **pod-level** setting (`PodSecurityContext`). Set it via `spec.mover.podSecurityContext.fsGroup` (the same `PodSecurityContext` you'd put on any pod). On mount, the kubelet makes the volume group-owned by that GID and group-writable, and adds it to the mover's supplementary groups — so an **unprivileged** mover (`runAsUser: 1000`) can populate a **freshly-provisioned** volume on restore (whose mount point is otherwise root-owned `0755`) **without** a root mover.

It already **defaults to `65532`** (the mover image's GID), so the *default* mover writes a fresh volume with no extra config. You only set `fsGroup` here when the mover runs as a **different** UID/GID (below) — match it to that identity:

```yaml
spec:
    mover:
        securityContext: { runAsUser: 1000, runAsNonRoot: true } # container: who writes
        podSecurityContext: # pod: make the volume writable
            fsGroup: 1000
            fsGroupChangePolicy: OnRootMismatch # skip the recursive chown when already correct
```

A pod-level `runAsUser: 0` / `runAsNonRoot: false` here is still treated as a **privileged** mover (it needs the namespace opt-in); `fsGroup` itself is not elevation.

///

## The default (hardened) context

If you set nothing, the mover runs **unprivileged** as the mover image's user — **UID `65532`** (distroless `nonroot`) — with a hardened context:

```yaml
securityContext:
  runAsNonRoot: true
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: false
  capabilities:
    drop: ["ALL"]
  seccompProfile:
    type: RuntimeDefault
```

This default is compatible with the Pod Security Admission **`restricted`** profile, so it runs in locked-down namespaces out of the box. What it can **read** is limited: files that are world-readable, or owned by UID `65532`. Most app images run as some other UID (`1000`, `1001`, `999`, …) and write files `0600`/`0640`, so the default mover gets **permission denied** on real app data. Whenever the source isn't world-readable, you'll set the context to match the data — read on.

At the **pod** level, every mover (backup, restore, maintenance, **and** the bootstrap connect/create Job) also gets a hardened default `podSecurityContext`:

```yaml
podSecurityContext:
  fsGroup: 65532 # the mover image's GID — makes mounted volumes group-writable by the mover
  fsGroupChangePolicy: OnRootMismatch # only chown when the volume root doesn't already match
```

The `fsGroup` matches the mover image's GID so the operator-managed **kopia cache** is writable out of the box. Without it, a PVC-backed cache (`moverDefaults.cache.mode: Ephemeral`/`Persistent`) is created `root:root` and the unprivileged mover fails with `mkdir /var/cache/kopia/logs: permission denied`. `OnRootMismatch` keeps it cheap — a volume already owned by the group isn't re-chowned on every run. Because this is the lowest merge layer, `moverDefaults.podSecurityContext` and a recipe's `mover.podSecurityContext` override it field-wise (e.g. set `fsGroup` to your app's GID for a restore; the rest of the hardened defaults stay put).

/// warning | `fsGroup` has no effect on NFS
`fsGroup` works by having the **kubelet** chown the volume on mount. The kubelet **skips that chown entirely for in-tree `nfs:` volumes** (and many NFS-backed CSI drivers) — so `fsGroup` is silently a **no-op on NFS**, not just on root-squashed exports. Two consequences:

- A kopia **cache** on an NFS StorageClass stays `root:root` and the mover gets `permission denied`. A content-addressed scratch cache has no business on networked storage anyway — leave `moverDefaults.cache` unset (the default is a node-local `emptyDir`, always writable) or point `cache.storageClass` at a block class (e.g. Ceph RBD) that honors `fsGroup`.
- An **inline-NFS filesystem repository** can't be made writable with `fsGroup`. Use `supplementalGroups` against a group-writable export, `runAsUser` matching the export owner, or remap server-side — see [NFS filesystem repositories](#nfs-filesystem-repositories) below. The admission webhook **warns** when an NFS filesystem repo relies only on `fsGroup`.
///

## How to decide what to set

The whole problem reduces to one number (sometimes two): the **numeric** UID/GID that owns the data.

1. **Find the owner.** Read it from the running app (`kubectl exec … -- id` / `ls -ln`) or, if nothing mounts the PVC, from a throwaway inspection pod. The step-by-step recipe — including the **lowest-common-denominator** rule (if any file you need is `0600` owned by `1000`, the mover must be UID `1000`; if everything is at least group-readable and shares a GID, matching the GID is enough) — lives in the [Permissions guide → Find the UID/GID](permissions.md#step-1--find-the-uidgid-that-owns-your-data).
2. **Decide backup vs restore intent:**
    - **Backup** — pick a UID/GID that can **read** the source.
    - **Restore** — pick the UID/GID that should **own** the restored files, so the app can read them afterward. (To reproduce the *original* ownership exactly, see [Preserving ownership on restore](#preserving-original-ownership-on-restore).)
3. **Choose how to express it** — one of the three approaches below.

## Three ways to set the context

### 1. Set it explicitly

The most direct: hard-code the UID/GID under `spec.mover.securityContext`, keeping the rest of the hardened context.

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

Full, apply-ready example:

```yaml
--8<-- "deploy/examples/09-mover-permissions.yaml"
```

### 2. Inherit it from the workload

If you'd rather "run as **whatever the app runs as**" than track a UID, `inheritSecurityContextFrom` copies the security context from a live workload pod onto the mover — **both** the container `securityContext` (UID/GID) **and** the pod-level `securityContext` (e.g. `fsGroup`). This is the answer to *"back up / restore as the pod that mounts this PVC,"* at both levels. It is an **externally-tagged choice** — pick exactly one form:

#### `pvcConsumer` — auto-derive from the source PVC (backup, recommended)

On a **backup**, Kopiur can find the pod that mounts the source PVC for you and inherit its security context — no selector to write or keep in sync:

```yaml
spec:
  mover:
    inheritSecurityContextFrom:
      pvcConsumer: {} # optionally: pvcConsumer: { container: app }
```

The controller lists pods in the source namespace, finds the one mounting this snapshot's source PVC (excluding Kopiur's own mover pods), prefers a **Running** one, and copies its container + pod `securityContext` onto the mover. If no workload pod currently mounts the PVC (e.g. it's scaled to zero), the Backup is **held** with an actionable condition — scale the workload up. If you'd rather it kept running, give the mover an explicit `securityContext` that pins a `runAsUser`: that becomes the fallback, and the run proceeds on it with a `SecurityContextInherited=False` / `InheritFallback` condition instead of being held (see [Combining inherit with an explicit context](#combining-inherit-with-an-explicit-context)).

/// warning | Your workload must pin `runAsUser` for this to do anything

Inheriting copies the workload's **pod spec** fields. It cannot see the UID baked into the workload's *image* (its `USER` line). If the workload pins no `runAsUser` at either the container or the pod level — a `securityContext` block that only sets `allowPrivilegeEscalation`/`capabilities` counts as pinning nothing — then there is **no UID to inherit**, and the mover falls back to its own image's UID `65532` — after which the backup typically fails with `permission denied`. Kopiur reports this as `SecurityContextInherited=False` / `InheritPinnedNoUid` plus a Warning Event naming the pod, rather than letting the run look correctly configured.

Check before relying on it:

```console
$ kubectl -n app get pod <consumer> \
    -o jsonpath='{.spec.securityContext}{"\n"}{range .spec.containers[*]}{.name}{" "}{.securityContext}{"\n"}{end}'
```

If no `runAsUser` appears, set one on the workload, or set `mover.securityContext.runAsUser` to the image's UID (the two combine — see below).

///

`pvcConsumer` is **backup-only**: a Restore writes a *target* PVC whose consumer may not exist yet, so use `workloadSelector` (below) there. (The admission webhook rejects `pvcConsumer` on a Restore.)

#### `workloadSelector` — name the workload by label (backup or restore)

```yaml
spec:
  mover:
    inheritSecurityContextFrom:
      workloadSelector:
        podSelector:
          matchLabels:
            app.kubernetes.io/name: app # the workload that owns the PVC
        container: app # optional; defaults to the pod's first container
```

Use this on a **Restore** (inherit from the pod that will *read* the restored data), or on a backup when you'd rather pin the selection explicitly. To find the right labels, list the pods that mount the claim:

```console
$ kubectl get pods -n app -o json \
    | jq -r '.items[]
        | select(.spec.volumes[]?.persistentVolumeClaim.claimName=="app-data")
        | .metadata.name'
app-7c9d8f5b6-h2k4p

$ kubectl get pod app-7c9d8f5b6-h2k4p -n app --show-labels
```

How it resolves: the controller lists pods matching the selector, prefers a **Running** one, picks the named container (or the pod's first), and copies **that container's `securityContext` and the pod's pod-level `securityContext`** onto the mover. If no pod matches, the selector is empty, the named container is absent, or the pod sets *neither* a container nor a pod-level `securityContext`, the Backup/Restore is held with an actionable `MissingDependency`-style condition telling you exactly what to fix — unless the recipe also sets a `mover.securityContext` pinning a `runAsUser`, which is then used as the fallback (`InheritFallback`) and the run proceeds. The matched workload must be **running** so its identity can be read.

Things to remember:

- **Inheriting a *root* workload is still elevated.** The *resolved* contexts are what's evaluated — container **and** pod — so inheriting from a pod that runs as root (or with `runAsUser: 0` at either level, or added capabilities) trips the [privileged-mover gate](#privileged-and-root-movers) exactly like an explicit root context would.
- **Inheriting root just works — you don't hand-set `runAsNonRoot`.** When the workload runs as `runAsUser: 0`, Kopiur produces a *valid* root mover for you (it reconciles `runAsNonRoot` to `false`, since `runAsNonRoot: true` + `runAsUser: 0` is a contradiction the kubelet rejects with `CreateContainerConfigError`). So you only opt the namespace into [privileged movers](#privileged-and-root-movers); you never need to add `runAsNonRoot: false` to an `inheritSecurityContextFrom` recipe.

#### Combining inherit with an explicit context

`inheritSecurityContextFrom` and `securityContext`/`podSecurityContext` are **layers, not alternatives** — you can set both. The full merge order is:

```
hardened  ⊂  moverDefaults  ⊂  inherited  ⊂  mover.securityContext
```

So **what you write always wins**, inheritance fills in whatever the workload pins that you left blank, and your context stands in **alone** when inheritance can't resolve a pod. That last property makes it the natural fallback:

```yaml
spec:
  mover:
    inheritSecurityContextFrom:
      pvcConsumer: {}
    securityContext:
      runAsUser: 1000 # used when the workload is scaled to zero — and it OVERRIDES
      # the inherited UID whenever it resolves, because explicit wins
```

/// warning | An explicit field is an override, not a default

Because explicit wins, `runAsUser: 1000` above pins the mover to `1000` **always** — inheritance never gets a say on that field, even when the workload is running as something else. There is deliberately no way to express "prefer the workload's UID, else `1000`": one field, one rule.

If that's not what you meant, leave `runAsUser` out and let inherit supply it. Kopiur raises `SecurityContextInherited=False` / `InheritOverridden` naming both UIDs when your explicit UID displaces a resolved one, so this can't silently drift.

///

#### The `SecurityContextInherited` condition

`SecurityContextCompatible` answers *"can the mover read the source?"*. `SecurityContextInherited` answers a different question — *"did inheriting do what you think it did?"* — and appears **only** when the answer is "not quite". No condition means inheritance resolved a workload and its values stuck.

| `reason` | What happened | What to do |
| --- | --- | --- |
| `InheritFallback` | No workload pod resolved (scaled to zero, mid-rollout, selector matches nothing). Your explicit context stood in, so the run proceeded rather than being held. | Nothing, if that's the intent. Otherwise scale the workload up. |
| `InheritPinnedNoUid` | A pod resolved, but pins no `runAsUser`/`runAsGroup`/`fsGroup`/`supplementalGroups` — its identity is in its image. Inheriting copied nothing; the mover runs as `65532`. | Set `runAsUser` on the workload, or set `mover.securityContext.runAsUser`. |
| `InheritOverridden` | Inheritance resolved a UID, but your explicit `runAsUser` (or `moverDefaults`) overrode it. Correct by design — but inherit is a no-op for that field and won't follow the workload. | Drop the explicit `runAsUser` to track the workload, or drop `inheritSecurityContextFrom`. |

```console
$ kubectl get snapshot pg-backup -o jsonpath='{.status.conditions[?(@.type=="SecurityContextInherited")]}'
```

Each also emits one Warning Event, fired on the status transition rather than every reconcile.

Two consequences worth internalizing:

- **Only `runAsUser` de-escalates an inherited root UID.** Setting `runAsNonRoot: true` against an inherited `runAsUser: 0` does *not* produce a non-root mover — the kubelet rejects that pair, so Kopiur normalizes it to a (gated) root mover. Override `runAsUser` itself.
- **Partial overrides only tighten.** The hardened base still supplies `drop: [ALL]` and the seccomp profile; setting one field never drops the rest.

Full, apply-ready example (SnapshotPolicy + the same knob on a `Restore`):

```yaml
--8<-- "deploy/examples/18-inherit-security-context.yaml"
```

The pattern that composes best is overriding a *group* while inheriting the *identity* — `supplementalGroups` is additive, so it doesn't fight inherit the way a `runAsUser` override does:

```yaml
--8<-- "deploy/examples/18-inherit-security-context.yaml:merged"
```

### 3. Go root (privileged mover)

When the data is owned by **assorted UIDs you can't match** (a `lost+found`, a multi-user volume, an app that writes as root), a root mover reads everything — and on restore it can reproduce original ownership. It is **elevated** and gated (see below):

```yaml
spec:
  mover:
    securityContext:
      runAsUser: 0
      runAsNonRoot: false
    privilegedMode: true # also preserves UID/GID ownership on RESTORE
```

/// tip | Prefer matching the UID over going root

A root mover widens the blast radius of the minted mover ServiceAccount. Reach for it only when you genuinely can't match the owning UID/GID. Most single-app PVCs back up fine as their app's UID.

///

## Catching permission mismatches early

A mover that can't read the data it's backing up is the classic footgun. By default kopia treats an unreadable file or directory as **fatal** — the backup fails loudly with `permission denied` (classified `PermissionDenied`), so nothing is silent. The dangerous case is when you've set an `ignoreFileErrors`/`ignoreDirErrors` policy: kopia then **completes** the snapshot while *skipping* the files it couldn't read, leaving you with a silently *incomplete* backup. Kopiur surfaces all of this:

1. **At `kubectl apply` (admission warning).** When a SnapshotPolicy's `source.pvc` is mounted by a workload whose UID the mover's explicit `runAsUser` clearly can't match (no shared UID or group), the webhook attaches a non-blocking **warning** to the apply. Best-effort — it can't see file modes, and the workload may not be running yet.

2. **On reconcile (status condition).** A Backup's `SecurityContextCompatible` condition is **positive-only and certain** — it's never a guess:
   - `True` — provably fine, checked against the *resolved* mover identity and the live workloads mounting the source: either the mover is root, or its UID exactly matches **every** container that writes the source (init containers included). Using `inheritSecurityContextFrom` is **not** itself a basis — inheriting only helps if the workload actually pins a `runAsUser`, and the condition says so only once it has confirmed the UIDs match.
   - `False` — set **only** by the certain post-run signal (#3): the completed backup actually excluded unreadable entries. It is never set from an up-front heuristic, so a successful backup of world-readable data is never falsely flagged.
   - **Absent** — the common case: not provable from the spec alone (nobody pinned a UID, several UIDs write the volume, …). Absence is not a warning; it means "no claim", and the run proceeds.

   ```console
   $ kubectl get snapshot pg-backup -o jsonpath='{.status.conditions[?(@.type=="SecurityContextCompatible")]}'
   ```

   A Restore carries the analogous `RestoreSecurityContextCompatible` condition, which is positive-only (`True` when the future consumer can read what the mover writes — matching UID or a shared `fsGroup`). A restore has no certain runtime signal, so its advisory negative lives entirely in the apply-time admission warning.

3. **From kopia's own output (the authoritative signal).** The mover doesn't re-walk the tree — kopia already reports exactly which entries it skipped. When a backup **completes with excluded entries** (the ignore-errors case), the mover records the count on `status.stats.filesFailed`, and the controller raises `SecurityContextCompatible=False` + a Warning **Event** naming the count and the fix. A *fatal* permission error needs no special handling — kopia exits non-zero and the run already fails as `PermissionDenied`.

   ```console
   $ kubectl get snapshot pg-backup -o jsonpath='{.status.stats.filesFailed}'
   $ kubectl get events --field-selector involvedObject.name=pg-backup
   ```

The fix in every case is the same: match the mover to the workload — the easiest being `inheritSecurityContextFrom.pvcConsumer: {}` (above), or a matching `runAsUser`/`fsGroup`.

## Privileged and root movers

Anything that makes the mover's **effective** context elevated requires a per-namespace admin opt-in. The "elevated" detector trips on any of:

- `runAsUser: 0` (root)
- `privileged: true`
- `allowPrivilegeEscalation: true`
- added Linux `capabilities`
- `runAsNonRoot: false`
- `privilegedMode: true`

…and it evaluates the **resolved** context, so an inherited-from-root mover counts too. If the namespace hasn't opted in, the Backup/Restore is refused with a clear `MoverPermitted=False` condition and a Warning Event naming the fix. Opt the namespace in by applying a `Namespace` carrying the opt-in annotation:

```yaml
--8<-- "deploy/examples/privileged-mover-namespace.yaml"
```

```console
$ kubectl apply -f privileged-mover-namespace.yaml
```

…or imperatively: `kubectl annotate namespace <ns> kopiur.home-operations.com/privileged-movers=true`.

The operator watches namespaces, so the annotation takes effect within seconds —
the blocked Snapshot/Restore proceeds without being re-applied.

Why the gate exists, and the revoke path, are covered in [Movers → Privileged movers](movers.md#privileged-movers). The rationale mirrors VolSync's `privileged-movers` model: the operator mints a mover ServiceAccount in the workload namespace, and a tenant there could otherwise reuse it at the mover's privilege.

## Complex circumstances

### Preserving original ownership on restore

kopia records each file's original UID/GID in the snapshot. An **unprivileged** restore mover writes everything owned by its own UID instead — fine when one UID owns everything, wrong for multi-user data. To restore files with their **original** ownership, the mover must be able to `chown` to arbitrary UIDs, which needs root:

```yaml
spec:
  mover:
    securityContext: { runAsUser: 0, runAsNonRoot: false }
    privilegedMode: true
```

This is the same elevation the gate covers, so the restore namespace must opt in. There's an inherent trade: *"preserve arbitrary ownership exactly"* and *"run unprivileged"* are largely mutually exclusive.

### ReadWriteMany / multi-writer volumes

`fsGroup` ownership-remapping (`spec.mover.podSecurityContext.fsGroup`) is most effective on `ReadWriteOnce` volumes; on `ReadWriteMany` it may be a no-op or ignored depending on the CSI driver. For RWX, match the owning UID/GID directly via the container `securityContext`, or — if the volume holds files from several UIDs — use a root mover to read/write regardless of owner.

### Mixed ownership, `lost+found`, root-written data

If `stat` shows several different owners and some files are owner-only (`0600`), no single non-root UID can read them all. A root mover is the pragmatic answer; pair it with `privilegedMode: true` if you also need restores to land with the original ownership.

### NFS sources and filesystem repositories

- **NFS exports** often apply `root_squash` (root is remapped to `nobody`) and their own UID mapping server-side. A root mover may *not* help there; match the UID the NFS server expects, or relax the export.
- A **filesystem repository** adds a second, separate permission surface: the **repository path** must be writable by the operator/mover UID. That's not a `securityContext` knob — see [Permissions → Filesystem repositories](permissions.md#filesystem-repositories-the-other-permission).

#### NFS filesystem repositories

A filesystem repository backed by an inline NFS export (`backend.filesystem.volume.nfs`) is the case where the two surfaces collide: a **single** mover pod must *read the source PVC* (as the app's UID, e.g. `1000`) **and** *write the repo backend* (which lives on an NFS export owned by some dedicated UID/GID, e.g. `3001`). And because [`fsGroup` is a no-op on NFS](#the-default-hardened-context), you can't lean on it.

The clean answer is to **decouple the two**: read the source as the app's UID, write the repo through a **shared supplemental group**. Supplemental GIDs *are* sent to the NFS server over AUTH_SYS (within the 16-group limit), so a group-writable export grants write without changing the process's primary UID.

1. **On the NAS** — own the export by the shared group and make it group-writable + setgid (so new repo blobs inherit the GID):

    ```console
    chown -R root:3001 /export/kopia    # or: chown -R 3001:3001
    chmod -R 2775 /export/kopia         # 2 = setgid
    ```

2. **On the repository** — every pod that *writes the backend* (the bootstrap connect/create Job, every snapshot/maintenance mover, **and** the kopia-ui server) must carry the shared group:

    ```yaml
    spec:
      moverDefaults:
        podSecurityContext:
          supplementalGroups: [3001] # movers + bootstrap join the export's group
      server: # only if you enable the web UI
        podSecurityContext:
          supplementalGroups: [3001] # the long-lived server joins it too
    ```

3. **Per recipe** — source reads stay correct because each `SnapshotPolicy`/`Restore` reads as the *app's* identity, e.g. via `inheritSecurityContextFrom`. The supplemental group is additive and doesn't disturb the primary UID:

    ```yaml
    # SnapshotPolicy
    spec:
      mover:
        inheritSecurityContextFrom:
          workloadSelector:
            podSelector: { matchLabels: { app.kubernetes.io/name: my-app } }
    ```

Full, apply-ready example:

```yaml
--8<-- "deploy/examples/backends/nfs-shared-group.yaml"
```

/// note | Alternatives
- **`runAsUser`** matching the export owner also works (it changes the actual process UID, unlike `fsGroup`) — but if the source PVC is owned by a *different* UID, the mover then can't read it, which is why the shared-group split is usually better.
- **Server-side remap** (TrueNAS **Mapall User/Group**, or `all_squash`/`anonuid` on a Linux exporter) makes *every* client write land as one identity on the server, regardless of the pod's UID. Zero pod-side config; the admission warning is then a false positive you can ignore.
///

### Restricted namespaces (Pod Security Admission)

The hardened default satisfies the `restricted` PSA profile, so unprivileged movers run anywhere. A **root/elevated** mover violates `restricted` — beyond Kopiur's own opt-in annotation, the namespace's PSA level (and any OpenShift SCC) must also permit it, or the pod won't schedule.

## Try it end-to-end

Prove [inherit-from-the-workload](#2-inherit-it-from-the-workload) end to end: a **running** Deployment whose container runs as UID `1000`, and a `SnapshotPolicy` that copies *that* identity onto the mover. The mover pod ends up running as `1000` — and you never wrote a UID on the policy.

One apply-ready bundle, [`deploy/examples/tryit/inherit-security-context.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/inherit-security-context.yaml): the `app` `Namespace`, a PVC, a long-running `app` Deployment (the identity source), a Secret, an S3 `Repository`, an `inheritSecurityContextFrom` `SnapshotPolicy`, and a manual `Snapshot`.

The workload is the identity source — `inheritSecurityContextFrom` needs a **live** pod to read from:

```yaml
--8<-- "deploy/examples/tryit/inherit-security-context.yaml:workload"
```

The policy points a label selector at it (no hard-coded UID):

```yaml
--8<-- "deploy/examples/tryit/inherit-security-context.yaml:policy"
```

**1. Fill in the credentials** (`AWS_*` + `KOPIA_PASSWORD`) in the `secret` section, then apply the bundle:

```console
$ kubectl apply -f deploy/examples/tryit/inherit-security-context.yaml
```

**2. Wait for the workload to be Running first** — inherit reads a live pod, so the Deployment must be up before the Snapshot:

```console
$ kubectl -n app rollout status deploy/app --timeout=2m
$ kubectl -n app wait --for=condition=Ready repository/app-primary --timeout=2m
```

**3. Take the backup** (the `Snapshot` uses `generateName`, so `create` it):

```console
$ kubectl create -f deploy/examples/tryit/inherit-security-context.yaml
snapshot.kopiur.home-operations.com/app-data-manual-abc12 created

$ kubectl -n app wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/app-data-manual-abc12 --timeout=5m
```

**4. Prove the inherited identity (deep).** The mover pod's UID equals the workload's, copied automatically:

```console
# what the WORKLOAD runs as:
$ kubectl -n app get pod -l app.kubernetes.io/name=app \
    -o jsonpath='{.items[0].spec.containers[?(@.name=="app")].securityContext.runAsUser}{"\n"}'
1000

# the mover Job is named after the Snapshot CR (no -snap suffix):
$ kubectl -n app get snapshot app-data-manual-abc12 -o jsonpath='{.status.job.name}{"\n"}'
app-data-manual-abc12

# the MOVER pod's UID — also 1000, inherited, never set on the policy:
$ kubectl -n app get pods --selector=job-name=app-data-manual-abc12
$ kubectl -n app get pod <mover-pod> \
    -o jsonpath='{.spec.containers[0].securityContext.runAsUser}{"\n"}'
1000
```

/// note | Illustrative names

`app-data-manual-abc12` and `<mover-pod>` stand in for the server-generated names your run gets. Substitute the names `kubectl create` / `kubectl get pods` print. The matching `1000` on both sides is the point — inherit copied the workload's identity.

///

/// tip | Scale the workload to zero and inherit fails loudly

`kubectl -n app scale deploy/app --replicas=0`, then re-create the Snapshot: with no Running pod to read, the Snapshot is held with an actionable condition telling you exactly that — inherit is selection of a live pod, not a stored value. Scale back up and it proceeds.

That's the behavior when the recipe has **nothing else to go on**, as here. Add a `mover.securityContext` that pins a `runAsUser` and the same scale-to-zero instead *proceeds* on that context, reporting `SecurityContextInherited=False` / `InheritFallback` — see [Combining inherit with an explicit context](#combining-inherit-with-an-explicit-context). Holding is the default precisely because a wrong-UID backup is worse than a missing one; you opt out of it by writing the identity you want used instead.

///

## Backup vs Restore at a glance

| | Backup | Restore |
| --- | --- | --- |
| The mover must… | **read** the source PVC | **write** the target PVC |
| Set the UID/GID to… | an identity that can read the data | the identity that should **own** the restored files |
| Default if unset | UID `65532` (reads world-readable / `65532`-owned only), pod `fsGroup: 65532` | UID `65532` (files land owned by `65532`), pod `fsGroup: 65532` |
| Preserve original ownership | n/a (kopia records it) | needs root + `privilegedMode: true` |
| Inherit from workload | `SnapshotPolicy.spec.mover.inheritSecurityContextFrom` | `Restore.spec.mover.inheritSecurityContextFrom` |
| Elevated context | namespace `privileged-movers` opt-in | same opt-in |
| Tolerate permission errors | fails on unreadable files | `spec.options.ignorePermissionErrors` (default `true`) reports instead of failing |

## Verify what the mover actually ran as

After a run, confirm the mover's effective identity and that it actually moved data:

```console
# the mover Job's name is on the owning Snapshot/Restore (named after the CR); find its pod from that:
$ kubectl get snapshot <snapshot-name> -n app -o jsonpath='{.status.job.name}'
app-data-manual-abc12
$ kubectl get pods -n app --selector=job-name=app-data-manual-abc12

# the container's effective UID (sanity-check it matches the data owner):
$ kubectl get pod <mover-pod> -n app \
    -o jsonpath='{.spec.containers[0].securityContext.runAsUser}{"\n"}'
1000

# permission errors, if any:
$ kubectl logs <mover-pod> -n app | grep -i "permission denied"
```

A backup that reports **`Succeeded` but zero files/bytes** is the classic sign the mover couldn't read the source — recheck the UID. The full verification workflow (status conditions, what a healthy run looks like) is in [Permissions → Verify it worked](permissions.md#step-3--verify-it-worked).

## Quick reference

| Thing | Value |
| --- | --- |
| Where to set it | `spec.mover.securityContext` (container) + `spec.mover.podSecurityContext` (pod) on `SnapshotPolicy` / `Restore` / `Maintenance` |
| `fsGroup` | `spec.mover.podSecurityContext.fsGroup` — make a fresh restore volume writable by an unprivileged mover. **Defaults to `65532`** so the kopia cache is writable; override for a restore that must own files as the app's GID |
| Default | container: UID `65532`, `runAsNonRoot: true`, drop ALL caps, seccomp `RuntimeDefault`, no escalation. pod: `fsGroup: 65532`, `fsGroupChangePolicy: OnRootMismatch` |
| Set the UID/GID | `securityContext.runAsUser` / `runAsGroup` (match the data owner) |
| Inherit from a workload | `inheritSecurityContextFrom.podSelector` (+ optional `container`) — copies container **and** pod context (UID + fsGroup). Needs the workload to pin `runAsUser`; combines with `securityContext`/`podSecurityContext`, which override it field-wise and act as the fallback |
| Root / preserve ownership | `runAsUser: 0` + `runAsNonRoot: false` (+ `privilegedMode: true` for restore ownership) |
| Privileged-mover opt-in | `kubectl annotate namespace <ns> kopiur.home-operations.com/privileged-movers=true` |
| Find the owning UID | [Permissions → Find the UID/GID](permissions.md#step-1--find-the-uidgid-that-owns-your-data) |

## See also

- [Permissions, UID & GID](permissions.md) — the task-oriented "my backup reads nothing / my restore is unreadable" workflow.
- [Movers, RBAC & credentials](movers.md) — privileged movers, the minted ServiceAccount, credential placement.
- [Restores](restores.md) — restore targets, options, and `ignorePermissionErrors`.
- [Example 09](examples.md#example-09--mover-uidgid--permissions) · [Example 18](examples.md#example-18--inherit-the-mover-security-context-from-a-workload).
