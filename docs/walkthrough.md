# Complete walkthrough

[Getting started](getting-started.md) is the 15-minute first run. **This page is the long version.** It follows the same journey, from install to repository to policy to schedule to snapshot to restore, but it explains the _reasoning_ behind every choice and value. It also adds a second track for NAS users, and it uses the [`kubectl kopiur` plugin](cli/index.md) as the day-2 interface throughout.

Read this page when you're past "does it work" and into "what should _my_ setup look like, and why".

Here is the mental model in one paragraph; the [Concepts page](concepts/how-kopia-works.md) has the full story. A **`Repository`** is _where_ snapshots live. A **`SnapshotPolicy`** is the **recipe** for _what_ to back up, and it runs nothing on its own. A **`Snapshot`** is one **invocation** of a recipe, as a Kubernetes object. A **`SnapshotSchedule`** is the **cron** that creates those invocations. A **`Restore`** reads a snapshot back into a PVC. Everything below is those five pieces, in order.

/// note | Pick your track

Every manifest step below has two tabs. **S3** covers AWS, MinIO, RustFS, Ceph RGW, and the rest. **Filesystem (NAS)** covers an NFS export on your NAS.

Pick one in any tab and the whole page follows. The selection is linked and it persists.

The two tracks are deliberately near-identical: **only the `Repository` backend and one restore detail change.**

Using Azure, GCS, B2, SFTP, WebDAV, or rclone instead? Follow the S3 track and swap in the `Repository` from your [backend's page](backends/index.md).

///

## What you'll build

| Stage | Resource | Day-2 CLI verb |
| ----------------------- | ------------------------ | ------------------------------------- |
| Where snapshots live | `Repository` + a Secret | `kubectl kopiur status` / `doctor` |
| What to back up | `SnapshotPolicy` | `kubectl kopiur snapshot now` |
| When it runs | `SnapshotSchedule` | `kubectl kopiur suspend` / `resume` |
| One backup | `Snapshot` (created for you) | `kubectl kopiur snapshots list` / `logs` |
| Proof it works | `Restore` | `kubectl kopiur restore` / `ls` / `browse` |

Each track ships as one apply-ready bundle: [`deploy/examples/walkthrough/s3.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/walkthrough/s3.yaml) and [`deploy/examples/walkthrough/nas.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/walkthrough/nas.yaml). Every YAML block below is pulled from them at build time, stage by stage.

You can follow along one step at a time, or fill in the `REPLACE_ME` values and apply a whole bundle at once. The operator works out the ordering.

You need a cluster at version **1.24 or later**, Helm, `kubectl`, and a PVC with data in it. The walkthrough assumes a PVC named `app-data` in namespace `demo`. The [Getting started prerequisites](getting-started.md#what-you-need) show how to create a throwaway one.

## Step 0 — Choices before you install

The Helm install itself is two commands. Here are the decisions worth making _deliberately_ first.

**Install scope.** The default is `installScope=cluster`. It lets one operator watch every namespace, because its RBAC is a `ClusterRole`, and it is **required** for [`ClusterRepository`](repositories.md). That is the shared-repository pattern, where a platform team owns the storage and tenant namespaces reference it without ever seeing credentials. Cluster scope is the default because a namespace-scoped `Role` silently disables that cluster-scoped kind.

Choose `installScope=namespaced` when you deliberately want less privilege, confining the operator to objects in its own release namespace with a `Role` instead of a `ClusterRole`. `ClusterRepository` is then not reconciled. Moving between scopes later is a Helm upgrade, not a migration. For details see [Installation → Install scope](install.md#install-scope).

**Webhook TLS.** Kopiur validates and defaults your resources through an admission webhook, and that webhook needs a serving certificate.

The default is `webhook.tls.mode=self`, which means the operator mints and rotates the certificate itself. It has no dependencies and is the right answer unless you already run cert-manager, in which case `cert-manager` keeps all your certificates in one system. Use `manual` for clusters where certificates must come from your own PKI. For details see [Installation → Webhook TLS](install.md#webhook-tls).

**CRD lifecycle.** The CRDs ship in the chart's special `crds/` directory. `helm install` installs them, but `helm upgrade` never touches them.

So on a helm-CLI upgrade that carries a schema change, you apply them yourself with `kubectl apply --server-side -f deploy/crds/`. A GitOps CRD pipeline with a `CreateReplace` sync handles it automatically. For details see [Installation → CRD lifecycle](install.md#crd-lifecycle).

With the defaults chosen deliberately, install:

```console
$ helm install kopiur oci://ghcr.io/home-operations/charts/kopiur --namespace kopiur-system --create-namespace
$ kubectl -n kopiur-system rollout status deploy/kopiur-controller
$ kubectl -n kopiur-system rollout status deploy/kopiur-webhook
```

Every other knob lives in [Helm chart values](configuration.md): images by digest, resources, replicas for high availability, observability. None of them block a first run.

## Step 1 — Install the kubectl plugin

Everything in this walkthrough _can_ be done with raw YAML and `kubectl get -w`.

The plugin exists because day-2 operations are imperative by nature. "Back up **now**." "What failed?" "Show me the files in that snapshot." Those deserve commands with `--wait`, streamed logs, and meaningful exit codes, rather than hand-rolled watch loops.

Install it through [krew](https://krew.sigs.k8s.io/). The kopiur repository doubles as its own krew index:

```console
$ kubectl krew index add kopiur https://github.com/home-operations/kopiur.git
$ kubectl krew install kopiur/kopiur
```

Smoke-test it. On a fresh install this prints an empty but healthy overview:

```console
$ kubectl kopiur status -n demo
```

No krew? Each GitHub release attaches per-platform binaries. See [the plugin page](cli/index.md#install).

## Step 2 — Credentials

The mover Job that actually runs kopia reads its secrets from a `Secret` **in the same namespace as the data**, which is `demo` here. The Secret is loaded with `envFrom`, which is namespace-local, so credentials never pass through the operator.

What goes in that Secret differs by track, and this is the first place the two tracks teach different lessons:

/// tab | S3

```yaml
--8<-- "deploy/examples/walkthrough/s3.yaml:secret"
```

///

/// tab | Filesystem (NAS)

```yaml
--8<-- "deploy/examples/walkthrough/nas.yaml:secret"
```

///

The S3 track carries the object-store keys, meaning the `AWS_*` values, which are read by well-known names. The [per-backend key table](repositories.md#credential-secret-keys-by-backend) covers the other backends.

The NAS track needs **only** `KOPIA_PASSWORD`. There is no storage account to authenticate to, but kopia still encrypts everything it writes, so your NAS holds ciphertext either way.

/// warning | Lose the password, lose the backups

`KOPIA_PASSWORD` encrypts the repository. If you lose it, the backups are **unrecoverable**, because kopia cannot decrypt without it.

Generate something long and random, and store it in your password manager or secret store, not only in the cluster. A backup password that exists only in the cluster it's backing up defeats the purpose.

///

## Step 3 — The Repository (where)

/// tab | S3

```yaml
--8<-- "deploy/examples/walkthrough/s3.yaml:repository"
```

///

/// tab | Filesystem (NAS)

```yaml
--8<-- "deploy/examples/walkthrough/nas.yaml:repository"
```

///

The values worth pausing on:

- **One bucket, many repositories.** On S3 you separate them with `prefix`; on a NAS, with a subdirectory per repository. A repository is an encryption boundary _and_ a blast-radius boundary, so a leaked password for `demo/` reads nothing from `prod/`. The trade-off is that kopia deduplicates **within** a repository, so two namespaces backing up similar data into separate repositories store it twice. For guidance on drawing that line, see [Repositories & backends](repositories.md).
- **`create.enabled: true`** initializes a brand-new kopia repository when the target is empty. When you're _adopting_ an existing repository, such as one written by another cluster or by VolSync, set it to `false` so a typo in the bucket name fails loudly instead of quietly initializing an empty repository. See [the migration guide](cli/migrate-volsync.md).
- **`maintenance` is managed by default.** Omit the block entirely and the operator creates an owned [`Maintenance`](maintenance.md) with exactly the values shown: quick every 6 hours, full daily. It's spelled out in the bundle so you can see the knobs, but most users should delete the block and take the default.
- **NAS only: permissions.** The export must be writable by the mover's UID, which defaults to 65532. If it isn't, the operator's Warning Event names the exact `chown` to run. See [Permissions, UID & GID](permissions.md).

Apply, then wait for `Ready`. Everything else waits on this, and every Kopiur CRD exposes a standard `Ready` condition for exactly that purpose:

```console
$ kubectl -n demo wait --for=condition=Ready repository/primary --timeout=2m
```

Stuck? Run `kubectl -n demo describe repository primary`. Its conditions and events name the actual cause: wrong keys, an unreachable endpoint or NAS, or a missing repository with `create.enabled: false`. You can also jump ahead to [`kubectl kopiur doctor`](cli/operations.md#doctor).

## Step 4 — The SnapshotPolicy (what, and for how long)

/// tab | S3

```yaml
--8<-- "deploy/examples/walkthrough/s3.yaml:policy"
```

///

/// tab | Filesystem (NAS)

```yaml
--8<-- "deploy/examples/walkthrough/nas.yaml:policy"
```

///

This is identical in both tracks, because the policy doesn't care where the bytes land. The reasoning behind each value:

- **Retention is GFS**, which stands for grandfather-father-son, and it is the _only_ thing that prunes successful backups. Don't pick numbers; answer recovery questions instead. `keepDaily: 14` means "I can restore any day from the last two weeks", which covers noticing corruption a week later. `keepWeekly: 8` means "any week from the last two months", which covers slow-burn mistakes. `keepMonthly: 6` means "any month from the last half year", which covers compliance and archaeology. Thanks to deduplication, the extra cost of the older tiers is small, because they share unchanged data with newer snapshots.
- **`copyMethod: Direct`** reads the live volume. It has no CSI requirements, and it is a reasonable choice for data that doesn't rewrite in place, when you'd rather not depend on the CSI snapshot stack. It is pinned explicitly here because it is no longer the CRD default. The moment a database is involved, reach for `Snapshot`, which is the default. That takes a CSI VolumeSnapshot first, so kopia reads a crash-consistent point in time, and it is also the only way to back up `ReadWriteOncePod` volumes. `Clone` suits drivers that clone faster than they snapshot. For the decision table, see [Copy methods](copy-methods.md#which-should-i-use). For application-_consistent_ backups, which quiesce the app first, see [hooks](backups.md).
- **`defaultDeletionPolicy: Delete`** ties each produced `Snapshot` CR to its kopia snapshot. Delete the CR and the data goes too, so cluster state stays truthful. `Retain` and `Orphan` decouple them, which is useful. Read [the deletionPolicy section](backups.md#deletionpolicy--what-happens-to-the-snapshot) before choosing, because "I deleted the CR but the repository kept growing" and "I deleted the CR and lost the snapshot" are both surprises you get with the wrong setting.

This policy backs up one PVC. Label-selector sources, meaning every PVC matching `app=web` and grouped consistently, and NFS sources are the same resource with a different `sources` entry. [Backups & schedules](backups.md) covers them.

## Step 5 — The SnapshotSchedule (when)

/// tab | S3

```yaml
--8<-- "deploy/examples/walkthrough/s3.yaml:schedule"
```

///

/// tab | Filesystem (NAS)

```yaml
--8<-- "deploy/examples/walkthrough/nas.yaml:schedule"
```

///

This is also identical in both tracks. Why these values:

- **`cron: "H 2 * * *"`.** `H` is a Jenkins-style placeholder. It resolves to a fixed minute derived from the schedule's identity, so this fires at, say, 02:17 _every_ night. Fifty teams writing "nightly at 2" then stop stampeding the repository at 02:00:00, with nobody coordinating. The resolved next firing is pinned to `status.nextSchedule.at`.
- **`jitter: 30m`** spreads the start across a window on top of `H`. It is kinder to the backend when many policies share it, and it costs you nothing for a nightly backup.
- **`runOnCreate: false`**, the default, means applying this manifest does **not** immediately fire a backup. That is what you want under GitOps, where a re-applied manifest shouldn't mean a surprise snapshot at 3 pm. The trade-off is that your first backup waits for tonight, which is why the next step triggers one by hand.
- **`concurrencyPolicy: Forbid`**, the default, skips a firing if the previous one is somehow still running, rather than stacking movers on the same PVC.

```console
$ kubectl -n demo get snapshotschedule app-data-nightly \
    -o jsonpath='{.status.nextSchedule.at}'
2026-06-13T02:17:00Z
```

## Step 6 — First snapshot, the day-2 way

The schedule will produce `Snapshot` CRs nightly. Don't wait for it. Trigger the recipe now, watch it run, and stream the mover's logs, all in one command:

```console
$ kubectl kopiur snapshot now --policy app-data -n demo --wait --logs
```

This creates a `Snapshot` CR with `origin: manual`. It is _exactly_ the object the schedule creates nightly with `origin: scheduled`, so what you just verified is what runs unattended from now on.

Two useful flags: `--tag reason=walkthrough` attaches searchable kopia tags, and `--pin` exempts a snapshot from GFS retention, which is handy before a risky migration.

`--wait` exits 0 on `Succeeded` and 1 on `Failed`, so the same command drops straight into CI and scripts.

Then look at what exists:

```console
$ kubectl kopiur snapshots list -n demo
NAME                            POLICY    ORIGIN  PHASE      SNAPSHOT-ID   SIZE     FILES  START                 AGE
app-data-manual-20260612140012  app-data  manual  Succeeded  a1b2c3d4e5f6  148 MiB  412    2026-06-12T14:00:12Z  1m
```

That view is richer than `kubectl get snapshots`, but the CRs are still ordinary objects, so both views work.

For a failed run, `kubectl kopiur logs snapshot <name> -n demo` replays the mover's logs even after the Job is gone. See [the logs command](cli/backup-restore.md#logs).

## Step 7 — Look inside the repository

Before you trust a restore during a 2 a.m. incident, look at what's actually in a snapshot. This is read-only, and it restores nothing:

```console
$ kubectl kopiur ls app-data-manual-20260612140012 -n demo
$ kubectl kopiur cat app-data-manual-20260612140012 etc/config.yaml -n demo
$ kubectl kopiur browse app-data-manual-20260612140012 -n demo   # interactive: ls/cd/cat/get
```

These run through a short-lived in-cluster **session pod**. It mounts nothing from your workloads and can only read the repository. The [session-pod model](cli/browse.md#the-session-pod-model) explains the security boundary.

Browsing has its own RBAC switch, `rbac.browse` in the chart, which defaults to `true`. Platform teams can turn it off for everyone.

## Step 8 — Restore (prove the round trip)

A backup you've never restored is a hope, not a backup. Restore into a **fresh** PVC and compare, so the original is never touched.

Here the two tracks differ for the second and last time:

/// tab | S3

Object-store backends name an explicit snapshot. As a one-liner, pick the snapshot from `snapshots list`, restore it, and follow it until it's done:

```console
$ kubectl kopiur restore --from-snapshot app-data-manual-20260612140012 \
    --create-pvc app-data-restored --size 1Gi -n demo --wait
```

Or do the same thing declaratively, as a manifest. Paste the `Snapshot` name into `snapshotRef`:

```yaml
--8<-- "deploy/examples/walkthrough/s3.yaml:restore"
```

///

/// tab | Filesystem (NAS)

Filesystem repositories can resolve "the **latest** snapshot for this policy" at restore time, so there is no snapshot name to paste:

```yaml
--8<-- "deploy/examples/walkthrough/nas.yaml:restore"
```

This `fromPolicy` source is what powers **deploy-or-restore**. The same manifests restore data on a fresh cluster and back it up everywhere else. See [example 05](examples.md#example-05--deploy-or-restore-gitops) and the [GitOps guide](gitops.md).

The CLI equivalent is `kubectl kopiur restore --from-policy app-data --create-pvc app-data-restored --size 1Gi -n demo --wait`.

///

/// note | `fromPolicy` resolves "latest" on every backend

Resolving "latest for a policy" lists the repository's snapshots inside the restore Job. So it works on S3 and the other object-store backends just as it does on a filesystem repository, with no controller-side repository mount needed.

You can still name the snapshot explicitly with `snapshotRef` or `--from-snapshot`, or pin an exact ID through the [`identity` source](restores.md#identity--a-raw-kopia-identity), when you don't want "latest".

///

```console
$ kubectl -n demo wait --for=jsonpath='{.status.phase}'=Completed restore/walkthrough-verify --timeout=5m
```

`Completed` means the data is in `app-data-restored`. Mount it in a pod and diff it against the original; that is the real proof.

The restored PVC is deliberately **not** owned by the `Restore`, so deleting the CR afterwards keeps the data.

If your app runs with an `fsGroup` and the restored files come out unreadable, see [restore-side permissions](permissions.md#restore-side-permissions).

## Step 9 — Day-2 operations

Here are the commands you'll actually use after today, one line each. [CLI → Operations](cli/operations.md) has the detail.

- **`kubectl kopiur status -n demo`** gives you one screen: repositories, policies, schedules, in-flight work, and last and next runs. This is the morning-coffee view.
- **`kubectl kopiur doctor -n demo`** is what you run when something is red. It checks the CRDs, the operator, the webhook, repository connectivity, credentials, and stuck work, and it exits 1 if any check fails, so it works in CI.
- **`kubectl kopiur suspend schedule app-data-nightly -n demo`** and **`resume`** pause and unpause firings around upgrades or maintenance windows. They set `spec.suspend`, so GitOps sees the change.
- **`kubectl kopiur maintenance run --repository primary -n demo --wait`** runs compaction and pruning out of band. You normally never need it, because maintenance is managed by default, as Step 3 explained. It exists for "I just deleted a terabyte and want the space back now".

## Teardown

```console
$ kubectl -n demo delete snapshotschedule app-data-nightly
$ kubectl -n demo delete restore walkthrough-verify     # the restored PVC stays
$ kubectl -n demo delete snapshot --all
$ kubectl -n demo delete snapshotpolicy app-data
$ kubectl -n demo delete repository primary
$ helm uninstall kopiur -n kopiur-system
```

/// warning | Deleting a Snapshot deletes its snapshot

With `deletionPolicy: Delete`, which is the default for produced snapshots and what Step 4 chose, removing a `Snapshot` CR runs `kopia snapshot delete` through a finalizer.

That is the lock-step behavior you opted into. Use `Retain` or `Orphan` per snapshot if a CR must go but the data must stay. See [deletionPolicy](backups.md#deletionpolicy--what-happens-to-the-snapshot).

///

## Where to go next

- **[Scenarios](scenarios/index.md)**: the same machinery aimed at specific problems. Protecting a database with hooks, recovering deleted data, disaster recovery, cross-cluster migration, and restore drills.
- **[Repositories & backends](repositories.md)**: `ClusterRepository` for one shared repository across namespaces, and the other six backends.
- **[Repository replication](replication.md)**: mirror the repository to a second backend. This is the "2" in 3-2-1.
- **[Examples](examples.md)**: the per-capability manifest ladder, when you need one specific pattern.
- **[Troubleshooting](troubleshooting.md)**: when a step above doesn't go green.
