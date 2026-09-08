# SFTP

The SFTP backend stores the kopia repository on a server reached over **SSH and SFTP**: a NAS, a VPS, anything you can `sftp` into. Kopiur uses **key-based authentication**, meaning an SSH private key plus a pinned `known_hosts` entry, both delivered as **files**.

Reach for SFTP when the target is a remote host with SSH but no object-store API. For the same NAS mounted as a volume, see [filesystem](filesystem.md).

## Provider prerequisites

- An **SFTP account** on the server and a **path** for the repository, such as `/volume1/kopia`. The account must be able to read, write, and create there.
- An **SSH private key** for that account. Kopiur supports key-based authentication only; there is no password option.  Add the matching public key to the server's `authorized_keys`.
- The server's **host key**, so you pin `known_hosts` instead of trusting on first use:

    ```console
    $ ssh-keyscan -p 22 nas.lan
    nas.lan ssh-ed25519 AAAAC3Nz...
    ```

/// example | Minting a dedicated keypair for the mover

Don't reuse your personal SSH key. Generate one that exists only for this repository, so you can rotate or revoke it on its own:

```console
# 1. A fresh ed25519 keypair, no passphrase (the mover can't answer a prompt):
$ ssh-keygen -t ed25519 -N "" -C kopiur-mover -f kopia_sftp

# 2. Authorize the PUBLIC half on the server, for the SFTP account:
$ ssh-copy-id -i kopia_sftp.pub kopia@nas.lan
#    (or append kopia_sftp.pub to ~kopia/.ssh/authorized_keys by hand)

# 3. Sanity-check before touching Kubernetes:
$ sftp -i kopia_sftp kopia@nas.lan

# 4. The PRIVATE half (the file `kopia_sftp`, the whole BEGIN…END block)
#    goes under KOPIA_SFTP_KEY_DATA in the Secret.
```

///

## The Secret shape

SFTP is one of the three **file-delivered** backends, and the most asked-about, so here is exactly what the Secret looks like.

kopia's SFTP backend has no environment-variable credential form. And a Secret key like `ssh-privatekey` is **not a valid environment-variable name**, because `envFrom` silently drops keys containing a dash.

So Kopiur standardizes on two environment keys that are valid identifiers. The mover reads them, writes each to a private file with mode `0600`, and passes `--keyfile` and `--known-hosts` to kopia.

| Secret key               | Required | What it is                                                            | Becomes                              |
| ------------------------ | -------- | --------------------------------------------------------------------- | ------------------------------------ |
| `KOPIA_SFTP_KEY_DATA`    | yes      | The SSH **private key** in PEM form, exactly as generated, including the whole `BEGIN…END` block. | a `0600` keyfile → kopia `--keyfile` |
| `KOPIA_SFTP_KNOWN_HOSTS` | yes      | One `known_hosts` line for the server (from `ssh-keyscan`).           | a file → kopia `--known-hosts`       |
| `KOPIA_PASSWORD`         | **yes**  | The repository encryption password.                                   | env var kopia reads                  |

```yaml
stringData:
    KOPIA_SFTP_KEY_DATA: |
        -----BEGIN OPENSSH PRIVATE KEY-----
        REPLACE_ME
        -----END OPENSSH PRIVATE KEY-----
    KOPIA_SFTP_KNOWN_HOSTS: "nas.lan ssh-ed25519 AAAAC3Nz...REPLACE_ME"
    KOPIA_PASSWORD: "choose-something-long-and-random"
```

The complete, apply-ready Secret + `Repository` is below.

/// info | Why these key names (and not ssh-privatekey)

The key **names** must be valid environment-variable identifiers, because the mover loads them with `envFrom`. `ssh-privatekey` contains a dash and would be dropped, so Kopiur uses `KOPIA_SFTP_KEY_DATA` and `KOPIA_SFTP_KNOWN_HOSTS`.

You provide the **values**. The mover writes the files, and never puts the key on kopia's command line.

///

## The Repository

```yaml
--8<-- "deploy/examples/backends/sftp.yaml:repository"
```

## Fields reference (`backend.sftp`)

| Field            | Required | Default | Example                     | What it controls                                                                                                |
| ---------------- | -------- | ------- | --------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `host`           | yes      | —       | `nas.lan`                   | SFTP server hostname or IP. It must match the name in the `known_hosts` line, because that is how the entry is looked up. |
| `path`           | yes      | —       | `/volume1/kopia`            | Remote **absolute** path on the server that holds the repository. The SSH user must be able to write it.         |
| `port`           | no       | `22`    | `2222`                      | TCP port. Any port other than 22 changes the `known_hosts` format. See the warning below.                        |
| `username`       | no       | —       | `kopia`                     | SSH user to connect as: the account whose `authorized_keys` holds the public key.                                |
| `auth.secretRef` | no       | —       | `{ name: sftp-repo-creds }` | Names the Secret holding the key + known_hosts above. Same namespace as the `Repository`; a `ClusterRepository` adds `namespace:`. |

SSH has no cloud IAM to federate with, so SFTP's `auth` is **Secret-only**. There is no `workloadIdentity` here. A stray `auth.workloadIdentity` is not rejected; the API server silently **prunes** it, because it isn't in the schema.

## Customization — the values you actually change

- **`host`, `port`, `path`, and `username`** are the connection coordinates.
- **`KOPIA_SFTP_KNOWN_HOSTS`** must be updated if the server is rebuilt or its host key rotates. Re-run `ssh-keyscan` and paste the new line.
- **`create.enabled`** initializes the repository if it's missing.

## As a `ClusterRepository`

The same `backend.sftp` stanza works on a cluster-scoped [`ClusterRepository`](../repositories.md#clusterrepository-a-shared-repository), with two requirements. Every Secret reference must carry an explicit `namespace:`, and the Secret holding the key, the `known_hosts` line, and the password must exist in the namespaces the movers run in. See [Movers](../movers.md).

## Try it end-to-end

Prove this backend really takes a backup. The same example file carries a tiny smoke-test: a throwaway PVC, a `SnapshotPolicy`, and a `Snapshot`, all pointed at the `sftp-primary` repository above. It takes you from "applied" to "a snapshot on my server" in one go.

/// warning | Fill in the credentials first

The smoke backup only goes green once `KOPIA_SFTP_KEY_DATA` and `KOPIA_SFTP_KNOWN_HOSTS` hold a **real** key and host-key line. With the `REPLACE_ME` placeholders the `Repository` stalls at `Failed`, because kopia can't reach the server, and the `Snapshot` stays `Pending`.

///

**1. Apply the bundle.** That is the `backups` namespace, the Secret, the Repository, and the smoke-test objects:

```console
$ kubectl apply -f deploy/examples/backends/sftp.yaml
```

**2. Wait for the repository to be `Ready`.** Everything else waits on this:

```console
$ kubectl -n backups wait --for=condition=Ready repository/sftp-primary --timeout=2m
repository.kopiur.home-operations.com/sftp-primary condition met
```

**3. Take the smoke backup.** The `Snapshot` uses `generateName`, so `create` it rather than apply it. The namespace, Secret, Repository, PVC, and policy already exist and report unchanged. The `Snapshot` is the one new object:

```console
$ kubectl create -f deploy/examples/backends/sftp.yaml
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

**5. Prove the data really moved.** `status.stats` shows non-zero `bytesNew` and `filesNew`, and `status.snapshot.kopiaSnapshotID` is the kopia snapshot ID on your server:

```console
$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.stats}'
{"sizeBytes":4096,"bytesNew":1280,"filesNew":2,"filesUnchanged":0}

$ kubectl -n backups get snapshot smoke-now-abc12 -o jsonpath='{.status.snapshot.kopiaSnapshotID}'
k1f1ec0a8
```

Both outputs are illustrative; sizes and the ID vary. Non-zero `bytesNew` proves the backup uploaded real content over SFTP.

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

/// warning | Host-key mismatch

If `KOPIA_SFTP_KNOWN_HOSTS` is empty, wrong, or stale because the server was rebuilt, the connection is **rejected**. Kopiur will not trust a host on first use.

Re-run `ssh-keyscan -p <port> <host>` and update the Secret. Match the port you actually use.

///

/// warning | Non-standard port? The known_hosts format changes

On any port other than 22, the `known_hosts` host field is written as `[host]:port`, brackets included:

```text
[nas.lan]:2222 ssh-ed25519 AAAAC3Nz...
```

`ssh-keyscan -p 2222 nas.lan` emits exactly that form, so copy its output unchanged. A plain `nas.lan ...` line will not match a port-2222 connection, and the failure looks identical to a wrong host key.

///

/// tip | Dashed Secret keys are dropped

Do **not** name the key `ssh-privatekey`. `envFrom` drops keys containing a dash, so the mover would see no key at all and connect with none. Use `KOPIA_SFTP_KEY_DATA`.

///

- **`permission denied (publickey)`.** Either the public key isn't in the server's `authorized_keys`, or the private key under `KOPIA_SFTP_KEY_DATA` is malformed: clipped, re-indented, or simply the wrong key.
- **Writes fail after connecting.** The SSH user can't write `path`. Fix the ownership and permissions on the server.

## See also

- [Repositories & backends](../repositories.md): the concepts, meaning scope, encryption, and creation.
- [Permissions, UID & GID](../permissions.md): server-side ownership of the repository path.
- [Movers, RBAC & credentials](../movers.md): where the credential Secret must live.
- Sibling backend: [filesystem](filesystem.md), the same NAS as a mounted volume.
