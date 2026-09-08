# Inspecting & browsing snapshots

List what is in the catalog, then read a snapshot's **files** without restoring anything. This is how you answer "is the file I need actually in last night's backup?" in seconds. All [global flags](index.md#global-flags) apply.

## `snapshots list`

A richer `kubectl get snapshots`. It shows the policy, the origin, the phase, the kopia snapshot ID, real size and file counts from the run's stats, and the start time. Rows are sorted newest first, can be filtered, and `-A` lists across namespaces.

```console
$ kubectl kopiur snapshots list -n media
NAME                        POLICY   ORIGIN     PHASE      SNAPSHOT-ID   SIZE     FILES  START                 AGE
nightly-20260611-030012     nightly  scheduled  Succeeded  a1b2c3d4e5f6  5.0 GiB  1000   2026-06-11T03:00:12Z  9h
```

The filters can be combined:

| Flag | Effect |
|---|---|
| `--policy NAME` | Only snapshots produced from this SnapshotPolicy. |
| `--origin scheduled\|manual\|discovered` | Only this origin. `discovered` = found in the repository by the catalog scan, not produced by this cluster. |
| `--repository NAME` | Only snapshots stored in this repository. Matches produced snapshots through their pinned `status.resolved.repository` and discovered ones through the repository-UID label. |
| `--repository-kind repository\|cluster-repository` | Which kind `--repository` names (default `repository`). |
| `--repository-namespace NS` | Where the `--repository` lives, when it differs from the query namespace. |

`-o wide` adds the kopia identity as `username@hostname:path`, the `deletionPolicy`, and the pin state Kopiur last observed.

## `ls` / `cat` / `download` / `browse`

Read a snapshot's **files** without restoring anything:

```console
$ kubectl kopiur ls nightly-20260611-030012 -n media
NAME       TYPE  SIZE     MODIFIED
config/    dir   1.2 MiB  2026-06-10 21:14:02
movies.db  file  4.8 GiB  2026-06-11 02:59:31

$ kubectl kopiur ls nightly-20260611-030012 config -n media
$ kubectl kopiur cat nightly-20260611-030012 config/app.yaml -n media
$ kubectl kopiur download nightly-20260611-030012 movies.db ./movies.db -n media
$ kubectl kopiur browse nightly-20260611-030012 -n media   # interactive ls/cd/cat/get
```

All four take a **Snapshot object name**, whether scheduled, manual or discovered, so anything `snapshots list` shows with a kopia snapshot ID. They also take an optional path **relative to the snapshot root**.

`cat` streams the file to stdout and is safe for binary data, so you can pipe it anywhere. `download` writes the file locally, defaulting to the file's own name, and verifies the byte count against the snapshot manifest; if the count does not match it deletes the partial file rather than leaving a truncated one behind. `browse` opens a small read-only shell with `ls`, `cd`, `cat`, `get`, `pwd`, `help` and `quit`.

`ls -o wide` adds the kopia object ID column, and `ls -o json` prints the kopia directory manifest exactly as kopia produced it.

### The session-pod model

The first command against a repository starts a **session pod**: a mover `Job` in the snapshot's namespace that connects to the repository **read-only** and then idles.

That first command takes a few seconds, for the pod start and the connect. Every later `ls`, `cat` or `download` against the same repository reuses the warm session and answers instantly.

The session expires on its own after `--session-ttl`, which defaults to **15m**, and the cluster garbage-collects the finished Job a minute later. An abandoned browse therefore cannot hold a repository connection, or a pod, open forever. `browse` ends its session when you exit, unless you pass `--keep`.

| Flag | Effect |
|---|---|
| `--session-ttl DURATION` | How long the session pod stays warm (default `15m`). |
| `--local` | Skip the session pod; read with a **local kopia binary** (below). |
| `--kopia-bin PATH` | Which local kopia to run (`--local` only; default `$KOPIUR_KOPIA_BINARY`, then `kopia` on `PATH`). |
| `--keep` | (`browse` only) keep the session warm on exit. |

There is deliberately **no `--image` flag**. The session pod runs the exact mover image the operator's controller Deployment is configured with, through `KOPIUR_MOVER_IMAGE`, falling back to the release default. Whatever browses your repository is always what backs it up.

### The security model

Read-only is enforced twice, but the two layers are different things.

The first layer is an anti-footgun, not a security boundary. The session connects with kopia's `--readonly`, and the CLI can only run a **closed, typed set** of kopia read commands, so snapshot list and object show. There is no way to express a mutating verb in the client, and the read-only connection refuses writes even if someone execs into the pod by hand.

The second layer is the actual boundary, and it is RBAC. Anyone who can create pods that reference the repository's credential `Secret` in that namespace can already read the repository. Browsing needs exactly that power: create the session Job and exec into it. Note that it does **not** need permission to read Secrets. The pod loads the credentials itself, and they never pass through the user's hands.

The chart ships an opt-in ClusterRole with exactly the browse permission set. Set `rbac.browseRole: true` and bind `<release>-browse` to the humans who should browse; a namespaced RoleBinding scopes them to one namespace. See the [RBAC reference](../rbac.md#browsing-snapshots-rbacbrowserole).

/// warning | `--local` moves the credentials to your machine
`--local` is for clusters you cannot run pods in, or backends only reachable from your workstation.

It fetches the repository's credential Secrets onto your machine, which needs `get secrets` RBAC that the session path never needs. It stages them in a private temp directory, removed afterwards, and runs a local `kopia` binary with a read-only connect. Install kopia first, or point `--kopia-bin` at it.

The backend endpoint must be reachable **from your machine**. An in-cluster MinIO needs a `kubectl port-forward` and a Repository endpoint your workstation can resolve.
///

/// note | Sessions are shared per repository
One warm session serves every command against the same repository. Exiting `browse` without `--keep` ends that shared session, so a second terminal in the middle of a read will see its next command fail and start a fresh session.

///

## `session end`

End a warm browse session early. It would expire by TTL anyway.

```console
$ kubectl kopiur session end nightly-20260611-030012 -n media   # via a snapshot in that repo
$ kubectl kopiur session end --repository nas -n media          # or the repository directly
session kopiur-browse-nas-1a2b3c4d ended (Job deleted)
```

The command deletes the session Job, and the session's whole spec rides on that Job. When no session is open it says so and exits 0, so it is safe to call from cleanup scripts.

Sessions are also labeled `kopiur.home-operations.com/session=browse`, so a plain `kubectl delete job -l kopiur.home-operations.com/session=browse` works too.

## Try it end-to-end

Read a snapshot's files without restoring anything: list them, `cat` a config file, `download` a blob, then poke around interactively.

/// note | Prerequisite: the playground + one snapshot

This arc runs against the shared CLI playground: the `media` namespace, repository `nas`, and a seeded PVC holding `config/app.yaml` and `movies.db`. Apply it and install the plugin first, as described in [the playground setup](index.md#try-it-end-to-end), then take one snapshot so there is something to browse:

```console
$ kubectl kopiur snapshot now --policy nightly -n media --wait
```

///

**1. List the snapshots** to get a name with a kopia snapshot ID:

```console
$ kubectl kopiur snapshots list -n media
NAME                              POLICY   ORIGIN   PHASE      SNAPSHOT-ID   SIZE     FILES  START                 AGE
nightly-manual-20260611030012     nightly  manual   Succeeded  a1b2c3d4e5f6  5.0 MiB  2      2026-06-11T03:00:12Z  1m
```

**2. List its files**, then read the config file straight to stdout:

```console
$ kubectl kopiur ls nightly-manual-20260611030012 -n media
NAME        TYPE  SIZE     MODIFIED
config/     dir   29 B     2026-06-11 03:00:10
movies.db   file  5.0 MiB  2026-06-11 03:00:11

$ kubectl kopiur cat nightly-manual-20260611030012 config/app.yaml -n media
# demo config the CLI arcs read back
name: media-app
replicas: 1
```

**3. Download the blob** locally. It is verified against the snapshot's byte count:

```console
$ kubectl kopiur download nightly-manual-20260611030012 movies.db ./movies.db -n media
downloading movies.db to ./movies.db…
wrote 5242880 bytes to ./movies.db
```

**4. Browse interactively (deep).** The shell reuses the warm session, and `quit` ends it:

```console
$ kubectl kopiur browse nightly-manual-20260611030012 -n media
browsing snapshot nightly-manual-20260611030012 (kopia a1b2c3d4e5f6) — read-only; `help` lists commands, `quit` leaves
kopiur:/> help
commands:
  ls            list the current directory
  cd <dir>      enter a directory (cd .. to go up)
  cat <file>    print a file to stdout
  get <file> [dest]  download a file
  pwd           print the current path
  help          this help
  quit          leave (also: exit, q, ctrl-d)
kopiur:/> ls
NAME        TYPE  SIZE     MODIFIED
config/     dir   29 B     2026-06-11 03:00:10
movies.db   file  5.0 MiB  2026-06-11 03:00:11
kopiur:/> cd config
kopiur:/config> cat app.yaml
# demo config the CLI arcs read back
name: media-app
replicas: 1
kopiur:/config> get app.yaml ./app.yaml
wrote 29 bytes to ./app.yaml
kopiur:/config> quit
```

**5. Tidy up** the warm session early. It would expire by TTL anyway:

```console
$ kubectl kopiur session end nightly-manual-20260611030012 -n media
session kopiur-browse-nas-1a2b3c4d ended (Job deleted)
```

/// note | Illustrative output

The kopia ids, sizes, timestamps, the session-pod suffix such as `…-1a2b3c4d`, and the byte counts all vary per run. What appears exactly as shown is the browse banner, the `kopiur:<path>>` prompt, the `help` text, and the `wrote N bytes to …` and `session … ended` lines.

///
