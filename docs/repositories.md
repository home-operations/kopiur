# Repositories & backends

A **`Repository`** is _where_ your snapshots live. It is the one resource that holds the storage backend, the encryption password, and the credentials: everything `SnapshotPolicy`, `Snapshot` and `Restore` need but shouldn't have to repeat. Get this right and the rest of Kopiur just points at it by name.

There are two flavors:

| CRD                     | Scope      | Use it when…                                                                                                                            |
| ----------------------- | ---------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| **`Repository`**        | Namespaced | One namespace owns the backups (the common case). The repo and its credential Secret live together in that namespace.                   |
| **`ClusterRepository`** | Cluster    | A platform team owns one shared repository that **many** tenant namespaces back up to, without each tenant knowing the backend details. |

Both have the **same** backend, encryption and create surface. `ClusterRepository` adds a tenancy gate (`allowedNamespaces`) and per-tenant identity CEL expressions. See [ClusterRepository](#clusterrepository-a-shared-repository) below.

/// tip | One shared repository is the recommended default

Point as many backups as you can at **one** repository. Kopia deduplicates by content hash across every writer, so pooling workloads into a single repository stores their common content once. Each `SnapshotPolicy` writes under its own identity, so their snapshots never collide. See [Recommended: one shared repository](concepts/how-kopia-works.md#recommended-one-shared-repository) for the mechanism and the trade-offs.

///

/// tip | Anatomy of a Repository

```yaml
spec:
    backend: { <one of eight>: { ... } } # WHERE: storage. Exactly one backend.
    encryption: { passwordSecretRef: ... } # the kopia repo password (Secret ref)
    create: { enabled: true, ecc: { ... } } # initialize the repo if absent (default: on)
    bootstrap: { failurePolicy: { ... } } # tune the connect/create Job deadline + retries
    moverDefaults: { ... } # base config for EVERY mover (SC, resources, cache, ...)
    catalog: { ... } # bounds "discovered" snapshot materialization
    maintenance: { ... } # default-managed; see the Maintenance guide
    onNamespaceDelete: Orphan # Orphan (default) | Delete — kubectl delete ns behavior
    mode: ReadWrite # ReadWrite (default) | ReadOnly
    suspend: false # pause connect/bootstrap + maintenance
```

Only `backend` and `encryption` are required. The rest have sane defaults.

///

## The two things every backend needs: identifiers + a Secret

Kopiur deliberately splits a backend into two parts. **Non-secret connection identifiers**, such as bucket, endpoint, host and path, go in the `Repository` spec. **Secrets**, meaning access keys and the encryption password, live in a Kubernetes `Secret` and are passed to kopia as environment variables, never on the command line and never in status. So every object-store backend looks like this:

```yaml
spec:
    backend:
        s3:
            bucket: my-backups # ← identifiers in the spec
            auth:
                secretRef:
                    name: repo-creds # ← a Secret holding the access keys
    encryption:
        passwordSecretRef:
            name: repo-creds # ← (can be the same Secret) holding KOPIA_PASSWORD
            key: KOPIA_PASSWORD
```

/// warning | Externally tagged: there is no `kind:` field

A backend is selected by **which key you set** (`backend.s3`, `backend.azure`, and so on), not by a `kind:` discriminator. `backend: { kind: S3 }` will **not** admit. This is the type-safety design: exactly one backend can be expressed. See the [API conventions](dev/api-conventions.md).

///

### Credential Secret keys by backend

The mover reads these **well-known keys** from the Secret you reference and feeds them to kopia. Put your credentials under these exact key names. `KOPIA_PASSWORD`, the repository encryption password, is required for **every** backend. See [Backend configuration](backends/index.md#credential-keys-at-a-glance) for the full per-backend setup and the env-vs-file credential detail.

| Backend        | Secret keys the mover reads                                               | Notes                                                                                                       |
| -------------- | ------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| **S3**         | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, _(opt)_ `AWS_SESSION_TOKEN` | Works for AWS and any S3-compatible store (MinIO, RustFS, Ceph RGW).                                        |
| **Azure**      | `AZURE_STORAGE_KEY` **or** `AZURE_STORAGE_SAS_TOKEN`                      | Account name can come from `spec.backend.azure.storageAccount`.                                             |
| **GCS**        | `KOPIA_GCS_CREDENTIALS` (service-account JSON)                            | The mover writes the JSON to a file and passes `--credentials-file`.                                        |
| **B2**         | `B2_KEY_ID`, `B2_KEY`                                                     | Backblaze application key ID + key.                                                                         |
| **SFTP**       | `KOPIA_SFTP_KEY_DATA`, `KOPIA_SFTP_KNOWN_HOSTS`                           | Key-based auth; the mover writes both to files (`--keyfile`/`--known-hosts`). See [SFTP](backends/sftp.md). |
| **WebDAV**     | `KOPIA_WEBDAV_USERNAME`, `KOPIA_WEBDAV_PASSWORD`                          | HTTP basic auth, via `auth.secretRef`.                                                                      |
| **rclone**     | `KOPIA_RCLONE_CONFIG` (the `rclone.conf`)                                 | Referenced by `backend.rclone.configSecretRef`, not `auth`.                                                 |
| **gdrive**     | `KOPIA_GDRIVE_CREDENTIALS` (service-account JSON)                         | Native Google Drive; referenced by `backend.gdrive.credentialsSecretRef`. Experimental; see [Google Drive](backends/gdrive.md). |
| **filesystem** | _(none: local path)_                                                     | Only `KOPIA_PASSWORD` is needed.                                                                            |

/// note | ClusterRepository Secret references need a namespace

A `ClusterRepository` is cluster-scoped and has no namespace of its own, so **every** Secret reference in it, meaning `auth.secretRef` and `encryption.passwordSecretRef`, **must** carry an explicit `namespace:`. The webhook enforces that. The credential Secret also has to exist in each **workload** namespace a mover runs in. Either place it there yourself, or, recommended for a shared repo, turn on [**credential projection**](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) and let Kopiur copy it for you.

///

## The nine backends

Kopiur supports nine backends. Each one is selected by the `spec.backend.<key>` you set.

| Backend                                                               | `spec.backend` key | Where it goes             |
| --------------------------------------------------------------------- | ------------------ | ------------------------- |
| Amazon S3 + any S3-compatible store (MinIO, RustFS, Ceph RGW, Wasabi) | `s3`               | a bucket                  |
| Azure Blob Storage                                                    | `azure`            | a container               |
| Google Cloud Storage                                                  | `gcs`              | a bucket                  |
| Backblaze B2                                                          | `b2`               | a bucket                  |
| Filesystem (NAS/PVC or inline NFS)                                    | `filesystem`       | a mounted PVC or NFS path |
| SFTP                                                                  | `sftp`             | a path on an SSH server   |
| WebDAV                                                                | `webDav`           | a collection URL          |
| rclone (everything else)                                              | `rclone`           | any rclone remote         |
| Google Drive (native, experimental)                                   | `gdrive`           | a Drive folder            |

/// tip | Per-backend setup lives on its own page

For each backend, see [**Backend configuration**](backends/index.md). It has the **provider prerequisites**, the exact **Secret keys**, the settings you'll actually change, and a **complete apply-ready manifest** with the Secret and Repository in one file. That page is the hands-on cookbook; this one is the concepts.

///

## Encryption and repository creation

```yaml
spec:
    encryption:
        passwordSecretRef:
            name: repo-creds
            key: KOPIA_PASSWORD # which key inside the Secret holds the password
    create:
        enabled: true # create the repo if it doesn't exist yet (default: true)
        # All of the below are consulted ONLY at creation time, then fixed forever
        # (webhook- AND apiserver-immutable: editing them is rejected):
        encryption: AES256-GCM-HMAC-SHA256
        splitter: DYNAMIC-4M-BUZHASH
        hash: BLAKE2B-256-128
        ecc: # Reed-Solomon parity guarding blobs against backend bit-rot
            algorithm: REED-SOLOMON-CRC32
            overheadPercent: 2
```

/// note | Create-time settings are immutable

`create.{splitter,hash,encryption,ecc}` and the pinned identity are fixed when the repository is created. Editing them is **rejected**, by the webhook and by CRD `x-kubernetes-validations` transition rules, with an actionable message. Create a new `Repository` instead of changing these.

The `encryption.passwordSecretRef` is **not** in this set. You may rename or repoint the Secret freely, for example during a GitOps secret rename, as long as it still resolves to the **same password value**. kopia bakes only the resolved value into the repository format, not the reference. Point it at a *different* password and kopia simply fails to open the repository at connect time. That is a recoverable runtime error, not an admission rejection.

///

/// warning | Lose the password, lose the backups

`KOPIA_PASSWORD` encrypts the repository. kopia cannot decrypt without it and there is no recovery. Store it outside the cluster, in a password manager or external secret store, and back up the Secret itself.

///

/// note | `create.enabled` defaults to **on**, and that's safe

Repository create and connect are **idempotent**. Every bootstrap *connects first* and only creates when the backend holds no repository yet; see [Safe by construction](#safe-by-construction). So creating on first use is the least-surprise default: a genuinely absent repository is initialized instead of erroring, and pointing `create` at one that already exists just adopts it. Omitting `create` entirely, or writing `create: {}`, is the same as `create.enabled: true`.

Set **`create.enabled: false`** for a strictly read-only or externally-managed repository the operator must never create. With creation disabled, a typo in `bucket` or `endpoint` shows up as a connect failure (`Bootstrapped=False`, `reason: RepositoryNotInitialized`) instead of spinning up a new empty repository. The cost is that you must opt in for every genuinely new repository. A wrong-password connect fails `AuthFailure` and **never** recreates over existing data; see [Safe by construction](#safe-by-construction) below.

Note that `create.enabled` governs the **first** bootstrap only. A repository that has been `Ready` is never re-created, whatever this field says. So an absent repository has **two** distinct reasons, and they need opposite fixes:

| Reason | Means | Fix |
|---|---|---|
| `RepositoryNotInitialized` | the backend holds no repository and `create.enabled` is `false` | set `create.enabled: true`, or point the backend at an existing repository |
| `RepositoryReinitializeBlocked` | the backend holds no repository but this one was once `Ready` (a pinned `status.uniqueId`), so it was **wiped** | restore the backend, or [deliberately re-initialize](repository-health.md#deliberately-re-initialize-a-wiped-repository) |

///

### Safe by construction

`create.enabled: true` does **not** mean "always create". Every bootstrap **connects first** and only creates as a fallback, gated so that an existing repository is never overwritten:

- **A repository already exists and the password is correct.** kopiur *connects and adopts* it, and materializes any snapshots already in the store as `discovered` Snapshots. No creation happens.
- **A repository already exists but the password is wrong.** The connect fails `AuthFailure` and kopiur **does not** create. Creating would risk a second repository, or mask the wrong-password error. The `Repository` goes `Failed` and the existing repository is left untouched.
- **No repository exists.** kopiur creates one, only because `create.enabled` is on. As a final backstop, kopia's own `repository create` refuses to overwrite an existing repository, so even a misclassified connect cannot clobber your data.

So enabling `create.enabled` for a repository that turns out to already exist is safe: kopiur adopts it, it does not re-initialize it.

And the gate is one-way in time. **Once a repository has been `Ready`** it carries a pinned `status.uniqueId`, and from then on kopiur refuses to create over it even when `create.enabled` is `true` and the backend comes back empty. That case parks at terminal `Failed` with `RepositoryReinitializeBlocked` and a condition message carrying the exact acknowledgement command. See [Deliberately re-initialize a wiped repository](repository-health.md#deliberately-re-initialize-a-wiped-repository).

## `seed` — initialize a new repository from a replica

`spec.seed` fills a **brand-new** repository with an existing replica's contents during its very first bootstrap, before the repository is ever reported `Ready`. It is the disaster-recovery counterpart of [repository replication](replication.md): the mirror you have been writing off-site becomes the *starting point* of the rebuilt cluster's own repository, instead of something you promote to production or copy by hand.

/// info | `spec.seed` seeds a repository, not a volume

Some example bundles contain a one-shot `Job` named `seed-…` that writes test data into a volume. That is unrelated. **`spec.seed` seeds a _repository_, from another repository.**

///

### When it is armed (and why it is safe to leave in Git)

A seed is armed only when **both** are true:

1. the repository has never been initialized, meaning `status.uniqueId` is unset, and
2. the mover's first connect reports the backend **uninitialized**, which means kopia's own "repository not initialized", never a missing path or an unbound mount.

On an already-initialized repository the block does nothing, and says so: `Seeded=True`, reason `AlreadyInitialized`, nothing copied, nothing touched. So `spec.seed` is a standing GitOps-safe field. Leave it in the manifest forever and it does something exactly once, on the day you rebuild. An `AuthFailure`, `Locked`, `AccessDenied` or `PermissionDenied` connect never seeds and never creates, for the same reason it never creates today: kopiur does not write over a backend it could not open.

While the seed is armed the bootstrap Job's deadline comes from `spec.seed.failurePolicy` and defaults to **86400 s (24 h)** instead of the usual 120 s, because a seed copies a whole repository, once. Every later connect uses `spec.bootstrap.failurePolicy` as before.

### Blob mode — from a mirror backend

```yaml
--8<-- "deploy/examples/41-repository-seed-from-backend.yaml:repository"
```

This runs `kopia repository sync-to` from a bare storage backend holding an exact mirror, which is what a [`RepositoryReplication`](replication.md) writes. The copy happens at the storage layer, so the seeded repository **inherits the mirror's format and password exactly**. That means `encryption.passwordSecretRef` must already hold the mirror's password.

### Migrate mode — from another repository CR

```yaml
--8<-- "deploy/examples/42-repository-seed-from-repository.yaml:repository"
```

This runs `kopia snapshot migrate` from another `Repository` or `ClusterRepository`. Source and destination are two independent repositories with their own formats and passwords, and kopiur creates the local one itself. The source is opened read-only and gated on being `Ready`. Until then the repository parks visibly, showing `Seeded=False` with reason `WaitingForSeedSource`, and re-checks every 15 s.

Seeding from a **`ClusterRepository`** makes this repository a *consumer* of it, so the source's [`allowedNamespaces`](#allowednamespaces--who-may-use-it-clusterrepository-only) must admit this `Repository`'s namespace. That is the same fail-closed tenancy gate every other consumer reference goes through. The webhook rejects the apply naming `spec.seed.from.repository` if it does not. A `ClusterRepository` seeding from another `ClusterRepository` is not gated, because a cluster-scoped author has no consumer namespace to check.

Both modes preserve each snapshot's `username@hostname:path` identity and its times, so seeded history stays restorable by `Restore.source.identity` and by `fromPolicy`.

### Throttling a seed

A seed is the single heaviest transfer Kopiur ever performs: a whole repository, pulled across a link that other people are also using, usually on the worst day you have had all year. Both modes can be capped, but they take different settings because they are different kopia operations.

**Blob mode** copies at the storage layer, so it uses `sync-to`'s own speed flags: `seed.sync.maxDownloadSpeedBytesPerSecond` and `maxUploadSpeedBytesPerSecond`.

**Migrate mode** has no speed flags at all, because `kopia snapshot migrate` offers none. The only lever is `kopia repository throttle set` on each *connection*, so the cap is expressed **per side**. A migrate seed involves two repositories:

```yaml
seed:
    from:
        repository: { kind: ClusterRepository, name: offsite }
    migrate:
        throttle:
            source: # reading OUT of the replica (the off-site link)
                downloadBytesPerSecond: 10485760 # 10 MiB/s
                readOpsPerSecond: 100
            destination: # writing INTO this repository (on-LAN, be generous)
                uploadBytesPerSecond: 209715200 # 200 MiB/s
```

Each side takes the same four settings as a repository's [`moverDefaults.throttle`](reference/crds/shared-types.md#moverdefaults): `uploadBytesPerSecond`, `downloadBytesPerSecond`, `readOpsPerSecond`, `writeOpsPerSecond`. Every value you set must be at least `1`. A `0` is rejected at admission, because kopia's "no limit" is the *absent* setting, not a zero.

**Two layers, merged field by field.** Each repository can already declare `moverDefaults.throttle`, which every mover that touches it honors. `seed.migrate.throttle` sits on top of that, per side:

- `throttle.source` overrides the **replica's** defaults, meaning the repository named by `seed.from.repository`, not this one;
- `throttle.destination` overrides **this** repository's defaults;
- the merge is per field: a value you set here wins, and one you leave unset keeps the repository's value. Setting one value never drops the repository's others;
- omit `migrate.throttle` entirely and each side simply uses its own repository's defaults.

The two sides never mix. kopia's limits are **per connection**, and a migrate seed opens both repositories under two separate kopia configurations. The override applies only while the seed is **armed**: every later connect to the now-initialized repository is capped by `moverDefaults.throttle` alone, so a number chosen for a one-time copy does not go on constraining routine work.

If any of the throttle settings fails to apply, the bootstrap **fails** rather than proceeding uncapped. Saturating exactly the link you asked to protect is worse than not running. The mover logs `applied repository throttle` once per capped connection. That is three times for a healthy first migrate seed, covering the replica, this repository's seed-local connect, and the post-seed reconnect. It is four times when it is *resuming* an interrupted attempt, because that run's opening probe connect finds the repository already there and so gets capped too:

```console
$ kubectl -n billing logs job/rebuilt-nas-bootstrap | grep 'applied repository throttle'
```

/// warning | A byte cap bites much harder than its number suggests

Byte-rate caps apply to **cold** backend traffic only. Content already in the mover's kopia cache is served without touching the limiter, so a warm re-run can look entirely unthrottled. And on small-object workloads the effective throughput lands far below the nominal number: a measured 2 MB/s cap took about 14 s to move a 28 KiB cold repository, while caps of 10 MB/s and above often did not bind at all at that size. Set these as a **ceiling for large transfers**, generously, and measure against your own data. A seed already runs against a 24 h deadline, and a cap picked to "feel safe" is a good way to make that deadline the thing that fails.

///

/// warning | New CRD fields need `kubectl apply`, not just `helm upgrade`

`seed.migrate.throttle` is a new CRD field. Helm's `crds/` directory is **install-only**, so a `helm upgrade` never updates CRD schemas. On a helm-CLI upgrade, an apiserver running the old schema **silently prunes** the field from your manifest: the object admits cleanly, `kubectl get -o yaml` shows no `throttle`, and the seed runs uncapped with nothing to see. Apply the CRDs first with `kubectl apply --server-side -f deploy/crds/`, or use a GitOps flow with a `CreateReplace` CRD policy. See [CRD lifecycle](install.md#crd-lifecycle).

///

### `create` and `seed` together

| You set | What happens |
| --- | --- |
| `seed` only | The seed initializes the repository. `create.enabled` keeps its ordinary meaning for later connects, but the **create fallback is never taken** while a seed is armed. A failed seed fails the bootstrap instead of quietly creating an empty repository. |
| `seed.from.backend` + `create.{splitter,hash,encryption,ecc}` | **Rejected at admission.** A blob copy takes the mirror's repository format exactly as it is, so those algorithms would never be applied. Remove them, or use migrate mode. |
| `seed.from.repository` + `create.{…}` | Honored: migrate mode really does create a local repository with the format you declare. |
| `seed` + `mode: ReadOnly` | **Rejected at admission.** Seeding is the largest write a repository ever takes. |
| `seed` on a bare-path `filesystem` backend (no `volume`) | **Rejected at admission**, for the repository *and* for a filesystem seed source. Seeding runs in a mover Job; a bare path exists only on the controller's own filesystem, so the Job would mount nothing. |

### The fields

| Field | Default | What it does |
| --- | --- | --- |
| `seed.from.backend` | — | Blob mode: a mirror's storage backend (the same externally-tagged `Backend` shape `spec.backend` uses). |
| `seed.from.repository` | — | Migrate mode: a `RepositoryRef` (`kind` defaults to `Repository`; an absent `namespace` resolves in this CR's namespace, and for a `ClusterRepository` in the operator's namespace). |
| `seed.sync.parallel` | kopia's `1` | Blob mode only: concurrent blob-copy workers. Raise it, because a first seed over a WAN is what sequential copying is worst at. |
| `seed.sync.maxDownloadSpeedBytesPerSecond` / `maxUploadSpeedBytesPerSecond` | unlimited | Blob mode only: throttle the copy. |
| `seed.migrate.parallel` | kopia's `1` | Migrate mode only: snapshots migrated concurrently. |
| `seed.migrate.latestOnly` | `false` | Copy only each identity's newest snapshot instead of its full history. |
| `seed.migrate.policies` | `none` | Whether the source's **kopia-side** policies come along. The default is an explicit `--no-policies`, unlike kopia's own copy-by-default: retention here is driven by `Snapshot` CRs, and imported kopia policies could delete manifests behind the operator's back. |
| `seed.migrate.throttle.source` / `.destination` | each repository's `moverDefaults.throttle` | Migrate mode only: bandwidth and ops caps for the seed run, **per side**. `source` is the replica, `destination` is this repository. Each overrides that side's repository defaults field by field; every value you set must be `>= 1`. Armed seeds only. See [Throttling a seed](#throttling-a-seed). |
| `seed.allowEmptySource` | `false` | Accept a source holding zero snapshots. Left at `false`, an empty source **fails the bootstrap** and retries. |
| `seed.failurePolicy.activeDeadlineSeconds` | `86400` (24 h) | Wall-clock cap for the seeding Job. |
| `seed.failurePolicy.backoffLimit` | as `bootstrap` | Pod retries within one seeding Job. |
| `seed.credentialProjection.enabled` | `false` | Migrate mode: copy the **source** repository's credential Secrets into the seeding Job's namespace for the run. Requires the operator's `features.credentialProjection.enabled` flag; see [Feature permissions](feature-permissions.md). |

The mode-specific blocks are **not** silently ignored when paired with the other mode's source. Using `sync` with a repository source, or `migrate` (or an enabled `credentialProjection`) with a backend source, is rejected at admission. Kopiur does not accept fields that would do nothing.

/// warning | Where the seed source's credential Secret must live

It is loaded with `envFrom`, which is namespace-local, so it must be in **the namespace the bootstrap Job runs in**. For a namespaced `Repository` that is its own namespace. For a `ClusterRepository` it is the operator's namespace, unless `encryption.passwordSecretRef.namespace` pins one, in which case the Job runs there and the seed Secret must be there too. A seed `secretRef` on a `ClusterRepository` may not pin a namespace at all, because a cluster-scoped spec cannot name the right one.

///

### Never clobbering anything

- Seeding **never writes to its source**. Both modes connect it read-only.
- A seed that ends with the repository holding **zero** snapshots is refused, with `Seeded=False` and reason `SeedLeftEmpty`, rather than reported `Ready`. The `allowEmptySource` opt-in is the only way to accept that.
- A migrate seed only ever *resumes* into an existing repository when kopiur itself recorded starting that seed, meaning `status.seed.startedAt` is set. A repository somebody else initialized, which is an ordinary adoption with `spec.seed` standing in the manifest, takes the `AlreadyInitialized` no-op instead.
- Blob mode gets a kopia-side backstop for free: `sync-to` refuses a destination whose format blob differs from the source's.

### Failures retry; an interrupted seed resumes

Source-side and copy-side failures (`SeedSourceNotFound`, `SeedSourceEmpty`, `SeedIncomplete`, `SeedLeftEmpty`) are **retryable**. The failed Job is recycled and a fresh one, with a fresh 24 h deadline, is launched roughly every **two minutes**. So a mirror that is briefly unreachable, or a replication that has not run yet, heals with no operator action.

Kopiur stamps `status.seed.startedAt` *before* creating the seeding Job, so a copy killed mid-flight is recognizable as its own on the next pass and the relaunch **resumes**. `sync-to` copies only the blobs the destination lacks, and `snapshot migrate` is idempotent by `(identity, startTime)`. Nothing at the backend should be deleted to "clean up" after an interrupted seed. `status.seed.snapshotsCopied` is therefore *cumulative*: it reports what is present after the run, not a per-attempt delta.

`MoverImageTooOldForSeed`, which means the running mover image predates `spec.seed` and dropped it, and `BootstrapInternalInconsistency` are **terminal**, because the same inputs reproduce them forever.

### Editing `spec.seed` mid-flight, and `suspend`

`spec.seed` is mutable. An in-flight seeding Job is **not** killed: it runs to completion, its result is discarded as stale, and the next pass launches a fresh seed for the live spec. The attempt marker survives, so that relaunch resumes, **against the new source**. Blob mode fails safe there, because `sync-to` refuses a mismatched format blob. Migrate mode does not, and will merge the new source's history into what the first attempt left behind. To deliberately repoint a migrate seed before it finishes, delete the half-seeded repository at the backend first, or let it finish and move the extra history with a [`SnapshotReplication`](snapshot-replication.md).

Using `spec.suspend` mid-seed leaves the Job running. Its result is consumed when you resume.

### Status and conditions

`status.seed` carries `startedAt`, `seededAt`, `mode`, `source`, `snapshotCount` and, for migrate mode only, `snapshotsCopied`. The `Seeded` condition carries the state, `kopiur_repository_seed_total{mode,outcome}` counts the outcomes, and `kubectl kopiur doctor` explains every `Seeded=False` reason. The full reason table, the DR walkthrough, and the identity and retention hazards to review **before** re-applying policies over seeded history are in [Scenario 10: DR from a replicated repository](scenarios/dr-with-replicated-repository.md).

/// danger | Review retention before re-applying policies over seeded history

Adoption re-attaches matching `discovered` snapshots to a live `SnapshotPolicy`. Under the default `deletionPolicy: Delete`, everything outside `spec.retention` is then pruned from the repository immediately, because retention prunes bypass `deletionProtection` by design. Widen the window, set `defaultDeletionPolicy: Retain`, use `spec.pin` on what must survive, or set `adoption: Ignore`. Also bound `catalog.retain` so a multi-year mirror does not materialize thousands of `Snapshot` CRs at once.

///

## `bootstrap` — tuning the connect/create Job

Object-store and volume-backed repositories connect, and with `create` also create, in a short-lived **bootstrap Job** that the operator cannot run in-process. By default that Job is bounded to **120s**, so a pod that never schedules, because of a missing mover ServiceAccount or an image-pull failure, becomes terminal `Failed` and raises an actionable Event instead of hanging. A valid but slow backend can need longer. The most common case is an [rclone](backends/rclone.md) remote whose metadata and indexes load through kopia's embedded `rclone serve` bridge.

`spec.bootstrap.failurePolicy` uses the same shape as a recipe's `failurePolicy`:

```yaml
spec:
    bootstrap:
        failurePolicy:
            activeDeadlineSeconds: 600 # wall-clock cap for the bootstrap Job (default 120)
            backoffLimit: 1 # retries before the Job is marked failed (default 2)
```

| Field                              | Default | What it controls                                                                 |
| ---------------------------------- | ------- | -------------------------------------------------------------------------------- |
| `failurePolicy.activeDeadlineSeconds` | `120`   | Wall-clock seconds before the bootstrap Job is killed and marked failed.        |
| `failurePolicy.backoffLimit`       | `2`     | Pod retries before the Job is failed.                                            |

/// note | `podStartupDeadlineSeconds` is not honored for bootstrap

`failurePolicy` is the shared type, so it also carries `podStartupDeadlineSeconds`. The bootstrap Job does not use it; only backup, restore and maintenance movers do. Use `activeDeadlineSeconds` to bound a slow bootstrap.

///

For an rclone-specific connect that's slow *before* metadata even loads, also raise [`backend.rclone.startupTimeout`](backends/rclone.md#fields-reference-backendrclone), which is kopia's wait for `rclone serve` to come up.

## The catalog — discovered snapshots

A kopia repository can hold snapshots Kopiur didn't produce: an adopted repository's history, a workstation `kopia` CLI, another cluster, a cron job. The **catalog scan** surfaces them as `Snapshot` CRs with `origin: discovered`, so they show up in `kubectl get snapshots`, in `kubectl kopiur snapshots list`, and as restore sources. No timestamp guessing needed.

```console
$ kubectl get snapshots -l kopiur.home-operations.com/origin=discovered
```

How it behaves, all of it automatic with nothing to install:

- **Discovered means "not produced through this Repository CR".** Snapshots Kopiur itself takes via your `SnapshotPolicy`s already have their own `Snapshot` CRs and are never duplicated as discovered rows.
- **Discovered rows are forced `deletionPolicy: Retain`.** Deleting a discovered `Snapshot` CR deletes only the CR. Kopiur **never** deletes a kopia snapshot it didn't create. And because the row mirrors repository state, it reappears on the next refresh; to keep rows away permanently, bound them with `catalog.retain` below.
- **An initial scan always runs**, on first bootstrap and again on any spec change, so adopting a repository surfaces its existing history immediately.
- **Repeated re-scanning is opt-in.** Set `catalog.periodicRefresh: true` to keep re-scanning every `catalog.refreshInterval`, default **1h**, minimum `30s`. That way snapshots written out-of-band *after* adoption keep appearing, and rows whose snapshot was pruned repository-side are expired. It is **off by default**, because for object-store and volume-backed repositories each re-scan re-runs the self-cleaning bootstrap Job. Leaving it off means the repository bootstraps once and isn't re-run on a timer. When it is on, the interval is the *only* thing that drives re-scans: a Job removed early by its `ttlSecondsAfterFinished` does **not** trigger an extra scan, so `refreshInterval: 24h` gives one scan a day regardless of the Job TTL.
- **The row carries the real data**: the kopia snapshot ID, the foreign `username@hostname:path` identity, the snapshot's timing and logical size.

The settings, all under `spec.catalog`:

```yaml
spec:
    catalog:
        periodicRefresh: true # opt in to repeated re-scans (default false = scan once)
        refreshInterval: 1h # how often to re-scan when periodicRefresh is on (default 1h, min 30s)
        retain:
            perIdentity: 100 # keep the newest N rows per username@hostname:path (0 = no rows)
            maxAgeDays: 90 # no rows for snapshots older than this
        fallbackNamespace: backups # ClusterRepository only — see below
        adoption: Adopt # Adopt (default) | Ignore — see below
```

`retain` bounds the **CR rows, never the data**. A row expired by `perIdentity` or `maxAgeDays` is just a deleted CR: the kopia snapshot stays in the repository and remains restorable via [`Restore.source.identity`](restores.md#restoring-a-snapshot-kopiur-didnt-create).

/// note | Where a ClusterRepository puts discovered Snapshots

A namespaced `Repository` materializes rows in its own namespace. A **`ClusterRepository`** places each row in the namespace named by the snapshot identity's **hostname**, when that namespace exists and passes the `allowedNamespaces` gate, so an adopted shared repository's snapshots land next to the workloads they belong to. Identities whose hostname maps to no allowed namespace go to `catalog.fallbackNamespace`. With no fallback configured they are skipped, and the `ClusterRepository` gets a Warning Event (`DiscoveredSnapshotUnplaced`) naming the hostnames and the fix.

///

### `catalog.adoption` — automatically re-attaching discovered snapshots

A discovered row can stop being a dead end. When a `discovered` snapshot's kopia identity **exactly** matches a live `SnapshotPolicy`'s resolved identity, meaning `username` AND `hostname` AND `sourcePath`, never a partial match, the policy **adopts** it. A fresh `origin: adopted` `Snapshot` CR is created in the policy's namespace carrying the policy's config label, and the discovered row is deleted. The adopted row is now GFS-governed exactly like a produced backup, see [Backups → Retention](backups.md#retention--how-long-backups-are-kept-gfs), instead of sitting in the catalog forever.

```yaml
spec:
    catalog:
        adoption: Ignore # Adopt (default) | Ignore
```

- **Resolution order**: `SnapshotPolicy.spec.adoption`, if set, always wins over the repository's `catalog.adoption`. An unset policy field inherits the repository's setting, and both unset defaults to `Adopt`. Set it on the repository to change the default for every policy against it, or on one policy to carve out an exception.
- **`Ignore`** turns adoption off, at whichever level it's set. Discovered rows keep accumulating, bounded only by `catalog.retain`, and you restore from them directly instead. See [Restores → discovered snapshots](restores.md#restoring-a-snapshot-kopiur-didnt-create).
- **A brand-new or delete-then-recreated policy nudges the catalog.** A `SnapshotPolicy` with no adoption history yet, that finds nothing to adopt on its first pass, requests an on-demand catalog scan on its repository. An `AdoptionScanRequested` Normal Event names the identity. It does not wait on a spec change or the periodic-refresh timer, and this is what makes "delete a policy, then re-apply it" self-heal without you manually forcing a re-scan. It fires exactly once per (policy, identity): a scan that turns up nothing stays quiet after that. See [Adopt an existing repo → Delete a policy, then recreate it](scenarios/adopt-existing-repo.md#delete-a-policy-then-recreate-it) for the full walkthrough.
- **The request is bounded, even on a repository shared by many policies.** Several policies recreated at once each stamp the SAME `catalog-scan-requested-at` annotation on the repository. The repository only ever needs to run one scan to satisfy all of them, because a fresh listing re-materializes every match at once, so a burst of simultaneous requests costs one scan, not one per policy.
- **Adoption is retention-aware when the adopted rows would be `Retain` or `Orphan`.** When the policy's effective `defaultDeletionPolicy` is `Retain` or `Orphan`, pruning an adopted row deletes only the `Snapshot` CR. The kopia snapshot survives, the next scan re-discovers it, and adoption would re-attach it in an endless loop. So under those policies Kopiur only adopts the candidates its GFS `spec.retention` window would actually **keep**. The out-of-window ones deliberately stay `discovered`, bounded by `catalog.retain`, the count lands in `status.adoption.skippedByRetention`, and an `AdoptionSkippedByRetention` Normal Event names your options: widen `spec.retention`, switch to `defaultDeletionPolicy: Delete`, `pin` a snapshot, or set `adoption: Ignore`. Under the default `defaultDeletionPolicy: Delete` every match is adopted as before, because pruning then genuinely deletes kopia data and the cycle converges.
- **`status.catalog.discoveredBackupCount` is briefly stale right after an adoption wave.** It's written by the catalog scan, which is the `Repository`/`ClusterRepository` reconciler, while adoption runs in a **separate** reconcile pass, the `SnapshotPolicy`, that deletes discovered rows without touching that count. Expect it to over-count by however many rows were just adopted until the next scan corrects it. It is not wrong, just one scan behind.
- **Foreign-cluster snapshots are never adopted**, even on an exact identity match. See [`identityDefaults.cluster`](#identitydefaultscluster--sharing-one-repository-across-clusters).

## `moverDefaults` — one place to configure every mover

A repository spawns movers for **everything**: bootstrap (connect and create), backup, restore, maintenance. `moverDefaults` is the single base they all inherit. A per-recipe `mover` block, on `SnapshotPolicy`, `Restore` or `Maintenance`, overlays it **field by field**. The recipe wins, the default fills in the rest, and the hardened security baseline sits underneath, so a partial override can only tighten, never drop `drop:[ALL]` or seccomp.

```yaml
spec:
    moverDefaults:
        securityContext: # container SC — runAsUser/runAsGroup, caps, seccomp
            runAsUser: 1000 # the UID every mover runs as
            runAsGroup: 1000 # the GID every mover runs as
        podSecurityContext: # pod SC — notably fsGroup
            fsGroup: 1000 # make fresh restore volumes group-writable, set once here
        resources: { requests: { cpu: 250m, memory: 512Mi } }
        cache: # kopia cache backing every mover (capacity/class/budgets)
            capacity: 10Gi
            storageClassName: fast-ssd
        scratch: # default size/class for the deep-verify scratch (restore-test) PVC
            capacity: 100Gi # inherited by SnapshotPolicy verification.deep
            storageClassName: fast-ssd
        nodeSelector: { kubernetes.io/arch: amd64 }
        tolerations: [{ key: backup, operator: Exists }]
        affinity: { ... }
        sourceColocation: { mode: Auto } # avoid RWO Multi-Attach (default Auto)
        ttlSecondsAfterFinished: 3600 # finished mover Jobs self-GC
        throttle: # cap kopia's bandwidth/ops so a run doesn't saturate the link
            uploadBytesPerSecond: 10485760
            downloadBytesPerSecond: 10485760
```

/// tip | Set the mover UID/GID once, for every mover

Set the UID and GID **once** on `moverDefaults.securityContext.runAsUser` and `runAsGroup`, plus `podSecurityContext.fsGroup`, and every mover the repository spawns inherits it, **including the bootstrap (connect/create) Job**. That means a filesystem or NFS repository on a directory not owned by `65532` is bootstrappable with no special-case setting: the bootstrap mover runs as the UID you set here. A per-recipe `mover` block can still tighten any of these for an individual `SnapshotPolicy`, `Restore` or `Maintenance`. See [example 09](examples.md#example-09--mover-uidgid--permissions) and [Permissions](permissions.md).

`podSecurityContext.fsGroup` already **defaults to `65532`**, the mover image's GID, so the operator-managed kopia cache is writable out of the box. You only set it here to match a non-default mover UID. Note that `fsGroup` can't fix a root-squashed **NFS** cache StorageClass. Keep `moverDefaults.cache` unset, which gives a node-local `emptyDir`, or use a block class for a sized cache. See [Security context](security-context.md#the-default-hardened-context).

`moverDefaults.scratch` is the repo-level default for the **deep-verification** scratch volume, the throwaway restore target a `verification.deep` restore-test writes into and then discards. It is the scratch sibling of `cache`: a `SnapshotPolicy`'s `verification.deep.{capacity,storageClassName}` overlays it field by field, so you set the scratch size and class **once** here instead of repeating it on every policy. Unlike the cache, scratch has **no `mode`**; it is always ephemeral and discarded after each run. `storageClassName` only applies when a `capacity` is set, because an `emptyDir` has no StorageClass. A class with no effective capacity does nothing, and the operator flags that via the `ScratchStorageClassIgnored` condition on the `SnapshotPolicy`. See [verification](backups.md#verification--prove-the-snapshots-are-restorable).

/// note | Verification movers inherit `moverDefaults` too

The `quick` and `deep` verification movers a `SnapshotPolicy` spawns inherit the **whole** `moverDefaults`: security context, resources, scheduling, TTL, throttle, **and** the kopia `cache` with its size, class and budgets. Exactly like backup and restore movers. The one exception is `cache.mode: Persistent`, which is **forced to a fresh per-run ephemeral cache** for verification, because a verify run must never attach the backup's warm `ReadWriteOnce` cache PVC. That would be a Multi-Attach race. Set `moverDefaults.cache` once and your verifications are sized and placed without any per-policy config.

///

### `sourceColocation`: avoid the RWO Multi-Attach error

A `ReadWriteOnce` (RWO) PVC can only be **attached to one node at a time**, though several pods **on that same node** can mount it. When your app pod already holds an RWO PVC on node A and Kopiur's mover lands on node B, the kubelet on B can't attach the volume and the mover pod is stuck with a `Multi-Attach error`. It never starts.

By default Kopiur prevents this. It finds the node the source PVC is attached to and **pins the mover there**, using a required `kubernetes.io/hostname` nodeAffinity plus the holder pod's tolerations, so the mover runs alongside your workload. This is automatic and you don't need to set anything.

```yaml
spec:
    moverDefaults:
        sourceColocation:
            mode: Auto # Auto (default) | Required | Disabled
```

- **`Auto`** (default): pin an RWO source or destination PVC to the node it's attached to, when that node can be discovered. Discovery uses the consuming pod, the bound PV's `nodeAffinity`, or a CSI `VolumeAttachment`. `ReadWriteMany` and `ReadOnlyMany` volumes, and RWO volumes nothing holds, are scheduled freely, since there is no Multi-Attach risk. A `ReadWriteOncePod` volume **held by a running pod** fails with guidance, because a second pod can't mount it even on the same node. Use `copyMethod: Snapshot` or scale the workload down; see [PVC access modes & RWOP](access-modes.md).
- **`Required`**: like `Auto`, but if an RWO PVC's node can't be determined, the run **fails** with an actionable error instead of scheduling freely. Use it when an RWO source must never be backed up from the wrong node.
- **`Disabled`**: never compute a node pin. The mover uses only the explicit `nodeSelector`, `affinity` and `tolerations`. This is the pre-fix behavior, and an escape hatch for topologies that manage placement themselves.

/// note | Discovery needs read access to PVs and VolumeAttachments

The fallbacks read cluster-scoped `persistentvolumes` and `storage.k8s.io/volumeattachments`. The shipped RBAC already grants this. If you run a hand-trimmed ClusterRole, add `get,list,watch` on both, or co-location silently degrades to "no pin found". See [RBAC](rbac.md).
///

/// warning | Application consistency

Co-location lets the mover read a **live** volume, and that snapshot is crash-consistent at best. For application-consistent backups of databases, quiesce the app with pre- and post-[snapshot hooks](backups.md).
///

///

## `scheduleDefaults` — set the cron timezone and jitter once

Every cron-driven consumer of a repository takes its own optional `timezone` and `jitter`. That covers `SnapshotPolicy.spec.verification`, `RepositoryReplication.spec.schedule`, `SnapshotReplication.spec.schedule`, `Maintenance.spec.schedule`, and `SnapshotSchedule.spec.schedule`, which is the recurring-backup cron. Repeating the same values on every one of them is tedious and easy to get out of sync. Set them **once** on the repository instead:

```yaml
spec:
    scheduleDefaults:
        timezone: America/New_York # IANA name, validated at admission
        jitter: 10m # Go-style duration; max 24h, enforced at admission
```

Precedence is resolved at reconcile time, not pinned at admission: the consuming cron's own value wins when set, then the `scheduleDefaults` value here, then the built-in fallback. For `timezone` that fallback is UTC. For `jitter` there is none, so absent at both levels simply means no spread. `Maintenance` already cascades per-cron, then schedule-level, before falling back to the repo default, so this becomes the **third and final** level for it.

### What `jitter` buys you

`jitter` spreads each firing over a window instead of stacking every schedule on the top of the hour. The spread is **deterministic**, derived from the schedule's UID and the slot rather than drawn at random. The same slot always lands on the same instant, so a controller restart or an HA failover never re-rolls it, and every replica computes the same answer. Setting `jitter: 10m` once on a repository de-synchronizes every schedule that repository serves, which is the cheapest thing you can do to stop a 02:00 thundering herd against one backend.

/// warning | Jitter is a spread within a period, not a schedule offset

A window over 24h is rejected: `jitter of 25h exceeds the 24h maximum`. Jitter shifts a firing *inside* its cron period. A window longer than a day is almost always someone trying to express "run it later", which is a cron change.

`scheduleDefaults.jitter` is a **brand-new field**, so there is nothing to worry about on upgrade: no stored repository can carry a value the rule rejects, because the field did not exist to be set. That is not true of the jitter windows on the **consuming** crons, meaning `SnapshotSchedule`, verification, `Maintenance`, and both replications, which already shipped. There the same 24h cap tightens a field stored objects may already carry, so it is deliberately enforced at admission only. A tightened rule that ran in the reconciler would stop backups on objects nobody touched. The same applies to a negative `startingDeadlineSeconds`. See [Upgrading](upgrade.md#admission-only-jitter-and-deadline-rules-re-apply-only).

///

/// note | How `SnapshotSchedule` inherits

A `SnapshotSchedule` with no `spec.schedule.timezone` or `spec.schedule.jitter` resolves its **target policy's** repository `scheduleDefaults` at slot-computation time, following `policyRef`, or each `policySelector` match. The resolved values are recorded in `status.nextSchedule.timezone` and `status.nextSchedule.jitter`. Editing either default re-triggers the affected schedules, through a repository referent watch, and recomputes the pinned slot in the new zone or window. You don't have to touch each schedule, and the edit lands on the next firing rather than an arbitrary number of firings later.

For a `policySelector` schedule whose matched policies' repositories **disagree**, there is no single right answer. A timezone disagreement falls back to UTC and raises a `TimezoneDefaultAmbiguous` condition. A jitter disagreement resolves to **no jitter** and logs the candidate windows. Both recommend the same fix: set `spec.schedule.timezone` or `spec.schedule.jitter` explicitly.

///

A complete, apply-ready example, with a `Repository` carrying `scheduleDefaults.timezone`, a `SnapshotPolicy.spec.verification.quick` cron, and a `SnapshotSchedule` that all inherit it:

```yaml
--8<-- "deploy/examples/29-repo-schedule-timezone.yaml"
```

## `concurrency` — cap the mover Jobs one repository runs at once

By default a repository runs as many mover Jobs at once as its consumers ask for. `maxConcurrentJobs` puts a ceiling on that:

```yaml
spec:
    concurrency:
        maxConcurrentJobs: 3 # absent or 0 = unlimited (the default)
```

Backups, restores, and the source side of both replication kinds draw from **one** pool per repository. The backend, and the bandwidth to it, are the shared resource, so a separate budget per work kind would let three separately "safe" limits still saturate it. Maintenance, verification, pin, batched snapshot deletions, bootstrap and catalog scans, and `kubectl kopiur browse` sessions are outside the pool entirely. Restores are always admitted and never queued, but they do occupy a slot, from the instant they are admitted rather than once their Job is visible, so a restore displaces backups instead of adding to them.

A run that arrives at a full pool is parked **before its Job is created**. It shows `phase: Pending` plus `RepositorySlotAvailable=False` with reason `WaitingForSlot`, naming the counts, and it launches automatically when a slot frees. The full behavior, the metric to watch, and how the cap composes with `concurrencyPolicy` and `startingDeadlineSeconds` are in [Backups → limiting concurrent jobs per repository](backups.md#limiting-concurrent-jobs-per-repository).

A cluster-wide backstop, the Helm value [`maxConcurrentJobs`](install.md#runtime-tuning), bounds the same pool across every repository, for operators whose constraint is the node pool rather than any one backend. A run must satisfy both.

### A tuned repository, end to end

Concurrency, `scheduleDefaults` and `moverDefaults.podLabels` are the three settings you reach for once a repository has real load on it. Applied together:

```yaml
--8<-- "deploy/examples/43-tuned-repository.yaml"
```

## `onNamespaceDelete` — what `kubectl delete ns` does to snapshots

`Orphan` (default) or `Delete`. A backup tool must not make deleting a namespace a silent data-loss event, so the default is fail-safe:

| Value | On namespace deletion |
| --- | --- |
| `Orphan` _(default)_ | Release ownership (drop the `Snapshot` finalizers) **without** deleting the kopia snapshots, so off-site history survives. |
| `Delete` | Cascade: each `Snapshot`'s own `deletionPolicy` applies (produced snapshots are `kopia snapshot delete`d). Opt-in. |

A namespace delete is only one of **three independent ways** a `Snapshot`'s kopia data can go away. The others are deleting the `Snapshot` itself and deleting its `SnapshotSchedule`. Each has its own opt-in field, and every one of them defaults to keeping the data:

| Axis | Trigger | `Snapshot` CRs | kopia data, by DEFAULT | Opt into cascading kopia data |
| --- | --- | --- | --- | --- |
| Single `Snapshot` | `kubectl delete snapshot <name>` | Removed once its finalizer clears | Honors *this* Snapshot's own `deletionPolicy`, which is `Delete` by default for `scheduled`/`manual` (see [Backups → deletionPolicy](backups.md#deletionpolicy--what-happens-to-the-snapshot)) | Set `deletionPolicy: Retain`/`Orphan` on the Snapshot, or `SnapshotPolicy.spec.defaultDeletionPolicy` |
| Namespace | `kubectl delete namespace <ns>` | Finalizers released, CRs removed | **Retained** by this field, `onNamespaceDelete: Orphan` (default, above) | This field, `onNamespaceDelete: Delete` |
| `SnapshotSchedule` | `kubectl delete snapshotschedule <name>` | GC'd via `ownerReference` once finalizers clear | **Retained**, rediscovered as `origin: discovered`; schedule `spec.deletion.onScheduleDelete: Retain` (default), which overrides even a produced Snapshot whose own `deletionPolicy` was `Delete` | Schedule `spec.deletion.onScheduleDelete: Delete`; see [Backups → what happens when the schedule is deleted](backups.md#what-happens-when-the-schedule-is-deleted) |

Whichever of these triggers an actual `kopia snapshot delete`, meaning an EXTERNAL deletion with effective `deletionPolicy: Delete` as opposed to one of Kopiur's own retention or `failedJobsHistoryLimit` prunes, is additionally subject to the per-repository mass-deletion circuit breaker, described next.

## `deletionProtection` — the mass-deletion circuit breaker

A single accidental or automated action can queue up far more legitimate-looking external Snapshot deletions than any one operator action should be able to erase at once. The wrong `SnapshotSchedule` deleted, a bulk namespace cleanup, a mis-scoped `kubectl delete snapshot -l ...`. Each deletion is individually authorized, because the CR's own `deletionPolicy` really is `Delete`. `deletionProtection` is a per-repository breaker against exactly that. It never slows a normal trickle of deletions, but a sudden wave is HELD until a human explicitly approves it.

```yaml
spec:
    deletionProtection:
        threshold: 25 # default 10; 0 disables the breaker
```

### What trips it

Every reconcile, the operator counts this repository's pending **EXTERNAL, destructive** Snapshot deletions. Those are the ones with `metadata.deletionTimestamp` set and effective `deletionPolicy: Delete`, that are NOT stamped by one of Kopiur's own prunes (described below), and not yet acknowledged (also below). At or above `threshold`, EVERY one of those deletions is HELD: the Snapshot's finalizer stays, no `kopia snapshot delete` runs, and the CR stays `phase: Deleting` until you act.

### What HELD looks like

- Each held `Snapshot` gets `DeletionHeld=True` with reason `MassDeletionBreaker`, and, on the transition into held, a `SnapshotDeletionHeld` Warning Event.
- The repository, either `Repository` or `ClusterRepository`, gets a `MassDeletionHeld=True` condition with reason `ThresholdExceeded`, recomputed every reconcile from the live pending count.
- The `Snapshot`'s condition and event message carries the exact `kubectl annotate` command to copy, so you never have to construct it by hand:

```console
$ kubectl describe snapshot nightly-abc12 -n billing
...
Conditions:
  Type          Status  Reason               Message
  DeletionHeld  True    MassDeletionBreaker  this snapshot's deletion is HELD by the mass-deletion
                                              breaker: 23 pending external destructive deletions for
                                              Repository `nas-primary` are at/above its threshold of
                                              10. No kopia data has been deleted and this Snapshot
                                              keeps its finalizer. To APPROVE this wave (releases
                                              every currently-held deletion for the repository), run:
                                              kubectl -n billing annotate repository/nas-primary
                                              kopiur.home-operations.com/allow-mass-deletion=
                                              "2026-07-16T18:04:11Z" --overwrite. To release THIS
                                              Snapshot alone WITHOUT deleting its kopia snapshot,
                                              annotate it kopiur.home-operations.com/skip-snapshot-
                                              cleanup: "true".
```

### Releasing the wave — copy the value, don't generate it

Run the command the condition or event gave you, exactly as printed:

```console
$ kubectl -n billing annotate repository/nas-primary \
    kopiur.home-operations.com/allow-mass-deletion="2026-07-16T18:04:11Z" --overwrite
```

A `ClusterRepository` is cluster-scoped, so the surfaced command omits `-n` and says `clusterrepository/<name>` instead.

The annotation's value is an RFC3339 **timestamp**, not a boolean. It means "I approve every deletion pending up to this instant." A held Snapshot releases only if its own `deletionTimestamp` is at or before that value:

- **Copy the value the operator surfaced**, from the condition or the event. It's the newest pending deletion's timestamp, chosen so this one `annotate` releases the WHOLE currently-held wave. A hand-generated value, for example from `date`, risks landing earlier than some pending deletions and releasing only part of the wave. The controller only ever echoes back the exact value that works; it does not compute one from an arbitrary input.
- **A stale acknowledgement is inert against a LATER wave.** Re-applying the same value later, including one committed to Git, does not pre-approve a fresh wave that starts afterward. Nothing requires removing the annotation once it has done its job.
- **A future-dated value is clamped to now.** An acknowledgement can never pre-approve deletions that haven't happened yet. That is a clock-skew guard.
- **An unparseable value is IGNORED**, which is fail-safe because the breaker stays armed, and it raises an `InvalidMassDeletionAck` Warning Event flagging the annotation as unparseable RFC3339.

### `threshold: 0` disables the breaker

Set it when a repository legitimately churns through more external deletions than the default in normal operation, for example many tenants each pruning by hand, and you accept the risk. There's no per-Snapshot opt-out from the breaker other than this repository-wide switch.

### Kopiur's own prunes always bypass it

GFS retention and `failedJobsHistoryLimit` pruning stamp the `Snapshot` they're about to delete with `kopiur.home-operations.com/pruned-by: retention` or `failed-history` **before** deleting it. The finalizer reads that as the operator's OWN lifecycle action, never external, so it is never held and never counted toward the threshold. This is deliberate: retention has to keep working, at its normal rate, even while a genuinely bulk EXTERNAL wave is being held for review. A missing or unrecognized value on that annotation is treated as external, which is fail-safe.

### The namespace-teardown corner

Opting a repository into [`onNamespaceDelete: Delete`](#onnamespacedelete--what-kubectl-delete-ns-does-to-snapshots) makes namespace-deletion cascades an EXTERNAL destructive deletion exactly like any other. The breaker deliberately doesn't care who or what triggered the deletion, so a namespace teardown that would delete more than `threshold` snapshots is HELD too, with the same acknowledgement flow as above. The default `onNamespaceDelete: Orphan` path never calls `kopia snapshot delete` at all, so it never interacts with the breaker.

### Escape hatches

- **Release one Snapshot without deleting its kopia data**: annotate it `kopiur.home-operations.com/skip-snapshot-cleanup: "true"`. That is the same per-CR lever documented in [Backups → the escape hatch](backups.md#what-delete-needs-to-succeed), and it overrides the breaker just like it overrides everything else. The kopia snapshot survives and is rediscovered as `origin: discovered` later.
- **Release the whole wave**: the acknowledgement annotation above.

A complete, apply-ready example combining a repository's `deletionProtection.threshold` with a schedule's `onScheduleDelete` cascade opt-in:

```yaml
--8<-- "deploy/examples/34-schedule-deletion-protection.yaml"
```

See also: [`kopiur_snapshot_deletions_held` / `_pending_external`](dev/observability.md), the gauges behind an alert on a wave forming, and the batched execution of an actually-approved deletion wave in [Backups → how a deletion actually runs](backups.md#how-a-deletion-actually-runs--batched-not-one-job-per-snapshot).

## `mode` — ReadWrite or ReadOnly

`mode: ReadWrite` (default) or `ReadOnly`. A `ReadOnly` repository connects read-only and serves **restores only**. The operator refuses backup Jobs and skips maintenance projection. Use it to decommission a backend, or to migrate between repositories, without any risk of writes.

## `parameters` — mutable kopia repository parameters

`spec.parameters.epoch` tunes kopia's epoch manager, which decides how quickly index blobs become compactable. Reach for it when your repository sits at thousands of index blobs, showing `IndexBlobHealth=False` and `TooManyIndexBlobs`, *even though maintenance is running*. kopia cannot compact a blob until its epoch closes, and an epoch cannot close before `minDuration`, which is 24h by default, no matter how many blobs pile up.

```yaml
spec:
  parameters:
    epoch:
      minDuration: 6h # kopia's default is 24h
```

Every field is optional and kopiur has no defaults of its own. **Absent means "leave kopia's current value alone"**, so a repository that declares nothing here is untouched by this. The corollary is that *removing* a value does not restore kopia's default: it leaves the repository at whatever you last applied.

kopiur applies these only when they drift from what the repository reports, because the call invalidates other kopia clients' cached format blob. It mirrors what it observes into `status.parameters.epoch`, so a value that failed to apply is visible as a mismatch rather than as silence. This is not available on `mode: ReadOnly`, because kopia refuses repository-wide writes on a read-only connection.

Full field set and rationale: [Maintenance → when maintenance is running and the count still won't fall](maintenance.md#when-maintenance-is-running-and-the-count-still-wont-fall).

## `suspend` — pause a repository

`suspend: true` pauses connect, bootstrap and maintenance projection declaratively, without deleting the `Repository`. It is surfaced via a condition. `suspend` is consistent across `Repository`, `ClusterRepository`, `SnapshotPolicy` and `RepositoryReplication`.

## `server` — the kopia web UI

`spec.server` runs kopia's built-in HTML UI in a `Deployment` behind a `Service`, so you can browse snapshots and policies and do ad-hoc restores. Setting the block enables it; there is no `enabled` bool. It defaults to operator-minted credentials on an in-cluster `ClusterIP` Service.

/// warning | The UI has no read-only mode

Anyone who can reach the UI can read, create, and **delete** backups, and the server pod holds the repository decryption key. Treat exposing it like exposing the repository itself.

///

Full guide, covering auth modes, exposing the Service, the ReadWriteMany requirement for filesystem backends, and status, is in **[Web UI (kopia server)](server.md)**.

## Watching a repository

```console
$ kubectl get repository -n demo
NAME      PHASE   BACKEND   AGE
primary   Ready   S3        4m

$ kubectl describe repository primary -n demo # Conditions + Events explain Pending/Failed
```

The phases run `Pending` → `Initializing` → **`Ready`**, which is healthy. `Degraded` means the [circuit breaker](repository-health.md#backend-health-probe-default-on) is open: the backend stopped answering connects, work is paused, and kopiur keeps retrying with backoff. Recovery is automatic, and kstatus reads it as `Reconciling`, so `flux wait` waits rather than fails. `Failed` means connect or create failed terminally, and the actionable reason is on the conditions. `SnapshotPolicy`, `Snapshot`, `Restore` and `Maintenance` all wait for `Ready` before doing anything, so this is the first thing to check when a backup won't start.

## ClusterRepository: a shared repository

A `ClusterRepository` is the same backend and encryption surface, made cluster-scoped and shared. A platform team defines it once, and tenant namespaces reference it by name with `repository: { kind: ClusterRepository, name: … }` without seeing the backend or credentials. One extra field makes cross-namespace sharing safe: `allowedNamespaces`, below. `identityDefaults` isn't actually ClusterRepository-exclusive, since a namespaced `Repository` has it too. It's documented here because a shared repo is where you reach for it most, and for the same reason: many `SnapshotPolicy`s, or more than one cluster (see [`cluster`](#identitydefaultscluster--sharing-one-repository-across-clusters) below), writing to one repository.

### `allowedNamespaces` — who may use it (ClusterRepository only)

A tenancy gate, enforced on every consumer CR. It is externally tagged, so you set exactly one form:

```yaml
spec:
    allowedNamespaces: { list: ["billing", "media", "wiki"] } # explicit names
    # or:  allowedNamespaces: { selector: { matchLabels: { backups: "yes" } } }
    # or:  allowedNamespaces: { all: true } # any namespace
```

### `identityDefaults` — per-tenant identity (CEL)

kopia records every snapshot under `username@hostname:path`. For a shared repo you usually want each tenant's snapshots distinguishable. `identityDefaults` are **CEL expressions**, the `*Expr` fields, following the kromgo `valueExpr`/`colorExpr` convention. Their syntax, return type, and variable scope are **validated at admission**, so `kubectl apply` rejects a bad expression immediately. But the expression is actually **rendered against the live consumer on every reconcile**; it is not evaluated once and frozen. A consumer's explicit `spec.identity` always wins.

```yaml
spec:
    identityDefaults:
        hostnameExpr: "namespace"
        usernameExpr: "namespace + '-' + policyName"
```

For namespace `billing` and policy `postgres-data`, that resolves to `billing-postgres-data@billing:/pvc/…`.

The CEL **environment** is the consuming `SnapshotPolicy`'s metadata:

| Variable | Type | Is |
| --- | --- | --- |
| `namespace` | string | the SnapshotPolicy's namespace |
| `policyName` | string | the SnapshotPolicy's name |
| `labels` | map | `metadata.labels` |
| `annotations` | map | `metadata.annotations` |
| `cluster` | string | `identityDefaults.cluster` (below), or `""` when unset |

Each `*Expr` must return a **string**. Conditionals and map access come for free:

```yaml
identityDefaults:
    hostnameExpr: "'team' in labels ? labels['team'] : namespace"
    usernameExpr: "namespace + '-' + policyName + (labels['env'] == 'prod' ? '-prod' : '')"
```

/// note | How `*Expr` evaluation is bounded

Each `*Expr` is a CEL expression returning a **string**. CEL is sandboxed, with no I/O and no arbitrary code, and the expression is **validated at admission**. A syntax error, a wrong return type, or a reference to a variable outside the documented environment (`namespace`, `policyName`, `labels`, `annotations`, `cluster`) is rejected on `kubectl apply`, not discovered at backup time. Evaluation is bounded by CEL's cost budget, and each expression is capped at about 1 KiB.

///

/// warning | Editing `identityDefaults` strands the history behind the old identity

`identityDefaults`, and [`cluster`](#identitydefaultscluster--sharing-one-repository-across-clusters) below, are rendered into **every** consumer's kopia identity on every reconcile. Change one and the consumers that relied on it re-render: new snapshots land under the new `username@hostname:path`, and the existing history stays behind the old one. It is still restorable [by identity](restores.md#restoring-a-snapshot-kopiur-didnt-create), but it is no longer the same chain.

On a live cluster the webhook's identity-fork guard challenges that edit and makes you acknowledge it with the `allow-identity-change` annotation. **On a freshly-rebuilt cluster nothing can.** The guard only fires on updates, and after a disaster every `Repository` and `SnapshotPolicy` arrives as a CREATE, so a repository whose `identityDefaults` drifted from the pre-disaster manifests hands its consumers new identities with no complaint at all. That's why DR ends with a positive check rather than a warning to wait for. See [Scenario 10 → verification checklist](scenarios/dr-with-replicated-repository.md#verification-checklist).

///

### `identityDefaults.cluster` — sharing one repository across clusters

`cluster` is a separate setting from the two CEL expressions above. It is an RFC 1123 label, **at most 32 characters**, with **no dots**. A dot is the delimiter `identityDefaults.cluster` reserves to split a hostname back into its namespace and cluster parts on the read path, so an embedded dot is rejected outright at admission rather than risked. Set it once per cluster that shares this repository:

```yaml
spec:
    identityDefaults:
        cluster: east # this cluster's identity suffix
```

Setting it changes three things:

1. **The default hostname.** With no `hostnameExpr` and no consumer override, the default kopia identity hostname becomes `<namespace>.<cluster>` instead of bare `<namespace>`. So two clusters backing up same-named namespaces, such as `billing` in `east` and `billing` in `west`, write distinct identities and never collide or cross-prune each other's snapshots. See [Backups → identity](backups.md#identity--what-kopia-records-usernamehostnamepath).
2. **The CEL variable `cluster`.** It becomes available to `hostnameExpr` and `usernameExpr` above. For example `hostnameExpr: "namespace + '.' + cluster"` is the default spelled out explicitly, and a starting point for a custom scheme.
3. **Foreign-snapshot classification and the maintenance lease.** Once set, the catalog scan can tell "written by this cluster" from "written by another cluster sharing this repository" (`catalog.foreignSnapshots`, [above](#the-catalog--discovered-snapshots)), and the operator-managed `Maintenance`'s lease becomes cluster-qualified so two clusters never fight over who runs maintenance. See [Maintenance → Ownership and shared repositories](maintenance.md#ownership-and-shared-repositories).

/// warning | Setting or changing `cluster` on a repository with consumer history is an identity change

If any consumer `SnapshotPolicy` already has snapshot history, setting `identityDefaults.cluster` for the first time, or changing it, silently re-identifies every consumer that resolves the default hostname, meaning any with no `hostnameExpr` and no per-policy `identity` override. New snapshots land under `<namespace>.<cluster>` while the old history stays under bare `<namespace>`, and both lineages keep competing in the **same** GFS timeline. Kopiur's retention buckets a policy's `Snapshot` CRs per (source, repository), never by kopia identity, so pre-flip and post-flip CRs of one source share a bucket and the pre-flip ones keep aging out normally rather than being frozen or orphaned outright. The webhook **rejects** this edit fleet-wide, exactly like any other `identityDefaults` change (see [Backups → identity](backups.md#identity--what-kopia-records-usernamehostnamepath)). Acknowledge it with the `allow-identity-change` annotation once you've read the consequences. For turning on multi-cluster sharing on a repository that's already in use, follow [Share one repository across clusters](scenarios/shared-repository-multi-cluster.md), which walks the safe order of operations end to end.

The guard only covers **edits**. Rebuilding a cluster re-creates this `Repository`, so a `cluster` value that drifted from the pre-disaster manifests, or went missing from them, silently re-identifies every consumer with nothing to challenge it. Restore `cluster` to exactly its old value during DR and then [verify adoption](scenarios/dr-with-replicated-repository.md#verification-checklist).

///

### `credentialProjection.allowed` — the owner gate for shared creds

By default a `ClusterRepository` will **not** let its credential Secret be projected into a foreign consumer namespace, because `credentialProjection.allowed` defaults to `false`. Projection is fail-closed: it requires the repository owner's `allowed: true` **and** the consumer's `credentialProjection.enabled: true` **and** the operator's `secrets` RBAC. A namespaced `Repository` has no such gate, since its repo and Secret live in the same namespace.

```yaml
spec:
    credentialProjection:
        allowed: true # owner permits projection; consumers still opt in per-CR
```

See [Movers → credential projection](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos).

/// warning | Two requirements for ClusterRepository backups

1. Install the operator with **`installScope=cluster`**, or `ClusterRepository` is never reconciled. See [Installation → scope](install.md#install-scope).
2. Get the credential Secret into each **workload** namespace a mover runs in. The easy way is to set [`credentialProjection.enabled: true`](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) on the `SnapshotPolicy`, `Restore` or `Maintenance` that uses this repository, and Kopiur copies it for you. That is off by default and recommended for shared repos. Otherwise replicate the Secret yourself.

///

A complete, apply-ready example is [`deploy/examples/02-cluster-repository.yaml`](examples.md#example-02--shared-platform-repository).

## The values you'll actually change

| Field                                                  | What it does                                      |
| ------------------------------------------------------ | ------------------------------------------------- |
| `backend.<kind>.bucket` / `container` / `path` / `url` | The storage location.                             |
| `backend.<kind>.prefix`                                | Key prefix so several repos can share one bucket. |
| `backend.s3.endpoint` / `region`                       | Non-AWS endpoint and region.                      |
| `backend.<kind>.auth.secretRef.name`                   | The Secret holding the backend keys.              |
| `encryption.passwordSecretRef.{name,key}`              | Where the kopia password lives.                   |
| `create.enabled`                                       | Whether to initialize a new repository.           |
| `seed.from.{backend,repository}`                        | Initialize a brand-new repository from a surviving replica on its first bootstrap (disaster recovery). |
| `backend.s3.tls.disableTls`                            | Plain-HTTP endpoints (in-cluster MinIO/RustFS).   |
| `backend.s3.tls.caBundleRef`                           | Trust a private-CA HTTPS endpoint: a ConfigMap key with the CA PEM (`key` defaults to `ca.crt`), resolved in the `Repository`'s namespace (the operator's namespace for a `ClusterRepository`) and inlined into every mover. See [Private-CA HTTPS](backends/s3.md#private-ca-https-trusting-your-own-ca). |
| `allowedNamespaces` _(ClusterRepository)_              | Which namespaces may use the repo.                |
| `identityDefaults` _(ClusterRepository)_               | Per-tenant snapshot identity (CEL `*Expr`) and, for a repository shared across clusters, `cluster`. |
| `moverDefaults`                                        | Base security context / resources / cache for every mover. |
| `parameters.epoch.minDuration`                          | Lower it (e.g. `6h`) when index blobs stay in the thousands despite maintenance running. |
| `scheduleDefaults.timezone`                             | Cron timezone inherited by verification/replication/maintenance and `SnapshotSchedule` crons. |
| `scheduleDefaults.jitter`                               | Deterministic firing spread (e.g. `10m`, max 24h) inherited by the same crons; it de-synchronizes a whole repository's schedules in one line. |
| `concurrency.maxConcurrentJobs`                         | Ceiling on this repository's in-flight mover Jobs (backups + restores + replication source side). Absent/`0` = unlimited. |
| `moverDefaults.podLabels` / `podAnnotations`            | Extra labels/annotations on every mover pod: a Kueue queue name, a `NetworkPolicy` selector, a sidecar-injection opt-out. |
| `onNamespaceDelete`                                    | `Orphan` (default) / `Delete` on namespace delete.|
| `deletionProtection.threshold`                          | Mass-deletion breaker: HOLD external destructive Snapshot deletions at/above this count (default 10; `0` disables). |
| `mode`                                                 | `ReadWrite` (default) / `ReadOnly`.               |
| `suspend`                                              | Pause connect/bootstrap + maintenance.            |

## See also

- [How Kopia works](concepts/how-kopia-works.md): dedup, the identity model, and why one shared repository maximizes it.
- [Backend configuration](backends/index.md): per-backend setup cookbook (prereqs, Secret keys, apply-ready manifests).
- [Movers, RBAC & credentials](movers.md): where the credential Secret must live.
- [Maintenance](maintenance.md): the default-managed space reclamation per repo.
- [`deploy/examples/01-single-pvc-scheduled.yaml`](examples.md#example-01--single-pvc-scheduled): S3 `Repository`, end to end.
- [`deploy/examples/02-cluster-repository.yaml`](examples.md#example-02--shared-platform-repository): `ClusterRepository`.
- [Scenario 10, DR from a replicated repository](scenarios/dr-with-replicated-repository.md): `spec.seed` end to end.
