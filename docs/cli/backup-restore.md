# Backups, restores & logs

Trigger a backup right now, drive the `Restore` CRD's full source × target
matrix from one command line, and follow the mover's logs — without writing CR
YAML. All [global flags](index.md#global-flags) apply.

## `snapshot now`

Run a SnapshotPolicy immediately: the plugin creates a manual `Snapshot` CR
(the same thing a `SnapshotSchedule` does on a cron slot, labeled
`origin: manual`) and, with `--wait`, follows it to its terminal phase —
exit 0 on `Succeeded` with a one-line stats summary, exit 1 on `Failed` with
the failure class, kopia stderr tail, and log tail on stderr.

```console
$ kubectl kopiur snapshot now --policy nightly -n media --tag reason=pre-upgrade --wait
snapshot.kopiur.home-operations.com/nightly-manual-20260611030012 created
snapshot nightly-manual-20260611030012 succeeded: kopia id a1b2c3d4e5f6, 5.0 GiB, took 300s
```

| Flag | Effect |
|---|---|
| `--policy NAME` | The SnapshotPolicy (recipe) to run. Checked up front — a typo fails fast with a fix hint. |
| `--name NAME` | Name the Snapshot (default `<policy>-manual-<timestamp>`). |
| `--tag KEY=VALUE` | kopia snapshot tag; repeatable. |
| `--deletion-policy delete\|retain\|orphan` | What happens to the kopia snapshot when the CR is deleted. |
| `--pin` | Exempt this snapshot from GFS retention until unpinned. |
| `--backoff-limit N` / `--active-deadline-seconds SECS` | Mover Job retry budget / wall-clock cap. |
| `--wait` | Watch until `Succeeded` (exit 0) or `Failed` (exit 1). |
| `--logs` | Stream the mover's logs while waiting (implies `--wait`). |
| `--timeout DURATION` | Give up waiting after e.g. `90s`, `30m`, `1h` (default 30m). Timing out stops the *waiting*, not the run. |

Without `--wait` the command returns as soon as the CR is admitted —
`-o yaml|json` then prints the created object, `-o name` just its name.

/// tip | A suspended policy still admits a manual Snapshot
If the policy is suspended the plugin warns but proceeds — the operator is
authoritative about what suspension means. Resume the policy if the run
doesn't start.

///

## `restore`

The `Restore` CRD's 3-sources × 3-targets matrix as one command line. Exactly
one source and exactly one target are required — the plugin enforces it at
parse time, the webhook enforces it again at admission:

| Source | Meaning |
|---|---|
| `--from-snapshot NAME [--snapshot-namespace NS]` | An explicit Snapshot CR (scheduled, manual, or discovered). |
| `--from-policy NAME [--policy-namespace NS] [--as-of RFC3339] [--offset N]` | Resolve via the SnapshotPolicy's identity — works with no Snapshot CR present (the GitOps deploy-or-restore pattern). |
| `--identity USER@HOST[:PATH] [--snapshot-id ID] [--as-of] [--offset]` | A raw kopia identity, for foreign writers or snapshots aged out of the catalog. Requires `--repository`. |

| Target | Meaning |
|---|---|
| `--to-pvc NAME` | Write into an existing PVC. |
| `--create-pvc NAME --size 10Gi [--storage-class X] [--access-mode RWO]…` | The operator creates the PVC. `--size` is required — kopiur never guesses a capacity. |
| `--populator` | Passive mode: the restore is claimed later by a PVC's `dataSourceRef`. |

```console
$ kubectl kopiur restore --from-policy nightly --create-pvc data-restored --size 10Gi -n media --wait
restore.kopiur.home-operations.com/restore-nightly-20260611120001 created
restore restore-nightly-20260611120001 completed: kopia id a1b2c3d4e5f6, 5.0 GiB / 1000 files into pvc/data-restored
```

Every `spec.options`/`spec.policy` knob has a flag: `--enable-file-deletion`
(exact-mirror restore; default is additive), `--ignore-permission-errors
true|false`, `--write-files-atomically true|false`, `--on-missing-snapshot
fail|continue`, `--wait-timeout 5m`, plus `--backoff-limit` /
`--active-deadline-seconds` for the mover Job. `--wait`, `--logs`, and
`--timeout` behave exactly as in `snapshot now` (Completed → exit 0 with a
summary; Failed → exit 1 with the failure block).

/// warning | Restores fail closed
With an explicit source (`--from-snapshot`/`--identity`), a missing snapshot
fails the restore (`onMissingSnapshot: Fail`) rather than silently no-opping.
Only `--from-policy` defaults to `continue`, for deploy-or-restore.

///

/// note | Identity-based sources work on every backend
The operator resolves `--from-policy` and un-pinned `--identity` sources by
listing the repository's snapshots **inside the restore Job**, so picking
"latest" (or `--as-of`/`--offset`) works the same on S3/GCS/Azure/B2/SFTP/
WebDAV/rclone as on a filesystem repo — no controller-side repo mount needed.
`--from-snapshot` and a pinned `--identity … --snapshot-id <ID>` still skip the
listing entirely.

///

## `logs`

Stream the mover Job's logs for a Snapshot or Restore without chasing
Job/pod names yourself:

```console
$ kubectl kopiur logs snapshot nightly-manual-20260611030012 -n media -f
```

The kind is explicit (`logs snapshot …` / `logs restore …`) because the two
CRs may share names. `-f/--follow`, `--tail N`, and `--previous` behave like
`kubectl logs`. A retried Job's **newest** pod is selected. When the Job and
its pods are already garbage-collected, the plugin prints the tail the
operator recorded in `status.logTail` (plus the structured failure block)
and says so honestly — it never pretends rotated logs are complete.

## Try it end-to-end

Take a backup and restore it into a fresh PVC, end to end, with two commands.

/// note | Prerequisite: the playground

This arc runs against the shared CLI playground (`media` namespace, repository `nas`, policy `nightly`, seeded PVC). Apply it and install the plugin first — see [the playground setup](index.md#try-it-end-to-end).

///

**1. Back up now** and wait for it to finish:

```console
$ kubectl kopiur snapshot now --policy nightly -n media --wait
snapshot.kopiur.home-operations.com/nightly-manual-20260611030012 created
snapshot nightly-manual-20260611030012 succeeded: kopia id a1b2c3d4e5f6, 5.0 GiB, took 12s
```

The `succeeded:` line (exit 0) is the proof — kopia id, size, and duration of the run.

**2. Restore that snapshot** into a brand-new PVC the operator creates for you:

```console
$ kubectl kopiur restore --from-snapshot nightly-manual-20260611030012 \
    --create-pvc data-restored --size 10Gi -n media --wait
restore.kopiur.home-operations.com/restore-nightly-manual-20260611030012 created
restore restore-nightly-manual-20260611030012 completed: kopia id a1b2c3d4e5f6, 5.0 GiB / 2 files into pvc/data-restored
```

The `completed:` line (exit 0) reports the bytes and file count written **into pvc/data-restored**.

**3. Confirm the restored PVC is real (deep)** — it exists and is `Bound`:

```console
$ kubectl -n media get pvc data-restored
NAME            STATUS   VOLUME       CAPACITY   ACCESS MODES   AGE
data-restored   Bound    pvc-9f3a…    10Gi       RWO            30s
```

Mount it in a throwaway pod and diff it against the source if you want the full round-trip proof.

/// note | Illustrative output

The kopia ids, sizes, durations, timestamps in the names, and the bound `VOLUME`/`AGE` vary per run. The load-bearing facts are the verbatim `succeeded:` / `completed:` line formats, exit 0, and `data-restored` reaching `Bound`.

///
