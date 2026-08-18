# Repository replication

A **`RepositoryReplication`** mirrors a repository's blobs to a **second backend** on a schedule — `kopia repository sync-to` wrapped as a Kubernetes resource. It is the off-site copy that turns one repository into a 3-2-1 strategy: the same data on a second medium / in a second location.

/// tip | When to reach for it

You already have a primary `Repository` and you want a durable copy elsewhere — a second cloud, a different region, or an on-prem NAS — kept in sync automatically. The mirror is restore-ready: point a `Repository`/`Restore` at the destination backend if the primary is ever lost.

///

/// info | The mirror is also a seed

When the primary is gone for good, you do not have to promote the mirror to production. [`Repository.spec.seed`](repositories.md#seed--initialize-a-new-repository-from-a-replica) copies it into a **new** repository during that repository's first bootstrap, so the rebuilt cluster gets its own store pre-loaded with the history and the mirror stays a pristine, read-only replica. Kick a final [on-demand run](#run-it-now) first so the mirror is current, then see [Scenario 10 — DR from a replicated repository](scenarios/dr-with-replicated-repository.md).

///

## How it works

- **Namespaced**, living alongside its source repository (like `Maintenance`). It references a `Repository` or `ClusterRepository` via `sourceRef`.
- The controller schedules a per-slot mover Job (croner + deterministic jitter, single-flight, repo-ready gate) — the same scheduling kernel `Maintenance` uses. The mover inherits the source repository's `moverDefaults`.
- `destination` is exactly one backend (the same externally-tagged `Backend` shape `Repository` uses) and **must differ** from the source's backend (webhook-enforced).

## Try it end-to-end

Watch a repository mirror itself to a second backend, end to end, with one self-contained bundle — [`deploy/examples/tryit/replication.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/replication.yaml). It builds the whole 3-2-1 picture on filesystem PVCs (no cloud credentials): a **source** `Repository` (`primary`) on one PVC with a seeded `Snapshot` so there are blobs to mirror, a **destination** filesystem on a *second* PVC, a `RepositoryReplication`, and a `verify-mirror` `Repository` connected to the destination so you can confirm the snapshot landed.

The `RepositoryReplication` is the new piece: it mirrors `sourceRef` to a second backend on a cron (here every minute so the demo fires promptly — a real mirror would run nightly):

```yaml
--8<-- "deploy/examples/tryit/replication.yaml:replication"
```

/// tip | Don't want to wait for the cron?

The bundle uses `schedule.cron: "* * * * *"` (every minute) so the demo fires promptly; a production mirror would run nightly (e.g. `0 5 * * *`, after the backups land) and you would trigger the first run yourself with [`kubectl kopiur replication run`](#run-it-now). Fill in the single `REPLACE_ME` (`KOPIA_PASSWORD`) and apply once.

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

**3. Prove the run succeeded (deep).** `status.phase` is `Succeeded` and `status.lastReplicated` carries a timestamp:

```console
$ kubectl -n kopiur-tryit get repositoryreplication primary-mirror \
    -o jsonpath='{.status.phase}{" "}{.status.lastReplicated}'
Succeeded 2026-06-17T14:05:07Z    # illustrative timestamp
```

**4. Confirm the snapshot is actually in the destination.** Wait for the `verify-mirror` Repository (connected to the destination PVC) to go `Ready`, then list the destination's snapshots:

```console
$ kubectl -n kopiur-tryit wait --for=condition=Ready repository/verify-mirror --timeout=2m
$ kubectl kopiur snapshots list --repository verify-mirror -n kopiur-tryit
# illustrative — the same snapshot identity that exists in `primary` now appears
# in the mirror, proving the blobs were copied.
```

/// tip | Real mirrors usually target a *different* backend

This demo mirrors filesystem→filesystem (two PVCs) only so it needs no cloud creds. For a true off-site copy, swap `destination.filesystem` for a different backend — e.g. `destination.s3` with a destination-credential `Secret` (the webhook requires the destination differ from the source backend). See [example 19](examples.md#example-19--repository-replication).

///

To tear down: `kubectl delete namespace kopiur-tryit`.

## Minimal manifest

Just the `RepositoryReplication` CR (the destination `Secret` it references is in
the full example below):

```yaml
--8<-- "deploy/examples/19-repository-replication.yaml:replication"
```

The full apply-ready manifest — including the destination-backend `Secret` — is
[`deploy/examples/19-repository-replication.yaml`](examples.md#example-19--repository-replication).

## The fields you'll change

| Field | What it does |
| --- | --- |
| `sourceRef` | The repository to mirror from (`Repository`/`ClusterRepository`; `kind` defaults to `Repository`). |
| `destination` | The backend to mirror to. Externally tagged (`destination.s3`, `destination.filesystem`, …). Must differ from the source backend. Its `auth.secretRef` supplies the destination backend's **own** access credentials — see [Destination credentials](#destination-credentials). |
| `schedule.cron` / `jitter` | When replication runs (Jenkins-style `H` supported, like a `SnapshotSchedule`). |
| `mover` | Per-run mover overrides (resources, scheduling, security context). Inherits the source repository's `moverDefaults`. |
| `suspend` | Pause replication without deleting the CR. |
| `sync` | Tuning knobs for the underlying `kopia repository sync-to` invocation — see [Tuning the sync](#tuning-the-sync) below. |

## Tuning the sync

By default `sync-to` copies blobs **one at a time**: fine for a small repository, but
an initial seed of a large one to a slow or high-latency destination (object storage
in particular) can take days to weeks at roughly one object per second. `spec.sync`
exposes the kopia flags that speed this up and otherwise tune the copy:

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

Every field is independently optional; omitting `sync` entirely (or any field within
it) reproduces kopia's own default for that flag — raising `parallel` is the main
knob most users reach for.

/// warning | `deleteExtra` deletes destination content

`deleteExtra` maps to kopia's `--delete`: with it `true`, every run **deletes** blobs
present at the destination but no longer present at the source, turning the mirror
into an exact copy rather than an additive one. This is the correct behavior for a
true 3-2-1 mirror, but it means a mistaken or emptied source repository will prune the
destination's copies too on the next scheduled run. It is named `deleteExtra` here
(not kopia's bare `delete`) precisely so a `deleteExtra: true` reads as deliberate
rather than being mistaken for a leftover default.

///

## Destination credentials

`kopia repository sync-to` is a **blob-level copy**: the destination inherits the
source repository's format and encryption password verbatim, so there is no separate
destination password to configure. What the destination *does* need is its own
backend **access** credentials — for example the S3 keys for the mirror bucket — set
via `destination.<backend>.auth.secretRef` (or `workloadIdentity`), exactly like a
source repository's backend auth.

Two rules the webhook enforces, because the replication runs in one mover pod that
talks to both backends:

- **Co-residence.** The destination's credential `Secret` must live in the
  `RepositoryReplication`'s own namespace. The mover loads it with `envFrom`, which is
  namespace-local, and replication does not project credentials across namespaces.
- **Same key names as a source Secret** (`AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, `B2_KEY_ID`/`B2_KEY`, `KOPIA_WEBDAV_*`, or the file-based
  `KOPIA_SFTP_KEY_DATA` / `KOPIA_GCS_CREDENTIALS` / `KOPIA_RCLONE_CONFIG`).

/// note | `inheritSecurityContextFrom` is rejected here

A replication mover copies blobs repository → repository and never reads a workload's
files, so there is no workload whose identity it could take. `spec.mover.inheritSecurityContextFrom`
is therefore **rejected at admission** rather than accepted and ignored.

Set `spec.mover.securityContext` explicitly if the destination needs a particular
UID/GID — e.g. a filesystem repository on an NFS export, where the usual answer is a
shared `supplementalGroups` (see [NFS filesystem repositories](security-context.md#nfs-filesystem-repositories)).

Versions ≤ 0.7.4 accepted the field and silently dropped it: the manifest said the
mover ran as the workload, and it did not. If you have such a manifest, it will now be
rejected — remove the field (it was never doing anything) or replace it with an
explicit `securityContext`.

///

The source and destination may use **entirely different credentials** — even two
different accounts on the same provider (e.g. mirroring MinIO → Cloudflare R2, or one
S3 account to another). Kopiur delivers the destination Secret to the pod under a
`KOPIUR_DEST_` env prefix and remaps it for the `sync-to` step only, so the two
sides' identically named keys never collide.

## Run it now

A `RepositoryReplication` normally fires on its cron, but you can ask for a mirror **right now** — after a big restore, before decommissioning the source, or just to see the pipe work the first time:

```console
$ kubectl kopiur replication run nas-primary-offsite -n billing --wait
repositoryreplication.kopiur.home-operations.com/nas-primary-offsite run requested (2026-06-11T12:00:00Z)
RepositoryReplication nas-primary-offsite run completed at 2026-06-11T12:04:18Z
```

The plugin stamps the `kopiur.home-operations.com/run-requested` annotation with an RFC3339 timestamp; `kubectl annotate` does exactly the same thing if you'd rather not install the plugin:

```console
$ kubectl annotate repositoryreplication nas-primary-offsite -n billing \
    kopiur.home-operations.com/run-requested="$(date -u +%Y-%m-%dT%H:%M:%SZ)" --overwrite
```

The timestamp pins *which* request the status answers, so re-applying the same value is a no-op (safe in GitOps) and a **new** timestamp starts a new run. Progress lands in `status.manualRun`:

```console
$ kubectl get repositoryreplication nas-primary-offsite -n billing -o jsonpath='{.status.manualRun}'
{"completedAt":"2026-06-11T12:04:18Z","phase":"Succeeded","requestedAt":"2026-06-11T12:00:00Z"}
```

The run goes through the **same** path as a scheduled one — the same mover, the same source-repository-`Ready` gate, the same single-flight rule that never runs two mirrors of one CR at once. `--wait` exits 0 on `Succeeded` and 1 on `Failed`.

/// note | A requested run re-anchors the schedule

The next cron slot is computed from `status.lastReplicated`, and a successful requested run stamps it just like a scheduled run does. So running at 14:00 on an `0 5 * * *` mirror means the next automatic run is 05:00 **tomorrow**, not tonight. That is intended: a cron here means "at least this often", and having just mirrored, another run hours later would be redundant.

///

/// warning | Suspended? The request waits, it does not vanish

Requesting a run on a `suspend: true` replication records it as `status.manualRun.phase: Pending` and surfaces `Ready=False` with reason `SuspendedWithPendingRun`. Nothing starts until you [resume](cli/operations.md#suspend--resume) it — at which point the still-unanswered request fires immediately.

///

## Watching it

```console
$ kubectl get repositoryreplications -n billing
NAME                  SOURCE        DESTINATION   SCHEDULE    LAST   AGE
nas-primary-offsite   nas-primary   s3            0 5 * * *   8h     6d
```

`status` surfaces `lastReplicated`, `nextScheduledAt`, and best-effort `lastReplicatedBytes`/`lastReplicatedBlobs`, plus standard `Ready`/`Reconciling`/`Stalled` conditions for `kubectl wait` — and `manualRun` once you have [requested a run](#run-it-now).

Every finished run is also counted in `kopiur_replication_runs_total{kind,trigger,outcome}`, so a Prometheus alert can catch "the nightly mirror has been failing for three days" without watching conditions.

## See also

- [`deploy/examples/19-repository-replication.yaml`](examples.md#example-19--repository-replication)
- [Repositories & backends](repositories.md)
- [Disaster recovery scenario](scenarios/disaster-recovery.md)
- [Scenario 10 — DR from a replicated repository](scenarios/dr-with-replicated-repository.md) — turning this mirror back into a live repository with `spec.seed`.
