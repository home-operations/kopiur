# rclone (everything else)

kopia shells out to [`rclone`](https://rclone.org), so the rclone backend reaches **any rclone-supported provider**: Google Drive, OneDrive, Dropbox, pCloud, Box, Mega, and dozens more. This is the escape hatch for providers without a native kopia backend.

If a native backend exists for your provider, prefer it. It is simpler and has fewer moving parts. Those are [S3](s3.md), [Azure](azure.md), [GCS](gcs.md), [B2](b2.md), and [WebDAV](webdav.md). Use rclone for everything else.

## Provider prerequisites

- A working **`rclone.conf`** that defines the remote you'll reference. Build and test it on your own machine first:

    ```console
    $ rclone config            # interactively create a remote, e.g. named "mydrive"
    $ rclone ls mydrive:       # confirm it works before putting it in a Secret
    ```

- The **remote name** in your `remotePath`, which is `mydrive:` below, must match a section header in that `rclone.conf`, which is `[mydrive]`.

/// example | Worked example: Google Drive, end to end

```console
# 1. On your workstation (needs a browser for the OAuth dance):
$ rclone config
    n) New remote
    name> mydrive
    Storage> drive
    scope> drive          # full access; kopia needs read+write+delete
    # accept defaults, complete the browser sign-in

# 2. Confirm the remote works and the target folder path:
$ rclone mkdir mydrive:backups/kopia
$ rclone ls mydrive:backups/kopia

# 3. Print the config. THIS goes under KOPIA_RCLONE_CONFIG, exactly as printed:
$ rclone config show mydrive
[mydrive]
type = drive
scope = drive
token = {"access_token":"ya29...","token_type":"Bearer","refresh_token":"1//0g...","expiry":"..."}
```

Then set `remotePath: mydrive:backups/kopia` in the `Repository`.

The short-lived `access_token` in the pasted config will have expired by the time a mover runs. That's fine: rclone refreshes it from the long-lived `refresh_token` on every run.

You only need to re-paste the config if the refresh token itself is revoked, by a password reset or a removed OAuth grant, or rotated, which some providers do. If backups start failing with auth errors, re-run `rclone config reconnect mydrive:` on your own machine and re-paste.

For heavy use, rclone's docs recommend creating your **own Google API `client_id`**, because the shared default is rate-limited across all rclone users worldwide.

OneDrive, Dropbox, Box, and the rest follow the same recipe: `rclone config`, verify with `rclone ls`, then paste the `rclone config show` output into the Secret.

///

## The Secret shape

rclone is one of the three **file-delivered** backends. The mover reads the whole config from a well-known environment key, writes it to a private file with mode `0600`, and runs rclone with `--config`.

| Secret key            | Required | What it is                                                                          |
| --------------------- | -------- | ----------------------------------------------------------------------------------- |
| `KOPIA_RCLONE_CONFIG` | yes      | The **entire `rclone.conf`**, exactly as printed. Written to a file, then passed to rclone as `--config`. |
| `KOPIA_PASSWORD`      | **yes**  | The repository encryption password.                                                 |

```yaml
stringData:
    KOPIA_RCLONE_CONFIG: |
        [mydrive]
        type = drive
        scope = drive
        token = {"access_token":"REPLACE_ME", ...}
    KOPIA_PASSWORD: "choose-something-long-and-random"
```

/// info | Why KOPIA_RCLONE_CONFIG and not rclone.conf

The mover loads credentials with `envFrom`, and a dotted key like `rclone.conf` is not a valid environment-variable name, so Kubernetes drops it. The config therefore goes under `KOPIA_RCLONE_CONFIG`, and the mover writes it out to `rclone.conf` for you.

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/rclone.yaml:repository"
```

/// warning | rclone uses `configSecretRef`, not `auth`

Unlike the object-store backends, rclone references its config through `backend.rclone.configSecretRef`. There is **no** `auth` block here. The remote name in `remotePath` must match a section in the `rclone.conf`.

///

## Fields reference (`backend.rclone`)

| Field             | Required | Default | Example                   | What it controls                                                                                                          |
| ----------------- | -------- | ------- | ------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `remotePath`      | yes      | —       | `mydrive:backups/kopia`   | rclone path in `remote:path` form. The part before `:` must match a `[section]` in the config. The part after it is the folder. |
| `configSecretRef` | no¹      | —       | `{ name: rclone-config }` | Secret holding the `rclone.conf`, under the key `KOPIA_RCLONE_CONFIG`. This is **not** `auth`. A `ClusterRepository` adds `namespace:`. |
| `startupTimeout`  | no       | `15s`   | `2m`                      | How long kopia waits for its embedded `rclone serve` to come up before failing the connect. It takes a Go duration. Raise it for slow remotes whose metadata and indexes load through the rclone-to-WebDAV bridge. |

¹ Optional in the schema, but required in practice. rclone can't reach a remote without its config.

/// tip | Bootstrap timing out on a slow remote?

A valid rclone repository can still be slow to connect, because kopia starts `rclone serve webdav` and loads repository metadata through it. That can trip the bootstrap Job's deadline before the connect finishes.

Two separate knobs help. **`backend.rclone.startupTimeout`** is kopia's wait for `rclone serve`. **`spec.bootstrap.failurePolicy.activeDeadlineSeconds`** is the bootstrap Job's wall-clock cap, defaulting to 120s. Raise either or both. See [Repositories](../repositories.md).

///

## Customization — the values you actually change

- **`remotePath`** is the `remote:path`. The remote name must exist in the config.
- **`KOPIA_RCLONE_CONFIG`** holds the config contents. Re-paste it when tokens rotate.
- **`create.enabled`** initializes the repository if it's missing.

## As a `ClusterRepository`

The same `backend.rclone` stanza works on a cluster-scoped [`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository), with two requirements. The `configSecretRef`, and the `encryption.passwordSecretRef`, must carry an explicit `namespace:`. And the Secret must exist in the namespaces the movers run in. See [Movers](../movers.md).

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny smoke-test: a throwaway PVC, a `SnapshotPolicy`, and a `Snapshot`, all pointed at the `rclone-primary` repository above. It takes you from "applied" to "a snapshot on my remote" in one go.

/// warning | Fill in the config first

The smoke backup only goes green once `KOPIA_RCLONE_CONFIG` holds a **real**, working `rclone.conf`. Validate it on your own machine first with `rclone ls <remote>:`. With the `REPLACE_ME` token the `Repository` stalls at `Failed`, because rclone can't reach the remote, and the `Snapshot` stays `Pending`.

///

**1. Apply the bundle.** That is the `backups` namespace, the Secret, the Repository, and the smoke-test objects:

```console
$ kubectl apply -f deploy/examples/backends/rclone.yaml
```

**2. Wait for the repository to be `Ready`.** Everything else waits on this:

```console
$ kubectl -n backups wait --for=condition=Ready repository/rclone-primary --timeout=2m
repository.kopiur.home-operations.com/rclone-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it rather than apply it. The namespace, Secret, Repository, PVC, and policy already exist and report unchanged. The `Snapshot` is the one new object:

```console
$ kubectl create -f deploy/examples/backends/rclone.yaml
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

**5. Prove the data really moved.** `status.stats` shows non-zero `bytesNew` and `filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia snapshot ID on your remote:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

Both outputs are illustrative; sizes and the ID vary. Non-zero `bytesNew` proves the backup uploaded real content through rclone.

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

/// warning | Remote name must match

`directory not found` and `didn't find section in config file` almost always mean the remote name in `remotePath` doesn't match a `[section]` in your `rclone.conf`. Keep the two identical.

///

- **Auth or token errors.** Tokens in `rclone.conf` expire or get revoked. Re-run `rclone config reconnect <remote>:` on your own machine and re-paste the config.
- **`configSecretRef` ignored.** Make sure you used `configSecretRef`, not `auth`. rclone has no `auth` block.
- Always validate with `rclone ls <remote>:` on your own machine before applying.

## See also

- [Repositories & backends](../repositories.md): the concepts, meaning scope, encryption, and creation.
- [Movers, RBAC & credentials](../movers.md): where the config Secret must live.
- Native backends, which you should prefer where one exists: [S3](s3.md) · [Azure](azure.md) · [GCS](gcs.md) · [B2](b2.md) · [WebDAV](webdav.md).
