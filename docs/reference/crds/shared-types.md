# Shared sub-objects

Types reused across several CRDs. For the short type-and-default table see the [field reference](../../field-reference.md).

## Backend

The storage backend for a kopia repository. Exactly one backend block is set.

You write the backend as a single-key object, so `backend: { s3: {...} }` or `backend: { filesystem: {...} }`, and the apiserver enforces "exactly one backend" when you apply. See the [backend guides](../../backends/index.md) for provider-specific setup.

The variants:

- **`s3`** is Amazon S3 or any S3-compatible store, such as MinIO, RustFS or Ceph RGW. `bucket` is required. `prefix` lets several repositories share one bucket, for example `clusters/prod/`. Omit `endpoint` for AWS and set it for S3-compatible stores. `region` is required by AWS and by some other providers. `tls` carries TLS overrides. `auth` is a [BackendAuth](#backendauth).
- **`azure`** is Azure Blob Storage. `container` is required. Set `storageAccount` when it cannot be inferred from the credentials.
- **`gcs`** is Google Cloud Storage. `bucket` is required.
- **`b2`** is Backblaze B2. `bucket` is required, and auth is Secret-only, because B2 has no cloud IAM federation.
- **`filesystem`** has kopia write the repository to a `path` inside the mover pod. A [RepoVolume](#repovolume) puts something at that path. Kopia has no NFS backend, so you reach NFS through the filesystem backend by mounting the export at `path`.
- **`sftp`** is an SFTP server. It takes `host`, `path`, an optional `port` that defaults to 22, and `username`. Auth is Secret-only: an SSH key and known-hosts data.
- **`webdav`** is a WebDAV endpoint, given as `url`. Auth is Secret-only HTTP basic auth.
- **`rclone`** is any rclone remote in `remote:path` form, with a Secret holding the `rclone.conf`. Kopia shells out to `rclone`, which reaches providers that have no native kopia backend.

### RepoVolume

What backs a filesystem repository's mount path. It is a single-key object, so exactly one of:

- **`pvc`**, written `volume: { pvc: { name: "repo-pvc" } }`, mounts a `PersistentVolumeClaim` read-write at the repository path, in the mover's namespace.
- **`nfs`**, written `volume: { nfs: { server: "nas.lan", path: "/export/kopia" } }`, mounts an NFS export directly, with no PVC and no StorageClass.

You may leave `volume` out when the path is already present on the node or in the image. That is mainly a baked-in mount used by the e2e harness.

## BackendAuth

Credentials for a cloud object-store backend that has an IAM plane, so S3, Azure and GCS. Set **exactly one of**:

- **`secretRef`** is a [SecretRef](#secretkeyref-and-secretref) to the Secret holding static access credentials. The operator reads well-known keys from it, such as `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` for S3.
- **`workloadIdentity`** uses a cloud-federated `ServiceAccount` instead of static keys.

Leaving `auth` out is also legal, because the well-known keys may ride on the encryption-password Secret instead.

Backends with no cloud IAM plane, so B2, SFTP, WebDAV and rclone, have a Secret-only auth surface. There is no way to express a workload identity on them.

### workloadIdentity

A cloud workload-identity binding: AWS IRSA or EKS Pod Identity, AKS Workload Identity, or GKE Workload Identity. The mover Job runs as a Kubernetes `ServiceAccount` you supply, federated to the cloud IAM role that grants backend access, instead of reading static keys from a Secret.

- **`serviceAccountName`** is the `ServiceAccount` the mover pod runs as, resolved in the mover Job's own namespace.

The `ServiceAccount` must already exist in every namespace mover Jobs run in, because the operator never creates it, and it must carry the cloud-specific annotation: `eks.amazonaws.com/role-arn`, `azure.workload.identity/client-id`, or `iam.gke.io/gcp-service-account`.

## MoverDefaults

Repository-wide mover defaults, inherited by every mover the repository starts: bootstrap, backup, restore, maintenance, verification and replication. A recipe can override each field through its own [`mover`](#moverspec) block. This is the single place a repository defines mover identity, hardening, resources and cache. See [movers](../../movers.md).

- **`securityContext`** is the base container security context for every mover.
- **`podSecurityContext`** is the base pod security context, most often used for `fsGroup`.
- **`resources`** is the base resource requests and limits for the mover container.
- **`cache`** sets kopia [CacheDefaults](#cachedefaults) for every mover.
- **`scratch`** sets [ScratchDefaults](#scratchdefaults) for the deep-verification scratch volume.
- **`nodeSelector`**, **`tolerations`** and **`affinity`** set pod scheduling for every mover.
- **`podLabels`** adds extra labels to every mover **pod**, and the same labels are mirrored onto the owning `Job`. This is the hook for cluster machinery that keys off pod labels and that Kopiur has no field for: a Kueue `kueue.x-k8s.io/queue-name`, a monitoring or `NetworkPolicy` selector, a mesh exclusion label. Your labels merge **under** Kopiur's own, so a value you set can never break the selectors the controller counts and cleans up by.
- **`podAnnotations`** adds extra annotations to every mover pod. They go on the **pod template only** and are not mirrored onto the `Job`. The usual reason is a sidecar-injection opt-out such as `sidecar.istio.io/inject: "false"`, which only means anything on the pod a mesh webhook actually sees. It matters here because an injected sidecar that never exits keeps a short-lived mover Job running forever.
- **`sourceColocation`** controls node co-location for a `ReadWriteOnce` source PVC. See below.
- **`ttlSecondsAfterFinished`** sets `Job.spec.ttlSecondsAfterFinished` on every mover Job, so finished Jobs clean themselves up. It defaults to 1 hour when neither the repository nor the recipe sets one.
- **`throttle`** sets repository throttle limits, applied with `kopia repository throttle set` by every mover after it connects to this repository, so one run does not saturate the link or hammer the object store. The four values are `uploadBytesPerSecond`, `downloadBytesPerSecond`, `readOpsPerSecond` and `writeOpsPerSecond`, and each one you leave out keeps kopia's current limit.

  The transfer movers honor it: bootstrap, backup, restore, maintenance, verification, repository replication, snapshot replication and seed.

  Kopia's limits are **per connection**. A run that opens two repositories caps each one from *that* repository's own `moverDefaults`. A [`SnapshotReplication`](snapshot-replication.md) caps its source connection from the source repository's defaults and its destination connection from the destination repository's, and you can override each per object through [`spec.migrate.throttle`](snapshot-replication.md#migrate). A migrate-mode [`seed`](repository.md#seed) works the same way: the replica's connection is capped from the replica's defaults and this repository's from its own, and you can override each while the seed is armed through `spec.seed.migrate.throttle.source` and `.destination`.

  Three things it does **not** cap today. The interactive [`serve`/browse](../../server.md) session is a read-only UI session rather than a batch transfer, so it is left alone. A `RepositoryReplication`'s **destination** side is not capped here, because `kopia repository sync-to` copies blobs without opening the destination as a repository; its caps live on [`spec.sync`](repository-replication.md) instead. **Batched snapshot deletions and pins** are not capped either, because their work specs leave the throttle block empty, so a repository-wide cap does not reach them. Deletion load is bounded instead by per-repository batching and by the cluster-wide `maxConcurrentDeleteJobs` backstop.

/// warning | A byte cap bites much harder than its number suggests

A `*BytesPerSecond` cap applies only to **cold** backend traffic. Content already in the mover's kopia cache is read without touching the limiter, so a warm re-run can look completely unthrottled.

On small-object workloads the effective throughput also lands far below the number you set. In one measurement a 2 MB/s cap took about 14 seconds to move a 28 KiB cold repository, while caps of 10 MB/s and above often did not bind at all at that size.

Treat these values as a **ceiling for large transfers**. Set them generously and check them against your own data, rather than tuning them down to a number that looks safe.

///

/// warning | Reserved pod-metadata keys are rejected, not ignored

Keys under `kopiur.home-operations.com/`, and the exact key `app.kubernetes.io/managed-by`, are refused at admission in both `podLabels` and `podAnnotations`.

Your metadata merges under Kopiur's own, so such a key would be dropped silently when the pod is rendered, and the manifest would claim a label the pod never carries. Kopiur rejects it when you apply instead.

An interactive `kubectl kopiur browse` session pod is deliberately asymmetric: it applies `podAnnotations` and ignores `podLabels`. A mesh sidecar would outlive the session's TTL, while a batch queue-name label would put a human waiting at a terminal behind a nightly backup window.

///

The `securityContext`, `podSecurityContext`, `resources` and `cache` fields resolve by merging one field at a time. A repository-wide default composes with a partial per-recipe override, and both sit on top of a hardened base. See [security context](../../security-context.md) for the hardened defaults and for how overrides can only tighten them.

### sourceColocation

Controls how a mover co-locates with the node its `ReadWriteOnce` (RWO) source or destination PVC is attached to. That avoids a Kubernetes Multi-Attach error, because an RWO PVC can only attach to one node at a time.

The `mode` is one of:

- **`Auto`**, the default, pins the mover to the attached node when Kopiur can discover it, and otherwise schedules freely. `ReadWriteMany` and `ReadOnlyMany` volumes are never pinned. This fixes the Multi-Attach error with no configuration.
- **`Required`** behaves like `Auto`, but **fails** the run with an actionable error when an RWO PVC's node cannot be determined.
- **`Disabled`** never computes a node pin. The mover uses only the explicit `nodeSelector`, `affinity` and `tolerations`.

## MoverSpec

Per-recipe mover overrides, set on a recipe's `mover` field. Each field overlays the repository's [MoverDefaults](#moverdefaults) one field at a time: the recipe wins, the repository default fills the gaps, and the hardened base sits underneath.

- **`resources`** sets resource requests and limits for the mover container.
- **`cache`** overrides the repository's [CacheDefaults](#cachedefaults).
- **`securityContext`** is the container security context. Set only the fields you want to change.
- **`podSecurityContext`** is the pod security context, notably `fsGroup`, which makes a freshly provisioned volume group-writable so an unprivileged mover can populate it on restore without root.
- **`privilegedMode`** is opt-in and gated per namespace. It preserves UID and GID on restore.
- **`inheritSecurityContextFrom`** is described below.
- **`ttlSecondsAfterFinished`** overrides the Job TTL for this recipe.

A pod-level `runAsUser: 0` or `runAsNonRoot: false` counts as privileged, exactly like the container-level setting.

### inheritSecurityContextFrom

Copies the UID and GID security context from a live workload instead of setting an explicit `securityContext` or `podSecurityContext`.

It combines with both of those. They sit in a higher merge layer, so an explicit field overrides the inherited one, and stands alone when no pod resolves. The workload must pin `runAsUser`, because a UID that comes from the image's `USER` line is invisible in the pod spec.

It is a single-key object, so exactly one of:

- **`workloadSelector`** matches the workload pods with a label selector (`podSelector` plus an optional `container`) and inherits the chosen container's `securityContext` plus the pod's `spec.securityContext`. It works for both backup and restore; on a restore it inherits from the pod that will read the data.
- **`pvcConsumer`** is for **backup sources only**. It derives the workload pod from the PVC this snapshot backs up: the operator finds the pods mounting the source claim and inherits their security context, with no hand-written selector. It means nothing on a restore, where the consuming pod may not exist yet, so a restore must use `workloadSelector`. `pvcConsumer` accepts an optional `container`.

The resolved context still enters as the recipe layer, so the hardened base still applies: `drop:[ALL]`, seccomp, and `fsGroup`.

## CacheDefaults

How a mover's kopia cache volume is provisioned and sized.

- **`capacity`** is the size of the PVC backing the cache, `10Gi` for example.
- **`storageClassName`** is the StorageClass for the cache PVC. Leave it out to use the cluster default.
- **`metadataCacheSizeMb`** is kopia's metadata cache budget in MiB, and maps to `--metadata-cache-size-mb`.
- **`contentCacheSizeMb`** is kopia's content cache budget in MiB, and maps to `--content-cache-size-mb`.
- **`mode`** is `Ephemeral` or `Persistent`. `Ephemeral` is the default and gives a fresh cache that lives only for the run. `Persistent` keeps a warm cache across runs in a controller-owned `ReadWriteOnce` PVC, which assumes that runs for a given owner do not overlap.

## ScratchDefaults

Defaults for the deep-verification **scratch** volume, the throwaway restore target a `deep` restore test writes into and then discards. A recipe's `verification.deep` inherits these unless it overrides them.

- **`storageClassName`** is the StorageClass for the throwaway scratch PVC. It only applies when `capacity` is set.
- **`capacity`** is the size of that PVC, `100Gi` for example. Size it to hold the restored snapshot comfortably. Leave it out and scratch falls back to a node-local `emptyDir`.

Scratch is always thrown away with the verify Job, unlike the cache, so it has no `mode` and no persistent-PVC form.

## CredentialProjection

Opt-in copying of a repository's credential `Secret`s into the namespace where each mover Job runs. It is off by default.

With `enabled: true`, the operator reads the source Secrets before each run and writes a Kopiur-managed copy into the Job's namespace. The copy is owned by the consuming object, so it is garbage-collected with it, and it is refreshed from the source on every run.

Even when enabled, projection does nothing where the source Secret already lives in the Job's namespace, which is the common namespaced-`Repository` layout. It only copies for the cross-namespace case, so a shared `ClusterRepository` whose Secret is pinned to one namespace. Keeping it off by default keeps the namespace as a trust boundary.

## CreateBehavior

What happens when the repository does not exist yet. This is a repository's `create` block.

- **`enabled`** creates the repository if it does not exist yet. It is off by default, so a typo in the backend cannot silently spin up a brand-new empty repository.
- **`encryption`** is the kopia encryption algorithm for a freshly created repository, `AES256-GCM-HMAC-SHA256` for example.
- **`splitter`** is the kopia object splitter for a freshly created repository.
- **`hash`** is the kopia content hash algorithm for a freshly created repository.
- **`ecc`** is Reed-Solomon error-correcting parity that guards repository blobs against backend bit-rot. It takes `algorithm`, `REED-SOLOMON-CRC32` for example, and `overheadPercent`.

All of these are read only at creation time, and all are immutable once the repository exists.

## CatalogBounds

Limits on how many `Snapshot` objects with `origin: discovered` Kopiur creates. This is a repository's `catalog` block.

Letting a discovered row expire never deletes the kopia snapshot behind it. Discovered snapshots are always `deletionPolicy: Retain`, and stay restorable through `Restore.source.identity`.

- **`retain`** is how many discovered `Snapshot` objects stay materialized, which is what bounds the etcd footprint on a large repository. `perIdentity` keeps the most recent N per `username@hostname:path` identity; snapshots this cluster produced do not count toward it, and `0` turns discovered-snapshot materialization off entirely. `maxAgeDays` expires discovered objects older than N days, with a minimum of 1.
- **`periodicRefresh`** opts in to repeated re-scans. It is a bool and defaults to `false`. An initial scan always runs, at the first bootstrap and on any spec change. Turn this on to keep re-scanning so out-of-band snapshots keep appearing. It is off by default because each re-scan re-runs the bootstrap Job for object-store and volume-backed repositories.
- **`refreshInterval`** is how often to re-scan **when `periodicRefresh` is on**. Write it as a Go-style duration such as `30s`, `5m` or `1h`. The minimum is `30s` and the default is `1h`. It does nothing while `periodicRefresh` is off.
- **`fallbackNamespace`** is where to materialize discovered `Snapshot`s whose identity hostname does not map to an allowed namespace. It is for a `ClusterRepository` only, and is rejected on a namespaced `Repository`, which always materializes into its own namespace.

## ServerSpec

Optional kopia web UI configuration on a `Repository` or `ClusterRepository`.

Adding `spec.server` is what enables the server; there is no `enabled` bool. The operator runs `kopia server start` in a Deployment and publishes it through a Service. Networking, so Ingress or HTTPRoute, is left to you. See [server](../../server.md).

- **`auth`** is the UI authentication mode, described below. Leaving it out defaults to `generate`, **never** to no auth.
- **`readOnly`** connects the server's repository read-only, so the UI cannot create, delete or alter backups. It leaves browse and restore-download working. A repository with `spec.mode: ReadOnly` forces this on, and setting an explicit `readOnly: false` on a `ReadOnly` repository is rejected. Read-only blocks mutation, not reading: the server still holds the decryption key, so anyone who reaches the UI can read and restore every backup.
- **`service`** is how the server is published as a `Service`. It takes `type` (`ClusterIP` by default, or `NodePort` or `LoadBalancer`), `port` (default `51515`), and `annotations`, which is where you wire up your own Ingress or LoadBalancer.
- **`resources`** sets resource requests and limits for the server pod.
- **`securityContext`** overrides the hardened default container security context.
- **`podSecurityContext`** is the pod-level security context. It notably carries `supplementalGroups`, which is how you grant the long-lived server write access to a group-owned filesystem export. `fsGroup` quietly does nothing on NFS, because the kubelet does not recursively chown in-tree NFS mounts.

On a `ClusterRepository` the server config additionally **requires** a `namespace`, because a cluster-scoped server has no implicit one.

By default the server holds a **read-write** repository connection. Kopia's UI can read, write and delete, and the server process holds the repository decryption key, so exposing the UI exposes full mutation of every backup. That is why auth defaults to `generate` and why no-auth requires `acknowledgeInsecure`.

### Server auth

The UI authentication mode. It is a single-key object, so exactly one of:

- **`generate`** has the operator mint random UI credentials into a Secret it owns, and pin the reference in `status.server.generatedSecretRef`. This is the safe default. `username` is optional and defaults to `kopia`.
- **`secretRef`** takes UI credentials you supply in a Secret: `name`, `usernameKey` and `passwordKey`, all required, so the keys are always present.
- **`insecure`** turns UI authentication off. It requires an explicit `acknowledgeInsecure: true`, and the webhook rejects it otherwise, because it exposes read, write and delete of the repository with no login.

## Retention

Grandfather-father-son (GFS) retention: how many snapshots to keep per time bucket. It is the only thing that prunes successful backups.

Every field is an optional count. `keepLatest` keeps the N most recent regardless of age. `keepHourly`, `keepDaily`, `keepWeekly`, `keepMonthly` and `keepAnnual` each keep one snapshot per bucket, for the most recent N buckets of that kind.

## Identity

What kopia records as `username@hostname:path` for a snapshot. Kopiur resolves it at admission, pins it to status, and never re-renders it.

- **`username`** overrides the username part. Leave it out to use the resolved default, which is the repository's `identityDefaults` CEL expression, or the object name.
- **`hostname`** overrides the hostname part. Leave it out to use the resolved default, which is the `identityDefaults` expression, or the namespace.

Status pins the fully resolved `username`, `hostname` and, where it applies, the `sourcePath`.

## CronSpec

One cron entry with optional deterministic jitter, shared by `Maintenance`'s quick and full schedules.

- **`cron`** is the cron expression, parsed by `croner`. It may contain an `H` placeholder, which gives each schedule its own deterministic slot.
- **`jitter`** is an optional jitter window, written as a Go-style duration such as `30m`. It is derived from the schedule's UID and slot, so it stays the same across restarts. Leave it out and the schedule inherits the repository's [`scheduleDefaults.jitter`](#scheduledefaults); missing at both levels means no spread. It is capped at 24 hours at admission.
- **`timezone`** is an optional IANA zone the cron is evaluated in. Leave it out and it uses the enclosing schedule's timezone, then the repository's [`scheduleDefaults.timezone`](#scheduledefaults), then UTC.

## ScheduleDefaults

Repository-level scheduling defaults, set on `Repository` or `ClusterRepository` `spec.scheduleDefaults`. Every cron consumer that does not set its own equivalent field inherits them at reconcile time: `SnapshotSchedule`, `SnapshotPolicy` verification for both tiers, `Maintenance` for both tiers, and both replication kinds.

It is a sub-object rather than two separate leaf fields, so a future default can be added without breaking the API. That is exactly how `jitter` joined `timezone`.

- **`timezone`** is an IANA zone name such as `America/New_York`, validated at admission against the same `chrono-tz` database the scheduler uses. The fallback beneath it is UTC.
- **`jitter`** is a Go-style duration such as `10m`. The spread is deterministic per schedule UID and slot rather than random, so a restart or an HA failover never re-rolls it. There is **no** fallback beneath it: missing at both levels means no spread. It is capped at 24 hours at admission, because jitter is a spread inside a cron period and not a schedule offset.

Precedence works the same at both levels: the cron's own value wins, then this default, then the built-in fallback if there is one.

A `SnapshotSchedule` resolves the repository of the policy it **targets**, so a `policySelector` schedule can see several candidate defaults. If they disagree, timezone resolves to UTC and jitter resolves to no spread, and each case emits a diagnostic recommending an explicit per-schedule value. See [Repositories → `scheduleDefaults`](../../repositories.md#scheduledefaults--set-the-cron-timezone-and-jitter-once).

## ConcurrencySpec

Limits on the mover Jobs a repository runs, set on `Repository` or `ClusterRepository` `spec.concurrency`.

- **`maxConcurrentJobs`** is the ceiling on this repository's in-flight mover Jobs. Absent or `0` means **unlimited**, which is the default. The schema deliberately emits no `default:` for the field, because absent and `0` are the same state; a materialized default would stamp `{maxConcurrentJobs: 0}` onto every stored repository for no change in behavior.

There is one pool per repository rather than one per kind of work. **Backups**, **restores**, and the **source** side of `RepositoryReplication` and `SnapshotReplication` all draw from it, because the repository's backend and the bandwidth to it are the shared resource.

Outside the pool entirely: maintenance, which is already single-flight per repository and is the cure for an overloaded one; verification; pin; batched snapshot deletions; repository bootstrap and catalog scans; and `kubectl kopiur browse` session pods.

Restores are **always admitted**, because a recovery in progress must not queue behind routine backups. A running restore still occupies a slot, so it displaces backups rather than adding to them. A `Job` that a queueing system has suspended with `spec.suspend: true` occupies no slot, so Kueue and Kopiur cannot deadlock each other.

There is also a cluster-wide backstop, `KOPIUR_MAX_CONCURRENT_JOBS`, exposed as the Helm value [`maxConcurrentJobs`](../../install.md#runtime-tuning) and defaulting to `0`, meaning uncapped. It bounds the same pool across every repository, and a run must satisfy both caps. With no cap set anywhere the gate costs one branch and no API call. See [Backups → limiting concurrent jobs per repository](../../backups.md#limiting-concurrent-jobs-per-repository).

## RepositoryRef

A reference from a consumer object (`SnapshotPolicy`, `Snapshot`, `Restore` or `Maintenance`) to a `Repository` or `ClusterRepository`. See [repositories](../../repositories.md).

- **`kind`** is `Repository`, the default and namespaced, or `ClusterRepository`, which is cluster-scoped.
- **`name`** is the name of the referenced repository.
- **`namespace`** makes it a cross-namespace `Repository` reference. With `kind: ClusterRepository`, `namespace` MUST be absent, and the webhook enforces that, because a cluster-scoped repository has no namespace.

## SecretKeyRef and SecretRef

`SecretKeyRef` references a single key inside a `Secret`, with `name`, optional `namespace` and optional `key`. A repository password, given through `Encryption`, is always a `SecretKeyRef` and never an inline value.

`SecretRef` references a whole `Secret`, with `name` and optional `namespace`. The operator reads well-known keys from it.

For both, leaving `namespace` out means the same namespace as the object that refers to it.

## FailurePolicy

Per-run failure controls passed through to the mover `Job`, set on a recipe's `failurePolicy`.

- **`backoffLimit`** is `Job.spec.backoffLimit`: how many retries happen before a run is marked failed.
- **`activeDeadlineSeconds`** is `Job.spec.activeDeadlineSeconds`: a wall-clock cap after which a still-running run is killed. It is meant for long-running work.
- **`podStartupDeadlineSeconds`** is how long a mover **pod** may sit in a non-starting state before the run fails with an actionable reason. Non-starting means a container `CreateContainerConfigError`, `ImagePullBackOff` or `InvalidImageName`, or a pod that is `Unschedulable`. A wedged pod never reaches a terminal phase, so `backoffLimit` never trips, and this is the only thing that bounds it. The default is 300 seconds.
