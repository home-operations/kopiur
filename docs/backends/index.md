# Backend configuration

This is the **index** for Kopiur's storage backends. Each backend has its own page with the provider prerequisites, the exact Secret shape, a field-by-field reference, the knobs you can change, and troubleshooting for that backend.

Start here for the rules that apply to every backend: the mental model and the credential-key conventions. Then jump to your backend below. If you want the _concepts_ behind a repository, meaning the namespaced-versus-cluster split, encryption, and repository creation, read [Repositories & backends](../repositories.md) first.

/// tip | The mental model (read this once)

A **`Repository`** is _where_ snapshots live. It splits cleanly into two halves:

- **Non-secret connection identifiers.** Bucket, container, endpoint, host, path. These go in `spec.backend.<kind>`.
- **Secrets.** Backend access keys and the kopia encryption password. These live in a Kubernetes `Secret`, are read by **well-known keys**, and are passed to kopia as environment variables. They never appear on the command line and never in status.

`SnapshotPolicy`, `Snapshot`, and `Restore` never repeat any of this. They point at the `Repository` by name.

///

/// warning | Externally tagged, so there is no `kind:` field

You choose a backend by **which key you set**, such as `backend.s3` or `backend.azure`. There is no `kind:` discriminator, so `backend: { kind: S3 }` will **not** admit. Exactly one backend key is allowed. For the reasoning, see [API conventions](../dev/api-conventions.md).

///

## Pick your backend

| Backend                                          | Covers                                                       | Spec key             |
| ------------------------------------------------ | ------------------------------------------------------------ | -------------------- |
| [S3 & S3-compatible](s3.md)             | Amazon S3, MinIO, RustFS, Ceph RGW, Wasabi, Cloudflare R2, … | `backend.s3`         |
| [Azure Blob Storage](azure.md)          | Azure Blob containers (key or SAS token)                     | `backend.azure`      |
| [Google Cloud Storage](gcs.md)          | GCS buckets (service-account key)                            | `backend.gcs`        |
| [Backblaze B2](b2.md)                   | Backblaze B2 native API                                      | `backend.b2`         |
| [Filesystem (PVC / NFS)](filesystem.md) | A NAS/PVC or inline NFS export mounted into the mover        | `backend.filesystem` |
| [SFTP](sftp.md)                         | Any server reachable over SSH/SFTP                           | `backend.sftp`       |
| [WebDAV](webdav.md)                     | Nextcloud, Apache `mod_dav`, …                               | `backend.webDav`     |
| [rclone](rclone.md)                     | Google Drive, OneDrive, Dropbox, and dozens more             | `backend.rclone`     |
| [Google Drive (native)](gdrive.md)      | Google Drive via kopia's native `gdrive` (experimental)     | `backend.gdrive`     |

## How to use these pages

1. Find your backend above.
2. **Provider prerequisites.** Create the bucket, container, or share, and a scoped credential on the provider side.
3. Copy the manifest on the page, fill in every `REPLACE_ME`, set a long random `KOPIA_PASSWORD`, and `kubectl apply -f`.
4. Watch it become `Ready`. See [Watching a repository](../repositories.md#watching-a-repository).

/// warning | Lose the password, lose the backups

`KOPIA_PASSWORD` encrypts the repository. kopia cannot decrypt without it, and there is no recovery.

Use a long random value, store it **outside** the cluster in a password manager or external secret store, and back up the Secret itself.

The three create-time tunables (`encryption`, `splitter`, `hash`) are fixed forever at creation. See [Encryption and repository creation](../repositories.md#encryption-and-repository-creation).

///

## Credential keys at a glance

The mover reads these **exact** key names from the Secret you reference and feeds them to kopia. `KOPIA_PASSWORD` is required for **every** backend.

| Backend                                 | Secret keys (besides `KOPIA_PASSWORD`)                                    | Spec key             |
| --------------------------------------- | ------------------------------------------------------------------------- | -------------------- |
| [S3 / S3-compatible](s3.md)    | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, _(opt)_ `AWS_SESSION_TOKEN` | `backend.s3`         |
| [Azure](azure.md)              | `AZURE_STORAGE_KEY` **or** `AZURE_STORAGE_SAS_TOKEN`                      | `backend.azure`      |
| [Google Cloud Storage](gcs.md) | `KOPIA_GCS_CREDENTIALS` (the SA-key JSON)                                 | `backend.gcs`        |
| [Backblaze B2](b2.md)          | `B2_KEY_ID`, `B2_KEY`                                                     | `backend.b2`         |
| [Filesystem](filesystem.md)    | _(none: only `KOPIA_PASSWORD`)_                                          | `backend.filesystem` |
| [SFTP](sftp.md)                | `KOPIA_SFTP_KEY_DATA`, `KOPIA_SFTP_KNOWN_HOSTS`                           | `backend.sftp`       |
| [WebDAV](webdav.md)            | `KOPIA_WEBDAV_USERNAME`, `KOPIA_WEBDAV_PASSWORD`                          | `backend.webDav`     |
| [rclone](rclone.md)            | `KOPIA_RCLONE_CONFIG` _(via `configSecretRef`)_                           | `backend.rclone`     |
| [Google Drive](gdrive.md)      | `KOPIA_GDRIVE_CREDENTIALS` _(via `credentialsSecretRef`)_                 | `backend.gdrive`     |

/// tip | Cloud clusters: workload identity instead of static keys

On EKS, AKS, or GKE the three cloud backends can skip the static keys entirely. Set `auth.workloadIdentity.serviceAccountName` and the mover Jobs run as a user-supplied, IAM-federated ServiceAccount. Then only `KOPIA_PASSWORD` remains in the cluster.

See [S3 / IRSA](s3.md#workload-identity-irsa--eks-pod-identity), [Azure / AKS](azure.md#workload-identity-aks), and [GCS / GKE](gcs.md#workload-identity-gke).

The non-cloud backends (B2, SFTP, WebDAV) are Secret-only. They have no IAM plane to federate with.

///

/// info | Env-delivered vs. file-delivered credentials

Most backends authenticate with environment variables kopia reads directly. The mover loads the Secret with `envFrom`, so the keys above become environment variables.

Four backends need their credentials as **files** instead. kopia's SFTP, GCS, rclone, and gdrive flags have no environment-variable form, and a Secret key like `ssh-privatekey` isn't a valid environment-variable name, so `envFrom` would silently drop it. For those, the mover reads a well-known env key, writes it to a private file with mode `0600`, then points kopia at the path:

| Secret key                 | Becomes                       | kopia flag           |
| -------------------------- | ----------------------------- | -------------------- |
| `KOPIA_SFTP_KEY_DATA`      | the SSH private key file      | `--keyfile`          |
| `KOPIA_SFTP_KNOWN_HOSTS`   | the `known_hosts` file        | `--known-hosts`      |
| `KOPIA_GCS_CREDENTIALS`    | the service-account JSON file | `--credentials-file` |
| `KOPIA_RCLONE_CONFIG`      | the `rclone.conf` file        | rclone `--config`    |
| `KOPIA_GDRIVE_CREDENTIALS` | the service-account JSON file | `--credentials-file` |

You don't manage the files. Just put the value under the right key. The secret never lands on kopia's command line.

///

/// note | ClusterRepository: where Secret refs resolve

The per-backend pages use a namespaced `Repository`. The same backend stanzas apply to a cluster-scoped `ClusterRepository`, with one wrinkle: it has no namespace of its own, so "absent means the referrer's namespace" has nothing to resolve to.

- **When the operator itself reads the Secret**, to connect, to bootstrap, or to run the repository server, a Secret reference with no `namespace:` resolves to the **operator's namespace**. That is `KOPIUR_NAMESPACE`, which the Helm chart sets for you. Setting `namespace:` explicitly always wins. This is what makes a `ClusterRepository` with no namespaces on its refs reach `Ready` at all.
- **When a workload mover reads it**, meaning a `Snapshot`, `Restore`, or `Maintenance` Job, the Secret must exist in **that workload's namespace**, because `envFrom` is namespace-local. Either replicate it there yourself, or turn on [credential projection](../movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) and let Kopiur copy it in. Projection is the recommendation for shared repositories.

/// warning | Set `namespace:` explicitly if anything other than the operator reads the Secret

The operator-namespace default above applies **only** to the repository's own connect, bootstrap, and server.

The mover side does **not** default it. Credential projection needs an explicit `namespace:` on the reference to know what to copy, and without projection a mover looks for a same-named Secret in its own namespace. So a `ClusterRepository` with no `namespace:` on its refs will reach `Ready` and then fail its `Snapshot` and `Restore` Jobs with a `MissingDependency` naming the Secret.

**If you run backups, which is almost always, pin the namespace:**

```yaml
encryption:
  passwordSecretRef:
    name: kopiur-repository-secret
    namespace: kopiur-system # where the Secret actually lives
    key: KOPIA_PASSWORD
```

///

A worked S3 example is on the [S3 page](s3.md#as-a-clusterrepository).

///

## See also

- [Repositories & backends](../repositories.md): the concepts, meaning scope, encryption, creation, and `ClusterRepository`.
- [Permissions, UID & GID](../permissions.md): the filesystem and SFTP ownership story, and the mover UID knob.
- [Movers, RBAC & credentials](../movers.md): where the credential Secret must live, and why.
