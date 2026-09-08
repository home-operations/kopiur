# CRD reference

Field-by-field reference for the nine CRDs in `kopiur.home-operations.com/v1alpha1`. Each page explains what a field does, its default, the values it accepts, and when you would set it. This is the detail that used to live in the CRD `description` text.

There are three layers, depending on how much detail you want:

- **[Field reference](../../field-reference.md)** is the short, complete table of every field's type, default and immutability across all CRDs. Start there to look up a single field fast.
- **These pages** cover the same fields with the fuller explanation: defaults, allowed values, gotchas, and how a field maps to kopia behavior.
- **Task guides** such as [Backups](../../backups.md), [Restores](../../restores.md) and [Repositories](../../repositories.md) tie the fields together for a goal.

If you want the *rationale* behind the field shapes, so sub-objects, materialized defaults, CEL cost budgets and immutability rules, that lives in [CRD design rationale](../../dev/design-rationale.md) for contributors.

## The CRDs

- [Repository](repository.md) is a namespaced kopia repository.
- [ClusterRepository](cluster-repository.md) is a cluster-scoped, multi-tenant repository.
- [SnapshotPolicy](snapshot-policy.md) is the backup recipe: what to back up.
- [Snapshot](snapshot.md) is one backup invocation.
- [SnapshotSchedule](snapshot-schedule.md) is the cron that fires snapshots.
- [Restore](restore.md) restores data from a repository.
- [Maintenance](maintenance.md) runs repository maintenance, quick and full.
- [RepositoryReplication](repository-replication.md) keeps an off-site mirror.
- [SnapshotReplication](snapshot-replication.md) copies snapshots between repositories.
- [Shared sub-objects](shared-types.md) covers the types reused across CRDs, such as Backend and MoverSpec.
