---
title: CRD reference
---

Field-by-field reference for the nine CRDs in `kopiur.home-operations.com/v1alpha1`. Each page explains what a field does, its default, the values it accepts, and when you would set it. This is the detail that used to live in the CRD `description` text.

There are three layers, depending on how much detail you want:

- **[Field reference](/field-reference/)** is the short, complete table of every field's type, default and immutability across all CRDs. Start there to look up a single field fast.
- **These pages** cover the same fields with the fuller explanation: defaults, allowed values, gotchas, and how a field maps to kopia behavior.
- **Task guides** such as [Backups](/backups/), [Restores](/restores/) and [Repositories](/repositories/) tie the fields together for a goal.

If you want the _rationale_ behind the field shapes, so sub-objects, materialized defaults, CEL cost budgets and immutability rules, that lives in [CRD design rationale](/dev/design-rationale/) for contributors.

## The CRDs

- [Repository](/reference/crds/repository/) is a namespaced kopia repository.
- [ClusterRepository](/reference/crds/cluster-repository/) is a cluster-scoped, multi-tenant repository.
- [SnapshotPolicy](/reference/crds/snapshot-policy/) is the backup recipe: what to back up.
- [Snapshot](/reference/crds/snapshot/) is one backup invocation.
- [SnapshotSchedule](/reference/crds/snapshot-schedule/) is the cron that fires snapshots.
- [Restore](/reference/crds/restore/) restores data from a repository.
- [Maintenance](/reference/crds/maintenance/) runs repository maintenance, quick and full.
- [RepositoryReplication](/reference/crds/repository-replication/) keeps an off-site mirror.
- [SnapshotReplication](/reference/crds/snapshot-replication/) copies snapshots between repositories.
- [Shared sub-objects](/reference/crds/shared-types/) covers the types reused across CRDs, such as Backend and MoverSpec.
