# kubectl plugin (`kubectl kopiur`)

Kopiur ships a kubectl plugin that wraps the day-to-day operations, so you do not have to hand-write CR YAML for routine tasks. It suspends and resumes resources, inspects snapshots, triggers backups and restores, runs maintenance, browses snapshot contents and reads files out of them, and migrates from VolSync.

To see these commands in the context of a full install-to-restore journey, follow the [Complete walkthrough](../walkthrough.md).

The plugin is a single static binary, shipped as `kopiur`. kubectl discovers plugins by binary name: any executable called `kubectl-kopiur` on your `PATH` makes `kubectl kopiur …` work. krew creates that link for you, and Homebrew installs the standalone `kopiur` command instead. Having both installed never causes a collision.

The plugin talks to the cluster with the **same configuration kubectl uses**, so `$KUBECONFIG`, `~/.kube/config`, or in-cluster credentials. It needs nothing besides API-server access.

/// note | Alpha, like the operator
The plugin tracks the `v1alpha1` CRDs and is versioned with the operator. A plugin build talking to a much older or newer operator may not know fields the other side uses, so keep them on the same release.

///

## The command map

| Command | What it does | Page |
|---|---|---|
| `snapshot now` | Run a SnapshotPolicy immediately (a manual `Snapshot` CR) | [Backups, restores & logs](backup-restore.md#snapshot-now) |
| `restore` | The `Restore` CRD's source × target matrix as one command line | [Backups, restores & logs](backup-restore.md#restore) |
| `logs` | Stream a Snapshot/Restore mover Job's logs | [Backups, restores & logs](backup-restore.md#logs) |
| `snapshots list` | A richer `kubectl get snapshots` (origin, kopia id, size, filters) | [Inspecting & browsing](browse.md#snapshots-list) |
| `ls` / `cat` / `download` / `browse` | Read snapshot **contents** without restoring | [Inspecting & browsing](browse.md#ls--cat--download--browse) |
| `session end` | End a warm browse session early | [Inspecting & browsing](browse.md#session-end) |
| `status` | One-screen health overview | [Operations](operations.md#status) |
| `doctor` | Diagnose an installation, exit 1 on failure | [Operations](operations.md#doctor) |
| `maintenance run` | Trigger an out-of-band maintenance run | [Operations](operations.md#maintenance-run) |
| `suspend` / `resume` | Pause/unpause reconciliation declaratively | [Operations](operations.md#suspend--resume) |
| `migrate volsync` | Translate VolSync restic objects into kopiur manifests | [Migrating from VolSync](migrate-volsync.md) |

## Install

Install with [krew](https://krew.sigs.k8s.io/). The kopiur repository doubles as its own [custom index](https://krew.sigs.k8s.io/docs/developer-guide/custom-indexes/), which is the `plugins/` directory at the repo root. The plugin is not in the official krew-index yet; that submission waits until kopiur leaves heavy development.

```console
$ kubectl krew index add kopiur https://github.com/home-operations/kopiur.git
$ kubectl krew install kopiur/kopiur
$ kubectl kopiur --version
```

`kubectl krew upgrade` picks up new releases after a `kubectl krew update`, which pulls the index.

Or install with [Homebrew](https://brew.sh/). The release workflow publishes a cask to [home-operations/homebrew-tap](https://github.com/home-operations/homebrew-tap) from the same release assets, covering macOS and Linux on amd64 and arm64. Brew installs the standalone `kopiur` command rather than the `kubectl-kopiur` shim, so it coexists with a krew install of the same plugin:

```console
$ brew install home-operations/tap/kopiur
$ kopiur --version
```

`brew upgrade` picks up new releases.

Without krew or Homebrew, use the release assets. Every GitHub release attaches per-platform archives named `kubectl-kopiur_<version>_<os>_<arch>.tar.gz`, for linux and darwin on amd64 and arm64, and the Linux binaries are fully static musl builds. Each archive ships with a `.sha256` checksum, an SBOM, and a keyless Cosign signature.

`kubectl krew install --manifest-url https://raw.githubusercontent.com/home-operations/kopiur/main/plugins/kopiur.yaml` also works, using the in-repo index copy, which always points at the latest release. Or put the archive's `kopiur` binary on your `PATH`, named `kubectl-kopiur` if you want kubectl discovery, or left as-is for the standalone command.

From source, with the repo checked out and [mise](https://mise.jdx.dev/) installed:

```console
$ mise run build
$ install -m 0755 target/debug/kopiur ~/.local/bin/kubectl-kopiur
```

## Try it end-to-end

Stand up a small playground that the rest of the CLI pages build on, then prove the plugin can see it with `doctor`.

The playground is one apply-ready bundle, [`deploy/examples/tryit/cli-playground.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/cli-playground.yaml). It contains a `media` `Namespace`, a PVC seeded with a `config/app.yaml` file and a multi-MB blob, the backend Secret, an S3 `Repository` called `nas`, and a `SnapshotPolicy` plus `SnapshotSchedule` called `nightly`. It deliberately has **no `Snapshot`**, because the other CLI arcs create those.

This is the shared starting point. Every CLI page's "Try it end-to-end" assumes you have applied this bundle and that the repository is `Ready`.

**1. Apply the playground.** Fill in the `AWS_*` and `KOPIA_PASSWORD` `REPLACE_ME` values first, then apply and wait for the repository:

```console
$ kubectl apply -f deploy/examples/tryit/cli-playground.yaml
$ kubectl -n media wait --for=condition=Ready repository/nas --timeout=2m
$ kubectl -n media wait --for=condition=complete job/seed-app-data --timeout=2m
```

**2. Install the plugin** with [krew](#install):

```console
$ kubectl krew index add kopiur https://github.com/home-operations/kopiur.git
$ kubectl krew install kopiur/kopiur
$ kubectl kopiur --version
```

**3. Prove the install with `doctor` (deep).** It exits **0** when all nine checks pass:

```console
$ kubectl kopiur doctor -n media
  ok    CRDs installed
  ok    controller running
  ok    webhook running
  ok    webhook admission (live dry-run probe)
  ok    repositories ready
  ok    credential secrets present
  ok    no blocked or stuck work
  ok    no recent failed snapshots/restores
  ok    recent warning events

9 check(s): 0 failed, 0 warning(s)

$ echo $?
0
```

A non-zero exit, plus a `FAIL` line with `why:` and `fix:`, means something needs attention. See [`doctor`](operations.md#doctor) for the full check list and for how a restricted kubeconfig degrades to warnings.

From here, follow any CLI page's "Try it end-to-end" against this same `media` playground: [take a backup and restore it](backup-restore.md#try-it-end-to-end), [browse a snapshot's files](browse.md#try-it-end-to-end), [run day-2 operations](operations.md#try-it-end-to-end), or [migrate from VolSync](migrate-volsync.md#try-it-end-to-end).

## Global flags

Every subcommand accepts the same connection and output flags kubectl uses:

| Flag | Meaning |
|---|---|
| `--kubeconfig PATH` | Use this kubeconfig instead of `$KUBECONFIG` / `~/.kube/config`. |
| `--context NAME` | Use this kubeconfig context instead of the current one. |
| `-n, --namespace NS` | Operate in this namespace (default: the context's namespace). |
| `-A, --all-namespaces` | List across all namespaces (list commands). |
| `-o, --output FORMAT` | `table` (default), `wide`, `yaml`, `json`, or `name`. |
| `-v` / `-vv` | Debug / trace diagnostics on stderr (`KOPIUR_LOG` accepts a full filter). |

`-o yaml` and `-o json` always emit the **exact Kubernetes objects**, and a `v1/List` for list commands. The output is therefore ready to pipe into `kubectl apply`, `jq` or `yq`; the table is just one rendering of the same data.
