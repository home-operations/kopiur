# Restores

A `Restore` reads a snapshot back into a PersistentVolumeClaim (PVC). It answers three questions. Where to read from (`source`), where to write to (`target`), and how to write (`options` and `policy`). It also gives you the same mover settings a backup has: the UID/GID the mover runs as, the kopia cache, and a retry and deadline policy for the Job. Those are covered in [Mover, cache & failure policy](#mover-cache--failure-policy) below.

/// tip | The shape of a Restore

```yaml
spec:
    source: { <one of three>: ... } # FROM: which snapshot
    target: { <one of three>: ... } # TO: pvc | pvcRef | populator: {}  (REQUIRED)
    options: { ... } # HOW kopia writes (file deletion, permissions)
    policy: { ... } # what to do if the snapshot is missing
```

You must set both `source` and `target`. `options` and `policy` are optional and have safe defaults.

///

A restore is "pick a row, write it somewhere". In the common case there is no timestamp arithmetic to do. A `Restore` resolves its source **once**, the first time the operator reconciles it, and records the answer in its status. It never quietly switches to a different snapshot later.

## Where to restore _from_ — `source`

Set exactly one of three keys. The keys are externally tagged, so you write one key and its options underneath.

### `snapshotRef` — restore a specific Snapshot (the default)

Browse the catalog, pick a `Snapshot` CR, and name it. No timestamps involved. See [example 03](examples.md#example-03--restore-by-picking-a-snapshot).

```yaml
source:
    snapshotRef:
        name: postgres-data-20260524-021300
        namespace: billing # optional; defaults to the Restore's namespace
```

To find candidates:

```console
$ kubectl get snapshots -n billing \
    -l kopiur.home-operations.com/config=postgres-data \
    --sort-by=.status.timing.startTime
```

### `fromPolicy` — resolve via a SnapshotPolicy's identity

Restore the latest snapshot for a `SnapshotPolicy`'s identity, or one at an offset or a point in time. This works **even when no `Snapshot` CR exists yet**. It is what powers deploy-or-restore (see below) and point-in-time rollback. See [example 14](examples.md#example-14--point-in-time--offset-restore) and [scenario 07](scenarios/point-in-time-rollback.md). The default is `onMissingSnapshot: Continue`.

```yaml
source:
    fromPolicy:
        name: postgres-data
        namespace: billing # optional; defaults to the Restore's namespace
        offset: 0 # 0 = latest, 1 = previous, ...
        # asOf: 2026-05-01T00:00:00Z   # or: newest snapshot at/before this instant
```

`asOf` takes the newest snapshot at or before an RFC3339 instant. `offset` counts back from the latest. You normally set one or the other. `asOf` is the "roll back to a known-good time" knob; `offset` is "the previous one". Setting both does work: `asOf` filters first, then `offset` counts back within what is left, giving you "the one before the last known-good".

/// warning | A multi-repository policy needs an explicit `spec.repository`

When the policy uses [`spec.repositories` fan-out](backups.md#repositories--one-recipe-several-repositories-fan-out), a `fromPolicy` restore must also set `spec.repository` to **one member** of the policy's set. The N repositories are independent captures that can diverge, so Kopiur refuses to guess. The rejection lists the valid choices, and a reference outside the set is refused too. Single-repository policies need nothing extra.

```yaml
repository: { kind: Repository, name: offsite-s3 } # which member to read
source:
    fromPolicy:
        name: postgres-data
```

///

/// note | The resolution is recorded once, so a restore never silently retargets
Whatever the source resolves to is written ONCE to `status.resolved.kopiaSnapshotID` and reused for the rest of the restore's life. New snapshots that appear mid-flight, such as a schedule firing, cannot change which snapshot this Restore writes.
///

/// note | `fromPolicy` / `identity`-without-`snapshotID` resolve "latest" on **every** backend

Resolving "latest", `asOf`, or `offset` for a policy means listing the repository's snapshots **inside the restore mover Job**. The Job reaches every backend the same way a backup does, so this works on S3, Azure, GCS, B2, SFTP, WebDAV, rclone, and filesystem alike. The controller never has to mount the repository.

If no matching snapshot exists yet, `onMissingSnapshot` applies. `Continue` comes up empty, which is deploy-or-restore; `Fail` fails the Restore. `waitTimeout` keeps the Job re-checking for the snapshot to appear before that decision is made, so it must be shorter than the Job's `failurePolicy.activeDeadlineSeconds`. The admission webhook enforces that when you set both.

///

#### `sourcePath` — which volume of a multi-PVC policy to read

A [`pvcSelector` policy](backups.md#sources--what-to-back-up) backs up **several** volumes. Each one is backed up under its own kopia source path: `/pvc/<name>` under `sourcePathStrategy: PvcName`, or `/pvc/<namespace>/<name>` under `PvcNamespacedName`. So a `fromPolicy` restore has to say which volume it wants. Usually you do not have to say it out loud, because Kopiur works it out from the PVC it is filling.

Kopiur picks the source path using the first rule that applies:

1. Use `source.fromPolicy.sourcePath` if you set it. Your value always wins.
2. Otherwise, if a plain `pvc:` source in the policy names **exactly** this target (same name, same namespace), use that source's path. An exact match beats both the derivation in rule 4 and the fallback in rule 3.
3. Otherwise, if the policy has **no** selector sources, use `sources[0]`'s own path. That is what a restore always did.
4. Otherwise, if every selector source agrees on `(sourcePathStrategy, sourcePathOverride)` **and** that override is unset, derive the path from the **target PVC's name**. The restore runs the same code the backup side used, so the two strings cannot drift apart.
5. Otherwise, **fail**. The claim goes `Failed` with reason `SourcePathAmbiguous`, and the message tells you to set `source.fromPolicy.sourcePath`, for example `/pvc/postgres-data`.

```yaml
source:
    fromPolicy:
        name: billing-app
        # Pin ONE member. Leave unset to derive the path from the PVC being
        # filled — which is what a multi-PVC populator wants.
        sourcePath: /pvc/postgres-data
```

/// danger | Why this is not cosmetic

A restore with **no** source path becomes the kopia filter `username@hostname:`. That path is *empty*, and an empty path matches every volume in the policy. Before Kopiur derived a path per PVC, restoring one volume of a multi-PVC policy took the newest snapshot of **any** volume in the policy. A volume could come back holding a different volume's data, with a green `Completed` over it. Rule 5 exists so that Kopiur refuses rather than guesses.

Rule 2 runs before rule 3 for a related reason. A policy may list several plain `pvc:` sources, and **today only the first one is actually backed up**. A policy with no `pvcSelector` is never fanned out, so `sources: [{pvc: {name: a}}, {pvc: {name: b}}]` captures `/pvc/a` and nothing else. That is a pre-existing limitation on the backup side, tracked separately.

Reading `sources[0]`'s path for *every* target therefore returned a path that was real but belonged to another volume. Restoring into PVC `b` quietly filled it with `a`'s data. With rule 2 running first, `b` resolves to `/pvc/b`. The repository has never seen that path, so you get an honest `SnapshotNotFound`, or an empty volume under `Continue`, instead of the wrong data under a green `Completed`.

A target that matches **no** plain source still falls back to `sources[0]` (rule 3), but **only for a policy with no selector sources**. That is what keeps "restore this plain-`pvc:` policy's data into a differently-named scratch volume" working. A policy with a `pvcSelector` never reaches rule 3. Rule 4 derives the path from the *target's* name, so a differently-named target derives a path the repository has never seen. See the upgrade note below. Name the volume you want with `sourcePath` whenever the target is not the volume that was backed up.

///

/// warning | A cross-namespace `pvcRef` must set `sourcePath` explicitly

Rules 2 and 4 both work inside the policy's **own** namespace. Rule 2 requires the target's namespace to match the policy's, because a same-named PVC in another namespace is a different volume. Rule 4 derives from the TARGET's namespace. So a `target.pvcRef` in namespace `staging`, against a policy in `billing` with `sourcePathStrategy: PvcNamespacedName`, derives `/pvc/staging/<name>`. The backup wrote `/pvc/billing/<name>`, so the repository has never seen the derived path. You get `SnapshotNotFound`, or an empty volume under `Continue`.

Three shapes that used to work also change when you upgrade:

- **A selector-policy member restored into a differently-named PVC.** This covers a direct `target.pvc` or `target.pvcRef`, and a populator claim, whose name is not the member's. It used to resolve a pathless identity, which matched the newest snapshot of *any* member. It now derives `/pvc/<target-name>` (rule 4), a path the repository has never seen, so the restore reports `SnapshotNotFound`. Under `Continue` it instead records `NoSnapshot` and provisions an **empty** volume with `Resolved=True reason=NoSnapshotContinue`. The message names the derived path and the fix: set `fromPolicy.sourcePath: /pvc/<member>`, or run `kubectl kopiur restore --from-policy <p> --source-path /pvc/<member> …`. A `Continue` decision is recorded once and never re-resolved, so create a new `Restore` (or re-create the claiming PVC) once you have set the path.
- **A mixed policy**, meaning a plain `pvc:` source plus a selector, restoring cross-namespace. It used to fall back to the plain source's path. It now derives the path from the target instead.
- **A policy with several plain sources**, or an `nfs` source listed ahead of a `pvc:` one, restoring into a PVC that a later source names. It used to read `sources[0]`'s path. It now reads the matching source's own path. This one is the bug fix: the old answer was another volume's data.

Set `sourcePath` explicitly for any cross-namespace restore against a selector or mixed policy:

```yaml
source:
    fromPolicy:
        name: billing-app
        namespace: billing
        sourcePath: /pvc/billing/postgres-data # what the BACKUP wrote
target:
    pvcRef:
        name: postgres-data # in this Restore's namespace
```

///

### `identity` — a raw kopia identity

Use this for snapshots written by a foreign kopia client, or ones that have aged out of the catalog. See [example 13](examples.md#example-13--restore-by-raw-kopia-identity). You give the raw `username@hostname:path`. This mode **requires** an explicit `spec.repository`, because there is no `Snapshot` or `SnapshotPolicy` to infer it from.

```yaml
repository: { kind: Repository, name: primary, namespace: backups }
source:
    identity:
        username: postgres-data
        hostname: billing
        sourcePath: /data
        snapshotID: k1f1ec0a8 # pin an exact snapshot, or use asOf / offset
```

## Where to restore _to_ — `target`

### `pvc` — create a new PVC

The operator creates the PVC and restores into it. This is the best choice for verification restores, where you restore alongside the original and compare:

```yaml
target:
    pvc:
        name: postgres-data-restored
        storageClassName: fast-ssd # optional; cluster default otherwise
        capacity: 100Gi # REQUIRED: the operator won't guess a size it creates
        accessModes: [ReadWriteOnce] # optional; defaults to [ReadWriteOnce]
```

`capacity` is required, and the webhook enforces it. The operator creates this PVC, and a guessed default could be smaller than the data being restored. Size it at least as large as the source. The created PVC is deliberately **not** owned by the `Restore`, so deleting the Restore CR afterwards leaves the restored data in place.

### `pvcRef` — write into an existing PVC

```yaml
target:
    pvcRef:
        name: postgres-data # an existing PVC in this namespace
```

The restore mover must **mount** this PVC to write into it. When a `ReadWriteOnce` target is held by a running pod, Kopiur puts the mover on that pod's node automatically. A **`ReadWriteOncePod`** target cannot be co-mounted at all while a pod holds it, so scale the workload down first. See [PVC access modes & RWOP](access-modes.md#restoring-into-an-rwop-volume).

### `populator: {}` — passive populator mode

Set `target.populator: {}` and the `Restore` becomes a **passive volume-populator source**. It does not act on its own. Instead a PVC's `spec.dataSourceRef` points at it, and the snapshot is restored as that PVC is provisioned. This is the GitOps deploy-or-restore pattern, covered in the next section.

```yaml
target:
    populator: {} # explicit passive-populator mode
```

**One `Restore` serves every claim.** Every PVC whose `spec.dataSourceRef` names this `Restore` claims it, not just the first one. Kopiur drives each claim on its own. Each claim gets its own prime PVC, its own mover `Job`, its own [per-PVC source path](#sourcepath--which-volume-of-a-multi-pvc-policy-to-read), its own `waitTimeout` window, and its own record under `status.claims.<pvc>`:

```console
$ kubectl get restore billing-app-restore -n billing -o jsonpath='{.status.claims}' | jq
{
  "postgres-data":    { "phase": "Populated", "sourcePath": "/pvc/postgres-data",    "reason": "RestoreSucceeded" },
  "postgres-uploads": { "phase": "Populating", "sourcePath": "/pvc/postgres-uploads", "reason": "PopulatingPrimePvc" }
}
```

The `Restore`'s own `PHASE` summarizes all of its claims. It reads `Completed` once every claim has settled, `Failed` if any claim failed, and `Restoring` or `Pending` while work is still outstanding. Each claim's own phase is one of `Pending`, `Populating`, `Rebinding`, `Populated`, `AlreadyBound`, or `Failed`.

With exactly **one** claim, the ordinary single-PVC app, the `Restore` *is* that claim. Kopiur mirrors the claim's state onto the top-level status: `status.resolved` (the recorded decision), `status.target`, and the `Ready` and `Resolved` conditions all carry that claim's values unchanged, exactly as they did before one `Restore` could serve several claims. With **several** claims there is no single answer. The top-level `resolved` and `target` are cleared, the conditions carry the summary across all claims, and `status.claims.<pvc>` is the record to read for each volume.

/// note | A failed claim stalls the Restore, and the other claims keep going

When one claim fails, the whole `Restore` reports `Failed` and `Stalled=True`, so `kubectl wait` and Flux or Argo see the failure. A claim can fail because its mover pod is stuck, because its source path is ambiguous, or because `onMissingSnapshot: Fail` found nothing to restore. It does **not** stop the other claims. They carry on being populated, and `status.claims` says which one is stuck and why.

**To make a failed claim run again, delete its PVC and create it again**, keeping the `dataSourceRef`. The new PVC gets a new uid, and that new uid is what tells Kopiur to clean up the dead claim's prime PVC, Job and volume and start that claim over from a clean record. You do not touch the `Restore`, and the healthy claims are left alone.

There is one exception. If a provisioner bound the claim to some other volume while the restore was still writing, the prime PVC is **kept** on purpose, because it holds the half-written data. Kopiur never deletes it. The claim's reason is `PopulateHijacked`, and the message tells you what to check.

///

/// warning | `target` is required — the empty-`target` form is gone

The webhook rejects a `Restore` with **no** `target`. Populator intent must be the **explicit** `target.populator: {}`, not an omitted `target`. The live-pod `inheritSecurityContextFrom` modes, `workloadSelector` and `pvcConsumer`, are invalid in populator mode: there is no workload pod at provision time. The webhook rejects them and points you at `moverDefaults`, an explicit `securityContext`, or `inheritSecurityContextFrom: { snapshot: {} }`. That last one **is** allowed, because it replays the identity recorded on the backup and needs no live pod. See [the re-bootstrap section](#declarative-re-bootstrap--restore-as-the-recorded-identity).

///

/// note | A populator `Restore` is reusable — recreate the PVC and it restores again

A populator `Restore` is a **living source, not a one-shot**. `Completed` reports the *last* populate. It does **not** mark the `Restore` used up. Every PVC that claims it through `dataSourceRef` is populated as that PVC is provisioned. So if you delete the claiming PVC and apply a new one with the same `dataSourceRef`, Kopiur restores into the new PVC again. You do not touch the `Restore` at all. The PVC event re-enqueues it, and a populator's `Completed` phase is deliberately **not** terminal until a *bound* consumer exists, so a fresh, unbound claim drives a new populate.

The catch is _which_ snapshot it restores. It re-restores the one recorded in `status.resolved` at the first resolution, not whatever is newest. That is the same resolve-once rule that governs every source on this page. To pick up a newer snapshot, delete and re-create the `Restore` so it resolves again.

A **direct** target, `pvc` or `pvcRef`, behaves differently: that restore _is_ one-shot. Once `Completed` it is terminal, and deleting then re-creating the target PVC does **not** restore again. Create a new `Restore` to restore again.

///

/// note | Re-creating a populator `Restore` over a **bound** PVC does nothing (by design)

The claim is what drives a populate, not the `Restore`. A volume populator can only hand a volume to an **unbound** claim. So if you delete and re-create the `Restore` while its claiming PVC is still **Bound** and the app is happily running on it, there is nothing to populate. This happens on a GitOps prune and re-apply, or a repository rebuild that cascades. Kopiur completes it as a no-op: `Completed` with `Ready=True reason=TargetAlreadyBound`, no prime PVC, no mover run, and your live volume untouched.

To actually restore into that claim, delete the **PVC**, keeping its `dataSourceRef`, and let it be re-created. See the reusability note above.

**Upgrading from ≤ 0.7.x?** That case used to run a full restore into a `prime-<uid>` PVC that could never be adopted, and then leak it. You were left with a Bound PVC holding a complete second copy of your data, one per re-created `Restore`. On upgrade, Kopiur cleans those up automatically for every orphan whose claiming PVC still exists. Watch for an `OrphanedPrimePvcReaped` event. Any left over from a claim that has since been deleted are garbage-collected when their `Restore` is. To find them: `kubectl get pvc -A -l kopiur.home-operations.com/op=restore-populate`.

///

## How to write — `options` and `policy`

```yaml
options:
    enableFileDeletion: false # default: additive restore (don't delete extra files in the target)
    ignorePermissionErrors: true # default true
    writeFilesAtomically: true # default true
    parallel: 4 # restore parallelism; kopia default 8
    skipTimes: true # skip restoring file modification times
    overwriteFiles: true # overwrite existing files in the target
policy:
    onMissingSnapshot: Fail # see table below
    waitTimeout: 5m # how long to wait for the source snapshot to appear
```

/// warning | `enableFileDeletion` makes the target a mirror

By default a restore is **additive**. It writes the snapshot's files and leaves anything else in the target alone. `enableFileDeletion: true` deletes files in the target that are not in the snapshot, making it an exact mirror. Use it deliberately.

///

`options` also exposes the rest of `kopia snapshot restore`'s own tuning flags directly, one field per flag: `writeSparseFiles`, `skipOwners`, `skipPermissions`, `skipTimes`, `overwriteFiles`, `overwriteDirectories`, `overwriteSymlinks`, `ignoreErrors`, and `skipExisting`. All of those are tri-state: `true`, `false`, or absent, where absent means "let kopia decide". `parallel` takes a count. See the [field reference](field-reference.md) for kopia's own default for each flag.

### `onMissingSnapshot` — fail-closed vs proceed

| Value      | Behavior                                                                      | Default for                                  |
| ---------- | ----------------------------------------------------------------------------- | -------------------------------------------- |
| `Fail`     | No matching snapshot ⇒ the restore fails.                                     | `snapshotRef` / `identity` (explicit sources). |
| `Continue` | No matching snapshot ⇒ provision a **fresh, empty** volume and complete.      | `fromPolicy`.                                |

The defaults are the point. An _explicit_ restore that finds nothing is an error you want surfaced. A _deploy-or-restore_ that finds nothing should let the app start with a fresh volume.

On `Continue` with no snapshot, Kopiur **actually provisions the empty volume**. It does not just mark the `Restore` complete. For `target.populator: {}` it provisions an empty prime PVC and rebinds it to the claiming PVC, so a workload pod can bind and start. For `target.pvc` it creates the empty PVC. The "no snapshot, so empty" decision is written to `status.resolved` as `resolution: NoSnapshot` **once and never re-resolved**, so a snapshot that appears *later* can never quietly restore over a volume the app is already using. Re-create the `Restore` if you want to pick up a new snapshot. The Restore reports `Completed` with `Resolved=True reason=NoSnapshotContinue`.

The empty-volume path applies to `fromPolicy` and `identity` sources on **every** backend. The restore Job resolves the source in place, so an object-store `fromPolicy` with no snapshot comes up empty under `Continue` just like a filesystem one.

### `waitTimeout` — wait before giving up

`waitTimeout` takes a Go-style duration such as `5m`. It opens a grace window during which "no matching snapshot yet" means *wait and re-check* instead of giving up. `onMissingSnapshot` applies only once the window closes. Use it when the Restore may be applied before the thing that produces its snapshot: a schedule about to fire, a GitOps apply ordering, or a populator claim racing the first backup.

The window opens when the restore can first actually **proceed**, not when you applied it. Kopiur stamps that instant into `status.waitStartedAt` and measures `waitTimeout` from there. Two things have to be true first. The restore's repository has reached `Ready`, and, for `target.populator`, a PVC already claims the Restore through `dataSourceRef`. Until then the clock has not started.

The window also does **not** open while the objects Kopiur reads the repository from are missing. If the referenced `Repository` object, or the `SnapshotPolicy` a `fromPolicy` source names, does not exist yet, the restore parks in `Pending` with `ReferentAvailable=False reason=RestoreReferentMissing` and `status.waitStartedAt` stays unstamped. It re-checks every 15 s, so it un-parks on its own once you apply the missing object. Two cases deliberately **do** open the window without that check. The first is a `snapshotRef` whose `Snapshot` row does not exist yet: that row is precisely what the window is waiting for, and `onMissingSnapshot` has to be able to fire for a reference that never appears. The second is a restore whose mover Job has already launched, which keeps the deadline it was dispatched with.

/// warning | The window is not measured from `metadata.creationTimestamp`

A `Restore` is very often applied long before it can do anything. GitOps applies it in the same commit as the `Repository` it reads from, or a `target.populator` sits in the repo for months waiting for an app to claim it. Anchoring the window at creation would mean that by the time the restore could finally act, the window had already expired. For a `fromPolicy` source, whose `onMissingSnapshot` defaults to `Continue`, "expired" means **provision an empty volume immediately**, which is precisely the outcome `waitTimeout` exists to prevent. Read `status.waitStartedAt` to see when the clock actually started. An absent value means it has not started yet, or that no `waitTimeout` is set and there is no window to anchor.

///

Where the waiting happens depends on the source. A `snapshotRef` waits for the referenced `Snapshot` CR to gain an id, and it re-checks **in the controller** roughly every 15 s, surfacing `Resolved=False reason=WaitingForSnapshot` on the conditions. A `fromPolicy` or `identity` source re-lists the repository **inside the restore Job**, the same mover run that does the restore, so it works on every backend. During that window the `Restore` shows `Restoring` rather than a per-poll condition. Either way the window is an absolute deadline computed from `status.waitStartedAt`, so it is bounded even across controller restarts or Job pod retries. Because the wait runs inside the Job for `fromPolicy` and `identity`, `waitTimeout` must be shorter than the Job's `failurePolicy.activeDeadlineSeconds`. The admission webhook rejects a Restore that sets both with `waitTimeout` greater than or equal to the deadline.

When a populator's claiming PVC is deleted and re-created, the restore resolves again, and that clears `status.waitStartedAt`. The re-created claim gets the full window again rather than inheriting the previous claim's spent one.

/// tip | Restoring on a freshly-seeded repository

A repository being initialized from a replica with [`spec.seed`](repositories.md#seed--initialize-a-new-repository-from-a-replica) does not reach `Ready` until the copy lands, which can legitimately take hours. A populator `Restore` waits for it, and its `waitTimeout` window opens only at that moment, so a long seed does not spend the window. Before re-applying policies over the recovered history, read the identity and retention hazards in [Scenario 10](scenarios/dr-with-replicated-repository.md#hazards-to-review-before-you-apply). Adoption under the default `deletionPolicy: Delete` prunes everything outside `spec.retention`.

///

## Mover, cache & failure policy

A restore writes data **into** a PVC, so the mover doing the writing has the same concerns a backup's mover does. `Restore.spec.mover` is the same `MoverSpec` a `SnapshotPolicy` exposes, and `Restore.spec.failurePolicy` mirrors `Snapshot.spec.failurePolicy`. See the full manifest in [example 12](examples.md#example-12--restore-mover-cache--failure-policy).

```yaml
spec:
    mover:
        securityContext: { runAsUser: 1000, runAsGroup: 1000, ... } # CONTAINER: own the restored files
        podSecurityContext: { fsGroup: 1000 } # POD: make a fresh volume writable
        # inheritSecurityContextFrom: { workloadSelector: { podSelector: {...} } }  # ...or copy from a live pod (restore: workloadSelector, not pvcConsumer)
        # inheritSecurityContextFrom: { snapshot: {} }                              # ...or replay the identity RECORDED on the backup (no live pod needed)
        cache: { capacity: 16Gi, mode: Persistent, contentCacheSizeMb: 10000 }
    failurePolicy:
        backoffLimit: 4
        activeDeadlineSeconds: 7200 # cap a RUNNING restore (default 48h backstop)
        podStartupDeadlineSeconds: 300 # fail a restore mover that can't START in 5m (default 300)
```

- **`mover.securityContext`** sets the UID/GID the restore mover's **container** runs as, which is the identity that will own the restored files. Without it the mover runs as the hardened default, UID 65532, which may write files the app cannot read. This is the fix for "the restore mover had no UID control".
- **`mover.podSecurityContext.fsGroup`** sets a **pod**-level `fsGroup` that makes a freshly-provisioned target volume group-writable. That lets an **unprivileged** `runAsUser: 1000` mover populate the volume on restore, instead of needing a root mover just to write a new volume. This is the headline case for restoring into a brand-new PVC as non-root. See [Security context → fsGroup](security-context.md).
- **`mover.inheritSecurityContextFrom`** copies **both** the container `securityContext` **and** the pod-level `securityContext` from somewhere authoritative, instead of you hard-coding them. That means the restore mover gets the app's UID *and* its `fsGroup`. On a Restore, two forms work. **`workloadSelector: { podSelector, container? }`** names a live pod that will *read* the restored data. **`snapshot: {}`** replays the identity **recorded on the backup itself** (`Snapshot.status.recorded`), which needs no live pod; see [the re-bootstrap section below](#declarative-re-bootstrap--restore-as-the-recorded-identity). The **`pvcConsumer`** form is **backup-only**. It derives the workload from a backup *source* PVC, which a restore does not have, because the target's consumer may not exist yet. The webhook therefore **rejects `pvcConsumer` on a `Restore`**. Inheriting combines with `securityContext` and `podSecurityContext`: those are the higher merge layer, so a field you set explicitly overrides the inherited one, and they stand in alone when no workload pod resolves. In that case the restore proceeds on your fields and reports `SecurityContextInherited=False` with reason `InheritFallback`, because the restored files will then be owned as *your* context says rather than as the workload you named. The condition `RestoreSecurityContextCompatible` reports, positively only, when the future consumer will be able to read what the mover writes. See [Security context → Inherit it from the workload](security-context.md#2-inherit-it-from-the-workload) and [example 18](examples.md#example-18--inherit-the-mover-security-context-from-a-workload).
- **`mover.cache`** sizes the kopia cache for a large restore. `mode: Ephemeral`, the default, gives a fresh per-run volume sized by `capacity`, or an `emptyDir` when `capacity` is unset. `mode: Persistent` keeps a controller-owned cache PVC and reuses it across runs for a warm cache. `contentCacheSizeMb` and `metadataCacheSizeMb` pass kopia's `--content/metadata-cache-size-mb` budgets. A repository's `moverDefaults.cache` is inherited, and `mover.cache` overlays it.
- **`failurePolicy`** sets the restore Job's `backoffLimit`, `activeDeadlineSeconds`, and `podStartupDeadlineSeconds`. Leaving it absent uses the defaults: 2 retries, a 48h `activeDeadlineSeconds` backstop so a *running* Job cannot linger forever, and a 5-minute `podStartupDeadlineSeconds` so a restore mover that cannot **start** fails fast with `MoverPodWedged` instead of hanging. A mover fails to start on a bad image, an unschedulable pod, or an impossible `securityContext`. The two deadlines are explained in [Backups → `failurePolicy`](backups.md#failurepolicy--retry--deadline-for-the-mover-job).

/// warning | An elevated restore mover needs the namespace to opt in

Kopiur refuses a restore mover that runs as root (`runAsUser: 0`), with added capabilities, or with `privilegedMode: true`, until the restore's namespace opts in. This includes a context **inherited** from a root workload pod. You see `MoverPermitted=False`, exactly as you would for a backup. Opt the namespace in by applying a `Namespace` carrying the opt-in annotation:

```yaml
--8<-- "deploy/examples/privileged-mover-namespace.yaml"
```

```console
$ kubectl apply -f privileged-mover-namespace.yaml
```

Or do it imperatively: `kubectl annotate namespace <ns> kopiur.home-operations.com/privileged-movers=true`.

See [Permissions](permissions.md) for how to choose the UID/GID and when a privileged mover is warranted.

///

## Deploy-or-restore (GitOps)

This is the headline pattern: commit one bundle and apply it to **any** cluster. On a fresh cluster pointed at an existing repository, the PVC restores the latest snapshot before the app starts. On a brand-new repository, the PVC comes up empty and is backed up going forward. You never write an "is this a new install or a recovery?" branch.

The mechanism is a **passive `Restore`** using `source.fromPolicy`, `target.populator: {}`, and `onMissingSnapshot: Continue`, consumed by a PVC's `dataSourceRef` as a volume populator. The full manifest is [example 05](examples.md#example-05--deploy-or-restore-gitops). The same `Restore` keeps serving claims for its whole life. Tear the app's PVC down and stand it back up, whether that is a `kubectl delete` and re-apply, a namespace rebuild, or a migration, and the new PVC is populated again from the recorded snapshot. Nothing about the `Restore` changes. See [the reusability note above](#populator---passive-populator-mode).

/// note | Kubernetes ≥ 1.24

The volume-populator handshake relies on the `AnyVolumeDataSource` feature, which is generally available from 1.24. The optional `volume-data-source-validator` surfaces a malformed `dataSourceRef` as an event instead of a silently-stuck PVC.

///

### Deploy-or-restore for a multi-PVC app

An app whose data lives on several PVCs needs exactly the same three objects: a `pvcSelector` policy, a schedule, and **one** `Restore`. One populator `Restore` serves every claim. Each PVC below reads its own volume's history, derived from its own name:

```yaml
--8<-- "deploy/examples/44-multi-pvc-deploy-or-restore.yaml:policy"
```

```yaml
--8<-- "deploy/examples/44-multi-pvc-deploy-or-restore.yaml:restore"
```

```yaml
--8<-- "deploy/examples/44-multi-pvc-deploy-or-restore.yaml:claims"
```

The full manifest, with the schedule and the commentary on the failure modes, is [`deploy/examples/44-multi-pvc-deploy-or-restore.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/44-multi-pvc-deploy-or-restore.yaml). Watch it land with `kubectl get restore <name> -o jsonpath='{.status.claims}'`. The cost is [N pooled movers](#a-restore-is-never-held-behind-a-concurrency-cap).

## Restoring a snapshot Kopiur didn't create

Snapshots written by a foreign kopia client, or ones predating your install, show up as **discovered** `Snapshot` CRs in the repository's namespace. They carry `origin=discovered` and a forced `deletionPolicy: Retain`. There are two ways to restore them, both in [example 07](examples.md#example-07--restore-a-discovered-backup):

- **(A)** reference the discovered `Snapshot` CR with `source.snapshotRef`, the same as any other backup; or
- **(B)** use `source.identity` with the raw kopia identity, which requires `spec.repository`, for snapshots that aged out of the catalog.

```console
$ kubectl get snapshots -n backups -l kopiur.home-operations.com/origin=discovered
```

/// note | Snapshots carry their recorded mover identity

Every snapshot Kopiur produces records the resolved mover identity, meaning the uid, gid, fsGroup and where they came from, on the snapshot itself as the `kopiur-meta` tag. The catalog scan decodes it into `status.recorded` on discovered rows. So the identity the data expects survives a cluster rebuild along with the repository. `kubectl kopiur snapshots list -o wide` shows it, and a `Restore` can run **as** it with `inheritSecurityContextFrom: { snapshot: {} }`, covered in the next section. See [Backups → tags](backups.md#tags--label-the-snapshot-in-the-repository).

///

## Declarative re-bootstrap — restore as the recorded identity

After a full cluster loss, the workload whose UID a restore should run as does not exist yet, so there is no pod to inherit from. `mover.inheritSecurityContextFrom: { snapshot: {} }` closes that gap. The restore mover runs as the identity **recorded on the backup**, read from `Snapshot.status.recorded`, which is decoded from the `kopiur-meta` kopia tag. No live pod is involved. The whole recovery is then one declarative apply:

1. GitOps applies **Repository + SnapshotPolicy + Restore** to the fresh cluster, with `source.fromPolicy` and `snapshot: {}`.
2. The repository connects. The **catalog scan** materializes `discovered` Snapshot CRs from the repository, decoding each snapshot's recorded identity into `status.recorded`.
3. Until a matching row exists, the Restore **holds** with `SecurityContextInherited=False` and reason `MissingRecordedIdentity`. A Warning Event says why, and it re-checks every few minutes. This is the expected intermediate state, not an error to fix.
4. The moment the scan lands, the Restore resolves the recorded identity, runs the mover as it, and completes.

You hand-author no snapshot names anywhere. `fromPolicy` re-resolves the kopia identity from the live `SnapshotPolicy`, and the controller picks the matching Snapshot CR from the catalog, honoring `asOf` and `offset`. `source.identity` works the same way, including a pinned `snapshotID`. `source.snapshotRef` also supports `snapshot: {}`: it reads that CR's `status.recorded` directly. Pair this with `target: { populator: {} }` for [deploy-or-restore](#deploy-or-restore-gitops). Because `snapshot` needs no live pod, it is the **one** inherit mode allowed with a populator target.

```yaml
--8<-- "deploy/examples/37-restore-recorded-identity.yaml"
```

What the mover runs as, how far you can trust recorded metadata (it is repository data and can be forged, so a recorded root identity stays gated on the namespace opt-in), and the full `SecurityContextInherited` reason table all live in [Security context → `snapshot`](security-context.md#snapshot--inherit-the-backups-recorded-identity-restore).

/// note | Restoring into a *renamed* cluster

The catalog scan filters out identities that belong to **other clusters**, recognized by foreign hostname suffixes. So a repository restored into a cluster with a *different* identity scheme may materialize no discovered rows for your old snapshots, and a `fromPolicy` search then holds forever. The escape hatch is the `identity` source. Give it the old raw kopia `username` and `hostname`, and it searches the CR catalog, and selects the snapshot, by exactly that identity. You can still use `snapshot: {}` alongside it.

///

/// note | `asOf` selects twice — CR-side identity vs repository-side data

With `fromPolicy` or `identity` plus `snapshot: {}`, **data and identity both come from the same catalog row**. The controller selects one matching Snapshot CR, honoring `asOf` and `offset`, records its kopia snapshot id as the data to restore, and replays that same snapshot's recorded identity. The two can never diverge, so snapshot B's data is never restored under snapshot A's uid, gid and fsGroup. The trade-off is deliberate. Selection runs against the **CR catalog**, not the live repository listing, so under catalog lag the restore picks the newest *catalogued* snapshot. The scan converges the catalog, and a not-yet-catalogued snapshot is simply not eligible yet. The condition message names the exact Snapshot both came from.

///

## Watching a restore

```console
$ kubectl get restore -n billing -w
NAME              PHASE        AGE
postgres-verify   Resolving    2s
postgres-verify   Restoring    9s
postgres-verify   Completed    41s
```

The phases run `Pending` → `Resolving` (picking the source snapshot) → `Restoring` (mover writing data) → `Completed` or `Failed`. Live byte and file progress is in `status.progress`. The resolved snapshot and target PVC are in `status.resolved` and `status.target`. If it will not progress, `kubectl describe restore <name>` shows the reason on the conditions and as an Event. See [Troubleshooting](troubleshooting.md).

Every phase write also carries the [kstatus](gitops.md) conditions. `Completed` gives `Ready=True`. `Failed` gives `Stalled=True`, because a Restore is one-shot: fix the cause and create a new Restore. Anything in flight gives `Reconciling=True`. So `kubectl wait --for=condition=Ready restore/<name>` and Flux or Argo health checks gate on a restore the same way they do on every other kopiur kind. The domain conditions `Resolved`, `MoverPermitted`, `CredentialsAvailable` and `AwaitingClaim` survive phase transitions alongside them.

## A restore is never held behind a concurrency cap

A [`Repository`'s `concurrency.maxConcurrentJobs`](repositories.md#concurrency--cap-the-mover-jobs-one-repository-runs-at-once) bounds how many mover Jobs run against it at once, and backups queue behind it. **Restores never do.** A restore is a recovery in progress, and holding one behind a queue of routine nightly backups is exactly backwards. So a restore is admitted at or over the cap, and it never carries a `RepositorySlotAvailable` condition at all, either way.

It does still **count**, and it counts from the moment the operator decides to run it, not from the moment its Job shows up in `kubectl get jobs`. A restore occupies a slot like any other pooled Job, so it displaces backups rather than adding to them, which is what a cap is actually being asked for. If you set `maxConcurrentJobs: 3` and start three large restores, backups against that repository queue until the restores finish.

The "from the moment of the decision" part matters. A restore that only became countable once its Job existed would be invisible for the short window in which the operator is still resolving its source, staging its target PVC and projecting credentials. A backup reconciling in that window would read spare capacity and start a second mover beside the recovery. A restore reserves its slot up front, so the cap holds even when a restore and a backup arrive at the same instant.

**N claims means N pooled movers.** A populator `Restore` over a multi-PVC app runs one mover per claiming PVC, and each takes its own slot on the same terms as above: admitted at and over the cap, counted from the decision. So a ten-volume recovery displaces routine backups against that repository until it finishes. There is no per-`Restore` cap, because a recovery is not the thing to throttle, but size `maxConcurrentJobs` knowing that one `Restore` can be worth ten movers.

None of this needs configuration. It is fixed behavior of the pool. See [Backups → limiting concurrent jobs per repository](backups.md#limiting-concurrent-jobs-per-repository) for the whole picture.

## Credentials in a fresh namespace — `credentialProjection`

A restore mover loads the repository credentials with `envFrom` from a Secret **in its own namespace**. Restoring into a namespace that has never run a backup, such as a disaster recovery or a clone target, will not have one. Set `credentialProjection.enabled: true` and the operator copies the referenced repository's Secret into the mover's namespace for the run. The copy is owned by the `Restore` and garbage-collected with it. See [example 17](examples.md#example-17--restore-from-a-shared-repo-projection):

```yaml
spec:
    repository: { kind: ClusterRepository, name: platform-shared }
    credentialProjection:
        enabled: true # off by default; needs Helm features.credentialProjection.enabled
```

It is **off by default**, because copying Secrets across namespaces is opt-in, and it needs the operator's Secret-projection RBAC through the Helm flag `features.credentialProjection.enabled`. The alternative is placing the Secret in the namespace yourself. See [Movers → credential projection](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos).

## Field reference — every value, and when to change it

The full `Restore` surface, with the examples that exercise each field. `source` is the only required field.

| Field | What it does | When to set it |
| --- | --- | --- |
| `repository` | The repository to read from (`{ kind, name, namespace? }`). Inferred from `source` for `snapshotRef`/`fromPolicy`; **required** for `identity`. | Cross-namespace / cluster restores, or any `identity` source. ([13](examples.md#example-13--restore-by-raw-kopia-identity), [16](examples.md#example-16--cross-namespace-clone-restore)) |
| `source.snapshotRef` | Restore a specific `Snapshot` CR (`{ name, namespace? }`). | The common case: you picked a row from the catalog. ([03](examples.md#example-03--restore-by-picking-a-snapshot), [16](examples.md#example-16--cross-namespace-clone-restore)) |
| `source.fromPolicy` | Resolve via a `SnapshotPolicy`'s identity (`{ name, namespace?, asOf?, offset? }`). | No `Snapshot` CR (deploy-or-restore), or point-in-time (`asOf`) / positional (`offset`) recovery. ([05](examples.md#example-05--deploy-or-restore-gitops), [14](examples.md#example-14--point-in-time--offset-restore)) |
| `source.identity` | Raw kopia identity (`{ username, hostname, sourcePath?, snapshotID?, asOf?, offset? }`). | Foreign / aged-out snapshots; needs `repository`. ([13](examples.md#example-13--restore-by-raw-kopia-identity)) |
| `target.pvc` | Create a new PVC and restore into it (`{ name, storageClassName?, capacity?, accessModes? }`). | The safe default: restore beside the original, verify, then cut over. ([03](examples.md#example-03--restore-by-picking-a-snapshot)) |
| `target.pvcRef` | Restore into an **existing** PVC (`{ name }`). | In-place restore (scale the app down first). ([15](examples.md#example-15--in-place-mirror-restore)) |
| `target.populator` | Explicit passive volume-populator source (`populator: {}`). | GitOps deploy-or-restore via a PVC `dataSourceRef`. ([05](examples.md#example-05--deploy-or-restore-gitops)) |
| `options.enableFileDeletion` | Delete target files not in the snapshot (exact **mirror**); wired to kopia's `--delete-extra`. Default `false` (additive). | A faithful in-place restore. Destructive, so use it deliberately. ([15](examples.md#example-15--in-place-mirror-restore)) |
| `options.ignorePermissionErrors` | Complete and _report_ permission problems vs. fail hard. Default `true`. | `false` to fail-closed when exact permissions matter. |
| `options.writeFilesAtomically` | Write via a temp file + rename. Default `true`. | Rarely changed. |
| `options.parallel` | Restore parallelism (`--parallel`). Kopia default `8`. | Large restores on fast storage/network. |
| `options.writeSparseFiles` / `skipOwners` / `skipPermissions` / `skipTimes` | Tri-state passthroughs to kopia's `--[no-]write-sparse-files` / `--[no-]skip-owners` / `--[no-]skip-permissions` / `--[no-]skip-times`. Absent ⇒ kopia's own default. | Sparse-file-heavy targets; cross-platform restores where owners/permissions/times don't translate. |
| `options.overwriteFiles` / `overwriteDirectories` / `overwriteSymlinks` | Tri-state passthroughs to kopia's `--[no-]overwrite-*` (kopia default `true` for all three). | `false` to refuse clobbering an existing target. |
| `options.ignoreErrors` / `skipExisting` | Tri-state passthroughs to kopia's `--[no-]ignore-errors` / `--[no-]skip-existing`. Kopia default `false` for both. | Best-effort restores; resuming a partially-written target. |
| `policy.onMissingSnapshot` | `Fail` (explicit sources) vs `Continue` (fromPolicy default). | `Fail` for deliberate recoveries; `Continue` for deploy-or-restore. |
| `policy.waitTimeout` | How long to wait for the source snapshot to appear. | Sources that may lag behind the Restore being applied. |
| `mover.securityContext` / `podSecurityContext` | Container UID/GID, and the pod-level `fsGroup` that makes a fresh target volume writable. | Own restored files as the app's UID; populate a fresh PVC as non-root (`fsGroup`). See [Mover, cache & failure policy](#mover-cache--failure-policy). ([12](examples.md#example-12--restore-mover-cache--failure-policy)) |
| `mover.cache` / `resources` / `inheritSecurityContextFrom` | Cache sizing/mode, mover resources, inherit-from-pod. | Large-restore cache, resource limits, run-as-the-app. ([12](examples.md#example-12--restore-mover-cache--failure-policy)) |
| `failurePolicy` | Restore Job `backoffLimit` / `activeDeadlineSeconds`. | Retry/deadline control for big or flaky restores. ([12](examples.md#example-12--restore-mover-cache--failure-policy)) |
| `credentialProjection` | Project the repo Secret into the mover's namespace. | Restoring into a fresh namespace from a shared repo. ([17](examples.md#example-17--restore-from-a-shared-repo-projection)) |

## See also

- [Backups & schedules](backups.md) — producing the snapshots you restore.
- [Repositories & backends](repositories.md) — where the snapshots live.
- [Permissions](permissions.md) — choosing the mover's UID/GID and the privileged-movers opt-in (applies to restores too).
- [Scenarios](scenarios/index.md) — [02 recover lost data](scenarios/recover-lost-data.md), [07 point-in-time rollback](scenarios/point-in-time-rollback.md), [08 clone to another namespace](scenarios/clone-app-to-namespace.md), [10 DR from a replicated repository](scenarios/dr-with-replicated-repository.md).
- [Examples](examples.md) — [03 by Snapshot](examples.md#example-03--restore-by-picking-a-snapshot), [05 deploy-or-restore](examples.md#example-05--deploy-or-restore-gitops), [07 discovered](examples.md#example-07--restore-a-discovered-backup), [12 mover/cache/failure policy](examples.md#example-12--restore-mover-cache--failure-policy), [13 by identity](examples.md#example-13--restore-by-raw-kopia-identity), [14 point-in-time](examples.md#example-14--point-in-time--offset-restore), [15 in-place mirror](examples.md#example-15--in-place-mirror-restore), [16 cross-namespace](examples.md#example-16--cross-namespace-clone-restore), [17 shared-repo projection](examples.md#example-17--restore-from-a-shared-repo-projection).
