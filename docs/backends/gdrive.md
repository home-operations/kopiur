# Google Drive (native `gdrive`)

kopia has a **native Google Drive backend** (`gdrive`) that talks to Drive
directly with a Google **service-account** key — no `rclone` in the path. Kopiur
exposes it as `backend.gdrive`.

/// warning | Experimental / not maintained upstream

kopia marks the `gdrive` provider **experimental and not actively maintained**.
Prefer a native object store ([S3](s3.md), [GCS](gcs.md), [Azure](azure.md),
[B2](b2.md)) when you have one. Reach for `gdrive` only when Google Drive is the
storage you have.

///

/// warning | Not interchangeable with rclone-backed Drive

A native `gdrive` repository and an [rclone](rclone.md) Drive remote lay out data
**differently** and cannot read each other. Pick one per repository and stay on
it — you can't switch a repository between them, and a restore must use the same
backend that wrote the snapshots.

///

## When to use native gdrive vs rclone

Both reach Google Drive. They differ in the connection path:

- **Native `gdrive`** — kopia talks to the Drive API directly with a
  service-account JSON. Fewer moving parts at connect time; no embedded `rclone
  serve`/WebDAV bridge.
- **[rclone](rclone.md) Drive remote** — kopia shells out to `rclone`, which can
  be slower to come up during `repository connect`. Use it if you already run
  rclone or need OAuth-user (not service-account) auth.

## Provider prerequisites

1. A **Google Cloud service account** with a JSON key, and the Drive API enabled
   on its project.
2. A **Drive folder** to hold the repository. Copy its **folder ID** — the last
   path segment of the folder URL (`https://drive.google.com/drive/folders/<FOLDER_ID>`).
3. **Share the folder** with the service account's `client_email` as an
   **Editor**, or the mover gets a permission error on connect.

## The Secret shape

gdrive is **file-delivered**: the mover reads the whole service-account JSON from
a well-known env key, writes it to a private (`0600`) file, and runs kopia with
`--credentials-file`.

| Secret key                 | Required | What it is                                                                       |
| -------------------------- | -------- | -------------------------------------------------------------------------------- |
| `KOPIA_GDRIVE_CREDENTIALS` | yes¹     | The **entire service-account key JSON**, verbatim. Materialized → `--credentials-file`. |
| `KOPIA_PASSWORD`           | **yes**  | The repository encryption password.                                              |

¹ Optional in the schema — when absent, kopia falls back to ambient credentials
(`GOOGLE_APPLICATION_CREDENTIALS` / instance metadata). In a normal cluster you
supply the Secret.

/// info | Why `KOPIA_GDRIVE_CREDENTIALS` and not `credentials.json`

The mover loads credentials with `envFrom`, and a dotted key like
`credentials.json` is not a valid environment-variable name — Kubernetes drops
it. So the JSON goes under `KOPIA_GDRIVE_CREDENTIALS`; the mover writes it to a
file for you and passes `--credentials-file`.

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/gdrive.yaml:repository"
```

## Fields reference (`backend.gdrive`)

| Field                  | Required | Default | Example                     | What it controls                                                                                      |
| ---------------------- | -------- | ------- | --------------------------- | ----------------------------------------------------------------------------------------------------- |
| `folderId`             | yes      | —       | `0AbCdEf...`                | The Drive folder ID that holds the repository (last segment of the folder URL).                       |
| `credentialsSecretRef` | no¹      | —       | `{ name: gdrive-repo-creds }` | Secret holding the service-account JSON under `KOPIA_GDRIVE_CREDENTIALS`. A `ClusterRepository` adds `namespace:`. |

¹ Optional in the schema; in practice required unless the mover already has
ambient Google credentials.

## Customization — the values you actually change

- **`folderId`** — which Drive folder backs the repository. Share it with the SA.
- **`KOPIA_GDRIVE_CREDENTIALS`** — the service-account JSON; rotate by re-pasting.
- **`create.enabled`** — defaults to `true` (connect-or-create is idempotent);
  set `false` only for a read-only / externally-managed repository.

## As a `ClusterRepository`

The same `backend.gdrive` stanza works on a cluster-scoped
[`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository); the
`credentialsSecretRef` (and the `encryption.passwordSecretRef`) must carry an
explicit `namespace:`, and the Secret must exist where the movers run — see
[Movers](../movers.md).

## Troubleshooting

- **`403` / permission denied on connect** — the Drive folder isn't shared with
  the service account's `client_email` (needs **Editor**), or the Drive API isn't
  enabled on the SA's project.
- **`folderId` not found** — copy the ID from the folder URL, not the folder name.
- **Slow or contradictory results vs an old rclone Drive repo** — they're
  different repositories; a native `gdrive` repo can't read rclone-written data.

## See also

- [rclone](rclone.md) — the other way to reach Google Drive (OAuth-user auth).
- [Repositories & backends](../repositories.md) — concepts: scope, encryption, creation.
- [Movers, RBAC & credentials](../movers.md) — where the credentials Secret must live.
