# Backups, restores & logs

Trigger a backup right now, drive the `Restore` CRD's full source-by-target matrix from one command line, and follow the mover's logs, all without writing CR YAML. All [global flags](index.md#global-flags) apply.

## `snapshot now`

Run a SnapshotPolicy immediately. The plugin creates a manual `Snapshot` object, which is the same thing a `SnapshotSchedule` does on a cron slot, labeled `origin: manual`.

With `--wait` it follows the run to its terminal phase. On `Succeeded` it exits 0 and prints a one-line stats summary. On `Failed` it exits 1 and prints the failure class, a tail of kopia's stderr, and the log tail on stderr. `Unchanged`, meaning nothing had changed so kopia created no new snapshot, also exits 0, with a line saying so instead of the stats. It is a successful run, not a failure.

```console
$ kubectl kopiur snapshot now --policy nightly -n media --tag reason=pre-upgrade --wait
snapshot.kopiur.home-operations.com/nightly-manual-20260611030012 created
snapshot nightly-manual-20260611030012 succeeded: kopia id a1b2c3d4e5f6, 5.0 GiB, took 300s
```

| Flag | Effect |
|---|---|
| `--policy NAME` | The SnapshotPolicy (recipe) to run. Checked up front, so a typo fails fast with a fix hint. |
| `--name NAME` | Name the Snapshot (default `<policy>-manual-<timestamp>`). |
| `--tag KEY=VALUE` | kopia snapshot tag; repeatable. |
| `--deletion-policy delete\|retain\|orphan` | What happens to the kopia snapshot when the CR is deleted. |
| `--pin` | Exempt this snapshot from GFS retention until unpinned. |
| `--backoff-limit N` / `--active-deadline-seconds SECS` | Mover Job retry budget / wall-clock cap. |
| `--wait` | Watch until `Succeeded` (exit 0) or `Failed` (exit 1). |
| `--logs` | Stream the mover's logs while waiting (implies `--wait`). |
| `--timeout DURATION` | Give up waiting after e.g. `90s`, `30m`, `1h` (default 30m). Timing out stops the *waiting*, not the run. |

Without `--wait` the command returns as soon as the object is admitted. `-o yaml` and `-o json` then print the created object, and `-o name` prints just its name.

/// tip | A suspended policy still admits a manual Snapshot
If the policy is suspended the plugin warns and proceeds anyway, because the operator is the authority on what suspension means. Resume the policy if the run does not start.

///

## `restore`

The `Restore` CRD's three sources and three targets as one command line. Exactly one source and exactly one target are required. The plugin enforces that when it parses your flags, and the webhook enforces it again at admission.

| Source | Meaning |
|---|---|
| `--from-snapshot NAME [--snapshot-namespace NS]` | An explicit Snapshot CR (scheduled, manual, or discovered). |
| `--from-policy NAME [--policy-namespace NS] [--as-of RFC3339] [--offset N]` | Resolve via the SnapshotPolicy's identity. Works with no Snapshot CR present (the GitOps deploy-or-restore pattern). |
| `--identity USER@HOST[:PATH] [--snapshot-id ID] [--as-of] [--offset]` | A raw kopia identity, for foreign writers or snapshots aged out of the catalog. Requires `--repository`. |

| Target | Meaning |
|---|---|
| `--to-pvc NAME` | Write into an existing PVC. |
| `--create-pvc NAME --size 10Gi [--storage-class X] [--access-mode RWO]…` | The operator creates the PVC. `--size` is required, because kopiur never guesses a capacity. |
| `--populator` | Passive mode: the restore is claimed later by a PVC's `dataSourceRef`. |

```console
$ kubectl kopiur restore --from-policy nightly --create-pvc data-restored --size 10Gi -n media --wait
restore.kopiur.home-operations.com/restore-nightly-20260611120001 created
restore restore-nightly-20260611120001 completed: kopia id a1b2c3d4e5f6, 5.0 GiB / 1000 files into pvc/data-restored
```

Every `spec.options` and `spec.policy` knob has a flag: `--enable-file-deletion` for an exact-mirror restore, since the default is additive, plus `--ignore-permission-errors true|false`, `--write-files-atomically true|false`, `--on-missing-snapshot fail|continue` and `--wait-timeout 5m`. `--backoff-limit` and `--active-deadline-seconds` configure the mover Job.

`--credential-projection` copies the repository's credential Secret into the restore's namespace. It needs the owning `ClusterRepository`'s `credentialProjection.allowed` gate; see [Movers → credential projection](../movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos).

`--wait`, `--logs` and `--timeout` behave exactly as they do in `snapshot now`: `Completed` exits 0 with a summary, and `Failed` exits 1 with the failure block.

/// warning | Restores fail closed
With an explicit source, so `--from-snapshot` or `--identity`, a missing snapshot fails the restore through `onMissingSnapshot: Fail` rather than quietly doing nothing. Only `--from-policy` defaults to `continue`, for deploy-or-restore.

///

/// note | Identity-based sources work on every backend
The operator resolves `--from-policy` sources, and `--identity` sources with no pinned snapshot id, by listing the repository's snapshots **inside the restore Job**. Picking "latest", or using `--as-of` or `--offset`, therefore works the same on S3, GCS, Azure, B2, SFTP, WebDAV and rclone as it does on a filesystem repository, with no controller-side repository mount. `--from-snapshot`, and a pinned `--identity … --snapshot-id <ID>`, skip the listing entirely.

///

## `logs`

Stream the mover Job's logs for a Snapshot or Restore, without chasing Job and pod names yourself:

```console
$ kubectl kopiur logs snapshot nightly-manual-20260611030012 -n media -f
```

You name the kind explicitly, as `logs snapshot …` or `logs restore …`, because the two kinds may share names. `-f`/`--follow`, `--tail N` and `--previous` behave like `kubectl logs`, and for a retried Job the **newest** pod is selected.

When the Job and its pods are already garbage-collected, the plugin prints the tail the operator recorded in `status.logTail`, plus the structured failure block, and says that is what it is doing. It never presents rotated logs as complete.

## Try it end-to-end

Take a backup and restore it into a fresh PVC, end to end, with two commands.

/// note | Prerequisite: the playground

This arc runs against the shared CLI playground: the `media` namespace, repository `nas`, policy `nightly`, and a seeded PVC. Apply it and install the plugin first, as described in [the playground setup](index.md#try-it-end-to-end).

///

**1. Back up now** and wait for it to finish:

```console
$ kubectl kopiur snapshot now --policy nightly -n media --wait
snapshot.kopiur.home-operations.com/nightly-manual-20260611030012 created
snapshot nightly-manual-20260611030012 succeeded: kopia id a1b2c3d4e5f6, 5.0 GiB, took 12s
```

The `succeeded:` line, with exit 0, is the proof. It carries the kopia id, the size, and the duration of the run.

**2. Restore that snapshot** into a brand-new PVC the operator creates for you:

```console
$ kubectl kopiur restore --from-snapshot nightly-manual-20260611030012 \
    --create-pvc data-restored --size 10Gi -n media --wait
restore.kopiur.home-operations.com/restore-nightly-manual-20260611030012 created
restore restore-nightly-manual-20260611030012 completed: kopia id a1b2c3d4e5f6, 5.0 GiB / 2 files into pvc/data-restored
```

The `completed:` line, with exit 0, reports the bytes and file count written **into pvc/data-restored**.

**3. Confirm the restored PVC is real (deep).** It exists and is `Bound`:

```console
$ kubectl -n media get pvc data-restored
NAME            STATUS   VOLUME       CAPACITY   ACCESS MODES   AGE
data-restored   Bound    pvc-9f3a…    10Gi       RWO            30s
```

Mount it in a throwaway pod and diff it against the source if you want the full round-trip proof.

/// note | Illustrative output

The kopia ids, sizes, durations, the timestamps in the names, and the bound `VOLUME` and `AGE` all vary per run. What you should check exactly is the `succeeded:` and `completed:` line formats, exit 0, and `data-restored` reaching `Bound`.

///
