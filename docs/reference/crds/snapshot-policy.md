# SnapshotPolicy

A `SnapshotPolicy` is the backup recipe: what to back up, under which identity, how long to keep it, which kopia policy to use, and which hooks to run.

A `SnapshotPolicy` is idempotent and runs nothing on its own. A `Snapshot` invocation or a `SnapshotSchedule` is what drives it.

For the short type-and-default table see the [field reference](../../field-reference.md). For task guidance see [Backups & schedules](../../backups.md).

## `spec`

### `repository` / `repositories`

Where the backups go. Set **exactly one** of the two fields. Both the webhook and an apiserver CEL rule enforce that.

- `repository` points at one repository. It carries a `kind`, so you always say whether you mean a namespaced `Repository` or a cluster-scoped `ClusterRepository`.
- `repositories` is a list of **1 to 8 distinct** such references, used for [multi-repository fan-out](../../backups.md#repositories--one-recipe-several-repositories-fan-out). Every run backs each source up into **each** listed repository. You get one `Snapshot` object and one mover Job per source-and-repository pair, and each capture is independent, with its own identity, retention, verification and cache.

Duplicate entries in `repositories` are refused. `repositories` is also mutually exclusive with `hooks`, because a quiesce window cannot span several concurrent children. Use a single-repository policy plus a [`SnapshotReplication`](snapshot-replication.md) instead.

### `identity`

Overrides for what kopia records as `username@hostname:path`. Kopiur resolves the identity at admission and pins it to `status.resolved.identity`, and never re-renders it afterwards. Leave the field unset to take the operator's defaults.

### `sources`

What to back up. At least one source is required, and the webhook enforces that.

Each entry is exactly one of three things: a single PVC by name, a `pvcSelector` matching many PVCs by label and namespace, or an inline `nfs` export. The webhook enforces that too. The `sourcePathOverride` and `sourcePathStrategy` siblings apply per source.

Per-source fields:

- `pvc` names a single `PersistentVolumeClaim` in the policy's namespace.
- `pvcSelector` is a Kubernetes `labelSelector` plus an optional `namespaceSelector`. Its `matchNames` restricts the search; leaving it out searches the policy's own namespace.
- `nfs` backs up an NFS export directly, with no PVC. Kopiur mounts it read-only.
- `sourcePathOverride` sets what kopia records as the source path. It defaults to `/pvc/<name>` for a PVC, or to the NFS export's `path` for an NFS source.
- `sourcePathStrategy` applies to `pvcSelector` sources only, and decides how each matched PVC's source path is derived. `PvcName`, the default, uses the name alone. `PvcNamespacedName` uses `<namespace>/<name>`, which disambiguates same-named PVCs across namespaces.
- `readOnly` mounts the source read-only. It defaults to `true`, because kopia only reads the source. Set it `false` **only** to make `fsGroup` apply. The kubelet implements `fsGroup` by recursively rewriting the volume's group ownership, and it skips that rewrite entirely on a read-only mount, so a mover's `fsGroup` and `fsGroupChangePolicy` otherwise do nothing here. It is rejected at admission on an `nfs` source, because the kubelet never applies `fsGroup` to in-tree NFS volumes, and rejected when `staging.accessModes` is `[ReadOnlyMany]`, because a read-only stage cannot be mounted read-write.
- `acknowledgeLiveMutation` is required with `copyMethod: Direct` plus `readOnly: false`, the one combination that reaches the **live** volume. In that combination the kubelet recursively changes the group of your running application's files to the mover's `fsGroup`, `65532` by default, and makes them group-writable, permanently. Under `Snapshot` or `Clone` the rewrite lands on a throwaway staged PVC and no acknowledgement is needed. The field is ignored where it is not needed, so switching `copyMethod` is never a two-step edit. See [Copy methods](../../copy-methods.md#making-fsgroup-apply-to-the-source).

### `copyMethod`

How the source volume is captured before kopia reads it.

`Snapshot` is the default. It takes a point-in-time CSI volume snapshot, which gives a crash-consistent capture that does not depend on the application's node. It needs the CSI snapshot stack and a `VolumeSnapshotClass` for the source's driver.

`Clone` takes a CSI volume clone instead. Use it for drivers that support cloning but not snapshotting.

Both stages are mounted according to `sources[].readOnly`, so read-only by default.

`Direct` reads the live PVC with no intermediate snapshot or clone. It works on **any** storage and needs no CSI snapshot stack, but it gives no point-in-time guarantee. For a `ReadWriteOnce` volume the mover runs on the volume's node. Set `Direct` explicitly for non-CSI or static sources, or for clusters without the snapshot stack. See [Copy methods](../../copy-methods.md).

### `volumeSnapshotClassName`

The `VolumeSnapshotClass` used when `copyMethod` is `Snapshot` or `Clone`.

Absent and empty mean the same thing: auto-select the default class for the source PVC's CSI driver. That matters when the field is templated. A Flux or Kustomize post-build substitution such as `volumeSnapshotClassName: ${KOPIUR_SNAPSHOTCLASS}` renders empty whenever the variable is undefined, and Kopiur treats that as unset rather than as a class whose name happens to be blank.

### `staging`

Settings for the CSI capture that runs before the mover, so for `copyMethod: Snapshot` and `Clone`.

- `timeout` is the staging deadline. Write it as a Go-style duration; the default is `10m`, and `"0"` waits indefinitely. It bounds the `VolumeSnapshot` becoming `readyToUse`, and then, as a fresh budget, the staged PVC binding, which is the CSI restore or clone window. On `Immediate` classes both happen before the Job; on `WaitForFirstConsumer` classes the binding happens while the mover Job runs.
- `storageClassName` sets the StorageClass for the **staged PVC** only. Leave it out to copy the source PVC's class. It must belong to the **same CSI driver** as the source; a mismatch fails fast with `StagedClassMismatch`. The main use is a rook-ceph CephFS class with `backingSnapshot: "true"`, which gives a near-instant shallow read-only mount instead of a full subvolume clone that takes minutes.
- `accessModes` sets the access modes for the staged PVC. Leave it out to copy the source's. It is a closed enum of the four Kubernetes modes. `[ReadOnlyMany]` pairs with snapshot-backed read-only classes. The mover mounts the stage read-only unless a source sets `readOnly: false`, and that combination is rejected with `[ReadOnlyMany]`, because a read-only stage cannot be mounted read-write.

Both overrides need a staged PVC to act on, so they are **rejected at admission** for `copyMethod: Direct`, for NFS sources, and for `pvcSelector` sources. See [Copy methods → staging overrides](../../copy-methods.md#staging-overrides).

### `groupBy`

Consistency grouping across several PVCs.

`VolumeGroupSnapshot` is the default for multi-PVC sources. It takes one consistent group snapshot across all the PVCs. `None` opts into independent per-PVC snapshots.

You must set `None` **explicitly**. There is no silent per-PVC fallback, because that would produce inconsistent backups.

/// note | Single-PVC today

Group snapshotting is not yet fully wired; multi-PVC group consistency is a
work in progress. For the current single-PVC behavior this field has no
observable effect.

///

### `retention`

Grandfather-father-son (GFS) retention. The operator enforces it by pruning the `Snapshot` objects this recipe produced. See [Backups & schedules](../../backups.md).

### `groupBy`

Whether the PVCs a [`pvcSelector`](#sources) expands to are captured **together**.

`VolumeGroupSnapshot`, the default, takes one CSI `VolumeGroupSnapshot` across every matched PVC, so they all share one instant. That is what you want for an application whose volumes must agree with each other. `None` captures each PVC independently.

Group capture needs four things: the `groupsnapshot.storage.k8s.io` API group, from external-snapshotter 8.2 or later; a `VolumeGroupSnapshotClass` for your driver; a driver that advertises `CREATE_DELETE_GET_VOLUME_GROUP_SNAPSHOT`; and `installScope: cluster`. If a piece is missing, Kopiur fails and names it rather than quietly downgrading to independent capture. See [copy methods](../../copy-methods.md#multi-pvc-and-consistency-groups).

### `defaultDeletionPolicy`

The `deletionPolicy` stamped onto `Snapshot` objects created against this recipe: `Delete`, `Retain` or `Orphan`. It controls whether deleting the object also deletes its kopia snapshot.

### `compression`

Compression policy. `compressor` names a kopia compressor such as `zstd`; leaving it out keeps kopia's default. `neverCompress` is a list of filename globs to leave uncompressed, which is useful for media that is already compressed.

### `files`

File-ignore policy.

`ignoreRules` is a list of filename and path globs to exclude from the snapshot, such as `*.tmp` or `*/cache/*`. `ignoreCacheDirs` honors `CACHEDIR.TAG`. `ignoreIdenticalSnapshots`, default `false`, tells kopia not to write a new manifest when the source is identical to the previous snapshot. The run still reads and hashes the whole source, so what you save is a manifest and not the work, and the `Snapshot` object ends in the [`Unchanged`](snapshot.md#status) phase instead of `Succeeded`.

!!! warning "It changes what a backup run produces"

    An `Unchanged` run owns **no kopia snapshot**, so you cannot restore from
    that CR — the previous snapshot is still the restore point, and it belongs
    to the previous CR. It is a success for every liveness purpose (last-backup
    timestamp, policy health, `Ready` condition, `snapshot now --wait` exit
    code) and it takes no retention slot, so it can never displace a real
    restore point. Leave it off unless you specifically want fewer manifests.

    Kopiur pins `--ignore-identical-snapshots=false` at the kopia identity
    scope on every run, so a repository-global kopia policy cannot turn this on
    behind your back. Only this field enables it.

`ignoreRules` defaults to a set of five OS-artifact excludes: `/lost+found`, `System Volume Information`, `$RECYCLE.BIN`, `@eaDir` and `.snapshot`. They apply even when you leave `files` out of the spec entirely. The apiserver only server-side-defaults a nested field when the parent object is present, so the controller applies this default again when it resolves the mover work spec.

An explicit `ignoreRules` list **replaces** the default outright rather than adding to it, and `ignoreRules: []` turns off ignoring anything at all. See [Backups → `files.ignoreRules` default](../../backups.md#filesignorerules-default-os-artifact-excludes) for why each entry is there, plus a copy-paste block of recommended extras.

### `extraArgs`

An escape hatch for kopia flags that do not yet have a field of their own.

### `errorHandling`

Backup-side error handling. It lets a snapshot complete with errors instead of failing outright. Each flag defaults to `false`, which is kopia's fail-on-error default.

- `ignoreFileErrors` maps to `--ignore-file-errors` and continues past unreadable files.
- `ignoreDirErrors` maps to `--ignore-dir-errors` and continues past unreadable directories.
- `ignoreUnknownTypes` maps to `--ignore-unknown-types` and continues past entries of unknown type.

`failFast` maps to `--fail-fast` and defaults to `false`. It is the opposite kind of knob: it aborts the snapshot at the *first* error instead of collecting errors and continuing. It rides on `kopia snapshot create`'s own arguments rather than on `policy set`, which is why it lives here beside its opposites rather than under `upload`.

### `upload`

Upload parallelism, which is kopia's upload policy.

`maxParallelSnapshots` maps to `--max-parallel-snapshots` and is how many sources snapshot at the same time. `maxParallelFileReads` maps to `--max-parallel-file-reads` and is the file-read concurrency within one snapshot. `limitMb` maps to `--upload-limit-mb`, which kopia leaves unlimited by default, and aborts the snapshot once that many MB have been uploaded. It is named `limitMb` rather than `uploadLimitMb` to avoid the `upload.uploadLimitMb` stutter.

Like `failFast`, `limitMb` is a `snapshot create` argument rather than a `policy set` value, but it lives here beside its parallelism siblings. Any knob you leave out keeps kopia's default.

### `verification`

Verification that proves snapshots are **restorable**, not just that maintenance ran. It is opt-in: leave the block out and no verification runs.

There are two tiers, both shaped `{ schedule: CronSpec, ... }`.

`quick` is a `QuickVerification { schedule?: CronSpec, parallel?, fileParallelism?, fileQueueLength?, maxErrors? }`. It is the frequent blob-level `kopia snapshot verify`. Leave `quick.schedule` out and no quick verification runs. `verifyFilesPercent` maps to `--verify-files-percent` and tunes how many files are verified fully; it sits on `verification` itself, beside `quick` and `deep`, and leaving it out keeps kopia's default. The four tuning knobs map straight onto `kopia snapshot verify`'s own flags: `parallel` is `--parallel` (kopia default 8), `fileParallelism` is `--file-parallelism`, `fileQueueLength` is `--file-queue-length` (kopia default 20000), and `maxErrors` is `--max-errors` (kopia default 0, which stops at the first error). All four are optional. `maxErrors` is the only one left unconstrained at admission, because 0 is a meaningful value there; the other three must be `1` or greater when set.

`deep` is a `DeepVerification { schedule: CronSpec, storageClassName?, capacity?, parallel? }`. It is the rarer scratch-restore test: it restores the latest snapshot into a throwaway volume, sanity-checks it, then discards it. `deep.schedule` is its cron and jitter, weekly for example. `deep.capacity` sizes the throwaway scratch PVC, `10Gi` for example; leave it out and the scratch volume is a node-local `emptyDir`. `deep.storageClassName` picks the scratch PVC's StorageClass; leave it out to use the cluster default. It only applies when `capacity` is set. `deep.parallel` maps onto `restore --parallel`, because a deep verify is a restore underneath, so this is the restore's own parallelism knob rather than a separate concept.

`successExpr` is a CEL pass/fail predicate over the verify result, and applies to both tiers. The environment exposes `stats{files,bytes,errors}`, `snapshot`, and, for deep verification only, `restored{files,checksumMatches}`. Returning `false` fails the run, which is how you catch the silent "0 files" success. Example: `"stats.files > 0 && stats.errors == 0"`.

Scheduling is gated. A policy with `verification` set does not start a verify Job until it has either one successful backup, or, on an adopted repository, discovered snapshots already present. See [Backups → verification scheduling](../../backups.md#verification-scheduling--gated-until-there-is-something-to-verify).

/// note | Old flat `quick: { cron, jitter }` shape
`quick`'s `schedule` field is optional for one reason: an object already stored in the old shape (`quick: { cron, jitter }`, from before #174) keeps decoding, with the quick tier treated as disabled until you migrate it. A **new** write in that old shape is rejected at admission, with a message pointing at the move to `quick.schedule`.
///

### `preflight`

CEL preconditions you declare, which a backup must satisfy before its mover Job launches. It generalizes the built-in repository-readiness gate, and it is opt-in: leave the block out and no preflight runs.

- `checks` is a list of `{ name, expr, message? }`. Each `expr` is a CEL predicate returning a bool, and **all** of them must pass before the backup launches. `name` is unique and identifies the check in the `Snapshot`'s status. `message` is an optional hint for humans.
- `timeout` is how long the `Snapshot` is held in `Pending` while a check is unsatisfied, before the run fails. Write it as a Go-style duration; the default is `10m`, and `0` holds indefinitely. The clock starts when a check first fails while the repository is `Ready`, so a repository that is slow to connect does not eat the budget. The `Failed` Snapshots this produces are bounded by the schedule's `failedJobsHistoryLimit`.

The CEL environment exposes `repository.{phase,ready,backendReachable,snapshotCountKnown,snapshotCount,indexBlobCountKnown,indexBlobCount,sizeBytesKnown,sizeBytes,lastHealthyKnown,lastHealthyAgeSeconds,lastReverifyKnown,lastReverifyAgeSeconds}` and `maintenance.{hasRun,lastSuccessAgeSeconds}`. Kopiur validates your expressions at admission.

A value that has not been observed is `i64::MAX`. A freshness check written as `< N` therefore fails closed, but a count or size check written as `> N` fails **open**, so always pair one with its `*Known` or `hasRun` companion. Example: `"maintenance.hasRun && maintenance.lastSuccessAgeSeconds < 604800"`. See [Repository health → Backup preflight](../../repository-health.md#backup-preflight-opt-in).

### `suspend`

Pause this recipe from the manifest. Schedules skip a suspended `SnapshotPolicy`, and so does its own reconcile, so there is no retention prune and no backup creation. The state shows up in the `SUSPENDED` condition and column.

### `hooks`

Pre- and post-snapshot hooks that run in the workload, not in the mover. `beforeSnapshot` hooks run in order before the snapshot is taken, for example to quiesce a database. `afterSnapshot` hooks run in order after it completes, for example to resume the workload.

Each hook is exactly one of three forms:

- `workloadExec` runs a `kubectl exec`-style command in a matched workload pod or container. It is the default form, and carries the pod and container selector, the `command`, and a `timeout`.
- `runJob` runs a full Kubernetes `JobSpec` as a one-shot Job. It is the k8up `PreBackupPod` equivalent.
- `httpRequest` sends a typed HTTP request, for orchestrating another system. It takes `url`, `method` (default `POST`), an optional `body`, optional `headers`, and a `timeout`. `headers` is a list of `{name, value}` objects, validated at admission: names are case-insensitive RFC 7230 tokens, values must be a single line, and duplicate names are rejected. Kopiur sends no default `Content-Type` with a `body`, so set one through `headers` if the endpoint needs it. An explicit `Authorization` header replaces `user:pass@…` credentials in the `url`, and setting both is rejected.

A failed hook aborts the backup by default. Set `continueOnFailure: true` on any hook form to let the backup carry on past that hook. Timeouts are Go duration strings, such as `2m`.

### `mover`

Per-recipe mover overrides for resources, cache and security context, layered over the repository's `moverDefaults`. See [Movers](../../movers.md) and [Security context](../../security-context.md).

### `credentialProjection`

Opt-in projection of the credential Secret for this recipe's backup movers. It is off by default.

With `enabled: true`, the operator copies the referenced repository's credential Secrets into the namespace where each backup mover runs. It is a no-op when the Secrets already live there. That way a workload backing up to a shared `ClusterRepository` does not need the Secret created in its own namespace first. `Snapshot`s produced from this recipe inherit the setting.

## `status`

### `resolved`

The recipe as kopia would see it, pinned at admission and never re-rendered.

`resolved.identity` is the resolved `username@hostname` identity. `resolved.sources` is the concrete list of PVCs and source paths after selector expansion; each entry pairs a `namespace/name` `pvc` with the `sourcePath` kopia records for it.

### `retention`

A summary of the most recent GFS retention prune. `activeSnapshotCount` is how many objects are currently inside the GFS window. `lastPruneAt` is the RFC 3339 timestamp of the last prune pass. `lastPruneDeleted` is how many `Snapshot` objects that pass deleted.

### Other status fields

- `observedGeneration` is the `metadata.generation` last reconciled, for staleness detection.
- `lastSuccessfulSnapshot` is the RFC 3339 timestamp of the most recent successful child `Snapshot` from this recipe. It backs the `LAST-SNAPSHOT` column.
- `lastVerified` is the RFC 3339 timestamp of the most recent successful verification of either tier. It backs the `LAST-VERIFIED` column.
- `conditions` holds the standard Kubernetes conditions, such as `RepositoryReachable` and `GroupSnapshotSupported`.
