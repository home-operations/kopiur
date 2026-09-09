# Operations: status, doctor, maintenance, suspend

The day-2 commands: a one-screen health overview, an installation diagnostic, out-of-band maintenance, and the pause switch you can also set in Git. All [global flags](index.md#global-flags) apply.

## `status`

A one-screen health overview. It shows repositories, with the `Ready` condition message inlined for anything that is not Ready; policies, with the last successful snapshot and last verification; schedules, with the last and next fire and the consecutive-failure count; counts of work in flight; and anything reporting `Stalled=True`.

```console
$ kubectl kopiur status -n media
REPOSITORIES
KIND        NAME  NAMESPACE  PHASE  BACKEND  MODE       SUSPENDED  MAINTENANCE  CLUSTER  FOREIGN  DISCOVERED
Repository  nas   media      Ready  S3       ReadWrite  false      configured   -        -        0

POLICIES
NAME     NAMESPACE  REPOSITORY      SUSPENDED  LAST-SNAPSHOT  LAST-VERIFIED
nightly  media      Repository/nas  false      9h ago         -
…
IN FLIGHT: 0 snapshot(s), 0 restore(s)
```

`DISCOVERED` is the repository's `status.catalog.discoveredBackupCount`: how many `Snapshot` objects the last catalog scan materialized from snapshots already in the repository. It shows `-` when the repository has never been scanned.

That column is plain inventory rather than a warning. A repository shared by several apps, or one re-seeded from a replica, legitimately carries a non-zero count. `CLUSTER` and `FOREIGN` are the multi-cluster pair: this cluster's identity suffix, and how many snapshots the last scan attributed to another cluster.

`--repository NAME`, optionally with `--repository-kind`, narrows everything to one repository and to the policies, schedules and work attached to it. `-o yaml` and `-o json` emit the full typed report for dashboards and scripts.

## `doctor`

Diagnoses an installation and exits 1 if anything failed. There are nine checks: the 9 CRDs are installed and serve `v1alpha1`; the controller Deployment is ready, and the webhook Deployment too when it is installed; a **live dry-run admission probe** succeeds, using an intentionally invalid SnapshotPolicy that must be denied, and changing nothing in the cluster; every repository is `Ready` and unblocked; every repository's credential Secrets resolve; no work is **blocked or stuck**; no Snapshot or Restore failed within `--failure-lookback`, which defaults to `24h`; and recent Warning events are summarized.

```console
$ kubectl kopiur doctor -n media
  ok    CRDs installed
  ok    controller running
  ok    webhook running
  ok    webhook admission (live dry-run probe)
  ok    repositories ready
  FAIL  credential secrets present: Repository/nas: secret media/kopia-creds not found
        why: movers load credentials via namespace-local envFrom; a missing Secret fails every run against that repository
        fix: create the Secret in the named namespace (or enable credentialProjection where supported)
  ok    no blocked or stuck work
  ok    no recent failed snapshots/restores
  ok    recent warning events

9 check(s): 1 failed, 0 warning(s)
```

Checks the user lacks RBAC for degrade to warnings that name the missing grant, so doctor never crashes on a restricted kubeconfig. Warnings do **not** change the exit code: `doctor` exits `1` only when a check *failed*, and `0` otherwise — including a run that could not fully verify itself. In CI, read the `warning(s)` count in the summary line (or the `outcome` fields of `-o json`) when "could not check" needs to be distinguished from "all clear".

### Blocked is not the same as old

`no blocked or stuck work` reads **conditions**, not just phases.

Some states never fix themselves. The operator parks the object and waits for a change only you can make: a namespace opt-in annotation for a privileged mover, a missing credential `Secret` or `ServiceAccount`, an acknowledgement for the [mass-deletion breaker](../repositories.md#deletionprotection--the-mass-deletion-circuit-breaker), or a `SnapshotSchedule` whose previous run sits at a phase this plugin cannot read. The object's phase stays an unremarkable `Pending`, so an age threshold would hide it.

Those are reported **immediately, whatever the object's age**, and the FAIL line quotes the operator's own condition message, which already contains the exact command to run:

```console
$ kubectl kopiur doctor -n media
  FAIL  no blocked or stuck work: snapshot media/nightly-1759: blocked on MoverPermitted=False (PrivilegedMoverNotPermitted): the mover for SnapshotPolicy media/nightly needs elevated privileges; run: kubectl annotate namespace media kopiur.home-operations.com/privileged-movers=allow
        why: a structural gate never self-heals — the operator has parked the object until a human makes an out-of-band change, so it will wait forever however new it is
        fix: the condition message above is the operator's own diagnosis and carries the exact command to run; apply it and the object proceeds on its own
```

`--stuck-threshold`, default `1h`, governs only the **age**-based verdict, meaning an in-flight Snapshot or Restore that is not blocked but is slow. A Snapshot being deleted is measured from its `deletionTimestamp` rather than its creation, so a routine retention prune of month-old snapshots is never reported as stuck. Only a finalizer that is genuinely wedged is.

/// tip | Failed work is a separate check

`no recent failed snapshots/restores` fails on a `Failed` Snapshot or Restore whose failure is inside `--failure-lookback`, default `24h`, and only **warns** for older ones. `failedJobsHistoryLimit` keeps failed objects around by design, so without that window one bad night last month would leave doctor permanently red.

A failure that a *deliberate configuration* explains is listed but never counted red, whatever its age. A repository you flipped to `mode: ReadOnly` refuses backups by design, so a schedule still firing against it warns rather than failing for the whole migration.

///

Every check reports what it could actually see. If one kind cannot be listed, say with a kubeconfig that lacks `list snapshotschedules`, only that kind degrades: the objects that did list are still examined, and the unreadable kind is named next to the verdict. A green report never stands in for a cluster the plugin could not read.

If the plugin is older than the operator it says so instead of reporting green. A phase it cannot read, a gate reason it does not know, or a response it cannot decode all render as a check telling you to upgrade `kubectl-kopiur`.

## `maintenance run`

Trigger an out-of-band maintenance run, either by `Maintenance` name or by the repository it covers, since the operator manages one per repository by default.

The plugin stamps the `run-requested` and `run-mode` annotations, which you can also set with plain `kubectl annotate`; see [Maintenance](../maintenance.md). The operator then runs it through the same lease and single-flight path as the cron slots, and answers in `status.manualRun`.

```console
$ kubectl kopiur maintenance run --repository nas --full -n media --wait
maintenance.kopiur.home-operations.com/nas full run requested (2026-06-11T12:00:00Z)
maintenance nas full run completed at 2026-06-11T12:01:42Z
```

`--full` selects the full pass, which compacts and reclaims. The default is the quick pass. `--wait` exits 0 on `Succeeded` and 1 on `Failed`.

## `replication run`

Trigger an out-of-band replication run, for either replication kind.

The plugin stamps the `run-requested` annotation, which you can also set with plain `kubectl annotate`; see [Repository replication](../replication.md#run-it-now) and [Snapshot replication](../snapshot-replication.md#run-it-now). The operator runs it through the same gates and single-flight path as the cron slots, and answers in `status.manualRun`.

```console
$ kubectl kopiur replication run nas-primary-offsite -n billing --wait
repositoryreplication.kopiur.home-operations.com/nas-primary-offsite run requested (2026-06-11T12:00:00Z)
RepositoryReplication nas-primary-offsite run completed at 2026-06-11T12:04:18Z
```

The plugin detects the kind from the name. Pass `--kind repository` or `--kind snapshot` when a namespace holds a `RepositoryReplication` **and** a `SnapshotReplication` with the same name, because the plugin refuses to guess. `--wait` exits 0 on `Succeeded` and 1 on `Failed`.

/// note | A successful run re-anchors the schedule

The next cron slot is computed from `status.lastReplicated`, and a successful requested run stamps that field just like a scheduled one does. Running at 14:00 on an `0 5 * * *` mirror therefore moves the next automatic run to 05:00 tomorrow.

///

/// warning | A suspended replication holds the request

The run is recorded as `status.manualRun.phase: Pending` and starts on `kubectl kopiur resume`. That means `--wait` on a suspended object waits out its timeout instead of failing fast. Resume it first.

///

## `suspend` / `resume`

Pause and unpause reconciliation from the command line.

Suspending a **SnapshotSchedule** stops it firing. Suspending a **SnapshotPolicy** makes schedules skip it. Suspending a **Repository** or **ClusterRepository** pauses all work against that repository. Suspending a **RepositoryReplication** pauses replication runs.

This is the same `suspend` field you can set in GitOps; the plugin just flips it for you. It is idempotent, so re-suspending prints `unchanged`.

```console
$ kubectl kopiur suspend schedule nightly -n media
snapshotschedule.kopiur.home-operations.com/nightly suspended

$ kubectl kopiur resume schedule nightly -n media
snapshotschedule.kopiur.home-operations.com/nightly resumed
```

The kind is one of `policy`, `schedule`, `repository`, `cluster-repository` (with the alias `clusterrepo`), or `replication`.

/// tip | GitOps users: this is a spec edit
`suspend` patches `spec`, with field manager `kubectl-kopiur`, so a GitOps controller that owns the object will revert it on the next sync. For a durable pause, set `suspend: true` in Git instead. The plugin is for the interactive "stop the bleeding now" moment.

///

## Try it end-to-end

Walk the day-2 commands against a live install: a health overview, the diagnostic, the pause switch, and an out-of-band maintenance run.

/// note | Prerequisite: the playground

This arc runs against the shared CLI playground: the `media` namespace, repository `nas`, and the `nightly` policy and schedule. Apply it and install the plugin first, as described in [the playground setup](index.md#try-it-end-to-end).

///

**1. One-screen health** with `status`:

```console
$ kubectl kopiur status -n media
REPOSITORIES
KIND        NAME  NAMESPACE  PHASE  BACKEND  MODE       SUSPENDED  MAINTENANCE  CLUSTER  FOREIGN  DISCOVERED
Repository  nas   media      Ready  S3       ReadWrite  false      configured   -        -        0

POLICIES
NAME     NAMESPACE  REPOSITORY      SUSPENDED  LAST-SNAPSHOT  LAST-VERIFIED
nightly  media      Repository/nas  false      -              -

SCHEDULES
NAME     NAMESPACE  SCHEDULE   SUSPENDED  LAST-FIRE  NEXT-FIRE             FAILURES
nightly  media      H 2 * * *  false      -          2026-06-12T02:17:00Z  0

IN FLIGHT: 0 snapshot(s), 0 restore(s)
```

**2. Diagnose** with `doctor`. It exits 0 when everything is healthy:

```console
$ kubectl kopiur doctor -n media
  ok    CRDs installed
  ok    controller running
  ok    webhook running
  ok    webhook admission (live dry-run probe)
  ok    repositories ready
  ok    credential secrets present
  ok    no blocked or stuck work
  ok    no recent failed snapshots/restores
  ok    recent warning events

9 check(s): 0 failed, 0 warning(s)
```

**3. Pause and unpause** the schedule. It is idempotent, so re-running prints `unchanged`:

```console
$ kubectl kopiur suspend schedule nightly -n media
snapshotschedule.kopiur.home-operations.com/nightly suspended

$ kubectl kopiur resume schedule nightly -n media
snapshotschedule.kopiur.home-operations.com/nightly resumed
```

**4. Run maintenance out of band (deep).** A full pass against the `nas` repository, waited to completion:

```console
$ kubectl kopiur maintenance run --repository nas --full -n media --wait
maintenance.kopiur.home-operations.com/nas full run requested (2026-06-11T12:00:00Z)
maintenance nas full run completed at 2026-06-11T12:01:42Z
```

The plugin stamps the `run-requested` and `run-mode` annotations on the operator-managed `Maintenance`, which runs it through the same lease and single-flight path as the cron slots. Confirm it landed in status:

```console
$ kubectl -n media get maintenance nas -o jsonpath='{.status.manualRun.phase}{"\n"}'
Succeeded
```

/// note | Illustrative output

The `NEXT-FIRE` time, the maintenance timestamps, and `LAST-SNAPSHOT` and `LAST-FIRE`, which read `-` until the first run, all vary per install. What appears exactly as shown is the table headers, the `IN FLIGHT:` line, the doctor checks and footer, the suspend and resume lines, and the `requested (…)` and `completed at …` maintenance lines.

///
