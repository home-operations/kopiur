# Shared sub-objects

Types reused across multiple CRDs. For the terse type/default table see the
[field reference](../../field-reference.md).

## Backend

The storage backend for a kopia repository. Exactly one backend block is set —
the wire shape is externally tagged, so you write `backend: { s3: {...} }`,
`backend: { filesystem: {...} }`, and so on, and `kubectl` enforces "exactly one
backend" at admission. See the [backend guides](../../backends/index.md) for
provider-specific setup.

Variants:

- **`s3`** — Amazon S3 or any S3-compatible store (MinIO, RustFS, Ceph RGW, …).
  `bucket` is required; `prefix` lets several repositories share one bucket
  (e.g. `clusters/prod/`); `endpoint` is omitted for AWS and set for
  S3-compatible stores; `region` is required by AWS and some providers; `tls`
  carries TLS overrides; `auth` is a [BackendAuth](#backendauth).
- **`azure`** — Azure Blob Storage. `container` is required; `storageAccount` is
  set when it can't be inferred from credentials.
- **`gcs`** — Google Cloud Storage. `bucket` is required.
- **`b2`** — Backblaze B2. `bucket` is required; auth is Secret-only (B2 has no
  cloud IAM federation).
- **`filesystem`** — kopia writes the repository to a `path` inside the mover
  pod, populated by a [RepoVolume](#repovolume). kopia has no NFS backend; NFS is
  reached through the filesystem backend by mounting the export at `path`.
- **`sftp`** — SFTP server (`host`, `path`, optional `port` defaulting to 22,
  `username`); Secret-only auth (SSH key / known-hosts).
- **`webdav`** — WebDAV endpoint (`url`); Secret-only HTTP basic-auth.
- **`rclone`** — any rclone remote in `remote:path` form, with a Secret holding
  the `rclone.conf`. kopia shells out to `rclone`, broadening reach to providers
  without a native kopia backend.

### RepoVolume

What backs a filesystem repository's mount path. Externally tagged, exactly one
of:

- **`pvc`** — `volume: { pvc: { name: "repo-pvc" } }`: a `PersistentVolumeClaim`
  mounted read-write at the repo path (in the mover's namespace).
- **`nfs`** — `volume: { nfs: { server: "nas.lan", path: "/export/kopia" } }`: an
  inline NFS export mounted directly, no PVC and no StorageClass.

`volume` may be absent when the path is already present on the node/image (a
baked-in mount, mainly the e2e harness).

## BackendAuth

Credentials for a cloud object-store backend that has an IAM plane (S3, Azure,
GCS). Set **exactly one of**:

- **`secretRef`** — a [SecretRef](#secretkeyref-and-secretref) to the Secret holding static access
  credentials. The operator reads well-known keys (e.g.
  `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` for S3).
- **`workloadIdentity`** — a cloud-federated `ServiceAccount` instead of static
  keys.

An absent `auth` is also legal — the well-known keys may ride the
encryption-password Secret.

Backends without a cloud IAM plane (B2, SFTP, WebDAV, rclone) use a Secret-only
auth surface instead, so a workload identity on them is structurally
unrepresentable.

### workloadIdentity

A cloud workload-identity binding (AWS IRSA / EKS Pod Identity, AKS Workload
Identity, GKE Workload Identity): the mover Job runs as a user-supplied
Kubernetes `ServiceAccount` federated to the cloud IAM role that grants backend
access, instead of reading static keys from a Secret.

- **`serviceAccountName`** — the `ServiceAccount` the mover pod runs as, resolved
  in the mover Job's own namespace.

The `ServiceAccount` must already exist in each namespace mover Jobs run in (the
operator never creates it) and carry the cloud-specific annotation
(`eks.amazonaws.com/role-arn`, `azure.workload.identity/client-id`,
`iam.gke.io/gcp-service-account`).

## MoverDefaults

Repository-wide mover defaults inherited by every mover the repository spawns —
bootstrap, backup, restore, maintenance, verification, replication. Each field
is overridable per-recipe via the recipe's [`mover`](#moverspec) block. This is
the single place a repository defines mover identity, hardening, resources, and
cache. See [movers](../../movers.md).

- **`securityContext`** — container security-context base for every mover.
- **`podSecurityContext`** — pod security-context base (notably `fsGroup`).
- **`resources`** — resource requests/limits base for the mover container.
- **`cache`** — kopia [CacheDefaults](#cachedefaults) for every mover.
- **`scratch`** — [ScratchDefaults](#scratchdefaults) for the deep-verification
  scratch volume.
- **`nodeSelector`**, **`tolerations`**, **`affinity`** — pod scheduling for
  every mover.
- **`sourceColocation`** — RWO source-PVC node co-location (see below).
- **`ttlSecondsAfterFinished`** — `Job.spec.ttlSecondsAfterFinished` for every
  mover Job so finished Jobs self-GC. Defaults to 1h when neither the repo nor
  the recipe sets one.
- **`throttle`** — repository throttle limits (`kopia repository throttle set`)
  applied by every mover after it connects to this repository, so a run doesn't
  saturate the link or hammer the object store: `uploadBytesPerSecond`,
  `downloadBytesPerSecond`, `readOpsPerSecond`, `writeOpsPerSecond` (each unset
  leaves kopia's current limit). Honored by the transfer movers — bootstrap,
  backup, restore, maintenance, verification, repository replication and snapshot
  replication. kopia's limits are **per connection**, so a run that opens two
  repositories caps each from *that* repository's own `moverDefaults`: a
  [`SnapshotReplication`](snapshot-replication.md) caps its source connection
  from the source repo's defaults and its destination connection from the
  destination repo's, each overridable per CR via
  [`spec.migrate.throttle`](snapshot-replication.md#migrate). A migrate-mode
  [`seed`](repository.md#seed) is the same shape: the replica's connection is
  capped from the *replica's* defaults and this repository's from its own, each
  overridable while the seed is armed via `spec.seed.migrate.throttle.source` /
  `.destination`.

  Three things it does **not** cap today: the interactive
  [`serve`/browse](../../server.md) session (a read-only UI session, not a batch
  transfer); a `RepositoryReplication`'s **destination** side, because `kopia
  repository sync-to` copies blobs without opening the destination as a
  repository — its caps live on [`spec.sync`](repository-replication.md) instead;
  and **batched snapshot deletions and pins**, whose work specs leave the throttle
  block empty, so a repository-wide cap does not currently reach them (deletion
  load is instead bounded by per-repository batching and the cluster-wide
  `maxConcurrentDeleteJobs` backstop).

/// warning | A byte cap bites much harder than its number suggests

A `*BytesPerSecond` cap is enforced against **cold** backend traffic only —
content already in the mover's kopia cache is read without touching the limiter,
so a warm re-run can look completely unthrottled. And on small-object workloads
the effective throughput lands far below the nominal rate: a measured 2 MB/s cap
took ~14 s to move a 28 KiB cold repository, while caps of 10 MB/s and up often
did not bind at all at that size. Treat these as a **ceiling for large transfers**
— set them generously and verify against your own data, rather than tuning them
down to a number that looks safe.

///

The `securityContext`/`podSecurityContext`/`resources`/`cache` fields resolve by
field-wise merge: a repo-wide default composes with a partial per-recipe
override, layered over a hardened base. See
[security context](../../security-context.md) for the hardened defaults and how
overrides only tighten them.

### sourceColocation

Controls how a mover co-locates with the node its `ReadWriteOnce` (RWO)
source/destination PVC is attached to, to avoid a Kubernetes Multi-Attach error
(an RWO PVC can only attach to one node at a time). The `mode` is one of:

- **`Auto`** (default) — pin to the attached node when discoverable, otherwise
  schedule freely (`ReadWriteMany`/`ReadOnlyMany` are never pinned). Fixes the
  Multi-Attach error with no configuration.
- **`Required`** — like `Auto`, but **fail** the run with an actionable error
  when an RWO PVC's node cannot be determined.
- **`Disabled`** — never compute a node pin; the mover uses only the explicit
  `nodeSelector`/`affinity`/`tolerations`.

## MoverSpec

Per-recipe mover overrides on a recipe's `mover` field. Each field overlays the
repository's [MoverDefaults](#moverdefaults) field-wise (recipe wins, the repo
default fills, the hardened base underneath):

- **`resources`** — resource requests/limits for the mover container.
- **`cache`** — override the repository's [CacheDefaults](#cachedefaults).
- **`securityContext`** — container security context; set only the fields you
  want to change.
- **`podSecurityContext`** — pod security context, notably `fsGroup`, which makes
  a freshly-provisioned volume group-writable so an unprivileged mover can
  populate it on restore without root.
- **`privilegedMode`** — opt-in, namespace-gated; preserves UID/GID on restore.
- **`inheritSecurityContextFrom`** — see below.
- **`ttlSecondsAfterFinished`** — per-recipe override of the Job TTL.

A pod-level `runAsUser: 0` / `runAsNonRoot: false` is still gated as privileged,
exactly like the container-level setting.

### inheritSecurityContextFrom

Copies the UID/GID security context from a live workload instead of setting an
explicit `securityContext`/`podSecurityContext`. Combines with both: they are the
higher merge layer, so an explicit field overrides the inherited one and stands in
alone when no pod resolves. Requires the workload to pin `runAsUser` — a UID from
the image's `USER` line is invisible in the pod spec. Externally tagged, exactly one
of:

- **`workloadSelector`** — match the workload pod(s) by a label selector
  (`podSelector` + optional `container`) and inherit the chosen container's
  `securityContext` plus the pod's `spec.securityContext`. Works for both backup
  and restore (restore inherits from the pod that will read the data).
- **`pvcConsumer`** — **backup sources only.** Auto-derive the workload pod from
  the PVC this snapshot backs up: the operator finds the pod(s) mounting the
  source claim and inherits their security context — no hand-written selector.
  Meaningless on
  a restore (the consuming pod may not exist yet), so restore must use
  `workloadSelector`. `pvcConsumer` accepts an optional `container`.

The resolved context still enters as the recipe layer, so the hardened base
(`drop:[ALL]`, seccomp, `fsGroup`) still applies.

## CacheDefaults

How a mover's kopia cache volume is provisioned and sized.

- **`capacity`** — size of the PVC backing the cache (e.g. `10Gi`).
- **`storageClassName`** — StorageClass for the cache PVC; absent uses the
  cluster default.
- **`metadataCacheSizeMb`** — kopia metadata cache budget in MiB
  (`--metadata-cache-size-mb`).
- **`contentCacheSizeMb`** — kopia content cache budget in MiB
  (`--content-cache-size-mb`).
- **`mode`** — `Ephemeral` (default; the cache lives only for the run, fresh each
  time) or `Persistent` (a warm cache reused across runs in a controller-owned
  `ReadWriteOnce` PVC, which assumes non-overlapping runs for a given owner).

## ScratchDefaults

Defaults for the deep-verification **scratch** volume — the throwaway restore
target a `deep` restore-test writes into and then discards. Inherited by a
recipe's `verification.deep` unless overridden there.

- **`storageClassName`** — StorageClass for the ephemeral scratch PVC; only
  applies when `capacity` is set.
- **`capacity`** — size of the ephemeral scratch PVC (e.g. `100Gi`); size it to
  comfortably hold the restored snapshot. When absent, scratch falls back to a
  node-ephemeral `emptyDir`.

Unlike the cache, scratch is always ephemeral (auto-deleted with the verify
Job), so it has no `mode`/persistent-PVC form.

## CredentialProjection

Opt-in projection of a repository's credential `Secret`(s) into the namespace
where each mover Job runs (default off). When `enabled: true`, before each run
the operator reads the source Secret(s) and writes a kopiur-managed copy into the
Job's namespace, owned by the consuming CR (garbage-collected with it) and
refreshed from source every run.

Even when enabled, projection is a no-op where the source Secret already lives in
the Job's namespace (the common namespaced-`Repository` layout) — there is
nothing to copy. It only actually copies for the cross-namespace case (a shared
`ClusterRepository` whose Secret is pinned to one namespace). Keeping it off by
default preserves the namespace-as-trust-boundary posture.

## CreateBehavior

Behavior when the repository does not yet exist (a repository's `create` block).

- **`enabled`** — create the repository if it does not exist yet (off by default,
  so a typo'd backend can't silently spin up a brand-new empty repository).
- **`encryption`** — kopia encryption algorithm for a freshly-created repository
  (e.g. `AES256-GCM-HMAC-SHA256`).
- **`splitter`** — kopia object splitter for a freshly-created repository.
- **`hash`** — kopia content hash algorithm for a freshly-created repository.
- **`ecc`** — Reed-Solomon error-correcting-code parity guarding repo blobs
  against backend bit-rot: `algorithm` (e.g. `REED-SOLOMON-CRC32`) and
  `overheadPercent`.

All of these are consulted only at creation time and are immutable after the
repository is created.

## CatalogBounds

Bounds on materialization of `origin: discovered` `Snapshot` CRs (a repository's
`catalog` block). Expiring a CR row never deletes the kopia snapshot behind it —
discovered snapshots are always `deletionPolicy: Retain`, and stay restorable via
`Restore.source.identity`.

- **`retain`** — how many discovered `Snapshot` CRs to keep materialized (bounds
  etcd footprint for large repositories): `perIdentity` keeps the most-recent N
  per `username@hostname:path` identity (snapshots this cluster produced don't
  count; `0` disables discovered-Snapshot materialization entirely), and
  `maxAgeDays` expires discovered CRs older than N days (minimum 1).
- **`periodicRefresh`** — opt in to repeated re-scans (bool, default `false`). An
  initial scan always runs (first bootstrap + on any spec change); set this to keep
  re-scanning so out-of-band snapshots keep appearing. Off by default because each
  re-scan re-runs the bootstrap Job for object-store / volume-backed repos.
- **`refreshInterval`** — how often to re-scan **when `periodicRefresh` is on**
  (Go-style duration like `30s`/`5m`/`1h`; minimum `30s`, default `1h`; inert when off).
- **`fallbackNamespace`** — where to materialize discovered `Snapshot`s whose
  identity hostname doesn't map to an allowed namespace (ClusterRepository only;
  rejected on a namespaced `Repository`, which always materializes into its own
  namespace).

## ServerSpec

Optional kopia web-UI server configuration on a `Repository` /
`ClusterRepository`. Presence of `spec.server` means **enabled** — there is no
`enabled` bool. The operator runs `kopia server start` in a Deployment and
exposes it via a Service; networking (Ingress/HTTPRoute) is left to the user. See
[server](../../server.md).

- **`auth`** — UI authentication mode (see below). Omitted defaults to
  `generate` — **never** no-auth.
- **`readOnly`** — connect the server's repository read-only so the UI cannot
  create, delete, or alter backups (browse + restore-download only). A repository
  with `spec.mode: ReadOnly` forces this on; setting an explicit `readOnly: false`
  on a `ReadOnly` repository is rejected. Read-only blocks mutation, not reading —
  the server still holds the decryption key, so anyone reaching the UI can read
  and restore every backup.
- **`service`** — how the server is exposed as a `Service`: `type`
  (`ClusterIP` default, `NodePort`, `LoadBalancer`), `port` (default `51515`),
  and `annotations` (the seam users wire their own Ingress/LoadBalancer onto).
- **`resources`** — resource requests/limits for the server pod.
- **`securityContext`** — override the hardened default container security
  context.
- **`podSecurityContext`** — pod-level security context. Notably carries
  `supplementalGroups`, the way to grant the long-lived server write access to a
  group-owned filesystem export — `fsGroup` is silently a no-op on NFS (the
  kubelet does not recursively chown in-tree NFS mounts).

On a `ClusterRepository` the server config additionally **requires** a
`namespace` (a cluster-scoped server has no implicit namespace).

By default the server holds a **read-write** repository connection. kopia's UI is
full read-write-delete, and the server process holds the repository decryption
key, so exposing it exposes full mutation of all backups — hence the
`generate`-by-default and the mandatory `acknowledgeInsecure` on no-auth.

### Server auth

UI authentication mode, externally tagged, exactly one of:

- **`generate`** — the operator mints random UI credentials into an owned
  `Secret` and pins the reference to `status.server.generatedSecretRef`. The safe
  default. Optional `username` (default `kopia`).
- **`secretRef`** — the user supplies UI credentials via a `Secret`: `name`,
  `usernameKey`, `passwordKey` (all required, so "keys are present" is a
  structural guarantee).
- **`insecure`** — no UI authentication. Requires an explicit
  `acknowledgeInsecure: true` (webhook-rejected otherwise) because it exposes full
  read/write/delete of the repository with no login.

## Retention

GFS (grandfather-father-son) retention — how many snapshots to keep per time
bucket. The single successful-retention driver. All fields are optional counts:
`keepLatest` (the N most-recent regardless of age), `keepHourly`, `keepDaily`,
`keepWeekly`, `keepMonthly`, `keepAnnual` (one snapshot per bucket for the
most-recent N of that bucket).

## Identity

What kopia records as `username@hostname:path` for a snapshot. Resolved at
admission and pinned to status, never re-rendered.

- **`username`** — override the `username` portion; absent uses the resolved
  default (the repository's `identityDefaults` CEL expression, or the object
  name).
- **`hostname`** — override the `hostname` portion; absent uses the resolved
  default (the `identityDefaults` expression, or the namespace).

The fully-resolved values are pinned in status as `username`, `hostname`, and
(where applicable) the `sourcePath`.

## CronSpec

A single cron entry with optional deterministic jitter, shared by `Maintenance`'s
quick/full schedules.

- **`cron`** — the cron expression, parsed by `croner`. May contain an `H`
  placeholder for deterministic per-schedule jitter.
- **`jitter`** — optional deterministic jitter window as a Go-style duration
  string (e.g. `30m`), derived from `(scheduleUID, slot)` so it is stable across
  restarts.

## RepositoryRef

A reference from a consumer CR (`SnapshotPolicy`, `Snapshot`, `Restore`,
`Maintenance`) to a `Repository` or `ClusterRepository`. See
[repositories](../../repositories.md).

- **`kind`** — `Repository` (default, namespaced) or `ClusterRepository`
  (cluster-scoped).
- **`name`** — name of the referenced repository.
- **`namespace`** — cross-namespace `Repository` reference. When
  `kind: ClusterRepository`, `namespace` MUST be absent (webhook-enforced — a
  cluster-scoped repository has no namespace).

## SecretKeyRef and SecretRef

`SecretKeyRef` references a single key within a `Secret` (`name`, optional
`namespace`, optional `key`); a repository password (via `Encryption`) is always
a `SecretKeyRef`, never inline. `SecretRef` references an entire `Secret` (the
operator reads well-known keys from it; `name` + optional `namespace`). For both,
an absent `namespace` means the same namespace as the referrer.

## FailurePolicy

Per-run failure controls passed through to the mover `Job` (on a recipe's
`failurePolicy`):

- **`backoffLimit`** — `Job.spec.backoffLimit`, retries before a failed run is
  marked failed.
- **`activeDeadlineSeconds`** — `Job.spec.activeDeadlineSeconds`, a wall-clock cap
  after which a still-running run is killed (meant for long-running work).
- **`podStartupDeadlineSeconds`** — how long a mover **pod** may sit in a
  non-starting state (a container `CreateContainerConfigError` /
  `ImagePullBackOff` / `InvalidImageName`, or `Unschedulable`) before the run is
  failed with an actionable reason. A wedged pod never reaches a terminal phase,
  so `backoffLimit` never trips — this is the only thing that bounds it.
  Default 300s.
