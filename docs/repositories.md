# Repositories & backends

A **`Repository`** is _where_ your snapshots live. It is the one resource that holds the storage backend, the encryption password, and the credentials — everything `SnapshotPolicy`/`Snapshot`/`Restore` need but shouldn't have to repeat. Get this right and the rest of Kopiur just points at it by name.

There are two flavors:

| CRD                     | Scope      | Use it when…                                                                                                                            |
| ----------------------- | ---------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| **`Repository`**        | Namespaced | One namespace owns the backups (the common case). The repo and its credential Secret live together in that namespace.                   |
| **`ClusterRepository`** | Cluster    | A platform team owns one shared repository that **many** tenant namespaces back up to, without each tenant knowing the backend details. |

Both have the **same** backend/encryption/create surface; `ClusterRepository` adds a tenancy gate (`allowedNamespaces`) and per-tenant identity CEL expressions. See [ClusterRepository](#clusterrepository-a-shared-repository) below.

/// tip | One shared repository is the recommended default

Point as many backups as you can at **one** repository. Kopia deduplicates by content hash across every writer, so pooling workloads into a single repository stores their common content once — and each `SnapshotPolicy` writes under its own identity, so their snapshots never collide. See [Recommended: one shared repository](concepts/how-kopia-works.md#recommended-one-shared-repository) for the mechanism and the trade-offs.

///

/// tip | Anatomy of a Repository

```yaml
spec:
    backend: { <one of eight>: { ... } } # WHERE: storage. Exactly one backend.
    encryption: { passwordSecretRef: ... } # the kopia repo password (Secret ref)
    create: { enabled: true, ecc: { ... } } # initialize the repo if absent (default: on)
    bootstrap: { failurePolicy: { ... } } # tune the connect/create Job deadline + retries
    moverDefaults: { ... } # base config for EVERY mover (SC, resources, cache, ...)
    catalog: { ... } # bounds "discovered" snapshot materialization
    maintenance: { ... } # default-managed; see the Maintenance guide
    onNamespaceDelete: Orphan # Orphan (default) | Delete — kubectl delete ns behavior
    mode: ReadWrite # ReadWrite (default) | ReadOnly
    suspend: false # pause connect/bootstrap + maintenance
```

Only `backend` and `encryption` are required. The rest have sane defaults.

///

## The two things every backend needs: identifiers + a Secret

Kopiur deliberately splits a backend into **non-secret connection identifiers** (bucket, endpoint, host, path — these go in the `Repository` spec) and **secrets** (access keys, the encryption password — these live in a Kubernetes `Secret` and are passed to kopia as environment variables, never on the command line or in status). So every object-store backend looks like:

```yaml
spec:
    backend:
        s3:
            bucket: my-backups # ← identifiers in the spec
            auth:
                secretRef:
                    name: repo-creds # ← a Secret holding the access keys
    encryption:
        passwordSecretRef:
            name: repo-creds # ← (can be the same Secret) holding KOPIA_PASSWORD
            key: KOPIA_PASSWORD
```

/// warning | Externally tagged — no `kind:` field

A backend is selected by **which key you set** (`backend.s3`, `backend.azure`, …), not a `kind:` discriminator. `backend: { kind: S3 }` will **not** admit. This is the type-safety design: exactly one backend is representable. (See the [API conventions](dev/api-conventions.md).)

///

### Credential Secret keys by backend

The mover reads these **well-known keys** from the Secret you reference and feeds them to kopia. Put your credentials under these exact key names. `KOPIA_PASSWORD` (the repository encryption password) is required for **every** backend. See [Backend configuration](backends/index.md#credential-keys-at-a-glance) for the full per-backend setup and the env-vs-file credential detail.

| Backend        | Secret keys the mover reads                                               | Notes                                                                                                       |
| -------------- | ------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| **S3**         | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, _(opt)_ `AWS_SESSION_TOKEN` | Works for AWS and any S3-compatible store (MinIO, RustFS, Ceph RGW).                                        |
| **Azure**      | `AZURE_STORAGE_KEY` **or** `AZURE_STORAGE_SAS_TOKEN`                      | Account name can come from `spec.backend.azure.storageAccount`.                                             |
| **GCS**        | `KOPIA_GCS_CREDENTIALS` (service-account JSON)                            | The mover writes the JSON to a file and passes `--credentials-file`.                                        |
| **B2**         | `B2_KEY_ID`, `B2_KEY`                                                     | Backblaze application key ID + key.                                                                         |
| **SFTP**       | `KOPIA_SFTP_KEY_DATA`, `KOPIA_SFTP_KNOWN_HOSTS`                           | Key-based auth; the mover writes both to files (`--keyfile`/`--known-hosts`). See [SFTP](backends/sftp.md). |
| **WebDAV**     | `KOPIA_WEBDAV_USERNAME`, `KOPIA_WEBDAV_PASSWORD`                          | HTTP basic auth, via `auth.secretRef`.                                                                      |
| **rclone**     | `KOPIA_RCLONE_CONFIG` (the `rclone.conf`)                                 | Referenced by `backend.rclone.configSecretRef`, not `auth`.                                                 |
| **gdrive**     | `KOPIA_GDRIVE_CREDENTIALS` (service-account JSON)                         | Native Google Drive; referenced by `backend.gdrive.credentialsSecretRef`. Experimental — see [Google Drive](backends/gdrive.md). |
| **filesystem** | _(none — local path)_                                                     | Only `KOPIA_PASSWORD` is needed.                                                                            |

/// note | ClusterRepository Secret references need a namespace

Because a `ClusterRepository` is cluster-scoped it has no namespace of its own, so **every** Secret reference in it (`auth.secretRef`, `encryption.passwordSecretRef`) **must** carry an explicit `namespace:` (webhook-enforced). The credential Secret also has to exist in each **workload** namespace a mover runs in — either place it there yourself, or (recommended for a shared repo) turn on [**credential projection**](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) and let Kopiur copy it for you.

///

## The nine backends

Kopiur supports nine backends; each is selected by the `spec.backend.<key>` you set.

| Backend                                                               | `spec.backend` key | Where it goes             |
| --------------------------------------------------------------------- | ------------------ | ------------------------- |
| Amazon S3 + any S3-compatible store (MinIO, RustFS, Ceph RGW, Wasabi) | `s3`               | a bucket                  |
| Azure Blob Storage                                                    | `azure`            | a container               |
| Google Cloud Storage                                                  | `gcs`              | a bucket                  |
| Backblaze B2                                                          | `b2`               | a bucket                  |
| Filesystem (NAS/PVC or inline NFS)                                    | `filesystem`       | a mounted PVC or NFS path |
| SFTP                                                                  | `sftp`             | a path on an SSH server   |
| WebDAV                                                                | `webDav`           | a collection URL          |
| rclone (everything else)                                              | `rclone`           | any rclone remote         |
| Google Drive (native, experimental)                                   | `gdrive`           | a Drive folder            |

/// tip | Per-backend setup lives on its own page

For each backend — the **provider prerequisites**, the exact **Secret keys**, the knobs you'll actually change, and a **complete apply-ready manifest** (Secret + Repository in one file) — see [**Backend configuration**](backends/index.md). That page is the hands-on cookbook; this one is the concepts.

///

## Encryption and repository creation

```yaml
spec:
    encryption:
        passwordSecretRef:
            name: repo-creds
            key: KOPIA_PASSWORD # which key inside the Secret holds the password
    create:
        enabled: true # create the repo if it doesn't exist yet (default: true)
        # All of the below are consulted ONLY at creation time, then fixed forever
        # (webhook- AND apiserver-immutable: editing them is rejected):
        encryption: AES256-GCM-HMAC-SHA256
        splitter: DYNAMIC-4M-BUZHASH
        hash: BLAKE2B-256-128
        ecc: # Reed-Solomon parity guarding blobs against backend bit-rot
            algorithm: REED-SOLOMON-CRC32
            overheadPercent: 2
```

/// note | Create-time settings are immutable

`create.{splitter,hash,encryption,ecc}` and the pinned identity are fixed at repository creation. Editing them is **rejected** (by the webhook and by CRD `x-kubernetes-validations` transition rules) with an actionable message — create a new `Repository` instead of mutating these.

The `encryption.passwordSecretRef` is **not** in this set: you may rename or repoint the Secret freely (e.g. a GitOps secret rename) as long as it still resolves to the **same password value** — kopia bakes only the resolved value into the repository format, not the reference. Point it at a *different* password and kopia simply fails to open the repository at connect time (a recoverable runtime error, not an admission rejection).

///

/// warning | Lose the password, lose the backups

`KOPIA_PASSWORD` encrypts the repository. kopia cannot decrypt without it — there is no recovery. Store it outside the cluster (password manager / external secret store), and back up the Secret itself.

///

/// note | `create.enabled` defaults to **on** — and that's safe

Repository create/connect is **idempotent**: every bootstrap *connects first* and only creates when the backend holds no repository yet (see [Safe by construction](#safe-by-construction)). So creating-on-first-use is the least-surprise default — a genuinely-absent repository is initialized instead of erroring, and pointing `create` at one that already exists just adopts it. Omitting `create` entirely (or `create: {}`) is the same as `create.enabled: true`.

Set **`create.enabled: false`** for a strictly read-only or externally-managed repository the operator must never create. With creation disabled, a typo in `bucket`/`endpoint` surfaces as a connect failure (`Bootstrapped=False`, `reason: RepositoryNotInitialized`) instead of spinning up a new empty repository — at the cost of opting in for every genuinely-new repo. (A wrong-password connect fails `AuthFailure` and **never** recreates over existing data — see [Safe by construction](#safe-by-construction) below.)

///

### Safe by construction

`create.enabled: true` does **not** mean "always create". Every bootstrap **connects first** and only creates as a fallback, gated so that an existing repository is never overwritten:

- **A repository already exists and the password is correct** → kopiur *connects and adopts* it (and materializes any snapshots already in the store as `discovered` Snapshots). No creation happens.
- **A repository already exists but the password is wrong** → connect fails `AuthFailure`; kopiur **does not** create (that would risk a second repository or mask the wrong-password error). The `Repository` goes `Failed` and the existing repository is left untouched.
- **No repository exists** → kopiur creates one (only because `create.enabled` is on). As a final backstop, kopia's own `repository create` refuses to overwrite an existing repository, so even a misclassified connect cannot clobber your data.

So enabling `create.enabled` for a repository that turns out to already exist is safe: kopiur will adopt it, not re-initialize it.

## `bootstrap` — tuning the connect/create Job

Object-store and volume-backed repositories connect (and, with `create`, create) in a short-lived **bootstrap Job** the operator cannot run in-process. By default that Job is bounded to **120s** so a pod that never schedules (missing mover ServiceAccount, image-pull failure) becomes terminal-`Failed` and surfaces an actionable Event instead of hanging. A valid-but-slow backend can need longer — most commonly an [rclone](backends/rclone.md) remote whose metadata/indexes load through kopia's embedded `rclone serve` bridge.

`spec.bootstrap.failurePolicy` reuses the same shape as a recipe's `failurePolicy`:

```yaml
spec:
    bootstrap:
        failurePolicy:
            activeDeadlineSeconds: 600 # wall-clock cap for the bootstrap Job (default 120)
            backoffLimit: 1 # retries before the Job is marked failed (default 2)
```

| Field                              | Default | What it controls                                                                 |
| ---------------------------------- | ------- | -------------------------------------------------------------------------------- |
| `failurePolicy.activeDeadlineSeconds` | `120`   | Wall-clock seconds before the bootstrap Job is killed and marked failed.        |
| `failurePolicy.backoffLimit`       | `2`     | Pod retries before the Job is failed.                                            |

/// note | `podStartupDeadlineSeconds` is not honored for bootstrap

`failurePolicy` is the shared type, so it also carries `podStartupDeadlineSeconds` — but the bootstrap Job does not consume it (only backup/restore/maintenance movers do). Use `activeDeadlineSeconds` to bound a slow bootstrap.

///

For an rclone-specific connect that's slow *before* metadata even loads, also raise [`backend.rclone.startupTimeout`](backends/rclone.md#fields-reference-backendrclone) (kopia's wait for `rclone serve` to come up).

## The catalog — discovered snapshots

A kopia repository can hold snapshots Kopiur didn't produce: an adopted repository's history, a workstation `kopia` CLI, another cluster, a cron job. The **catalog scan** surfaces them as `Snapshot` CRs with `origin: discovered`, so they show up in `kubectl get snapshots`, in `kubectl kopiur snapshots list`, and as restore sources — no timestamp guessing.

```console
$ kubectl get snapshots -l kopiur.home-operations.com/origin=discovered
```

How it behaves (all of this is automatic — there is nothing to install):

- **Discovered means "not produced through this Repository CR".** Snapshots Kopiur itself takes via your `SnapshotPolicy`s already have their own `Snapshot` CRs and are never duplicated as discovered rows.
- **Discovered rows are forced `deletionPolicy: Retain`.** Deleting a discovered `Snapshot` CR deletes only the CR — Kopiur **never** deletes a kopia snapshot it didn't create. (And because the row mirrors repository state, it reappears on the next refresh; to keep rows away permanently, bound them with `catalog.retain` below.)
- **An initial scan always runs** (on first bootstrap and again on any spec change), so adopting a repository surfaces its existing history immediately.
- **Repeated re-scanning is opt-in.** Set `catalog.periodicRefresh: true` to keep re-scanning every `catalog.refreshInterval` (default **1h**, minimum `30s`) so snapshots written out-of-band *after* adoption keep appearing — and rows whose snapshot was pruned repository-side are expired. **Off by default**, because for object-store / volume-backed repos each re-scan re-runs the (self-cleaning) bootstrap Job; leaving it off means the repository bootstraps once and isn't re-run on a timer. When on, the interval is the *only* thing that drives re-scans — a Job removed early by its `ttlSecondsAfterFinished` does **not** trigger an extra scan, so `refreshInterval: 24h` gives one scan a day regardless of the Job TTL.
- **The row carries the real data**: the kopia snapshot ID, the foreign `username@hostname:path` identity, the snapshot's timing and logical size.

The knobs, all under `spec.catalog`:

```yaml
spec:
    catalog:
        periodicRefresh: true # opt in to repeated re-scans (default false = scan once)
        refreshInterval: 1h # how often to re-scan when periodicRefresh is on (default 1h, min 30s)
        retain:
            perIdentity: 100 # keep the newest N rows per username@hostname:path (0 = no rows)
            maxAgeDays: 90 # no rows for snapshots older than this
        fallbackNamespace: backups # ClusterRepository only — see below
```

`retain` bounds the **CR rows, never the data**: a row "expired" by `perIdentity`/`maxAgeDays` is just a deleted CR — the kopia snapshot stays in the repository and remains restorable via [`Restore.source.identity`](restores.md#restoring-a-snapshot-kopiur-didnt-create).

/// note | Where a ClusterRepository puts discovered Snapshots

A namespaced `Repository` materializes rows in its own namespace. A **`ClusterRepository`** places each row in the namespace named by the snapshot identity's **hostname** — when that namespace exists and passes the `allowedNamespaces` gate — so an adopted shared repository's snapshots land next to the workloads they belong to. Identities whose hostname maps to no allowed namespace go to `catalog.fallbackNamespace`; with no fallback configured they are skipped, and the `ClusterRepository` gets a Warning Event (`DiscoveredSnapshotUnplaced`) naming the hostnames and the fix.

///

## `moverDefaults` — one place to configure every mover

A repository spawns movers for **everything** — bootstrap (connect/create), backup, restore, maintenance. `moverDefaults` is the single base they all inherit; a per-recipe `mover` block (on `SnapshotPolicy`/`Restore`/`Maintenance`) overlays it **field-wise** (the recipe wins, the default fills, the hardened security baseline sits underneath — so a partial override can only tighten, never drop `drop:[ALL]`/seccomp).

```yaml
spec:
    moverDefaults:
        securityContext: # container SC — runAsUser/runAsGroup, caps, seccomp
            runAsUser: 1000 # the UID every mover runs as
            runAsGroup: 1000 # the GID every mover runs as
        podSecurityContext: # pod SC — notably fsGroup
            fsGroup: 1000 # make fresh restore volumes group-writable, set once here
        resources: { requests: { cpu: 250m, memory: 512Mi } }
        cache: # kopia cache backing every mover (capacity/class/budgets)
            capacity: 10Gi
            storageClassName: fast-ssd
        scratch: # default size/class for the deep-verify scratch (restore-test) PVC
            capacity: 100Gi # inherited by SnapshotPolicy verification.deep
            storageClassName: fast-ssd
        nodeSelector: { kubernetes.io/arch: amd64 }
        tolerations: [{ key: backup, operator: Exists }]
        affinity: { ... }
        sourceColocation: { mode: Auto } # avoid RWO Multi-Attach (default Auto)
        ttlSecondsAfterFinished: 3600 # finished mover Jobs self-GC
        throttle: # cap kopia's bandwidth/ops so a run doesn't saturate the link
            uploadBytesPerSecond: 10485760
            downloadBytesPerSecond: 10485760
```

/// tip | Set the mover UID/GID once, for every mover

Set the UID/GID **once** on `moverDefaults.securityContext.runAsUser/runAsGroup` (and `podSecurityContext.fsGroup`), and every mover the repository spawns — **including the bootstrap (connect/create) Job** — inherits it. That means a filesystem/NFS repository on a directory not owned by `65532` is bootstrappable with no special-case knob: the bootstrap mover runs as the UID you set here. A per-recipe `mover` block can still tighten any of these for an individual `SnapshotPolicy`/`Restore`/`Maintenance`. See [example 09](examples.md#example-09--mover-uidgid--permissions) and [Permissions](permissions.md).

`podSecurityContext.fsGroup` already **defaults to `65532`** (the mover image's GID), so the operator-managed kopia cache is writable out of the box — you only set it here to match a non-default mover UID. Note `fsGroup` can't fix a root-squashed **NFS** cache StorageClass; keep `moverDefaults.cache` unset (node-local `emptyDir`) or use a block class for a sized cache (see [Security context](security-context.md#the-default-hardened-context)).

`moverDefaults.scratch` is the repo-level default for the **deep-verification** scratch volume (the throwaway restore target a `verification.deep` restore-test writes into, then discards). It's the scratch sibling of `cache`: a `SnapshotPolicy`'s `verification.deep.{capacity,storageClassName}` overlays it field-wise, so you set the scratch size/class **once** here instead of repeating it on every policy. Unlike the cache, scratch has **no `mode`** — it's always ephemeral and discarded after each run. `storageClassName` only applies when a `capacity` is set (an `emptyDir` has no StorageClass); a class with no effective capacity is a no-op and the operator flags it via the `ScratchStorageClassIgnored` condition on the `SnapshotPolicy`. See [verification](backups.md#verification--prove-the-snapshots-are-restorable).

/// note | Verification movers inherit `moverDefaults` too

The `quick`/`deep` verification movers a `SnapshotPolicy` spawns inherit the **whole** `moverDefaults` — security context, resources, scheduling, TTL, throttle, **and** the kopia `cache` (size/class + budgets) — exactly like backup and restore movers. The one exception: a `cache.mode: Persistent` is **coerced to a fresh per-run ephemeral cache** for verification, because a verify run must never attach the backup's warm `ReadWriteOnce` cache PVC (a Multi-Attach race). Set `moverDefaults.cache` once and your verifications are sized/placed without any per-policy config.

///

### `sourceColocation`: avoid the RWO Multi-Attach error

A `ReadWriteOnce` (RWO) PVC can only be **attached to one node at a time**, though it can be mounted by several pods **on that same node**. When your app pod already holds an RWO PVC on node A and Kopiur's mover lands on node B, the kubelet on B can't attach the volume and the mover pod is stuck with a `Multi-Attach error` (it never starts).

By default Kopiur prevents this: it finds the node the source PVC is attached to and **pins the mover there** (a required `kubernetes.io/hostname` nodeAffinity, plus the holder pod's tolerations), so the mover co-locates with your workload. This is automatic — you don't need to set anything.

```yaml
spec:
    moverDefaults:
        sourceColocation:
            mode: Auto # Auto (default) | Required | Disabled
```

- **`Auto`** (default): pin an RWO source/destination PVC to the node it's attached to when that node is discoverable (via the consuming pod, the bound PV's `nodeAffinity`, or a CSI `VolumeAttachment`). `ReadWriteMany`/`ReadOnlyMany` volumes and unheld RWO volumes are scheduled freely (no Multi-Attach risk). A `ReadWriteOncePod` volume **held by a running pod** fails with guidance — a second pod can't mount it even on the same node; use `copyMethod: Snapshot` or scale the workload down (see [PVC access modes & RWOP](access-modes.md)).
- **`Required`**: like `Auto`, but if an RWO PVC's node can't be determined, the run **fails** with an actionable error instead of scheduling freely. Use when an RWO source must never be backed up from the wrong node.
- **`Disabled`**: never compute a node pin; the mover uses only the explicit `nodeSelector`/`affinity`/`tolerations`. The pre-fix behavior — an escape hatch for topologies that manage placement themselves.

/// note | Discovery needs read access to PVs and VolumeAttachments

The fallbacks read cluster-scoped `persistentvolumes` and `storage.k8s.io/volumeattachments`. The shipped RBAC already grants this; if you run a hand-trimmed ClusterRole, add `get,list,watch` on both, or co-location silently degrades to "no pin found". See [RBAC](rbac.md).
///

/// warning | Application consistency

Co-location lets the mover read a **live** volume — that snapshot is crash-consistent at best. For application-consistent backups (databases), quiesce the app with pre/post [snapshot hooks](backups.md).
///

///

## `scheduleDefaults` — set the cron timezone once

Every cron-driven consumer of a repository (`SnapshotPolicy.spec.verification`,
`RepositoryReplication.spec.schedule`, `Maintenance.spec.schedule`, and
`SnapshotSchedule.spec.schedule` — the recurring-backup cron) takes its own
optional `timezone`. Repeating the same IANA zone on every one of them is
tedious and easy to drift. Set it **once** on the repository instead:

```yaml
spec:
    scheduleDefaults:
        timezone: America/New_York # IANA name, validated at admission
```

Precedence (resolved at reconcile time, not admission-pinned): the consuming
cron's own `timezone` wins when set, else `scheduleDefaults.timezone` here,
else UTC. `Maintenance` already cascades per-cron → schedule-level before
falling to the repo default, so this becomes the **third and final** level for
it.

/// note | How `SnapshotSchedule` inherits

A `SnapshotSchedule` with no `spec.schedule.timezone` resolves its **target
policy's** repository `scheduleDefaults.timezone` (following `policyRef`, or each
`policySelector` match) at slot-computation time. The resolved zone is recorded in
`status.nextSchedule.timezone`, and editing `scheduleDefaults.timezone` re-triggers
the affected schedules (a repository referent watch) and recomputes the pinned
slot in the new zone — you don't have to touch each schedule.

For a `policySelector` schedule whose matched policies' repositories **disagree**
on the zone, there is no single right answer: the schedule falls back to UTC and
raises a `TimezoneDefaultAmbiguous` condition recommending you set
`spec.schedule.timezone` explicitly.

///

A complete, apply-ready example — a `Repository` with `scheduleDefaults.timezone`,
a `SnapshotPolicy.spec.verification.quick` cron, and a `SnapshotSchedule` that all
inherit it:

```yaml
--8<-- "deploy/examples/29-repo-schedule-timezone.yaml"
```

## `onNamespaceDelete` — what `kubectl delete ns` does to snapshots

`Orphan` (default) or `Delete`. A backup tool must not make deleting a namespace a silent data-loss event, so the default is fail-safe:

| Value | On namespace deletion |
| --- | --- |
| `Orphan` _(default)_ | Release ownership (drop the `Snapshot` finalizers) **without** deleting the kopia snapshots — off-site history survives. |
| `Delete` | Cascade: each `Snapshot`'s own `deletionPolicy` applies (produced snapshots are `kopia snapshot delete`d). Opt-in. |

A namespace delete is only one of **three independent axes** a `Snapshot`'s kopia data can go away through — deleting the `Snapshot` itself, deleting its namespace, or deleting its `SnapshotSchedule`. Each has its own opt-in field, and every one of them defaults to keeping the data:

| Axis | Trigger | `Snapshot` CRs | kopia data, by DEFAULT | Opt into cascading kopia data |
| --- | --- | --- | --- | --- |
| Single `Snapshot` | `kubectl delete snapshot <name>` | Removed once its finalizer clears | Honors *this* Snapshot's own `deletionPolicy` — `Delete` by default for `scheduled`/`manual` (see [Backups → deletionPolicy](backups.md#deletionpolicy--what-happens-to-the-snapshot)) | Set `deletionPolicy: Retain`/`Orphan` on the Snapshot, or `SnapshotPolicy.spec.defaultDeletionPolicy` |
| Namespace | `kubectl delete namespace <ns>` | Finalizers released, CRs removed | **Retained** — this field, `onNamespaceDelete: Orphan` (default, above) | This field, `onNamespaceDelete: Delete` |
| `SnapshotSchedule` | `kubectl delete snapshotschedule <name>` | GC'd via `ownerReference` once finalizers clear | **Retained**, rediscovered as `origin: discovered` — schedule `spec.deletion.onScheduleDelete: Retain` (default), which overrides even a produced Snapshot whose own `deletionPolicy` was `Delete` | Schedule `spec.deletion.onScheduleDelete: Delete` — see [Backups → what happens when the schedule is deleted](backups.md#what-happens-when-the-schedule-is-deleted) |

Whichever axis triggers an actual `kopia snapshot delete` — an EXTERNAL deletion with effective `deletionPolicy: Delete`, as opposed to one of Kopiur's own retention/`failedJobsHistoryLimit` prunes — is additionally subject to the per-repository mass-deletion circuit breaker, next.

## `deletionProtection` — the mass-deletion circuit breaker

A single accidental or automated action — the wrong `SnapshotSchedule` deleted, a bulk namespace cleanup, a mis-scoped `kubectl delete snapshot -l ...` — can queue up far more "legitimate-looking" external Snapshot deletions than any one operator action should be able to erase at once (each is individually authorized: the CR's own `deletionPolicy` really is `Delete`). `deletionProtection` is a per-repository breaker against exactly that: it never slows a normal trickle of deletions, but a sudden wave is HELD until a human explicitly approves it.

```yaml
spec:
    deletionProtection:
        threshold: 25 # default 10; 0 disables the breaker
```

### What trips it

Every reconcile, the operator counts this repository's pending **EXTERNAL, destructive** Snapshot deletions — `metadata.deletionTimestamp` set, effective `deletionPolicy: Delete`, NOT stamped by one of Kopiur's own prunes (below), and not yet acknowledged (below). At or above `threshold`, EVERY one of those deletions is HELD: the Snapshot's finalizer stays, no `kopia snapshot delete` runs, and the CR stays `phase: Deleting` until you act.

### What HELD looks like

- Each held `Snapshot` gets `DeletionHeld=True` (reason `MassDeletionBreaker`) and, on the transition into held, a `SnapshotDeletionHeld` Warning Event.
- The repository (`Repository`/`ClusterRepository`) gets a `MassDeletionHeld=True` (reason `ThresholdExceeded`) condition, recomputed every reconcile from the live pending count.
- The `Snapshot`'s condition/event message carries the exact `kubectl annotate` command to copy — you never have to construct it by hand:

```console
$ kubectl describe snapshot nightly-abc12 -n billing
...
Conditions:
  Type          Status  Reason               Message
  DeletionHeld  True    MassDeletionBreaker  this snapshot's deletion is HELD by the mass-deletion
                                              breaker: 23 pending external destructive deletions for
                                              Repository `nas-primary` are at/above its threshold of
                                              10. No kopia data has been deleted and this Snapshot
                                              keeps its finalizer. To APPROVE this wave (releases
                                              every currently-held deletion for the repository), run:
                                              kubectl -n billing annotate repository/nas-primary
                                              kopiur.home-operations.com/allow-mass-deletion=
                                              "2026-07-16T18:04:11Z" --overwrite. To release THIS
                                              Snapshot alone WITHOUT deleting its kopia snapshot,
                                              annotate it kopiur.home-operations.com/skip-snapshot-
                                              cleanup: "true".
```

### Releasing the wave — copy the value, don't generate it

Run the command the condition/event gave you, **verbatim**:

```console
$ kubectl -n billing annotate repository/nas-primary \
    kopiur.home-operations.com/allow-mass-deletion="2026-07-16T18:04:11Z" --overwrite
```

(A `ClusterRepository` is cluster-scoped: the surfaced command omits `-n` and says `clusterrepository/<name>` instead.)

The annotation's value is an RFC3339 **timestamp**, not a boolean — "I approve every deletion pending up to this instant." A held Snapshot releases iff its own `deletionTimestamp <= ` that value:

- **Copy the value the operator surfaced**, from the condition or the event. It's the newest pending deletion's timestamp, chosen so this one `annotate` releases the WHOLE currently-held wave. A hand-generated value (e.g. from `date`) risks landing earlier than some pending deletions and releasing only part of the wave — the controller only ever echoes back the exact value that works, it does not compute one from an arbitrary input.
- **A stale ack is inert against a LATER wave.** Re-applying the same value later (including one committed to Git) does not pre-approve a fresh wave that starts afterward — nothing requires removing the annotation once it has done its job.
- **A future-dated value is clamped to now** — an ack can never pre-approve deletions that haven't happened yet (a clock-skew guard).
- **An unparseable value is IGNORED** (fail-safe: the breaker stays armed) and raises an `InvalidMassDeletionAck` Warning Event flagging the annotation as unparseable RFC3339.

### `threshold: 0` disables the breaker

Set it when a repository legitimately churns through more external deletions than the default in normal operation (many tenants each pruning by hand, for example) and you accept the risk. There's no per-Snapshot opt-out from the breaker other than this repository-wide switch.

### Kopiur's own prunes always bypass it

GFS retention and `failedJobsHistoryLimit` pruning stamp the `Snapshot` they're about to delete with `kopiur.home-operations.com/pruned-by: retention` or `failed-history` **before** deleting it, and the finalizer reads that as the operator's OWN lifecycle action, never external — it is never held and never counted toward the threshold. This is deliberate: retention has to keep working, at its normal rate, even while a genuinely bulk EXTERNAL wave is being held for review. (A missing or unrecognized value on that annotation is treated as external — fail-safe.)

### The namespace-teardown corner

Opting a repository's [`onNamespaceDelete: Delete`](#onnamespacedelete--what-kubectl-delete-ns-does-to-snapshots) in makes namespace-deletion cascades an EXTERNAL destructive deletion exactly like any other — the breaker is deliberately owner/axis-independent, so a namespace teardown that would delete more than `threshold` snapshots is HELD too, same ack flow as above. The default `onNamespaceDelete: Orphan` path never calls `kopia snapshot delete` at all, so it never interacts with the breaker.

### Escape hatches

- **Release one Snapshot without deleting its kopia data**: annotate it `kopiur.home-operations.com/skip-snapshot-cleanup: "true"` — the same per-CR lever documented in [Backups → the escape hatch](backups.md#what-delete-needs-to-succeed); it overrides the breaker just like it overrides everything else. The kopia snapshot survives and is rediscovered as `origin: discovered` later.
- **Release the whole wave**: the ack annotation above.

A complete, apply-ready example combining a repository's `deletionProtection.threshold` with a schedule's `onScheduleDelete` cascade opt-in:

```yaml
--8<-- "deploy/examples/34-schedule-deletion-protection.yaml"
```

See also: [`kopiur_snapshot_deletions_held` / `_pending_external`](dev/observability.md) — the gauges behind an alert on a wave forming, and the batched execution of an actually-approved deletion wave in [Backups → how a deletion actually runs](backups.md#how-a-deletion-actually-runs--batched-not-one-job-per-snapshot).

## `mode` — ReadWrite or ReadOnly

`mode: ReadWrite` (default) or `ReadOnly`. A `ReadOnly` repository connects read-only and serves **restores only** — the operator refuses backup Jobs and skips maintenance projection. Use it to decommission a backend or migrate between repositories without any risk of writes.

## `parameters` — mutable kopia repository parameters

`spec.parameters.epoch` tunes kopia's epoch manager, which decides how quickly index blobs become compactable. Reach for it when your repository sits at thousands of index blobs (`IndexBlobHealth=False` / `TooManyIndexBlobs`) *even though maintenance is running* — kopia cannot compact a blob until its epoch closes, and an epoch cannot close before `minDuration` (24h by default) no matter how many blobs pile up.

```yaml
spec:
  parameters:
    epoch:
      minDuration: 6h # kopia's default is 24h
```

Every field is optional and kopiur has no defaults of its own: **absent means "leave kopia's current value alone"**, so a repository that declares nothing here is untouched by this. The corollary is that *removing* a value does not restore kopia's default — it leaves the repository at whatever you last applied.

kopiur applies these only when they drift from what the repository reports (the call invalidates other kopia clients' cached format blob), and mirrors what it observes into `status.parameters.epoch` — so a value that failed to apply is visible as a mismatch rather than as silence. Not available on `mode: ReadOnly`, which kopia refuses to accept repository-wide writes on.

Full field set and rationale: [Maintenance → when maintenance is running and the count still won't fall](maintenance.md#when-maintenance-is-running-and-the-count-still-wont-fall).

## `suspend` — pause a repository

`suspend: true` pauses connect/bootstrap and maintenance projection declaratively, without deleting the `Repository`. Surfaced via a condition. `suspend` is consistent across `Repository`/`ClusterRepository`/`SnapshotPolicy`/`RepositoryReplication`.

## `server` — the kopia web UI

`spec.server` runs kopia's built-in HTML UI in a `Deployment` behind a `Service`, so you can browse snapshots and policies and do ad-hoc restores. Presence of the block enables it (there is no `enabled` bool), and it defaults to operator-minted credentials on an in-cluster `ClusterIP` Service.

/// warning | The UI has no read-only mode

Anyone who can reach the UI can read, create, and **delete** backups, and the server pod holds the repository decryption key. Treat exposing it like exposing the repository itself.

///

Full guide — auth modes, exposing the Service, the ReadWriteMany requirement for filesystem backends, and status — in **[Web UI (kopia server)](server.md)**.

## Watching a repository

```console
$ kubectl get repository -n demo
NAME      PHASE   BACKEND   AGE
primary   Ready   S3        4m

$ kubectl describe repository primary -n demo # Conditions + Events explain Pending/Failed
```

Phases: `Pending` → `Initializing` → **`Ready`** (healthy). `Degraded` means reachable but a sub-operation (e.g. maintenance) is failing; `Failed` means connect/create failed — the actionable reason is on the conditions. `SnapshotPolicy`/`Snapshot`/`Restore`/`Maintenance` all wait for `Ready` before doing anything, so this is the first thing to check when a backup won't start.

## ClusterRepository: a shared repository

A `ClusterRepository` is the same backend/encryption surface, made cluster-scoped and shared. A platform team defines it once; tenant namespaces reference it by name (`repository: { kind: ClusterRepository, name: … }`) without seeing the backend or credentials. One extra field makes cross-namespace sharing safe (`allowedNamespaces`, below) — `identityDefaults` isn't actually ClusterRepository-exclusive: a namespaced `Repository` has it too (it's documented here because a shared repo is where you reach for it most), for the same reason — many `SnapshotPolicy`s, or more than one cluster ([`cluster`](#identitydefaultscluster--sharing-one-repository-across-clusters) below), writing to one repository.

### `allowedNamespaces` — who may use it (ClusterRepository only)

A tenancy gate, enforced on every consumer CR (externally tagged — exactly one form):

```yaml
spec:
    allowedNamespaces: { list: ["billing", "media", "wiki"] } # explicit names
    # or:  allowedNamespaces: { selector: { matchLabels: { backups: "yes" } } }
    # or:  allowedNamespaces: { all: true } # any namespace
```

### `identityDefaults` — per-tenant identity (CEL)

kopia records every snapshot under `username@hostname:path`. For a shared repo you usually want each tenant's snapshots distinguishable. `identityDefaults` are **CEL expressions** (`*Expr`, the kromgo `valueExpr`/`colorExpr` convention): syntax, return type, and variable scope are **validated at admission** (`kubectl apply` rejects a bad expression immediately), but the expression is actually **rendered against the live consumer on every reconcile** — it is not evaluated once and pinned. A consumer's explicit `spec.identity` always wins.

```yaml
spec:
    identityDefaults:
        hostnameExpr: "namespace"
        usernameExpr: "namespace + '-' + policyName"
```

For namespace `billing` + policy `postgres-data`, that resolves to `billing-postgres-data@billing:/pvc/…`.

The CEL **environment** is the consuming `SnapshotPolicy`'s metadata:

| Variable | Type | Is |
| --- | --- | --- |
| `namespace` | string | the SnapshotPolicy's namespace |
| `policyName` | string | the SnapshotPolicy's name |
| `labels` | map | `metadata.labels` |
| `annotations` | map | `metadata.annotations` |
| `cluster` | string | `identityDefaults.cluster` (below), or `""` when unset |

Each `*Expr` must return a **string**. Conditionals and map access come for free:

```yaml
identityDefaults:
    hostnameExpr: "'team' in labels ? labels['team'] : namespace"
    usernameExpr: "namespace + '-' + policyName + (labels['env'] == 'prod' ? '-prod' : '')"
```

/// note | How `*Expr` evaluation is bounded

Each `*Expr` is a CEL expression returning a **string**. CEL is sandboxed (no I/O, no arbitrary code) and the expression is **validated at admission** — a syntax error, a wrong return type, or a reference to a variable outside the documented environment (`namespace`/`policyName`/`labels`/`annotations`/`cluster`) is rejected on `kubectl apply`, not discovered at backup time. Evaluation is bounded by CEL's cost budget, and each expression is capped at ~1 KiB.

///

### `identityDefaults.cluster` — sharing one repository across clusters

`cluster` is a distinct knob from the two CEL expressions above: an RFC 1123 label, **at most 32 characters**, with **no dots** (a dot is the delimiter `identityDefaults.cluster` reserves to split a hostname back into its namespace and cluster parts on the read path, so an embedded dot is rejected outright at admission rather than risked). Set it once per cluster that shares this repository:

```yaml
spec:
    identityDefaults:
        cluster: east # this cluster's identity suffix
```

Setting it changes three things:

1. **The default hostname.** With no `hostnameExpr` and no consumer override, the default kopia identity hostname becomes `<namespace>.<cluster>` instead of bare `<namespace>` — so two clusters backing up same-named namespaces (`billing` in `east` and `billing` in `west`) write distinct identities and never collide or cross-prune each other's snapshots. See [Backups → identity](backups.md#identity--what-kopia-records-usernamehostnamepath).
2. **The CEL variable `cluster`.** Available to `hostnameExpr`/`usernameExpr` above (e.g. `hostnameExpr: "namespace + '.' + cluster"` — equivalent to the default, spelled out explicitly, or a starting point for a custom scheme).
3. **Foreign-snapshot classification and the maintenance lease.** Once set, the catalog scan can tell "written by this cluster" from "written by another cluster sharing this repository" (`catalog.foreignSnapshots`, [above](#the-catalog--discovered-snapshots)), and the operator-managed `Maintenance`'s lease becomes cluster-qualified so two clusters never fight over who runs maintenance. See [Maintenance → Ownership and shared repositories](maintenance.md#ownership-and-shared-repositories).

/// warning | Setting or changing `cluster` on a repository with consumer history is an identity change

If any consumer `SnapshotPolicy` already has snapshot history, setting `identityDefaults.cluster` for the first time — or changing it — silently re-identifies every consumer that resolves the default hostname (no `hostnameExpr`, no per-policy `identity` override): new snapshots land under `<namespace>.<cluster>` while the old history stays under bare `<namespace>`, and both lineages keep competing in the **same** GFS timeline — Kopiur's retention pools all of a policy's `Snapshot` CRs regardless of identity, so pre-flip CRs keep aging out normally rather than being frozen or orphaned outright. The webhook **rejects** this edit fleet-wide, exactly like any other `identityDefaults` change (see [Backups → identity](backups.md#identity--what-kopia-records-usernamehostnamepath)) — acknowledge it with the `allow-identity-change` annotation once you've read the consequences. For turning on multi-cluster sharing on a repository that's already in use, follow [Share one repository across clusters](scenarios/shared-repository-multi-cluster.md), which walks the safe order of operations end to end.

///

### `credentialProjection.allowed` — the owner gate for shared creds

By default a `ClusterRepository` will **not** let its credential Secret be projected into a foreign consumer namespace (`credentialProjection.allowed` defaults `false`). Projection is fail-closed: it requires the repository owner's `allowed: true` **and** the consumer's `credentialProjection.enabled: true` **and** the operator's `secrets` RBAC. A namespaced `Repository` has no such gate (its repo and Secret co-reside).

```yaml
spec:
    credentialProjection:
        allowed: true # owner permits projection; consumers still opt in per-CR
```

See [Movers → credential projection](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos).

/// warning | Two requirements for ClusterRepository backups

1. Install the operator with **`installScope=cluster`** (otherwise `ClusterRepository` is never reconciled — see [Installation → scope](install.md#install-scope)).
2. Get the credential Secret into each **workload** namespace a mover runs in. The easy way: set [`credentialProjection.enabled: true`](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) on the `SnapshotPolicy`/`Restore`/`Maintenance` that uses this repository, and Kopiur copies it for you (off by default, recommended for shared repos). Otherwise replicate it yourself.

///

A complete, apply-ready example is [`deploy/examples/02-cluster-repository.yaml`](examples.md#example-02--shared-platform-repository).

## The values you'll actually change

| Field                                                  | What it does                                      |
| ------------------------------------------------------ | ------------------------------------------------- |
| `backend.<kind>.bucket` / `container` / `path` / `url` | The storage location.                             |
| `backend.<kind>.prefix`                                | Key prefix so several repos can share one bucket. |
| `backend.s3.endpoint` / `region`                       | Non-AWS endpoint and region.                      |
| `backend.<kind>.auth.secretRef.name`                   | The Secret holding the backend keys.              |
| `encryption.passwordSecretRef.{name,key}`              | Where the kopia password lives.                   |
| `create.enabled`                                       | Whether to initialize a new repository.           |
| `backend.s3.tls.disableTls`                            | Plain-HTTP endpoints (in-cluster MinIO/RustFS).   |
| `allowedNamespaces` _(ClusterRepository)_              | Which namespaces may use the repo.                |
| `identityDefaults` _(ClusterRepository)_               | Per-tenant snapshot identity (CEL `*Expr`) and, for a repository shared across clusters, `cluster`. |
| `moverDefaults`                                        | Base security context / resources / cache for every mover. |
| `parameters.epoch.minDuration`                          | Lower it (e.g. `6h`) when index blobs stay in the thousands despite maintenance running. |
| `scheduleDefaults.timezone`                             | Cron timezone inherited by verification/replication/maintenance and `SnapshotSchedule` crons. |
| `onNamespaceDelete`                                    | `Orphan` (default) / `Delete` on namespace delete.|
| `deletionProtection.threshold`                          | Mass-deletion breaker: HOLD external destructive Snapshot deletions at/above this count (default 10; `0` disables). |
| `mode`                                                 | `ReadWrite` (default) / `ReadOnly`.               |
| `suspend`                                              | Pause connect/bootstrap + maintenance.            |

## See also

- [How Kopia works](concepts/how-kopia-works.md) — dedup, the identity model, and why one shared repository maximizes it.
- [Backend configuration](backends/index.md) — per-backend setup cookbook (prereqs, Secret keys, apply-ready manifests).
- [Movers, RBAC & credentials](movers.md) — where the credential Secret must live.
- [Maintenance](maintenance.md) — the default-managed space reclamation per repo.
- [`deploy/examples/01-single-pvc-scheduled.yaml`](examples.md#example-01--single-pvc-scheduled) — S3 `Repository`, end to end.
- [`deploy/examples/02-cluster-repository.yaml`](examples.md#example-02--shared-platform-repository) — `ClusterRepository`.
