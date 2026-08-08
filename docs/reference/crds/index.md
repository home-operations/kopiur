# CRD reference

Per-CRD, per-field reference for the nine CRDs in
`kopiur.home-operations.com/v1alpha1`. Each page explains, field by field, what a
field does, its default, the allowed values, and when you'd set it — the detail
that used to live in the CRD `description` text.

Three layers, by how much detail you want:

- **[Field reference](../../field-reference.md)** — the terse, exhaustive table of
  every field's type/default/immutability across all CRDs. Start here to look up a
  single field fast.
- **These pages** — the same fields with the fuller per-field explanation
  (defaults, allowed values, gotchas, how a field maps to a kopia behavior).
- **Task guides** ([Backups](../../backups.md), [Restores](../../restores.md),
  [Repositories](../../repositories.md), …) — narrative how-to that ties fields
  together for a goal.

The design *rationale* behind why these fields are shaped the way they are
(sub-objects, materialized defaults, CEL cost budgets, immutability rules) lives in
[CRD design rationale](../../dev/design-rationale.md) for contributors.

## The CRDs

- [Repository](repository.md) — a namespaced kopia repository.
- [ClusterRepository](cluster-repository.md) — a cluster-scoped, multi-tenant repository.
- [SnapshotPolicy](snapshot-policy.md) — the backup recipe (what to back up).
- [Snapshot](snapshot.md) — one backup invocation.
- [SnapshotSchedule](snapshot-schedule.md) — the cron that fires snapshots.
- [Restore](restore.md) — restore data from a repository.
- [Maintenance](maintenance.md) — repository maintenance (quick/full).
- [RepositoryReplication](repository-replication.md) — off-site mirror.
- `SnapshotReplication` — copy snapshots between repositories (see the
  [field reference](../../field-reference.md#snapshotreplication) for now; its
  per-field page ships with the feature docs).
- [Shared sub-objects](shared-types.md) — types reused across CRDs (Backend, MoverSpec, …).
