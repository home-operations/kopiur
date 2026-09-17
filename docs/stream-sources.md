# Streamed command sources (logical database backups)

A **stream source** captures a command's standard output as one file inside a normal
kopia snapshot. Nothing is mounted: the mover execs the command in your running
workload pod and pipes its stdout straight into kopia.

```text
running database Pod
  pg_dumpall / mysqldump stdout
          |
          v
  Kubernetes exec stream
          |
          v
  kopia snapshot create --stdin-file postgres.sql
          |
          v
  your existing Repository / ClusterRepository
```

## When you want this

A PVC source copies the **files** under a database. That is a crash-consistent
copy at best, and on a cluster with no CSI snapshot support (`copyMethod: Direct`)
it is a copy of a live, mid-write data directory — something a database is entitled
to refuse to start from.

A stream source captures what the database itself says its contents are. Use it when:

- your storage has no CSI snapshots or clones (Hetzner Cloud volumes, many on-prem
  setups), so there is no point-in-time PVC capture to read;
- you want a **portable, version-independent** artifact — a SQL dump restores into a
  different PostgreSQL build; a filesystem copy generally does not;
- you want application consistency without freezing or quiescing the workload.

Keep using a PVC source when you want the whole volume, when the data is not a
database, or when your storage *does* give you real snapshots and a fast
block-level restore matters more than portability.

/// warning | This is not a substitute for understanding your database

A `pg_dumpall` is consistent because PostgreSQL makes it so, not because Kopiur does.
If your workload needs something else (a `mysqldump --single-transaction`, a
`--quiesce` flag, an application-level flush), that belongs in the command you write.
///

## Enabling it in a namespace

A stream source runs commands **inside other pods** in its namespace. So a cluster
admin has to opt the namespace in, once:

```sh
kubectl annotate namespace bundlecop \
  kopiur.home-operations.com/stream-exec-movers=true
```

Until then, Snapshots for a stream policy sit in `Pending` with
`MoverPermitted=False` / `StreamExecNotPermitted`, and the message names this exact
command.

**Why a gate at all?** The mover needs `pods/exec` in the namespace to run your dump
command. Kubernetes deliberately separates that verb from ordinary write access —
being able to create Deployments does not let you exec into someone else's pod.
Without this annotation, anyone who could write a `SnapshotPolicy` in a namespace
would effectively acquire `pods/exec` there. The annotation is a cluster admin
saying "in this namespace, that is fine".

Kopiur keeps the blast radius as small as RBAC allows: stream mover Jobs run as a
**separate** `…-stream-mover` ServiceAccount bound to a **separate** role, so
`pods/exec` never reaches the ServiceAccount your ordinary backup Jobs use. It
cannot be narrowed below the namespace, though — `resourceNames` cannot help,
because the pod name is not known until the selector resolves at run time, and RBAC
has no label-selector form. The namespace is the boundary; the annotation is how you
consent to it.

### If Kopiur is installed namespace-scoped

Checking that annotation means **reading the Namespace**, which is a cluster-scoped
read. A `installScope: cluster` install already has it. A
`installScope: namespaced` install does **not** — its Role cannot reach a
cluster-scoped resource — so Kopiur cannot tell whether the namespace opted in.

When that happens it **refuses**, and says so:

> `SnapshotPolicy` … uses a `stream` source, but kopiur cannot read Namespace … to
> check the `kopiur.home-operations.com/stream-exec-movers` opt-in (the API server
> returned 403), so it refuses to mint the `pods/exec` permission the stream mover
> needs.

This is deliberate, and it differs from the [privileged-mover
opt-in](movers.md#privileged-movers), which fails *open* under a namespaced
install. `pods/exec` is
a much larger grant — arbitrary code execution in every pod in the namespace, on a
ServiceAccount that outlives the Job — and a 403 on `namespaces get` is
indistinguishable from an authorization layer an admin added on purpose. Kopiur will
not hand that out on a guess.

Two ways forward:

```yaml
# Option A — keep the namespaced install, add just the one read.
installScope: namespaced
rbacNamespaceReadForStreamSources: true
```

That emits a small supplementary `ClusterRole` granting **`get` on `namespaces`
and nothing else** (not even `list`/`watch`), plus its binding.

```yaml
# Option B — install cluster-scoped, which already has the read.
installScope: cluster
```

Neither is needed for `pvc` or `nfs` sources. If you do not use stream sources,
leave `rbacNamespaceReadForStreamSources` at its default `false`.

## Writing the policy

```yaml
--8<-- "deploy/examples/45-stream-source-postgres.yaml:policy"
```

### `fileName`

The name of the single file stored inside the snapshot. It must be **one file
name** — no `/`, not `.` or `..`. Admission rejects anything else, and that is a
security check, not tidiness: kopia stores this string verbatim as the entry name
without sanitizing it, so a path-shaped value would make a later
`kopia restore <id> <dir>` write **outside** `<dir>`.

Prefer storing **raw** SQL over piping through `gzip`. kopia compresses and
deduplicates for you, and successive dumps of a mostly-unchanged database
deduplicate very well — a pre-compressed stream destroys that, because a small
change near the start rewrites every byte after it.

### `workloadExec.command`

Exec'd **directly, not through a shell**: element 0 is the program. Use an explicit
`["sh", "-ec", "..."]` if you want pipes or variable expansion.

Reference credentials through the container's own environment or mounted Secrets.
Never inline a password: this argv is copied into the mover pod's spec, where anyone
with `pods:get` in the namespace can read it — the same reason it does not belong in
the `SnapshotPolicy` itself.

### `workloadExec.podSelector`

Must match **exactly one** pod in `Running` phase that is not terminating (no
`deletionTimestamp`). Zero matches, several such pods, and matches that are all
not-Running-or-terminating are each a distinct, named failure — never an arbitrary
pick.

**Readiness is not checked.** A pod can be `Running` and still fail its readiness
probe — still starting up, or unhealthy — and it is picked (or counted toward
"several") exactly like a Ready one. For a replicated database, add a label that
identifies the primary specifically (not just "any Running replica"), or the
selector can silently dump from an unready primary that has not finished starting.

### `workloadExec.timeout`

A Go duration (default `1h`) bounding the command. On expiry the run fails and
leaves nothing behind. Raise it for a large database.

## What "success" means here

A stream Snapshot succeeds only when **both** halves succeed: your command exits `0`
**and** kopia commits the snapshot.

This matters more than it sounds. kopia finishes a stdin snapshot when its input
reaches end-of-file — and from kopia's side, a dump that finished and a connection
that dropped mid-dump look identical. So Kopiur does not decide from the byte
stream. It holds kopia's input open until the exec reports an exit status, and only
then commits:

| What happened | What Kopiur does |
| --- | --- |
| Command exited `0` | Close kopia's input; the snapshot commits |
| Command exited non-zero | Kill kopia with the input still open — **no snapshot is written** |
| Exec connection dropped (no exit status) | Same: kill, no snapshot |
| `timeout` elapsed | Same: kill, no snapshot |
| Command exited `0`, but kopia doesn't finish finalizing in time | Kopiur has already closed kopia's input at this point, so the snapshot **may have committed** — Kopiur cannot tell, and reports the run as failed either way |

Because kopia only writes its manifest at end-of-file, aborting (any of the first
four rows) leaves nothing restorable — not a partial snapshot that gets cleaned up
afterwards, but no snapshot at all. A failed dump can never be retained as a
successful backup. The leftover data blocks are unreferenced and repository
maintenance reclaims them.

The last row is different: the input was already closed, so kopia may have finished
writing a complete manifest before the wait timed out — this is not a partial
snapshot either, kopia never leaves one of those. If it did commit, the manifest
sits in the repository like any snapshot kopia wrote outside Kopiur, until the next
[catalog scan](repositories.md#the-catalog--discovered-snapshots) surfaces it as a
discovered `Snapshot` CR (`origin: discovered`) and it becomes governable like any
other. A scan always runs once on bootstrap and again on any spec change — there is
no way to skip that. Only *repeating* it on a timer is opt-in
(`catalog.periodicRefresh`, off by default), so a manifest that committed between
spec changes can wait a while for the next scan. This is rare — the finalize wait has
its own budget precisely to make it rare — and it is not a false-backup risk (nothing
reads this manifest as your latest good backup), just an occasional orphaned manifest
that takes longer than usual to become visible.

## Your data never reaches the logs

The dump is copied pipe-to-pipe. It is never collected into a string, written to a
log line, attached to an error message, or stored in `Snapshot.status`. Only
**stderr** is captured, and only the last 8 KiB of it, for diagnostics.

Write your command so it keeps that true: send diagnostics to stderr, and never
`echo` row data.

## Restoring

```yaml
--8<-- "deploy/examples/46-restore-stream-exec.yaml:restore"
```

kopia streams the stored file straight into the command's stdin — no PVC, no
temporary volume. Both halves must succeed again: a `psql` that died halfway leaves
a half-loaded database, and calling that a completed restore would be worse than
failing.

/// danger | Point restores at a scratch database

`streamExec` runs whatever you name against whatever the selector matches, and
restoring a `pg_dumpall` with `psql` will overwrite roles and databases. The normal
use is a **restore drill** against a throwaway target — which is also the only way
to know your dumps are actually restorable.
///

## Fields that do not apply

A stream source mounts no volume, so the PVC-shaped knobs have nothing to act on.
Rather than accept them silently, admission rejects them and says why:

| Field | Why |
| --- | --- |
| `readOnly` | There is no mount to make read-only. |
| `acknowledgeLiveMutation` | It acknowledges the kubelet rewriting a mounted volume's ownership. |
| `sourcePathStrategy` | It derives a path from a matched PVC's name. |
| `volumeSnapshotClassName` | There is no PVC to CSI-snapshot. |
| `staging.storageClassName`, `staging.accessModes` | There is no staged PVC to override. |

`copyMethod` and `groupBy` are **ignored** rather than rejected, for the same reason
they are on an NFS source: `copyMethod` defaults to `Snapshot` server-side, so an
unset field is indistinguishable from a deliberate one and rejecting it would refuse
a policy nobody wrote wrong. `staging.timeout` is likewise **accepted and inert**
here — admission still checks that it parses as a Go duration, but a stream source
never stages a PVC, so the value itself has nothing to bound.

A stream source must also be the **only** source in its `SnapshotPolicy`. It produces
exactly one artifact per Snapshot and is never expanded, so pairing it with other
sources would silently back up only one of them. Give it its own policy.

## Identity and paths

A stream snapshot records `/stream/<fileName>` as its kopia source path —
deliberately a different root from a PVC source's `/pvc/<name>`, so a streamed
artifact can never share a kopia identity with a volume backup. Override it with
`sourcePathOverride` if you need to.

Everything else is ordinary Kopiur: GFS retention, maintenance, replication,
verification, `Snapshot` CRs, and metrics all behave exactly as they do for a PVC
source.
