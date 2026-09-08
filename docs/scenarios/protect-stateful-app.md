# Scenario 01 — Protect a stateful app, consistently

**The everyday case.** You run a database on a PVC. You want nightly backups that the database can actually be restored from, and a sane retention window. This is the usual reason to install Kopiur, and it takes four resources in one namespace.

It goes one step past [example 01](../examples.md#example-01--single-pvc-scheduled). It adds **hooks** that quiesce the database around the snapshot, so the captured bytes are application-consistent, not just crash-consistent.

/// info | What you'll deploy

Four objects, all in the app's namespace, because that's where the mover Job runs:

- a `Secret` holding the backend credentials and the repository password;
- a `Repository`, a new S3 repository for this app;
- a `SnapshotPolicy`, the recipe: source PVC, copy method, hooks, retention;
- a `SnapshotSchedule`, the nightly cron.

///

## The values you'll actually change

| Field | Where | What it does |
| --- | --- | --- |
| `backend.s3.bucket` / `prefix` / `endpoint` / `region` | `Repository` | Points at your object store. |
| `KOPIA_PASSWORD` | the `Secret` | Encrypts the repository. **Lose it, lose the backups.** |
| `sources[].pvc.name` | `SnapshotPolicy` | The PVC to back up. |
| `hooks.*.workloadExec` | `SnapshotPolicy` | The commands that quiesce and unquiesce your app. Swap the Postgres ones for your database's equivalents. |
| `retention.keep*` | `SnapshotPolicy` | The GFS window: how many daily, weekly, and monthly snapshots to keep. |
| `schedule.cron` / `jitter` | `SnapshotSchedule` | When it runs. `H` picks a stable minute for you. |

/// tip | Why hooks instead of just a snapshot?

`copyMethod: Snapshot` is the default. It needs the CSI snapshot stack and is the right choice for a database like this, and it already gives you a crash-consistent point-in-time copy.

The `beforeSnapshot` and `afterSnapshot` hooks add **application consistency** on top. They run _inside the workload_ to tell the database a backup is starting, so what's on disk at snapshot time is a clean, restorable state.

A hook failure aborts the backup unless you set `continueOnFailure: true`. We set it on the _after_ hook so the database never gets stuck in backup mode.

///

```yaml
--8<-- "deploy/examples/scenarios/01-protect-stateful-app.yaml:chain"
```

## Verify it worked

The `Repository` should reach `Ready` first:

```console
$ kubectl get repository -n billing
NAME               PHASE   AGE
postgres-primary   Ready   30s
```

Then fire one backup by hand before you trust the schedule. That confirms the whole chain: recipe, then mover, then snapshot. This `Snapshot` manifest ships with the example, in its `manual-snapshot` section:

```yaml
--8<-- "deploy/examples/scenarios/01-protect-stateful-app.yaml:manual-snapshot"
```

```console
$ kubectl apply -f deploy/examples/scenarios/01-protect-stateful-app.yaml

$ kubectl get snapshots -n billing -w
NAME                       PHASE       ORIGIN   SNAPSHOT    AGE
postgres-data-test-x9f     Running     manual               7s
postgres-data-test-x9f     Succeeded   manual   k1f1ec0a8   44s
```

A `SNAPSHOT` id on a `Succeeded` backup means the data is in the repository. From here the `SnapshotSchedule` takes over nightly.

## See also

- [Backups & schedules](../backups.md): every field on these three resources, including the other hook forms (`runJob`, `httpRequest`) and `copyMethod`.
- [Scenario 06, verification drills](verification-drills.md): prove these backups actually restore.
- [Movers, RBAC & credentials](../movers.md): where the mover runs and what it needs.
