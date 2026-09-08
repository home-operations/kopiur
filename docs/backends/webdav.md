# WebDAV

The WebDAV backend stores the kopia repository at a **WebDAV collection URL**, using HTTP basic authentication. That covers Nextcloud, Apache `mod_dav`, and any other WebDAV server.

Reach for WebDAV when your target speaks WebDAV but not S3. For Nextcloud you can also reach it as an [rclone](rclone.md) remote, but native WebDAV is simpler.

## Provider prerequisites

- A WebDAV **collection URL** for the repository, such as `https://dav.example.com/kopia`, or a Nextcloud `/remote.php/dav/files/<user>/...` path.
- **Basic-auth** credentials with write access to that collection.

### Finding your collection URL

The URL must point at a **collection**, meaning a folder, that already exists and is writable. The shape of the path depends on the server:

| Server               | Collection URL shape                                              | Notes                                                                |
| -------------------- | ----------------------------------------------------------------- | -------------------------------------------------------------------- |
| **Nextcloud**        | `https://cloud.example.com/remote.php/dav/files/<username>/kopia` | `<username>` is the login name, and it appears **in the path**.      |
| **Apache `mod_dav`** | `https://dav.example.com/kopia`                                   | Whatever `<Location>` the DAV block serves, plus your folder.        |
| **Caddy (webdav)**   | `https://dav.example.com/kopia`                                   | The route prefix configured for the webdav handler, plus the folder. |

/// example | Nextcloud: app password + the exact URL

1. Create the target folder (`kopia` here) in the Files app.
2. Go to _Personal settings → Security → Devices & sessions_ and **create a new app password**. Use it as `KOPIA_WEBDAV_PASSWORD`. With two-factor enabled, the account password will not work for WebDAV at all, and an app password can be revoked on its own either way.
3. The URL is the user-specific DAV path. It is **not** the share link and **not** the bare hostname. For login `alice` it is `https://cloud.example.com/remote.php/dav/files/alice/kopia`. The deprecated `/remote.php/webdav/…` form still works, but the `dav/files/<user>` form is the current one.

///

## The Secret shape

The mover loads this Secret with `envFrom`, so the keys reach kopia as environment variables.

| Secret key              | Required | What it is                                                          |
| ----------------------- | -------- | ------------------------------------------------------------------- |
| `KOPIA_WEBDAV_USERNAME` | yes      | Basic-auth username.                                                |
| `KOPIA_WEBDAV_PASSWORD` | yes      | Basic-auth password (an app password where the server supports it). |
| `KOPIA_PASSWORD`        | **yes**  | The repository encryption password.                                 |

```yaml
stringData:
    KOPIA_WEBDAV_USERNAME: "REPLACE_ME"
    KOPIA_WEBDAV_PASSWORD: "REPLACE_ME"
    KOPIA_PASSWORD: "choose-something-long-and-random"
```

/// warning | Lose the password, lose the backups

`KOPIA_PASSWORD` encrypts the repository, and it cannot be recovered if you lose it. It is **separate** from the WebDAV login password. Store it outside the cluster and back up the Secret. See [Encryption](../repositories.md#encryption-and-repository-creation).

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/webdav.yaml:repository"
```

## Fields reference (`backend.webDav`)

Note the spec key is `webDav` (camelCase, capital D).

| Field            | Required | Default | Example                                                      | What it controls                                                                                                 |
| ---------------- | -------- | ------- | ------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `url`            | yes      | —       | `https://cloud.example.com/remote.php/dav/files/alice/kopia` | The WebDAV collection URL holding the repository: the full scheme plus the path to an **existing, writable** folder. |
| `auth.secretRef` | no       | —       | `{ name: webdav-repo-creds }`                                | Names the basic-auth Secret above. Same namespace as the `Repository`; a `ClusterRepository` adds `namespace:`.   |

WebDAV has no cloud IAM to federate with, so its `auth` is **Secret-only**. There is no `workloadIdentity` here. A stray `auth.workloadIdentity` is not rejected; the API server silently **prunes** it, because it isn't in the schema.

## Customization — the values you actually change

- **`url`** is the collection URL. Include the full path to the repository folder.
- **`create.enabled`** initializes the repository if it's missing.
- **`moverDefaults.cache`** sizes the mover cache. See [movers](../movers.md).

## As a `ClusterRepository`

The same `backend.webDav` stanza works on a cluster-scoped [`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository), with two requirements. Every Secret reference must carry an explicit `namespace:`, and the Secret must exist in the namespaces the movers run in. See [Movers](../movers.md).

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny smoke-test: a throwaway PVC, a `SnapshotPolicy`, and a `Snapshot`, all pointed at the `webdav-primary` repository above. It takes you from "applied" to "a snapshot in my collection" in one go.

/// warning | Fill in the credentials first

The smoke backup only goes green once `KOPIA_WEBDAV_USERNAME` and `KOPIA_WEBDAV_PASSWORD` hold **real** basic-auth credentials, and `url` points at an existing, writable collection. With the `REPLACE_ME` placeholders the `Repository` stalls at `Failed` and the `Snapshot` stays `Pending`.

///

**1. Apply the bundle.** That is the `backups` namespace, the Secret, the Repository, and the smoke-test objects:

```console
$ kubectl apply -f deploy/examples/backends/webdav.yaml
```

**2. Wait for the repository to be `Ready`.** Everything else waits on this:

```console
$ kubectl -n backups wait --for=condition=Ready repository/webdav-primary --timeout=2m
repository.kopiur.home-operations.com/webdav-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it rather than apply it. The namespace, Secret, Repository, PVC, and policy already exist and report unchanged. The `Snapshot` is the one new object:

```console
$ kubectl create -f deploy/examples/backends/webdav.yaml
snapshot.kopiur.home-operations.com/smoke-now-abc12 created
```

**4. Watch it succeed:**

```console
$ kubectl -n backups get snapshots -w
NAME              PHASE       ORIGIN   SNAPSHOT     AGE
smoke-now-abc12   Pending     manual                2s
smoke-now-abc12   Running     manual                7s
smoke-now-abc12   Succeeded   manual   k1f1ec0a8    38s
```

The output above is illustrative. The `Snapshot` has no fixed `Succeeded` *condition*, so to wait on it in a script, key on the phase:

```console
$ kubectl -n backups wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/smoke-now-abc12 --timeout=5m
```

**5. Prove the data really moved.** `status.stats` shows non-zero `bytesNew` and `filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia snapshot ID in your collection:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

Both outputs are illustrative; sizes and the ID vary. Non-zero `bytesNew` proves the backup uploaded real content over WebDAV.

**6. Clean up** the smoke-test when you're done:

```console
$ kubectl -n backups delete snapshot --all       # finalizer also deletes the kopia snapshot
$ kubectl -n backups delete snapshotpolicy smoke
$ kubectl -n backups delete pvc smoke-data
```

/// warning | Deleting a Snapshot deletes its snapshot

A produced `Snapshot` defaults to `deletionPolicy: Delete`, so removing the CR runs `kopia snapshot delete` through a finalizer. Use `Retain` or `Orphan` to keep the data. See [Backups → deletionPolicy](../backups.md#deletionpolicy--what-happens-to-the-snapshot).

///

From here the rest of the lifecycle is the same on every backend. Only the `Repository` differs. Put it on a cron with a `SnapshotSchedule`, described in [Backups & schedules](../backups.md) and [Example 01](../examples.md#example-01--single-pvc-scheduled). Restore by picking a `Snapshot`, described in [Restores](../restores.md) and [Example 03](../examples.md#example-03--restore-by-picking-a-snapshot).

## Troubleshooting

- **`401 Unauthorized`.** Either the basic-auth username and password are wrong, or the server requires an **app password** rather than the account password. Nextcloud with two-factor does.
- **`404` or `409`.** The collection URL doesn't exist, or it isn't a writable collection. Create the folder and point `url` at it. On Nextcloud, also check that the username **in the path** matches the login the credentials belong to.
- **TLS errors.** The WebDAV backend speaks HTTPS, so use a valid certificate. The `tls` overrides available on S3 are not part of `backend.webDav`. For an internal server with a private CA, terminate with a publicly-trusted certificate or use a different backend.
- **Slow backups.** WebDAV is the chattiest backend: one HTTP request per blob operation, and no multipart uploads. That is fine for modest datasets. For large or high-churn sources, prefer an object store.

## See also

- [Repositories & backends](../repositories.md): the concepts, meaning scope, encryption, and creation.
- [Movers, RBAC & credentials](../movers.md): where the credential Secret must live.
- Sibling backends: [S3](s3.md) · [rclone](rclone.md), which also reaches Nextcloud and many others.
