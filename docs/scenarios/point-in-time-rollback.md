# Scenario 07 — Point-in-time rollback

**You need a specific moment, not "yesterday."** A bad deploy at 14:30 quietly corrupted data over the next hour. You want the volume exactly as it was at **14:00**, just before things went wrong, and you don't want to scroll through `Snapshot` CRs guessing which one that was.

Use `source.fromPolicy` with `asOf`. It resolves through the `SnapshotPolicy`'s **identity**, so it still works if the relevant `Snapshot` CR has aged out of the catalog, and it picks the newest snapshot **at or before** an instant. As always, restore into a **side-by-side** PVC and verify before cutting over.

## Step 1 — Choose the instant

You don't list snapshots. You name the time. `asOf` takes an RFC3339 timestamp and resolves to the newest snapshot at or before it. If you'd rather count backwards, use `offset` instead: `0` is the latest, `1` is the previous one, and so on.

/// tip | `asOf` vs `offset`

Use **`asOf`** when you know _when_ things were good, like "just before the 14:30 deploy".

Use **`offset`** when you know _how many snapshots back_ to go, like "the one before last".

Set one, not both.

///

## Step 2 — Restore into a clone and verify

The bundle restores into a fresh `postgres-data-1400` PVC. The live volume is untouched.

`fromPolicy` defaults to `onMissingSnapshot: Continue`, which is the deploy-or-restore behavior. A deliberate rollback sets it to **`Fail`** instead, so an instant with no snapshot is a loud error rather than a silently empty volume.

```yaml
--8<-- "deploy/examples/scenarios/07-point-in-time-rollback.yaml"
```

```console
$ kubectl get restore postgres-rollback-1400 -n billing -w
NAME                     PHASE        AGE
postgres-rollback-1400   Resolving    2s
postgres-rollback-1400   Restoring    9s
postgres-rollback-1400   Completed    44s
```

Point a throwaway client at the clone PVC and confirm the data is the moment you wanted.

## Step 3 — Cut over

Once you trust the clone, scale the app down. Then either repoint it at the clone, or do an **in-place mirror** restore into the live PVC. That means `target.pvcRef` plus `options.enableFileDeletion: true`, which makes the live volume an exact mirror of the chosen instant. See [example 15](../examples.md#example-15--in-place-mirror-restore). The in-place form is shown commented at the bottom of the bundle.

/// warning | Never roll back in place on the first try

Restoring over a mounted, running database can corrupt it. If you pick the wrong instant, the original is gone too. Clone, verify, _then_ cut over.

///

## See also

- [Restores → point-in-time](../restores.md#frompolicy--resolve-via-a-snapshotpolicys-identity): the `asOf` and `offset` reference.
- [Example 14 — point-in-time / offset restore](../examples.md#example-14--point-in-time--offset-restore) and [example 15 — in-place mirror](../examples.md#example-15--in-place-mirror-restore).
- [Scenario 02 — recover lost data](recover-lost-data.md): the same safe clone-and-verify habit for a known `Snapshot` CR.
