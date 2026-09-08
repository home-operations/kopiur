# Google Drive (native `gdrive`)

kopia has a **native Google Drive backend**, called `gdrive`. It talks to Drive directly with a Google **service-account** key, with no `rclone` in between. Kopiur exposes it as `backend.gdrive`.

/// warning | Experimental / not maintained upstream

kopia marks the `gdrive` provider **experimental and not actively maintained**.

Prefer a native object store when you have one: [S3](s3.md), [GCS](gcs.md), [Azure](azure.md), or [B2](b2.md). Reach for `gdrive` only when Google Drive is the storage you have.

///

/// warning | Not interchangeable with rclone-backed Drive

A native `gdrive` repository and an [rclone](rclone.md) Drive remote lay out data **differently**, and neither can read the other's.

Pick one per repository and stay on it. You cannot switch a repository between them, and a restore must use the same backend that wrote the snapshots.

///

## When to use native gdrive vs rclone

Both reach Google Drive. They differ in how they connect:

- **Native `gdrive`.** kopia talks to the Drive API directly with a service-account JSON. There are fewer moving parts at connect time, and no embedded `rclone serve` or WebDAV bridge.
- **[rclone](rclone.md) Drive remote.** kopia shells out to `rclone`, which can be slower to come up during `repository connect`. Use it if you already run rclone, or if you need OAuth-user authentication rather than a service account.

## Provider prerequisites

1. A **Google Cloud service account** with a JSON key, and the Drive API enabled on its project.
2. A **Drive folder** to hold the repository. Copy its **folder ID**, which is the last path segment of the folder URL `https://drive.google.com/drive/folders/<FOLDER_ID>`.
3. **Share the folder** with the service account's `client_email` as an **Editor**. Without that, the mover gets a permission error on connect.

## The Secret shape

gdrive is **file-delivered**. The mover reads the whole service-account JSON from a well-known environment key, writes it to a private file with mode `0600`, and runs kopia with `--credentials-file`.

| Secret key                 | Required | What it is                                                                       |
| -------------------------- | -------- | -------------------------------------------------------------------------------- |
| `KOPIA_GDRIVE_CREDENTIALS` | yes¹     | The **entire service-account key JSON**, exactly as issued. Written to a file, then passed as `--credentials-file`. |
| `KOPIA_PASSWORD`           | **yes**  | The repository encryption password.                                              |

¹ Optional in the schema. When it is absent, kopia falls back to ambient credentials, meaning `GOOGLE_APPLICATION_CREDENTIALS` or instance metadata. In a normal cluster you supply the Secret.

/// info | Why `KOPIA_GDRIVE_CREDENTIALS` and not `credentials.json`

The mover loads credentials with `envFrom`, and a dotted key like `credentials.json` is not a valid environment-variable name, so Kubernetes drops it. The JSON therefore goes under `KOPIA_GDRIVE_CREDENTIALS`. The mover writes it to a file for you and passes `--credentials-file`.

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/gdrive.yaml:repository"
```

## Fields reference (`backend.gdrive`)

| Field                  | Required | Default | Example                     | What it controls                                                                                      |
| ---------------------- | -------- | ------- | --------------------------- | ----------------------------------------------------------------------------------------------------- |
| `folderId`             | yes      | —       | `0AbCdEf...`                | The Drive folder ID that holds the repository. It is the last segment of the folder URL.               |
| `credentialsSecretRef` | no¹      | —       | `{ name: gdrive-repo-creds }` | Secret holding the service-account JSON under `KOPIA_GDRIVE_CREDENTIALS`. A `ClusterRepository` adds `namespace:`. |

¹ Optional in the schema. In practice it is required, unless the mover already has ambient Google credentials.

## Customization — the values you actually change

- **`folderId`** selects which Drive folder backs the repository. Share that folder with the service account.
- **`KOPIA_GDRIVE_CREDENTIALS`** holds the service-account JSON. Rotate it by re-pasting.
- **`create.enabled`** defaults to `true`, because connect-or-create is idempotent. Set it to `false` only for a read-only or externally-managed repository.

## As a `ClusterRepository`

The same `backend.gdrive` stanza works on a cluster-scoped [`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository), with two requirements. The `credentialsSecretRef`, and the `encryption.passwordSecretRef`, must carry an explicit `namespace:`. And the Secret must exist in the namespaces the movers run in. See [Movers](../movers.md).

## Troubleshooting

- **`403` or permission denied on connect.** Either the Drive folder isn't shared with the service account's `client_email`, which needs **Editor**, or the Drive API isn't enabled on the service account's project.
- **`folderId` not found.** Copy the ID from the folder URL, not the folder name.
- **Slow or contradictory results compared to an old rclone Drive repository.** They are different repositories. A native `gdrive` repository can't read rclone-written data.

## See also

- [rclone](rclone.md): the other way to reach Google Drive, using OAuth-user authentication.
- [Repositories & backends](../repositories.md): the concepts, meaning scope, encryption, and creation.
- [Movers, RBAC & credentials](../movers.md): where the credentials Secret must live.
