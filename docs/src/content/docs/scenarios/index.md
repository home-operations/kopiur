---
title: Scenarios
---

The [Examples](/examples/) page is a ladder of **component** manifests, one CRD capability per file. **Scenarios** are the layer above. Each one walks a real situation end to end, like "the cluster is gone, get my data back", and ties together the resources you apply, the `kubectl` commands you run, and the checks that tell you it worked.

Each scenario is backed by one apply-ready bundle under [`deploy/examples/scenarios/`](https://github.com/home-operations/kopiur/tree/main/deploy/examples/scenarios). Copy it, replace the `REPLACE_ME` values, and `kubectl apply -f`.

:::tip[The mental model (read this first if you're new)]

Kopiur splits one job into separate resources so each can change independently:

- a **`Repository`** is _where_ snapshots live: an S3 bucket, a NAS, B2, and so on;
- a **`SnapshotPolicy`** is the **recipe**, meaning _what_ to back up. It runs nothing on its own;
- a **`Snapshot`** is one **invocation**: a single snapshot as a Kubernetes object;
- a **`SnapshotSchedule`** is the **cron**, meaning _when_ the recipe runs;
- a **`Restore`** reads a snapshot back into a PVC;
- a **`Maintenance`** reclaims space in the repository.

**Identity** is the detail that makes or breaks the recovery and migration scenarios. kopia stores each snapshot under `username@hostname:path`, defaulting to `<policy-name>@<namespace>:/pvc/<pvcName>`. To find an _existing_ snapshot, you have to match that identity. See [How Kopia works](/concepts/how-kopia-works/).

:::

| #   | Scenario                                                                            | When you reach for it                                                                            | Complexity |
| --- | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ | ---------- |
| 01  | [Protect a stateful app](/scenarios/protect-stateful-app/)                          | Nightly, application-consistent backups of a database on a PVC.                                  | Simple     |
| 02  | [Recover from accidental data loss](/scenarios/recover-lost-data/)                  | Someone deleted/corrupted data; restore the last good snapshot safely.                           | Simple     |
| 03  | [Disaster recovery on a fresh cluster](/scenarios/disaster-recovery/)               | The cluster is gone; rebuild the app + its data from the repository.                             | Advanced   |
| 04  | [Migrate an app across clusters / namespaces](/scenarios/migrate-across-clusters/)  | Move a stateful app to a new namespace or cluster, data and all.                                 | Advanced   |
| 05  | [Adopt an existing kopia repository](/scenarios/adopt-existing-repo/)               | You already have a kopia repo; take it over without re-uploading.                                | Advanced   |
| 06  | [Backup verification / restore drills](/scenarios/verification-drills/)             | Prove backups are restorable on a schedule, and alert when one isn't.                            | Advanced   |
| 07  | [Point-in-time rollback](/scenarios/point-in-time-rollback/)                        | Roll a volume back to a specific known-good moment (`asOf`/`offset`).                            | Simple     |
| 08  | [Clone an app into another namespace](/scenarios/clone-app-to-namespace/)           | Copy prod data into staging/preview to reproduce a bug or seed an env.                           | Advanced   |
| 09  | [Share one repository across clusters](/scenarios/shared-repository-multi-cluster/) | Several clusters back up to (or restore from) the SAME repository at once, safely.               | Advanced   |
| 10  | [DR from a replicated repository](/scenarios/dr-with-replicated-repository/)        | The primary repository is gone; seed a NEW one from the surviving off-site mirror (`spec.seed`). | Advanced   |

:::caution[Alpha]

These use API group `kopiur.home-operations.com`, version `v1alpha1`. Backends are **externally tagged**: the bucket lives under `backend.s3`, not `backend: { kind: S3 }`.

:::

## See also

- [Getting started](/getting-started/): the install-to-first-restore walkthrough.
- [Examples](/examples/): the per-capability manifest ladder.
- [Backups & schedules](/backups/) and [Restores](/restores/): the field-level reference for the resources these scenarios combine.
- [Troubleshooting](/troubleshooting/): when a step doesn't go green.
