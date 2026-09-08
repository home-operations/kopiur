# Kopiur

**Kopiur**, from Kopia plus Rust, is a Kopia-native Kubernetes backup operator written in Rust on [`kube-rs`](https://github.com/kube-rs/kube).

It makes a kopia repository a first-class Kubernetes resource, and it splits a backup into three resources: the **recipe**, its **invocation**, and its **schedule**. That is what lets a backup be triggered by cron, by `kubectl create`, by Argo Events, or by a Helm hook. A kopia snapshot's lifecycle is tied to its `Snapshot` object through a finalizer and a `deletionPolicy`.

The whole CRD surface is modeled as Rust enums, so invalid states cannot be expressed and reconcilers handle every variant at compile time. For the high-level mental model start with [Concepts](concepts/how-kopia-works.md).

/// warning | Alpha

API group `kopiur.home-operations.com`, version `v1alpha1`. The CRD surface may still change between releases.

///

## The 9 CRDs (`kopiur.home-operations.com/v1alpha1`)

| CRD                 | Scope      | Layer                | Purpose                                                                         |
| ------------------- | ---------- | -------------------- | ------------------------------------------------------------------------------- |
| `Repository`        | Namespaced | Storage              | A kopia repository owned by one namespace: backend, encryption, credentials.    |
| `ClusterRepository` | Cluster    | Storage              | A shared repository for platform teams, gated by `allowedNamespaces`.           |
| `SnapshotPolicy`      | Namespaced | Recipe               | _What_ to back up: PVC sources, identity, retention, policy, hooks, into one repository or a 1–8 [multi-repository fan-out](backups.md#repositories--one-recipe-several-repositories-fan-out). Idempotent. |
| `Snapshot`            | Namespaced | Invocation + Catalog | One kopia snapshot as a Kubernetes object. The universal trigger entry point.   |
| `SnapshotSchedule`    | Namespaced | Cron                 | _When_ it runs: cron + jitter + timezone; creates `Snapshot` CRs.                 |
| `Restore`           | Namespaced | Operation            | Restore a snapshot to a PVC, or act as a passive volume-populator source.       |
| `Maintenance`       | Namespaced | Lifecycle            | Schedules `kopia maintenance` quick + full with an ownership lease.             |
| `RepositoryReplication` | Namespaced | Durability        | Mirror a repository's blobs to a second backend on a schedule (the "2" in 3-2-1). |
| `SnapshotReplication` | Namespaced | Durability        | Copy selected snapshots from one repository into another on a schedule (kopia `snapshot migrate`). |

## Where to next

- **[How Kopia works](concepts/how-kopia-works.md)** covers content-addressable dedup, snapshots, the `username@hostname:path` identity model, encryption and maintenance, plus why one shared repository is the recommended layout.
- **[Why Kopiur is designed this way](concepts/why-kopiur.md)** covers the recipe, invocation and schedule split, repository-as-resource, the type-safety argument, and tying a snapshot's lifecycle to its object.
- **[Getting started](getting-started.md)** is the end-to-end walkthrough: install, first backup, and a verified restore in about 15 minutes.
- **[Scenarios](scenarios/index.md)** are problem-driven, end-to-end walkthroughs: protect a database, recover deleted data, disaster recovery, migration, adopting an existing repository, verification drills.
- **[Installation](install.md)** covers prerequisites, install modes, and the CRD-lifecycle caveat.
- **[Repositories & backends](repositories.md)** points Kopiur at S3, Azure, GCS, B2, a NAS, or rclone.
- **[Backups & schedules](backups.md)** and **[Restores](restores.md)** cover the recipe, invocation and schedule model, and reading data back.
- **[Troubleshooting](troubleshooting.md)** is for when something does not go green.
- **[GitOps (Flux / Argo)](gitops.md)** covers kstatus health, `kubectl wait`, managed-by and ownerRefs, and drift-free applies.
- **[Field reference](field-reference.md)** lists every field of all 9 CRDs, with type, default and immutability.
- **[API reference (rustdoc)](api-reference.md)** is the generated Rust API docs for every crate in the workspace.
- **[API conventions](dev/api-conventions.md)** and **[Observability](dev/observability.md)** are developer notes.
