# Operations: status, doctor, maintenance, suspend

The day-2 commands: a one-screen health overview, an installation diagnostic,
out-of-band maintenance, and the declarative pause switch. All
[global flags](index.md#global-flags) apply.

## `status`

A one-screen health overview — repositories (with the `Ready` condition
message inlined for anything not Ready), policies (last successful snapshot /
last verification), schedules (last/next fire, consecutive failures),
in-flight work counts, and anything reporting `Stalled=True`:

```console
$ kubectl kopiur status -n media
REPOSITORIES
KIND        NAME  NAMESPACE  PHASE  BACKEND  MODE       SUSPENDED  MAINTENANCE
Repository  nas   media      Ready  S3       ReadWrite  false      configured

POLICIES
NAME     NAMESPACE  REPOSITORY      SUSPENDED  LAST-SNAPSHOT  LAST-VERIFIED
nightly  media      Repository/nas  false      9h ago         -
…
IN FLIGHT: 0 snapshot(s), 0 restore(s)
```

`--repository NAME [--repository-kind …]` narrows everything to one
repository and the policies/schedules/work attached to it. `-o yaml|json`
emits the full typed report for dashboards/scripts.

## `doctor`

Diagnoses an installation and exits 1 if anything failed: the 8 CRDs are
installed and serve `v1alpha1`, the controller (and webhook, when installed)
Deployments are ready, a **live dry-run admission probe** (an intentionally
invalid SnapshotPolicy that must be denied — zero cluster mutation), every
repository is `Ready`, every repository's credential Secrets resolve, nothing
has been Pending/Running longer than `--stuck-threshold` (default `1h`), and
recent Warning events are summarized.

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
  ok    no stuck snapshots/restores
  ok    recent warning events

8 check(s): 1 failed, 0 warning(s)
```

Checks the user lacks RBAC for degrade to warnings naming the missing grant —
doctor never crashes on a restricted kubeconfig.

## `maintenance run`

Trigger an out-of-band maintenance run, by Maintenance name or by the
repository it covers (the operator default-manages one per repository). The
plugin stamps the `run-requested`/`run-mode` annotations (also usable from
bare `kubectl annotate` — see [Maintenance](../maintenance.md)); the operator
runs it through the same lease and single-flight path as the cron slots and
answers in `status.manualRun`.

```console
$ kubectl kopiur maintenance run --repository nas --full -n media --wait
maintenance.kopiur.home-operations.com/nas full run requested (2026-06-11T12:00:00Z)
maintenance nas full run completed at 2026-06-11T12:01:42Z
```

`--full` selects the full (compaction + reclamation) pass; the default is
quick. `--wait` exits 0 on `Succeeded` and 1 on `Failed`.

## `suspend` / `resume`

Pause and unpause reconciliation declaratively. Suspending a
**SnapshotSchedule** stops it firing; suspending a **SnapshotPolicy** makes
schedules skip it; suspending a **Repository**/**ClusterRepository** pauses
all work against that repository; suspending a **RepositoryReplication**
pauses replication runs. This is the same `suspend` field you can set in
GitOps — the plugin just flips it for you (and is idempotent: re-suspending
prints `unchanged`).

```console
$ kubectl kopiur suspend schedule nightly -n media
snapshotschedule.kopiur.home-operations.com/nightly suspended

$ kubectl kopiur resume schedule nightly -n media
snapshotschedule.kopiur.home-operations.com/nightly resumed
```

The kind is one of `policy`, `schedule`, `repository`,
`cluster-repository` (alias `clusterrepo`), `replication`.

/// tip | GitOps users: this is a spec edit
`suspend` patches `spec` (with field manager `kubectl-kopiur`), so a GitOps
controller that owns the object will revert it on the next sync. For a
durable pause, set `suspend: true` in Git instead — the plugin is for the
interactive "stop the bleeding now" moment.

///

## Try it end-to-end

Walk the day-2 commands against a live install: a health overview, the diagnostic, the pause switch, and an out-of-band maintenance run.

/// note | Prerequisite: the playground

This arc runs against the shared CLI playground (`media` namespace, repository `nas`, policy + schedule `nightly`). Apply it and install the plugin first — see [the playground setup](index.md#try-it-end-to-end).

///

**1. One-screen health** with `status`:

```console
$ kubectl kopiur status -n media
REPOSITORIES
KIND        NAME  NAMESPACE  PHASE  BACKEND  MODE       SUSPENDED  MAINTENANCE
Repository  nas   media      Ready  S3       ReadWrite  false      configured

POLICIES
NAME     NAMESPACE  REPOSITORY      SUSPENDED  LAST-SNAPSHOT  LAST-VERIFIED
nightly  media      Repository/nas  false      -              -

SCHEDULES
NAME     NAMESPACE  SCHEDULE   SUSPENDED  LAST-FIRE  NEXT-FIRE             FAILURES
nightly  media      H 2 * * *  false      -          2026-06-12T02:17:00Z  0

IN FLIGHT: 0 snapshot(s), 0 restore(s)
```

**2. Diagnose** with `doctor` — exit 0 when everything is healthy:

```console
$ kubectl kopiur doctor -n media
  ok    CRDs installed
  ok    controller running
  ok    webhook running
  ok    webhook admission (live dry-run probe)
  ok    repositories ready
  ok    credential secrets present
  ok    no stuck snapshots/restores
  ok    recent warning events

8 check(s): 0 failed, 0 warning(s)
```

**3. Pause and unpause** the schedule declaratively (idempotent — re-running prints `unchanged`):

```console
$ kubectl kopiur suspend schedule nightly -n media
snapshotschedule.kopiur.home-operations.com/nightly suspended

$ kubectl kopiur resume schedule nightly -n media
snapshotschedule.kopiur.home-operations.com/nightly resumed
```

**4. Run maintenance out of band (deep)** — a full pass against the `nas` repository, waited to completion:

```console
$ kubectl kopiur maintenance run --repository nas --full -n media --wait
maintenance.kopiur.home-operations.com/nas full run requested (2026-06-11T12:00:00Z)
maintenance nas full run completed at 2026-06-11T12:01:42Z
```

The plugin stamps the `run-requested`/`run-mode` annotations on the operator-managed `Maintenance`, which runs it through the same lease + single-flight path as the cron slots. Confirm it landed in status:

```console
$ kubectl -n media get maintenance nas -o jsonpath='{.status.manualRun.phase}{"\n"}'
Succeeded
```

/// note | Illustrative output

The `NEXT-FIRE` time, the maintenance timestamps, and `LAST-SNAPSHOT`/`LAST-FIRE` (which read `-` until the first run) vary per install. The verbatim parts are the table headers, the `IN FLIGHT:` line, the doctor checks/footer, the suspend/resume lines, and the `requested (…)` / `completed at …` maintenance lines.

///
