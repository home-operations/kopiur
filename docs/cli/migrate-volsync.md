# Migrating from VolSync

Translate VolSync `ReplicationSource` and `ReplicationDestination` objects into kopiur `SnapshotPolicy`, `SnapshotSchedule` and `Restore` manifests. They are printed as apply-ready YAML, and `--apply` creates them with server-side apply. All [global flags](index.md#global-flags) apply.

Two VolSync movers are supported, and their **data semantics are very different**. The command detects which one each object uses, by looking for `spec.restic` or `spec.kopia`, and a namespace holding both translates in one run.

| Mover | Where it comes from | What migration means |
| --- | --- | --- |
| **restic** | upstream VolSync | Config translation ONLY: the repository formats are incompatible, so the kopiur repository starts empty. |
| **kopia** | the [perfectra1n/volsync fork](https://github.com/perfectra1n/volsync) | The repository **is** a kopia repository: kopiur **adopts it in place**, so all existing snapshots are preserved and history continues. |

```console
$ kubectl kopiur migrate volsync -n media --resolve-secrets --apply
```

| Flag | Effect |
|---|---|
| `--name NAME` | Translate one ReplicationSource (default: every one in the namespace). |
| `--repository NAME [--repository-kind …]` | Point the translated policies at an EXISTING kopiur repository. |
| `--resolve-secrets` | Instead, parse each repository Secret and EMIT a kopiur `Repository` derived from it. restic: + credential Secrets, with a `REPLACE_ME` kopia password **you must set** (a kopia repo needs its own new password); `--apply` refuses while any placeholder remains. kopia (fork): the existing repository is **adopted**: the Secret is referenced in place, no placeholder, so `--apply` works in one shot. |
| `--include-destinations` | Also translate ReplicationDestinations into `Restore`s. restic (and kopia without an identity): deploy-or-restore `fromPolicy` + `onMissingSnapshot: Continue`. kopia with `sourceIdentity` or `username`/`hostname`: a raw-identity restore (`source.identity`), no policy pairing needed. |
| `--strict` | Exit 1 (emitting nothing) when any field has no kopiur equivalent. A minimal fork-kopia source is fully mappable and passes. |
| `--apply` | Server-side-apply the translated objects. |
| `-f, --filename PATH` | Read VolSync objects from a YAML file, a directory, or `-` (stdin) instead of the cluster. **No kubeconfig required.** Repeatable. See [Offline / GitOps mode](#offline--gitops-mode). |
| `--secrets PATH` | In offline mode, resolve repository Secrets from plaintext Secret YAML on disk (file/dir/`-`). Repeatable; needs `--resolve-secrets`. |
| `--from-cluster-secrets` | In offline mode, fetch the referenced Secrets from the live cluster instead of `--secrets`. |
| `--out-dir DIR` | Write one YAML file per ReplicationSource (plus `_shared.yaml` for derived Repositories/Secrets) into `DIR` instead of stdout. |
| `--force` | Allow `--out-dir` to overwrite existing files. |

Every VolSync field the translator reads is accounted for on stderr. It is either `mapped`, with the kopiur destination named; `UNMAPPABLE`, with the reason and what to do instead, for example that restic's `retain.within` has no kopia equivalent; or `ignored`, with the reason it is not needed, for example that `pruneIntervalDays` is unnecessary because kopiur manages maintenance by default. Nothing is dropped silently.

## Offline / GitOps mode

If your VolSync objects live as YAML in a Git repository rather than only in the cluster, point `migrate volsync` at the files with `-f`/`--filename`. That takes a file, a directory containing `*.yaml` and `*.yml` files, or `-` for stdin.

With `-f`, the command reads **nothing from the cluster** unless you ask it to, so it needs no kubeconfig:

```console
$ kubectl kopiur migrate volsync -f ./apps/media/volsync.yaml --repository nas --out-dir ./apps/media/kopiur
wrote 2 file(s) to ./apps/media/kopiur:
  _shared.yaml
  media-app.yaml
```

`--out-dir` writes **one file per ReplicationSource**, named after it, plus a `_shared.yaml` holding any derived `Repository` and credential Secrets shared across sources. Those files are ready to commit. Without `--out-dir` the manifests stream to stdout as before.

You can also pipe the cluster's objects through it, and a `kind: List` is unwrapped automatically:

```console
$ kubectl get replicationsource -A -o yaml | kubectl kopiur migrate volsync -f - --repository nas
```

Credentials offline have three options, because your SOPS-encrypted Secrets cannot be read straight from Git:

| You want | Use |
| --- | --- |
| Point policies at a kopiur `Repository` you author/migrate separately, with no Secret reads | `--repository NAME` |
| Derive the `Repository` from **plaintext** Secret YAML on disk | `--resolve-secrets --secrets ./secrets.yaml` |
| Read VolSync from files but fetch Secrets from the **live** cluster (e.g. Flux already decrypted them) | `--resolve-secrets --from-cluster-secrets` |

`--apply` is rejected with offline input. Review the emitted files and `kubectl apply -f` them yourself.

Each object's namespace comes from its own `metadata.namespace`. `-n` only supplies a fallback for objects with no namespace, and it never filters offline input.

### Try it offline

[`deploy/examples/tryit/volsync-offline.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/volsync-offline.yaml) is a GitOps-shaped bundle: two fork-kopia `ReplicationSource`s in the `apps` namespace, plus the repository Secrets they reference. Nothing here touches a cluster, so you can run the whole arc on your laptop with no kubeconfig.

```yaml
--8<-- "deploy/examples/tryit/volsync-offline.yaml:sources"
```

**1. Point the policies at an existing kopiur `Repository`.** This is the simplest path, with no Secret reads at all:

```console
$ kubectl kopiur migrate volsync -f deploy/examples/tryit/volsync-offline.yaml --repository nas --out-dir ./migrated
wrote 2 file(s) to ./migrated:
  blog.yaml
  forum.yaml
```

You get one file per source, `migrated/blog.yaml` and `migrated/forum.yaml`. Each holds a `SnapshotPolicy` and a `SnapshotSchedule` pointing at the `nas` repository, ready to commit.

**2. Or derive the `Repository` from the Secrets** with `--resolve-secrets`. The Secrets live in the same bundle: `-f` reads only the `ReplicationSource`s and `--secrets` reads only the `Secret`s, so one file feeds both flags. In a real repository, decrypt your SOPS Secrets to a scratch file first.

```yaml
--8<-- "deploy/examples/tryit/volsync-offline.yaml:secrets"
```

```console
$ kubectl kopiur migrate volsync -f deploy/examples/tryit/volsync-offline.yaml \
    --resolve-secrets --secrets deploy/examples/tryit/volsync-offline.yaml --out-dir ./migrated --force
wrote 3 file(s) to ./migrated:
  _shared.yaml
  blog.yaml
  forum.yaml
```

Now `migrated/_shared.yaml` carries the two derived `Repository` objects, each **adopting** the fork repository in place, with no `create` block and the Secret referenced where it sits. The per-source files reference them. Fork-kopia has no password placeholder, so the result is ready to apply.

/// tip | Already running against the cluster?

Drop `--secrets` for `--from-cluster-secrets` and the same command reads the live Secrets instead, which is handy when Flux has already decrypted them into the cluster. Or pipe the cluster's objects straight in: `kubectl get replicationsource -A -o yaml | kubectl kopiur migrate volsync -f - --repository nas`.

///

**3. Review and apply.** `--apply` is refused offline by design, so commit the files, or apply them yourself:

```console
$ kubectl apply -f ./migrated
```

## restic sources (upstream VolSync)

/// danger | Config translation ONLY, no data is migrated
A VolSync **restic** repository is NOT a kopia repository. The kopiur repository the translated policies point at starts **empty**, and it fills as kopiur takes its own snapshots. Keep VolSync and its repository running until kopiur's retention coverage is enough for your recovery needs.

///

```console
$ kubectl kopiur migrate volsync -n media --repository nas --apply
```

With `--resolve-secrets`, the translator parses the restic Secret's `RESTIC_REPOSITORY` URL into a kopiur `Repository` backend, one of s3, b2, azure, gcs or filesystem. It carries the backend credentials into a new `<secret>-kopiur-creds` Secret under **kopia's** environment names, because restic's differ for B2 and Azure.

The kopia password is emitted as a `REPLACE_ME` placeholder you must replace. A restic password cannot initialize a kopia repository's encryption.

## kopia sources (perfectra1n/volsync fork)

/// tip | Repository ADOPTED in place, so data and history are preserved
The fork's mover writes a real kopia repository. The emitted `Repository` connects to it **as-is**, with the same backend and the same password, and with **no `create` block**, so the repository must already exist and a mis-parsed backend can never initialize a fresh empty one. Every existing snapshot is preserved and shows up as an `origin: discovered` [Snapshot](../repositories.md).

///

```console
$ kubectl kopiur migrate volsync -n media --resolve-secrets --apply
```

What the translation does, and why it is safe to switch over:

- **Identity continuity, which is the part that matters most.** The fork records snapshots as `<sanitized-name>@<sanitized-namespace>:/data`, or under your explicit `username`, `hostname` and `sourcePathOverride`. Every translated `SnapshotPolicy` pins `spec.identity` and `sources[0].sourcePathOverride` to exactly that identity, so the next kopiur snapshot continues the same history and retention sees old and new snapshots as one series. The pinned identity is shown in the accounting, on the `(fork snapshot identity)` line. Check it matches `kopia snapshot list` before you apply.
- **Secrets are referenced in place, never copied.** The `Repository`'s `encryption.passwordSecretRef` points at the existing VolSync Secret's `KOPIA_PASSWORD`, and for S3, whose `AWS_*` environment names already match, the backend `auth.secretRef` does too. Only key names kopia does not read get a small derived `<secret>-kopiur-creds` rename Secret: B2 (`B2_ACCOUNT_ID` and `B2_APPLICATION_KEY` become `B2_KEY_ID` and `B2_KEY`), legacy Azure (`AZURE_ACCOUNT_KEY` becomes `AZURE_STORAGE_KEY`), WebDAV (`WEBDAV_USERNAME` and `WEBDAV_PASSWORD` become `KOPIA_WEBDAV_*`), and SFTP known-hosts data.
- **`KOPIA_REPOSITORY` URL forms all translate**: `s3://`, `gcs://`, `azure://`, `b2://`, `filesystem://` (where the repository PVC is inferred from `moverVolumes`), `sftp://`, `webdav://` and `rclone://`. So do the fork's Secret-key overrides, such as `KOPIA_S3_BUCKET` beating the URL bucket, `AWS_S3_ENDPOINT`, and `*_DISABLE_TLS`. Quirks are preserved rather than corrected: only `s3://bucket/prefix` carries a prefix, with the fork's trailing slash. For `gcs`, `azure` and `b2` the fork always **ignored** the URL's path portion, so the repository is adopted at the bucket or container root and the dropped path is called out in the accounting. `gdrive://` has no kopiur backend, so write that `Repository` by hand.
- **Retention maps one-to-one**: `retain.latest` becomes `keepLatest`, `retain.yearly` becomes `keepAnnual`, and the rest map by name. So do `compression`, `parallelism` (which becomes `upload.maxParallelFileReads`), `additionalArgs` (which becomes `extraArgs`), and the cache size limits (which become `mover.cache.*`).

/// warning | KEEP the VolSync Secret(s)
kopiur reads the repository password, and where the names match the backend credentials too, **from the original VolSync Secret, in place**. When you decommission VolSync, delete its CRDs and `ReplicationSource`s, but keep the repository Secret, or the adopted `Repository` loses its credentials.

///

/// warning | Retire the fork's `KopiaMaintenance` objects
kopiur manages repository maintenance itself, and takes over kopia's maintenance ownership with `kopia maintenance set --owner` on its first run. A fork `KopiaMaintenance` left running will fight kopiur over that ownership. Delete it once the adopted repository is `Ready`.

///

A few fork fields have **no kopiur equivalent**. They are called out in the accounting rather than dropped silently. `actions.beforeSnapshot` and `actions.afterSnapshot` run in the fork's *mover* pod, while kopiur [hooks](../backups.md) run in the *workload* pod, so rewrite them for that context. `policyConfig` raw policy files are replaced by the typed `SnapshotPolicy` fields. `shallow` restore windows have no analog; use `asOf`, `offset` or `snapshotID` instead.

### Suggested cut-over

1. Suspend or delete the fork's `ReplicationSource`, so the two operators do not snapshot at the same time, and keep its Secret.
2. Run `kubectl kopiur migrate volsync -n <ns> --resolve-secrets` and review the accounting, especially the pinned identity line.
3. Re-run with `--apply`. Wait for the `Repository` to reach `Ready` and for the old snapshots to appear: `kubectl kopiur snapshots list -n <ns>`.
4. Prove continuity with `kubectl kopiur snapshot now --policy <name> --wait`, then confirm the new snapshot lists under the same identity.
5. Delete the fork's `KopiaMaintenance` objects and, when you are satisfied, the VolSync install. Do **not** delete the repository Secret.

## Try it end-to-end

Translate a fork-kopia VolSync source into kopiur manifests, review the accounting, apply it, and confirm the repository is adopted.

/// note | Prerequisite: a fork-kopia source

This arc reads one apply-ready input, [`deploy/examples/tryit/volsync-source.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/volsync-source.yaml): a fork-kopia `ReplicationSource` using `spec.kopia` in `media`, plus the repository Secret it references. It assumes the upstream VolSync CRDs are installed and `kubectl kopiur` is on your PATH; see the [playground setup](index.md#try-it-end-to-end) for the install.

```yaml
--8<-- "deploy/examples/tryit/volsync-source.yaml:source"
```

Fill in the `AWS_*` keys and the **existing** repository `KOPIA_PASSWORD`, reusing the password the fork already initialized the repository with, then apply the input:

```console
$ kubectl apply -f deploy/examples/tryit/volsync-source.yaml
```

///

**1. Dry-run the translation.** Nothing is changed, and the accounting prints on stderr:

```console
$ kubectl kopiur migrate volsync -n media --resolve-secrets
kopia sources: REPOSITORY ADOPTED IN PLACE — all existing snapshots are preserved and the snapshot identity is pinned so history continues. KEEP the referenced VolSync Secret(s); retire the fork's KopiaMaintenance objects.

  mapped      spec.sourcePVC -> SnapshotPolicy.spec.sources[0].pvc.name
  mapped      spec.kopia.retain -> SnapshotPolicy.spec.retention
  mapped      spec.trigger.schedule -> SnapshotSchedule.spec.schedule.cron
  ...
# (the kopiur Repository + SnapshotPolicy + SnapshotSchedule YAML on stdout)
```

Review the accounting before applying, especially the pinned **`(fork snapshot identity)`** line. Every field the translator reads is `mapped`, `UNMAPPABLE` or `ignored`, and nothing is dropped silently.

**2. Apply** when the accounting looks right. Fork-kopia adopts the repository in place, so `--apply` works in one shot with no `REPLACE_ME` to fill:

```console
$ kubectl kopiur migrate volsync -n media --resolve-secrets --apply
...
applied Repository/media
applied SnapshotPolicy/media
applied SnapshotSchedule/media
```

**3. Confirm the repository is healthy (deep)** with `doctor`:

```console
$ kubectl kopiur doctor -n media | grep repositories
  ok    repositories ready
```

**4. See the adopted history.** The fork's existing snapshots show up as `origin: discovered`:

```console
$ kubectl kopiur snapshots list --origin discovered -n media
NAME                          POLICY  ORIGIN      PHASE      SNAPSHOT-ID   SIZE     FILES  START                 AGE
media-20260601-020000         -       discovered  Succeeded  9f8e7d6c5b4a  4.7 GiB  982    2026-06-01T02:00:00Z  10d
```

/// note | Illustrative: discovered rows need a real fork repository

The `--origin discovered` listing above is illustrative. Those rows appear only once the catalog scan finds real snapshots in the adopted repository. Against an empty placeholder backend the table stays empty until the fork's repository, with its history, is reachable. What appears exactly as shown is the adoption banner, the `mapped` and `applied <kind>/<name>` line formats, and the `ok    repositories ready` doctor line.

///
