# Scenario 04 — Migrate an app across clusters or namespaces

**Move a stateful app, data and all, somewhere new.** In [disaster recovery](disaster-recovery.md) the app keeps its name and namespace. A migration is different: it changes the app's _coordinate_, either to a new namespace (`billing` becomes `payments`) or to a whole new cluster reusing the same repository.

That coordinate change is the catch.

/// note | Moving an app vs. running it in several clusters at once

This page is a one-time **move**. The app runs in exactly one place before and after. The destination takes over the original identity, and you decommission the source, as described below.

If you instead want **several clusters backing up to the same repository at the same time**, whether active-active or a warm DR standby, that is a different and also supported shape. [Share one repository across clusters](shared-repository-multi-cluster.md) uses `identityDefaults.cluster` so each cluster writes under its own distinct identity and never collides. That is exactly what this page's continue-the-lineage approach forbids, as the danger box below explains.

///

/// warning | Why you can't just use `fromPolicy` in the destination

kopia stores each snapshot under `username@hostname:path`, and **`hostname` defaults to the source namespace**. In the destination namespace, a `fromPolicy` restore would compute the _destination's_ identity and find nothing.

So the one-time data carry restores by the **raw `source.identity`**, meaning the source's own `username` plus `hostname`. That is what `identity` mode is for.

///

## The flow

```mermaid
flowchart LR
  subgraph src[Source — namespace billing]
    S[(snapshots<br/>postgres-data@billing)]
  end
  subgraph dst[Destination — namespace payments]
    REPO[Repository<br/>same bucket, connect] --> RST[Restore<br/>by raw identity]
    RST --> PVC[PVC postgres-data]
    BC[SnapshotPolicy<br/>identity pinned to billing] --> SCH[SnapshotSchedule]
  end
  S -. restore by identity .-> RST
```

Applied in the destination namespace, the bundle does three things. It connects a `Repository` to the same bucket. It restores the source's latest snapshot by identity into a new PVC. Then it sets up a `SnapshotPolicy` and a `SnapshotSchedule` to protect the app going forward.

## The decision that matters: continue or fork the lineage

The new `SnapshotPolicy`'s `identity` decides whether the destination's future snapshots **extend the original timeline** or **start a fresh one**:

| Choice | How | Result |
| --- | --- | --- |
| **Continue** the lineage | pin `identity` to the **original** `username`/`hostname` (the bundle does this) | new snapshots dedup against the carried-over history; one logical timeline. |
| **Fork** a fresh lineage | delete the `identity` block; let it default to the destination namespace | a clean new timeline under the new coordinate. |

/// danger | Continue-lineage is for a MOVE, not active-active

If you pin the destination to the original identity, **decommission the source first.** Two clusters writing the _same_ `username@hostname:path` at once will corrupt the snapshot timeline, because kopia has no cross-cluster write coordination of its own. Pinning to the original identity means "the app lives _here_ now", not "the app runs in both places".

If you actually want two or more clusters writing to this repository **at the same time**, don't reach for a shared identity. Set [`identityDefaults.cluster`](../repositories.md#identitydefaultscluster--sharing-one-repository-across-clusters) on the repository instead, so each cluster gets its own distinct `<namespace>.<cluster>` identity and never collides. See [Share one repository across clusters](shared-repository-multi-cluster.md) for that shape, including the safe order of operations to turn it on for a repository already in production.

///

```yaml
--8<-- "deploy/examples/scenarios/04-migrate-across-clusters.yaml"
```

## Verify the carry-over

```console
$ kubectl get restore postgres-migrate-in -n payments -w
NAME                  PHASE       AGE
postgres-migrate-in   Resolving   3s
postgres-migrate-in   Restoring   11s
postgres-migrate-in   Completed   38s

$ kubectl get pvc postgres-data -n payments
NAME            STATUS   VOLUME    CAPACITY   AGE
postgres-data   Bound    pvc-...   100Gi      40s
```

Start the app in `payments` against the restored PVC, confirm it, then tear down the source. The first scheduled backup in `payments` will dedup against the existing data rather than re-uploading it.

/// tip | Finding the source's exact identity

If you're unsure what the source recorded, read it off a source `Snapshot`'s status, or run `kopia snapshot list` against the repository. For a PVC source it is `<config-name>@<namespace>:/pvc/<pvcName>`, unless the source pinned a custom `identity`.

///

## See also

- [Restores → `identity` source](../restores.md#identity--a-raw-kopia-identity): the raw-identity restore mode.
- [Backups → identity](../backups.md#identity--what-kopia-records-usernamehostnamepath): how identity is resolved, and the fork guards that protect existing history.
- [Scenario 05 — adopt an existing repo](adopt-existing-repo.md): a close cousin, for when the "source" is foreign tooling rather than another Kopiur cluster.
- [Share one repository across clusters](shared-repository-multi-cluster.md): the active-active or standby shape this page's danger box points at, instead of a one-time move.
