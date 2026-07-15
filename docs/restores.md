# Restores

A `Restore` reads a snapshot back into a PVC. At its core it answers three questions: **from where** (`source`), **to where** (`target`), and **how** (`options`/`policy`). For real-world restores it also exposes the same mover knobs a backup has — UID/GID, kopia cache, and a Job retry/deadline policy — covered in [Mover, cache & failure policy](#mover-cache--failure-policy) below.

/// tip | The shape of a Restore

```yaml
spec:
    source: { <one of three>: ... } # FROM: which snapshot
    target: { <one of three>: ... } # TO: pvc | pvcRef | populator: {}  (REQUIRED)
    options: { ... } # HOW kopia writes (file deletion, permissions)
    policy: { ... } # what to do if the snapshot is missing
```

`source` **and** `target` are required; `options`/`policy` are optional with safe defaults.

///

Restore is "pick a row, write it somewhere" — there's no timestamp arithmetic in the common case. A `Restore` resolves its source **once at admission** and pins it to status, so it never silently retargets a different snapshot later.

## Where to restore _from_ — `source`

Exactly one of three modes (externally tagged — you set one key):

### `snapshotRef` — restore a specific Snapshot (the default)

You browsed the catalog and picked a `Snapshot` CR. No timestamps — just reference it. See [example 03](examples.md#example-03--restore-by-picking-a-snapshot).

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

Restore the latest (or an offset/point-in-time) snapshot for a `SnapshotPolicy`'s identity — **even when no `Snapshot` CR exists yet**. This is what powers deploy-or-restore (see below) and point-in-time rollback ([example 14](examples.md#example-14--point-in-time--offset-restore), [scenario 07](scenarios/point-in-time-rollback.md)). Defaults to `onMissingSnapshot: Continue`.

```yaml
source:
    fromPolicy:
        name: postgres-data
        namespace: billing # optional; defaults to the Restore's namespace
        offset: 0 # 0 = latest, 1 = previous, ...
        # asOf: 2026-05-01T00:00:00Z   # or: newest snapshot at/before this instant
```

`asOf` (newest snapshot at/before an RFC3339 instant) and `offset` (count back from latest) usually travel alone — `asOf` is the "roll back to a known-good time" knob; `offset` is "the previous one." They do compose if you set both: `asOf` filters first, then `offset` counts back within what's left ("the one before the last known-good").

/// note | The resolution is pinned — a restore never silently retargets
Whatever the source resolves to is written ONCE to `status.resolved.kopiaSnapshotID` and reused for the rest of the restore's life. New snapshots appearing mid-flight (a schedule firing) cannot change which snapshot this Restore writes.
///

/// note | `fromPolicy` / `identity`-without-`snapshotID` resolve "latest" on **every** backend

Resolving "latest / `asOf` / `offset` for a policy" lists the repository's snapshots **inside the restore mover Job**, which reaches every backend the same way a backup does — so this works on S3, Azure, GCS, B2, SFTP, WebDAV, rclone, and filesystem alike. No controller-side repo mount is needed.

If no matching snapshot exists yet, `onMissingSnapshot` applies (`Continue` comes up empty — deploy-or-restore; `Fail` fails the Restore). `waitTimeout` keeps the Job re-checking for the snapshot to appear before that decision — so it must be shorter than the Job's `failurePolicy.activeDeadlineSeconds` (the admission webhook enforces this when you set both).

///

### `identity` — a raw kopia identity

For snapshots written by a foreign kopia client, or ones that have aged out of the catalog ([example 13](examples.md#example-13--restore-by-raw-kopia-identity)). You give the raw `username@hostname:path`. This mode **requires** an explicit `spec.repository` (there's no `Snapshot`/`SnapshotPolicy` to infer it from).

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

The operator creates the PVC and restores into it. Best for verification restores (restore alongside the original and compare):

```yaml
target:
    pvc:
        name: postgres-data-restored
        storageClassName: fast-ssd # optional; cluster default otherwise
        capacity: 100Gi # REQUIRED: the operator won't guess a size it creates
        accessModes: [ReadWriteOnce] # optional; defaults to [ReadWriteOnce]
```

`capacity` is required (webhook-enforced): the operator creates this PVC, and a guessed default could be smaller than the data being restored. Size it at least as large as the source. The created PVC is deliberately **not** owned by the `Restore` — deleting the Restore CR afterwards leaves the restored data in place.

### `pvcRef` — write into an existing PVC

```yaml
target:
    pvcRef:
        name: postgres-data # an existing PVC in this namespace
```

The restore mover must **mount** this PVC to write into it. For a `ReadWriteOnce` target held by a running pod, Kopiur co-locates the mover onto that node automatically; a **`ReadWriteOncePod`** target can't be co-mounted at all while a pod holds it — scale the workload down first (see [PVC access modes & RWOP](access-modes.md#restoring-into-an-rwop-volume)).

### `populator: {}` — passive populator mode

Set `target.populator: {}` and the `Restore` becomes a **passive volume-populator source**: it doesn't act on its own; instead a PVC's `spec.dataSourceRef` points at it, and the snapshot is restored as that PVC is provisioned. This is the GitOps deploy-or-restore pattern (next section).

```yaml
target:
    populator: {} # explicit passive-populator mode
```

/// warning | `target` is required — the empty-`target` form is gone

A `Restore` with **no** `target` is rejected by the webhook. Populator intent must be the **explicit** `target.populator: {}` (not an omitted `target`). Also, `inheritSecurityContextFrom` is invalid in populator mode — there's no workload pod at provision time, so the webhook rejects it and points you at `moverDefaults` / an explicit `securityContext`.

///

/// note | A populator `Restore` is reusable — recreate the PVC and it restores again

A populator `Restore` is a **living source, not a one-shot**. `Completed` reports the *last* populate; it does **not** latch the `Restore` "consumed". Every PVC that claims it via `dataSourceRef` is populated as that PVC is provisioned — so if you delete the claiming PVC and apply a new one with the same `dataSourceRef`, Kopiur restores into the new PVC again. You don't touch the `Restore` at all: the PVC event re-enqueues it, and a populator's `Completed` phase is deliberately **not** terminal until a *bound* consumer exists, so a fresh, unbound claim drives a new populate.

The catch is _which_ snapshot: it re-restores the one **pinned in `status.resolved` at the first resolution**, not whatever is newest — the same pin-once rule that governs every source on this page. To pick up a newer snapshot, delete and re-create the `Restore` so it re-resolves.

Contrast a **direct** target (`pvc` / `pvcRef`): that restore _is_ one-shot. Once `Completed` it's terminal, and deleting then re-creating the target PVC does **not** re-restore — create a new `Restore` to restore again.

///

/// note | Re-creating a populator `Restore` over a **bound** PVC does nothing (by design)

The claim is what drives a populate, not the `Restore`. A volume populator can only hand a volume to an **unbound** claim, so if you delete and re-create the `Restore` (a GitOps prune and re-apply, a repository rebuild that cascades) while its claiming PVC is still **Bound** and the app is happily running on it, there is nothing to populate. Kopiur completes it as a no-op: `Completed` with `Ready=True reason=TargetAlreadyBound`, no prime PVC, no mover run, and your live volume untouched.

To actually restore into that claim, delete the **PVC** (keeping its `dataSourceRef`) and let it be re-created — see the reusability note above.

**Upgrading from ≤ 0.7.x?** That case used to run a full restore into a `prime-<uid>` PVC that could never be adopted, and then leak it: a Bound PVC holding a complete second copy of your data, one per re-created `Restore`. On upgrade, Kopiur reaps those automatically (watch for an `OrphanedPrimePvcReaped` event) for every orphan whose claiming PVC still exists. Any left over from a claim that has since been deleted are garbage-collected when their `Restore` is. To find them: `kubectl get pvc -A -l kopiur.home-operations.com/op=restore-populate`.

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

By default a restore is **additive** — it writes the snapshot's files and leaves anything else in the target alone. `enableFileDeletion: true` deletes files in the target that aren't in the snapshot, making it an exact mirror. Use it deliberately.

///

`options` also exposes the rest of `kopia snapshot restore`'s own tuning flags directly, one field per flag: `writeSparseFiles`, `skipOwners`, `skipPermissions`, `skipTimes`, `overwriteFiles`, `overwriteDirectories`, `overwriteSymlinks`, `ignoreErrors`, and `skipExisting` (all tri-state — `true`/`false`/absent, where absent means "let kopia decide") plus `parallel` (a count). See the [field reference](field-reference.md) for kopia's per-flag default.

### `onMissingSnapshot` — fail-closed vs proceed

| Value      | Behavior                                                                      | Default for                                  |
| ---------- | ----------------------------------------------------------------------------- | -------------------------------------------- |
| `Fail`     | No matching snapshot ⇒ the restore fails.                                     | `snapshotRef` / `identity` (explicit sources). |
| `Continue` | No matching snapshot ⇒ provision a **fresh, empty** volume and complete.      | `fromPolicy`.                                |

The defaults are the point: an _explicit_ restore that finds nothing is an error you want surfaced; a _deploy-or-restore_ that finds nothing should let the app start with a fresh volume.

On `Continue` with no snapshot, Kopiur **actually provisions the empty volume** — it doesn't just mark the `Restore` complete. For `target.populator: {}` it provisions an empty prime PVC and rebinds it to the claiming PVC (so a workload pod can bind and start); for `target.pvc` it creates the empty PVC. The "no snapshot ⇒ empty" decision is **pinned to `status.resolved` (`resolution: NoSnapshot`) once and never re-resolved**, so a snapshot that appears *later* can never silently restore over a volume the app is already using — re-create the `Restore` if you want to pick up a new snapshot. The Restore reports `Completed` with `Resolved=True reason=NoSnapshotContinue`.

The empty-volume path applies to `fromPolicy`/`identity` sources on **every** backend — the restore Job resolves the source in-place, so an object-store `fromPolicy` with no snapshot comes up empty under `Continue` just like a filesystem one.

### `waitTimeout` — wait before giving up

`waitTimeout` (a Go-style duration, e.g. `5m`) opens a grace window, anchored at the Restore's **creation**, during which "no matching snapshot yet" means *wait and re-check* instead of giving up. `onMissingSnapshot` applies only once the window closes. Use it when the Restore may be applied before the thing that produces its snapshot — a schedule about to fire, a GitOps apply ordering, a populator claim racing the first backup.

Where the waiting happens depends on the source. A `snapshotRef` (waiting for the referenced `Snapshot` CR to gain an id) re-checks **in the controller** (~15 s, surfacing `Resolved=False reason=WaitingForSnapshot` on the conditions). A `fromPolicy`/`identity` source re-lists the repository **inside the restore Job** (the same mover run that does the restore, so it works on every backend) — the `Restore` shows `Restoring` for that window rather than a per-poll condition. Either way the window is measured from creation, so it's bounded even across controller restarts or Job pod retries. Because the wait runs inside the Job for the latter, `waitTimeout` must be shorter than the Job's `failurePolicy.activeDeadlineSeconds` — the admission webhook rejects a Restore that sets both with `waitTimeout` ≥ the deadline.

## Mover, cache & failure policy

A restore writes data **into** a PVC, so the mover that does the writing has the same concerns a backup's does. `Restore.spec.mover` is the same `MoverSpec` a `SnapshotPolicy` exposes, and `Restore.spec.failurePolicy` mirrors `Snapshot.spec.failurePolicy`. See the full manifest in [example 12](examples.md#example-12--restore-mover-cache--failure-policy).

```yaml
spec:
    mover:
        securityContext: { runAsUser: 1000, runAsGroup: 1000, ... } # CONTAINER: own the restored files
        podSecurityContext: { fsGroup: 1000 } # POD: make a fresh volume writable
        # inheritSecurityContextFrom: { workloadSelector: { podSelector: {...} } }  # ...or copy from a live pod (restore: use workloadSelector, not pvcConsumer)
        cache: { capacity: 16Gi, mode: Persistent, contentCacheSizeMb: 10000 }
    failurePolicy:
        backoffLimit: 4
        activeDeadlineSeconds: 7200 # cap a RUNNING restore (default 48h backstop)
        podStartupDeadlineSeconds: 300 # fail a restore mover that can't START in 5m (default 300)
```

- **`mover.securityContext`** — run the restore mover (its **container**) as the UID/GID that should own the restored files. Without it the mover runs as the hardened default (UID 65532), which may write files the app can't read. This is the fix for "the restore mover had no UID control".
- **`mover.podSecurityContext.fsGroup`** — a **pod**-level `fsGroup` that makes a freshly-provisioned target volume group-writable, so an **unprivileged** `runAsUser: 1000` mover can populate it on restore (instead of needing a root mover just to write the new volume). The headline case for restoring into a brand-new PVC as non-root. See [Security context → fsGroup](security-context.md).
- **`mover.inheritSecurityContextFrom`** — instead of hard-coding them, copy **both** the container `securityContext` **and** the pod-level `securityContext` (so the restore mover gets the app's UID *and* its `fsGroup`) from a live workload pod. On a Restore, use **`workloadSelector: { podSelector, container? }`** to name the pod that will *read* the restored data. The **`pvcConsumer`** form is **backup-only** — it derives the workload from a backup *source* PVC, which a restore doesn't have (the target's consumer may not exist yet), so the webhook **rejects `pvcConsumer` on a `Restore`**. Combines with `securityContext`/`podSecurityContext`: they are the higher merge layer, so an explicit field overrides the inherited one, and they stand in alone when no workload pod resolves. The condition `RestoreSecurityContextCompatible` reports (positively) when the future consumer will be able to read what the mover writes. See [Security context → Inherit it from the workload](security-context.md#2-inherit-it-from-the-workload) and [example 18](examples.md#example-18--inherit-the-mover-security-context-from-a-workload).
- **`mover.cache`** — size the kopia cache for a large restore. `mode: Ephemeral` (default) gives a fresh per-run volume sized by `capacity` (or an `emptyDir` when unset); `mode: Persistent` keeps a controller-owned cache PVC and reuses it across runs for a warm cache. `contentCacheSizeMb` / `metadataCacheSizeMb` pass kopia's `--content/metadata-cache-size-mb` budgets. A repository's `moverDefaults.cache` are inherited and overlaid by `mover.cache`.
- **`failurePolicy`** — the restore Job's `backoffLimit`, `activeDeadlineSeconds`, and `podStartupDeadlineSeconds`. Absent uses the defaults (2 retries; a 48h `activeDeadlineSeconds` backstop so a *running* Job can't linger forever; a 5-minute `podStartupDeadlineSeconds` so a restore mover that can't **start** — bad image, unschedulable, impossible `securityContext` — fails fast with `MoverPodWedged` instead of hanging). The two deadlines are explained in [Backups → `failurePolicy`](backups.md#failurepolicy--retry--deadline-for-the-mover-job).

/// warning | An elevated restore mover needs the namespace to opt in

A restore mover that runs as root (`runAsUser: 0`), with added capabilities, or `privilegedMode: true` — including one **inherited** from a root workload pod — is refused with `MoverPermitted=False` until the restore's namespace opts in, exactly like a backup. Opt the namespace in by applying a `Namespace` carrying the opt-in annotation:

```yaml
--8<-- "deploy/examples/privileged-mover-namespace.yaml"
```

```console
$ kubectl apply -f privileged-mover-namespace.yaml
```

…or imperatively: `kubectl annotate namespace <ns> kopiur.home-operations.com/privileged-movers=true`.

See [Permissions](permissions.md) for how to choose the UID/GID and when a privileged mover is warranted.

///

## Deploy-or-restore (GitOps)

The headline pattern: commit one bundle and apply it to **any** cluster. On a fresh cluster pointed at an existing repository, the PVC restores the latest snapshot before the app starts; on a brand-new repository, the PVC comes up empty and is backed up going forward. No "is this a new install or a recovery?" branching.

The mechanism is a **passive `Restore`** (`source.fromPolicy`, `target.populator: {}`, `onMissingSnapshot: Continue`) consumed by a PVC's `dataSourceRef` as a volume populator. The full manifest is [example 05](examples.md#example-05--deploy-or-restore-gitops). The same `Restore` keeps serving claims for its whole life — tear the app's PVC down and stand it back up (a `kubectl delete` + re-apply, a namespace rebuild, a migration) and the new PVC is populated again from the pinned snapshot, no change to the `Restore` needed (see [the reusability note above](#populator---passive-populator-mode)).

/// note | Kubernetes ≥ 1.24

The volume-populator handshake relies on the `AnyVolumeDataSource` feature (GA from 1.24). The optional `volume-data-source-validator` surfaces a malformed `dataSourceRef` as an event instead of a silently-stuck PVC.

///

## Restoring a snapshot Kopiur didn't create

Snapshots written by a foreign kopia client, or predating your install, are materialized as **discovered** `Snapshot` CRs (`origin=discovered`, forced `deletionPolicy: Retain`) in the repository's namespace. Restore them two ways (see [example 07](examples.md#example-07--restore-a-discovered-backup)):

- **(A)** reference the discovered `Snapshot` CR with `source.snapshotRef` — same as any other backup; or
- **(B)** use `source.identity` with the raw kopia identity (requires `spec.repository`), for snapshots that aged out of the catalog.

```console
$ kubectl get snapshots -n backups -l kopiur.home-operations.com/origin=discovered
```

## Watching a restore

```console
$ kubectl get restore -n billing -w
NAME              PHASE        AGE
postgres-verify   Resolving    2s
postgres-verify   Restoring    9s
postgres-verify   Completed    41s
```

Phases: `Pending` → `Resolving` (pinning the source snapshot) → `Restoring` (mover writing data) → `Completed` / `Failed`. Live byte/file progress is in `status.progress`; the resolved snapshot and target PVC are in `status.resolved` / `status.target`. If it won't progress, `kubectl describe restore <name>` shows the reason on the conditions and as an Event — see [Troubleshooting](troubleshooting.md).

Every phase write also carries the [kstatus](gitops.md) conditions: `Completed` ⇒ `Ready=True`, `Failed` ⇒ `Stalled=True` (a Restore is one-shot — fix the cause and create a new Restore), anything in flight ⇒ `Reconciling=True`. So `kubectl wait --for=condition=Ready restore/<name>` and Flux/Argo health checks gate on a restore the same way they do on every other kopiur kind, and domain conditions (`Resolved`, `MoverPermitted`, `CredentialsAvailable`, `AwaitingClaim`) survive phase transitions alongside them.

## Credentials in a fresh namespace — `credentialProjection`

A restore mover loads the repository credentials via `envFrom` from a Secret **in its own namespace**. Restoring into a namespace that has never run a backup (disaster recovery, a clone target) won't have one. Set `credentialProjection.enabled: true` and the operator copies the referenced repository's Secret into the mover's namespace for the run — owned by the `Restore`, garbage-collected with it ([example 17](examples.md#example-17--restore-from-a-shared-repo-projection)):

```yaml
spec:
    repository: { kind: ClusterRepository, name: platform-shared }
    credentialProjection:
        enabled: true # off by default; needs Helm features.credentialProjection.enabled
```

It's **off by default** (cross-namespace Secret copying is opt-in) and needs the operator's Secret-projection RBAC (Helm `features.credentialProjection.enabled`). The alternative is placing the Secret in the namespace yourself. See [Movers → credential projection](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos).

## Field reference — every value, and when to change it

The full `Restore` surface, with the examples that exercise each. `source` is the only required field.

| Field | What it does | When to set it |
| --- | --- | --- |
| `repository` | The repository to read from (`{ kind, name, namespace? }`). Inferred from `source` for `snapshotRef`/`fromPolicy`; **required** for `identity`. | Cross-namespace / cluster restores, or any `identity` source. ([13](examples.md#example-13--restore-by-raw-kopia-identity), [16](examples.md#example-16--cross-namespace-clone-restore)) |
| `source.snapshotRef` | Restore a specific `Snapshot` CR (`{ name, namespace? }`). | The common case — you picked a row from the catalog. ([03](examples.md#example-03--restore-by-picking-a-snapshot), [16](examples.md#example-16--cross-namespace-clone-restore)) |
| `source.fromPolicy` | Resolve via a `SnapshotPolicy`'s identity (`{ name, namespace?, asOf?, offset? }`). | No `Snapshot` CR (deploy-or-restore), or point-in-time (`asOf`) / positional (`offset`) recovery. ([05](examples.md#example-05--deploy-or-restore-gitops), [14](examples.md#example-14--point-in-time--offset-restore)) |
| `source.identity` | Raw kopia identity (`{ username, hostname, sourcePath?, snapshotID?, asOf?, offset? }`). | Foreign / aged-out snapshots; needs `repository`. ([13](examples.md#example-13--restore-by-raw-kopia-identity)) |
| `target.pvc` | Create a new PVC and restore into it (`{ name, storageClassName?, capacity?, accessModes? }`). | The safe default — restore beside the original, verify, cut over. ([03](examples.md#example-03--restore-by-picking-a-snapshot)) |
| `target.pvcRef` | Restore into an **existing** PVC (`{ name }`). | In-place restore (scale the app down first). ([15](examples.md#example-15--in-place-mirror-restore)) |
| `target.populator` | Explicit passive volume-populator source (`populator: {}`). | GitOps deploy-or-restore via a PVC `dataSourceRef`. ([05](examples.md#example-05--deploy-or-restore-gitops)) |
| `options.enableFileDeletion` | Delete target files not in the snapshot (exact **mirror**); wired to kopia's `--delete-extra`. Default `false` (additive). | A faithful in-place restore — destructive, use deliberately. ([15](examples.md#example-15--in-place-mirror-restore)) |
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
- [Scenarios](scenarios/index.md) — [02 recover lost data](scenarios/recover-lost-data.md), [07 point-in-time rollback](scenarios/point-in-time-rollback.md), [08 clone to another namespace](scenarios/clone-app-to-namespace.md).
- [Examples](examples.md) — [03 by Snapshot](examples.md#example-03--restore-by-picking-a-snapshot), [05 deploy-or-restore](examples.md#example-05--deploy-or-restore-gitops), [07 discovered](examples.md#example-07--restore-a-discovered-backup), [12 mover/cache/failure policy](examples.md#example-12--restore-mover-cache--failure-policy), [13 by identity](examples.md#example-13--restore-by-raw-kopia-identity), [14 point-in-time](examples.md#example-14--point-in-time--offset-restore), [15 in-place mirror](examples.md#example-15--in-place-mirror-restore), [16 cross-namespace](examples.md#example-16--cross-namespace-clone-restore), [17 shared-repo projection](examples.md#example-17--restore-from-a-shared-repo-projection).
