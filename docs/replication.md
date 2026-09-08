# Repository replication

A **`RepositoryReplication`** mirrors a repository's blobs to a **second backend** on a schedule. It wraps `kopia repository sync-to` as a Kubernetes resource.

This is the off-site copy that turns one repository into a 3-2-1 strategy: the same data on a second medium, in a second location.

/// tip | When to reach for it

You already have a primary `Repository`, and you want a durable copy elsewhere, kept in sync automatically. That could be a second cloud, a different region, or an on-prem NAS.

The mirror is restore-ready. If the primary is ever lost, point a `Repository` and a `Restore` at the destination backend.

///

/// info | The mirror is also a seed

When the primary is gone for good, you do not have to promote the mirror to production.

[`Repository.spec.seed`](repositories.md#seed--initialize-a-new-repository-from-a-replica) copies the mirror into a **new** repository during that repository's first bootstrap. The rebuilt cluster gets its own store, pre-loaded with the history, and the mirror stays a pristine, read-only replica.

Kick off a final [on-demand run](#run-it-now) first so the mirror is current, then see [Scenario 10: DR from a replicated repository](scenarios/dr-with-replicated-repository.md).

///

## How it works

- It is **namespaced**, and lives alongside its source repository, like `Maintenance` does. It references a `Repository` or `ClusterRepository` through `sourceRef`.
- The controller schedules one mover Job per cron slot, using the same scheduling machinery `Maintenance` uses: croner, deterministic jitter, one run at a time, and a gate on the repository being `Ready`. The mover inherits the source repository's `moverDefaults`.
- `destination` is exactly one backend, using the same externally-tagged `Backend` shape `Repository` uses. It **must differ** from the source's backend, and the webhook enforces that.

## Try it end-to-end

Watch a repository mirror itself to a second backend, end to end, with one self-contained bundle: [`deploy/examples/tryit/replication.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/replication.yaml).

It builds the whole 3-2-1 picture on filesystem PVCs, so it needs no cloud credentials. It contains four pieces: a **source** `Repository` named `primary` on one PVC, with a seeded `Snapshot` so there are blobs to mirror; a **destination** filesystem on a *second* PVC; the `RepositoryReplication` itself; and a `verify-mirror` `Repository` connected to the destination, so you can confirm the snapshot landed.

The `RepositoryReplication` is the new piece. It mirrors `sourceRef` to a second backend on a cron. Here it runs every minute so the demo fires promptly; a real mirror would run nightly.

```yaml
--8<-- "deploy/examples/tryit/replication.yaml:replication"
```

/// tip | Don't want to wait for the cron?

The bundle uses `schedule.cron: "* * * * *"`, meaning every minute, so the demo fires promptly.

A production mirror would run nightly, for instance `0 5 * * *`, after the backups land. You would trigger the first run yourself with [`kubectl kopiur replication run`](#run-it-now).

Fill in the single `REPLACE_ME`, which is `KOPIA_PASSWORD`, and apply once.

///

**1. Apply and wait for the source to have data.**

```console
$ kubectl apply -f deploy/examples/tryit/replication.yaml
$ kubectl -n kopiur-tryit wait --for=condition=Ready repository/primary --timeout=2m
$ kubectl -n kopiur-tryit wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/app-data-seed --timeout=5m
```

**2. Watch the mirror run.** Within a minute or two the replication fires and stamps `status.lastReplicated`:

```console
$ kubectl -n kopiur-tryit get repositoryreplications -w
NAME             SOURCE    DESTINATION   SCHEDULE    LAST   AGE
primary-mirror   primary   filesystem    * * * * *          40s
primary-mirror   primary   filesystem    * * * * *   5s     75s
```

**3. Prove the run succeeded.** `status.phase` is `Succeeded`, and `status.lastReplicated` carries a timestamp:

```console
$ kubectl -n kopiur-tryit get repositoryreplication primary-mirror \
    -o jsonpath='{.status.phase}{" "}{.status.lastReplicated}'
Succeeded 2026-06-17T14:05:07Z    # illustrative timestamp
```

**4. Confirm the snapshot is actually in the destination.** The `verify-mirror` Repository is connected to the destination PVC. Wait for it to go `Ready`, then list the destination's snapshots:

```console
$ kubectl -n kopiur-tryit wait --for=condition=Ready repository/verify-mirror --timeout=2m
$ kubectl kopiur snapshots list --repository verify-mirror -n kopiur-tryit
# illustrative — the same snapshot identity that exists in `primary` now appears
# in the mirror, proving the blobs were copied.
```

/// tip | Real mirrors usually target a *different* backend

This demo mirrors filesystem to filesystem, using two PVCs, only so that it needs no cloud credentials.

For a true off-site copy, swap `destination.filesystem` for a different backend, such as `destination.s3` with a destination-credential `Secret`. The webhook requires the destination backend to differ from the source's. See [example 19](examples.md#example-19--repository-replication).

///

To tear down: `kubectl delete namespace kopiur-tryit`.

## Minimal manifest

This is just the `RepositoryReplication` CR. The destination `Secret` it references is in the full example below:

```yaml
--8<-- "deploy/examples/19-repository-replication.yaml:replication"
```

The full apply-ready manifest, including the destination-backend `Secret`, is [`deploy/examples/19-repository-replication.yaml`](examples.md#example-19--repository-replication).

## The fields you'll change

| Field | What it does |
| --- | --- |
| `sourceRef` | The repository to mirror from (`Repository`/`ClusterRepository`; `kind` defaults to `Repository`). |
| `destination` | The backend to mirror to. It is externally tagged, as `destination.s3`, `destination.filesystem`, and so on. It must differ from the source backend. Its `auth.secretRef` supplies the destination backend's **own** access credentials. See [Destination credentials](#destination-credentials). |
| `schedule.cron` / `jitter` | When replication runs. Jenkins-style `H` is supported, as on a `SnapshotSchedule`. An absent `jitter` inherits the **source** repository's [`scheduleDefaults.jitter`](repositories.md#scheduledefaults--set-the-cron-timezone-and-jitter-once), just as `timezone` already did. Both are capped at 24h at admission. |
| `mover` | Per-run mover overrides (resources, scheduling, security context). Inherits the source repository's `moverDefaults`. |
| `suspend` | Pause replication without deleting the CR. |
| `sync` | Tuning knobs for the underlying `kopia repository sync-to` invocation. See [Tuning the sync](#tuning-the-sync) below. |

## Tuning the sync

By default `sync-to` copies blobs **one at a time**. That is fine for a small repository. But the first copy of a large one, to a slow or high-latency destination, can take days or weeks at roughly one object per second. Object storage is the usual culprit.

`spec.sync` exposes the kopia flags that speed this up, and that otherwise tune the copy:

```yaml
spec:
  sync:
    parallel: 8 # concurrent blob-copy workers (kopia default: 1 — sequential)
    deleteExtra: false # prune destination-only blobs for a true mirror (default: false)
    mustExist: false # fail instead of initializing the destination (default: false)
    times: true # sync blob modification times, when supported (default: true)
    update: true # update blobs already at the destination when newer (default: true)
    maxDownloadSpeedBytesPerSecond: 50000000 # cap source read throughput
    maxUploadSpeedBytesPerSecond: 20000000 # cap destination write throughput
```

Every field is optional on its own. Omitting `sync` entirely, or any field within it, keeps kopia's own default for that flag. Raising `parallel` is the main knob most users reach for.

/// warning | `deleteExtra` deletes destination content

`deleteExtra` maps to kopia's `--delete`. With it set to `true`, every run **deletes** blobs that are present at the destination but no longer present at the source. That turns the mirror into an exact copy rather than an additive one.

This is the correct behavior for a true 3-2-1 mirror. It also means a mistaken or emptied source repository will prune the destination's copies too, on the next scheduled run.

The field is named `deleteExtra` here, rather than kopia's bare `delete`, precisely so that `deleteExtra: true` reads as deliberate instead of looking like a leftover default.

///

## Replication draws from the source repository's concurrency pool

A replication run reads the **source** repository, so it counts against that repository's [`concurrency.maxConcurrentJobs`](repositories.md#concurrency--cap-the-mover-jobs-one-repository-runs-at-once), alongside its backups and restores.

The source is the right pool for it. `sync-to` reads every blob out of that backend, and a destination described by an inline `destination` block has no repository object, so it has no pool of its own.

Like a backup, a replication run that arrives at a full pool is **parked before anything happens**. There is no mover Job, no credential projection, and not even the destination-Secret presence check. A queued run may wait an unbounded time, and it must not leave side effects behind while it does.

It reports `RepositorySlotAvailable=False`, with reason `WaitingForSlot`, naming the counts. It launches by itself when a slot frees up.

The condition heals to `True` with reason `SlotAcquired` once the Job exists. If that heal write fails, it is retried while the run is still in flight, and failing that, when the next run starts. So a running replication never sits advertising a queue it has already left.

## Destination credentials

`kopia repository sync-to` is a **blob-level copy**. The destination ends up with exactly the source repository's format and encryption password, so there is no separate destination password to configure.

What the destination *does* need is its own backend **access** credentials, such as the S3 keys for the mirror bucket. Set them with `destination.<backend>.auth.secretRef`, or with `workloadIdentity`, exactly as you would for a source repository's backend auth.

The replication runs in one mover pod that talks to both backends, so the webhook enforces two rules:

- **Same namespace.** The destination's credential `Secret` must live in the `RepositoryReplication`'s own namespace. The mover loads it with `envFrom`, which is namespace-local, and replication does not copy credentials across namespaces.
- **Same key names as a source Secret.** Those are `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `B2_KEY_ID` and `B2_KEY`, the `KOPIA_WEBDAV_*` pair, or the file-based `KOPIA_SFTP_KEY_DATA`, `KOPIA_GCS_CREDENTIALS`, and `KOPIA_RCLONE_CONFIG`.

/// note | `inheritSecurityContextFrom` is rejected here

A replication mover copies blobs from one repository to another and never reads a workload's files. There is no workload whose identity it could take. So `spec.mover.inheritSecurityContextFrom` is **rejected at admission**, rather than accepted and ignored.

Set `spec.mover.securityContext` explicitly if the destination needs a particular UID or GID. A filesystem repository on an NFS export is the usual case, and there the answer is normally a shared `supplementalGroups`. See [NFS filesystem repositories](security-context.md#nfs-filesystem-repositories).

Versions 0.7.4 and earlier accepted the field and silently dropped it. The manifest claimed the mover ran as the workload, and it did not. If you have such a manifest, it will now be rejected. Remove the field, which was never doing anything, or replace it with an explicit `securityContext`.

///

The source and destination may use **entirely different credentials**, even two different accounts on the same provider. Mirroring MinIO to Cloudflare R2, or one S3 account to another, both work.

Kopiur delivers the destination Secret to the pod under a `KOPIUR_DEST_` environment prefix, and remaps it for the `sync-to` step only. So identically named keys on the two sides never collide.

## Run it now

A `RepositoryReplication` normally fires on its cron. But you can ask for a mirror **right now**: after a big restore, before decommissioning the source, or just to watch it work the first time.

```console
$ kubectl kopiur replication run nas-primary-offsite -n billing --wait
repositoryreplication.kopiur.home-operations.com/nas-primary-offsite run requested (2026-06-11T12:00:00Z)
RepositoryReplication nas-primary-offsite run completed at 2026-06-11T12:04:18Z
```

The plugin stamps the `kopiur.home-operations.com/run-requested` annotation with an RFC3339 timestamp. `kubectl annotate` does exactly the same thing, if you'd rather not install the plugin:

```console
$ kubectl annotate repositoryreplication nas-primary-offsite -n billing \
    kopiur.home-operations.com/run-requested="$(date -u +%Y-%m-%dT%H:%M:%SZ)" --overwrite
```

The timestamp pins *which* request the status answers. Re-applying the same value does nothing, which makes it safe under GitOps, and a **new** timestamp starts a new run. Progress lands in `status.manualRun`:

```console
$ kubectl get repositoryreplication nas-primary-offsite -n billing -o jsonpath='{.status.manualRun}'
{"completedAt":"2026-06-11T12:04:18Z","phase":"Succeeded","requestedAt":"2026-06-11T12:00:00Z"}
```

The run goes through the **same** path as a scheduled one: the same mover, the same gate on the source repository being `Ready`, and the same rule that never runs two mirrors of one CR at once. `--wait` exits 0 on `Succeeded` and 1 on `Failed`.

/// note | A requested run re-anchors the schedule

The next cron slot is computed from `status.lastReplicated`, and a successful requested run stamps it just as a scheduled run does.

So running at 14:00 on an `0 5 * * *` mirror means the next automatic run is 05:00 **tomorrow**, not tonight. That is intended. A cron here means "at least this often", and having just mirrored, another run a few hours later would be redundant.

///

/// warning | Suspended? The request waits, it does not vanish

Requesting a run on a `suspend: true` replication records it as `status.manualRun.phase: Pending`, and surfaces `Ready=False` with reason `SuspendedWithPendingRun`.

Nothing starts until you [resume](cli/operations.md#suspend--resume) it. At that point the still-unanswered request fires immediately.

///

## Watching it

```console
$ kubectl get repositoryreplications -n billing
NAME                  SOURCE        DESTINATION   SCHEDULE    LAST   AGE
nas-primary-offsite   nas-primary   s3            0 5 * * *   8h     6d
```

`status` surfaces `lastReplicated`, `nextScheduledAt`, and best-effort `lastReplicatedBytes` and `lastReplicatedBlobs`. It also carries the standard `Ready`, `Reconciling`, and `Stalled` conditions for `kubectl wait`, plus `manualRun` once you have [requested a run](#run-it-now).

Every finished run is also counted in `kopiur_replication_runs_total{kind,trigger,outcome}`. So a Prometheus alert can catch "the nightly mirror has been failing for three days" without watching conditions.

## See also

- [`deploy/examples/19-repository-replication.yaml`](examples.md#example-19--repository-replication)
- [Repositories & backends](repositories.md)
- [Disaster recovery scenario](scenarios/disaster-recovery.md)
- [Scenario 10, DR from a replicated repository](scenarios/dr-with-replicated-repository.md): turning this mirror back into a live repository with `spec.seed`.
