# kubectl plugin (`kubectl kopiur`)

Kopiur ships a kubectl plugin that wraps the day-to-day operations — suspending
and resuming resources, inspecting snapshots, triggering backups and restores,
running maintenance, browsing (and reading files out of) snapshot contents, and
migrating from VolSync — so you don't have to hand-write CR YAML for routine
tasks.

To see these commands in the context of a full install-to-restore journey, follow the [Complete walkthrough](../walkthrough.md).

The plugin is a single static binary, shipped as `kopiur`. kubectl discovers
plugins by binary name: any executable called `kubectl-kopiur` on your `PATH`
makes `kubectl kopiur …` work — krew creates that link for you, Homebrew
installs the standalone `kopiur` command instead (both at once never collide).
It talks to the cluster with the **same
configuration kubectl uses** — `$KUBECONFIG`, `~/.kube/config`, or in-cluster
credentials — and never needs anything besides API-server access.

/// note | Alpha, like the operator
The plugin tracks the `v1alpha1` CRDs and is versioned with the operator.
A plugin build talking to a much older/newer operator may not know fields
the other side uses; keep them on the same release.

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

Via [krew](https://krew.sigs.k8s.io/) — the kopiur repository doubles as its
own [custom index](https://krew.sigs.k8s.io/docs/developer-guide/custom-indexes/)
(the `plugins/` directory at the repo root; the plugin is not yet in the
official krew-index — that submission waits for kopiur to leave heavy
development):

```console
$ kubectl krew index add kopiur https://github.com/home-operations/kopiur.git
$ kubectl krew install kopiur/kopiur
$ kubectl kopiur --version
```

`kubectl krew upgrade` picks up new releases after a `kubectl krew update`
(which pulls the index).

Via [Homebrew](https://brew.sh/) — the release workflow publishes a cask to
[home-operations/homebrew-tap](https://github.com/home-operations/homebrew-tap)
from the same release assets (macOS and Linux, amd64 + arm64). Brew installs
the standalone `kopiur` command — not the `kubectl-kopiur` shim — so it
coexists with a krew install of the same plugin:

```console
$ brew install home-operations/tap/kopiur
$ kopiur --version
```

`brew upgrade` picks up new releases.

Without krew or Homebrew: every GitHub release attaches per-platform archives
(`kubectl-kopiur_<version>_<os>_<arch>.tar.gz` for linux/darwin amd64+arm64;
the Linux binaries are fully static musl builds) with `.sha256` checksums,
SBOMs, and keyless Cosign signatures. `kubectl krew install --manifest-url
https://raw.githubusercontent.com/home-operations/kopiur/main/plugins/kopiur.yaml`
also works (the in-repo index copy, always the latest release). Or place the
archive's `kopiur` binary on your `PATH` — named `kubectl-kopiur` if you want
kubectl discovery, or as-is for the standalone command.

From source (requires the repo and [mise](https://mise.jdx.dev/)):

```console
$ mise run build
$ install -m 0755 target/debug/kopiur ~/.local/bin/kubectl-kopiur
```

## Try it end-to-end

Stand up a small playground the rest of the CLI pages build on, then prove the plugin can see it with `doctor`. The playground is one apply-ready bundle, [`deploy/examples/tryit/cli-playground.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/cli-playground.yaml): a `media` `Namespace`, a PVC seeded with a `config/app.yaml` file and a multi-MB blob, the backend Secret, an S3 `Repository` `nas`, and a `SnapshotPolicy` + `SnapshotSchedule` named `nightly`. It deliberately has **no `Snapshot`** — the other CLI arcs create those.

This is the shared starting point. Every CLI page's "Try it end-to-end" assumes you have applied this bundle and the repository is `Ready`.

**1. Apply the playground** (fill in the `AWS_*` + `KOPIA_PASSWORD` `REPLACE_ME`s first) and wait for the repository:

```console
$ kubectl apply -f deploy/examples/tryit/cli-playground.yaml
$ kubectl -n media wait --for=condition=Ready repository/nas --timeout=2m
$ kubectl -n media wait --for=condition=complete job/seed-app-data --timeout=2m
```

**2. Install the plugin** (via [krew](#install)):

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

A non-zero exit (and a `FAIL` line with `why:`/`fix:`) means something needs attention — see [`doctor`](operations.md#doctor) for the full check list and how a restricted kubeconfig degrades to warnings.

From here, follow any CLI page's "Try it end-to-end" against this same `media` playground: [take a backup and restore it](backup-restore.md#try-it-end-to-end), [browse a snapshot's files](browse.md#try-it-end-to-end), [run day-2 operations](operations.md#try-it-end-to-end), or [migrate from VolSync](migrate-volsync.md#try-it-end-to-end).

## Global flags

Every subcommand accepts the kubectl-alike connection and output flags:

| Flag | Meaning |
|---|---|
| `--kubeconfig PATH` | Use this kubeconfig instead of `$KUBECONFIG` / `~/.kube/config`. |
| `--context NAME` | Use this kubeconfig context instead of the current one. |
| `-n, --namespace NS` | Operate in this namespace (default: the context's namespace). |
| `-A, --all-namespaces` | List across all namespaces (list commands). |
| `-o, --output FORMAT` | `table` (default), `wide`, `yaml`, `json`, or `name`. |
| `-v` / `-vv` | Debug / trace diagnostics on stderr (`KOPIUR_LOG` accepts a full filter). |

`-o yaml|json` always emits the **verbatim Kubernetes objects** (a `v1/List`
for list commands), so the output is pipeable to `kubectl apply`, `jq`, or
`yq` — the table is just one rendering of the same data.
