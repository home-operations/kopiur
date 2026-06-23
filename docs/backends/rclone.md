# rclone (everything else)

kopia shells out to [`rclone`](https://rclone.org), so the rclone backend reaches
**any rclone-supported provider** — Google Drive, OneDrive, Dropbox, pCloud, Box,
Mega, and dozens more. This is the escape hatch for providers without a native
kopia backend.

If a native backend exists for your provider ([S3](s3.md), [Azure](azure.md),
[GCS](gcs.md), [B2](b2.md), [WebDAV](webdav.md)), prefer it — it's simpler and has
fewer moving parts. Use rclone for everything else.

## Provider prerequisites

- A working **`rclone.conf`** that defines the remote you'll reference. Build and
  test it locally first:

    ```console
    $ rclone config            # interactively create a remote, e.g. named "mydrive"
    $ rclone ls mydrive:       # confirm it works before putting it in a Secret
    ```

- The **remote name** in your `remotePath` (`mydrive:` below) must match a section
  header (`[mydrive]`) in that `rclone.conf`.

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

# 3. Print the config — THIS goes under KOPIA_RCLONE_CONFIG, verbatim:
$ rclone config show mydrive
[mydrive]
type = drive
scope = drive
token = {"access_token":"ya29...","token_type":"Bearer","refresh_token":"1//0g...","expiry":"..."}
```

Then set `remotePath: mydrive:backups/kopia` in the `Repository`. The
short-lived `access_token` in the pasted config will be expired by the time a
mover runs — that's fine; rclone refreshes it from the long-lived
`refresh_token` on every run. You only need to re-paste the config if the
refresh token itself is revoked (password reset, OAuth grant removed) — or
rotated, which some providers do; if backups start failing with auth errors,
re-run `rclone config reconnect mydrive:` locally and re-paste.

For heavy use, rclone's docs recommend creating your **own Google API
`client_id`** — the shared default is rate-limited across all rclone users
worldwide. OneDrive, Dropbox, Box, etc. follow the same recipe: `rclone config`,
verify with `rclone ls`, paste `rclone config show` output into the Secret.

///

## The Secret shape

rclone is one of the three **file-delivered** backends. The mover reads the whole
config from a well-known env key, writes it to a private (`0600`) file, and runs
rclone with `--config`.

| Secret key            | Required | What it is                                                                          |
| --------------------- | -------- | ----------------------------------------------------------------------------------- |
| `KOPIA_RCLONE_CONFIG` | yes      | The **entire `rclone.conf`**, verbatim. Materialized to a file → rclone `--config`. |
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

The mover loads credentials with `envFrom`, and a dotted key like `rclone.conf` is
not a valid environment-variable name — Kubernetes drops it. So the config goes
under `KOPIA_RCLONE_CONFIG`; the mover writes it to `rclone.conf` for you.

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/rclone.yaml:repository"
```

/// warning | rclone uses `configSecretRef`, not `auth`

Unlike the object-store backends, rclone references its config via
`backend.rclone.configSecretRef` — there is **no** `auth` block. The remote name in
`remotePath` must match a section in the `rclone.conf`.

///

## Fields reference (`backend.rclone`)

| Field             | Required | Default | Example                   | What it controls                                                                                                          |
| ----------------- | -------- | ------- | ------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `remotePath`      | yes      | —       | `mydrive:backups/kopia`   | rclone path in `remote:path` form. The part before `:` must match a `[section]` in the config; the part after is the folder. |
| `configSecretRef` | no¹      | —       | `{ name: rclone-config }` | Secret holding the `rclone.conf` (under `KOPIA_RCLONE_CONFIG`). **Not** `auth`. A `ClusterRepository` adds `namespace:`.      |
| `startupTimeout`  | no       | `15s`   | `2m`                      | How long kopia waits for its embedded `rclone serve` to come up before failing the connect (Go duration). Raise it for slow remotes whose metadata/indexes load through the rclone/WebDAV bridge. |

¹ Optional in the schema, but in practice required: rclone can't reach a remote
without its config.

/// tip | Bootstrap timing out on a slow remote?

A valid rclone repository whose connect is slow (kopia starts `rclone serve
webdav` and loads repo metadata through it) can trip the bootstrap Job's deadline
before the connect finishes. Two independent knobs help: raise
**`backend.rclone.startupTimeout`** (kopia's wait for `rclone serve`) and/or
**`spec.bootstrap.failurePolicy.activeDeadlineSeconds`** (the bootstrap Job's
wall-clock cap, default 120s) — see [Repositories](../repositories.md).

///

## Customization — the values you actually change

- **`remotePath`** — the `remote:path`. The remote name must exist in the config.
- **`KOPIA_RCLONE_CONFIG`** — the config contents; re-paste when tokens rotate.
- **`create.enabled`** — initialize the repository if missing.

## As a `ClusterRepository`

The same `backend.rclone` stanza works on a cluster-scoped
[`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository); the
`configSecretRef` (and the `encryption.passwordSecretRef`) must carry an explicit
`namespace:`, and the Secret must exist where the movers run — see [Movers](../movers.md).

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny
smoke-test (a throwaway PVC + a `SnapshotPolicy` + a `Snapshot`) that targets the
`rclone-primary` repository above, so you can go from "applied" to "a snapshot on
my remote" in one arc.

/// warning | Fill in the config first

The smoke backup only goes green once `KOPIA_RCLONE_CONFIG` holds a **real**,
working `rclone.conf` (validate it locally with `rclone ls <remote>:` first).
With the `REPLACE_ME` token the `Repository` stalls at `Failed` (rclone can't
reach the remote) and the `Snapshot` stays `Pending`.

///

**1. Apply the bundle** (namespace `backups`, Secret, Repository, and the
smoke-test objects):

```console
$ kubectl apply -f deploy/examples/backends/rclone.yaml
```

**2. Wait for the repository to be `Ready`** — the gate everything else waits on:

```console
$ kubectl -n backups wait --for=condition=Ready repository/rclone-primary --timeout=2m
repository.kopiur.home-operations.com/rclone-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it
(the namespace, Secret, Repository, PVC, and policy already exist and report
unchanged — the `Snapshot` is the one new object):

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

(Output illustrative.) The `Snapshot` has no fixed `Succeeded` *condition*; to
wait on it in a script, key on the phase:

```console
$ kubectl -n backups wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/smoke-now-abc12 --timeout=5m
```

**5. Deep proof — the data really moved.** `status.stats` shows non-zero
`bytesNew`/`filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia
snapshot ID on your remote:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

(Both outputs illustrative — sizes and the ID vary.) Non-zero `bytesNew` is the
proof the backup uploaded real content through rclone.

**6. Clean up** the smoke-test when you're done:

```console
$ kubectl -n backups delete snapshot --all       # finalizer also deletes the kopia snapshot
$ kubectl -n backups delete snapshotpolicy smoke
$ kubectl -n backups delete pvc smoke-data
```

/// warning | Deleting a Snapshot deletes its snapshot

A produced `Snapshot` defaults to `deletionPolicy: Delete`, so removing the CR
runs `kopia snapshot delete` via a finalizer. Use `Retain` (or `Orphan`) to keep
the data — see [Backups → deletionPolicy](../backups.md#deletionpolicy--what-happens-to-the-snapshot).

///

From here the full lifecycle is backend-independent — only the `Repository`
differs. Put it on a cron with a `SnapshotSchedule`
([Backups & schedules](../backups.md),
[Example 01](../examples.md#example-01--single-pvc-scheduled)) and restore by
picking a `Snapshot` ([Restores](../restores.md),
[Example 03](../examples.md#example-03--restore-by-picking-a-snapshot)).

## Troubleshooting

/// warning | Remote name must match

`directory not found` / `didn't find section in config file` almost always means
the remote name in `remotePath` doesn't match a `[section]` in your `rclone.conf`.
Keep them identical.

///

- **Auth/token errors** — tokens in `rclone.conf` expire or get revoked; re-run
  `rclone config reconnect <remote>:` locally and re-paste the config.
- **`configSecretRef` ignored** — make sure you used `configSecretRef`, not
  `auth`; rclone has no `auth` block.
- Always validate with `rclone ls <remote>:` locally before applying.

## See also

- [Repositories & backends](../repositories.md) — concepts: scope, encryption, creation.
- [Movers, RBAC & credentials](../movers.md) — where the config Secret must live.
- Native backends (prefer where available): [S3](s3.md) · [Azure](azure.md) · [GCS](gcs.md) · [B2](b2.md) · [WebDAV](webdav.md).
