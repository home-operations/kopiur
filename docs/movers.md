# Movers, RBAC & credentials

Every backup, restore, snapshot deletion, and repository bootstrap runs in a short-lived Kubernetes **Job** called a **mover**. The mover is where kopia actually executes: it mounts your data, connects to the repository, and streams snapshots. Understanding _where_ a mover runs, and what it needs to be there, explains the two things you must get right for a backup to succeed. Those are a **ServiceAccount** and a **credentials Secret**, both in the workload namespace.

/// info | The one rule to remember

A mover Job runs in the **same namespace as the data it backs up**, not in the operator's namespace. So everything the mover needs, its ServiceAccount and the repository's credential Secret, must exist **in that workload namespace**. Kopiur mints the ServiceAccount for you. The credential Secret is yours to place by default, but you don't have to: turn on [**credential projection**](#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) and Kopiur copies it into each mover namespace for you.

///

## Why movers run in the workload namespace

A backup reads a `PersistentVolumeClaim` (PVC), and a PVC is namespaced, so a Job can only mount a PVC in its own namespace. A `Snapshot` in namespace `media` therefore runs its mover Job in `media`, even when the repository it targets is a cluster-scoped `ClusterRepository` whose definition and credentials live in `kopiur-system`.

That split is what creates the two requirements below.

## The mover ServiceAccount (minted for you)

The mover patches its owning `Snapshot` or `Restore` `.status`, so it needs a ServiceAccount with Role-Based Access Control (RBAC) permissions, and that ServiceAccount must exist in the workload namespace. The operator's own ServiceAccount lives only in the operator namespace. So before each mover Job, the controller **mints**, in the Job's namespace:

- a **`kopiur-mover` ServiceAccount**, and
- a **RoleBinding** tying it to the **`kopiur-mover`** ClusterRole.

/// info | Least privilege

The `kopiur-mover` role grants only what a mover actually uses: `get` and `patch` on the owning CRDs' `/status` subresource, plus `get` and `patch` on the bootstrap-result ConfigMap. It needs `patch` to report progress and the terminal result, and `get` so a retried restore Job can re-read the snapshot a prior attempt recorded in `status.resolved` instead of re-resolving "latest". It does **not** grant the base CRD resources, Secrets, Jobs, Pods, or PVCs, which is a far smaller surface than the operator's own role. A namespace tenant who can read that ServiceAccount's token can do almost nothing with it. See [Privileged movers](#privileged-movers) for the one exception that needs your sign-off.

///

There is one exception. A [`SnapshotReplication`](snapshot-replication.md) mover runs as the dedicated **`kopiur-snapshot-replication-mover`** ServiceAccount instead, minted the same way. It creates the copy `Snapshot` CRs, a grant the ordinary mover deliberately lacks. See the [RBAC reference](rbac.md#the-snapshot-replication-mover-kopiur-snapshot-replication-mover).

You don't create or manage these. They're applied idempotently on every reconcile, labelled `app.kubernetes.io/managed-by: kopiur`. To see them:

```console
$ kubectl get serviceaccount,rolebinding -n media -l app.kubernetes.io/component=mover
NAME                            SECRETS   AGE
serviceaccount/kopiur-mover     0         3m

NAME                                              ROLE                      AGE
rolebinding/kopiur-mover   ClusterRole/kopiur-mover   3m
```

/// note | Names are chart-derived

The names above assume the default Helm release. The chart passes the real names to the controller through `KOPIUR_MOVER_SERVICE_ACCOUNT` and `KOPIUR_MOVER_CLUSTERROLE`, so a release-prefixed install such as `myrel-mover` stays consistent.

///

/// note | Workload identity: your ServiceAccount instead of the minted one

When a repository's cloud backend sets `auth.workloadIdentity.serviceAccountName`
([S3](backends/s3.md#workload-identity-irsa--eks-pod-identity) /
[Azure](backends/azure.md#workload-identity-aks) /
[GCS](backends/gcs.md#workload-identity-gke)), its mover Jobs run as **your**
federated ServiceAccount instead of the minted `kopiur-mover`. The controller never creates or modifies that ServiceAccount, because its cloud annotations are how you set up federation and are yours to own. It does two things instead. It preflights that the ServiceAccount exists, and a missing one surfaces as `CredentialsAvailable=False` naming it. And it applies one extra RoleBinding, `kopiur-mover-wi-<sa>`, tying your ServiceAccount to the same least-privilege `kopiur-mover` role, because the mover still patches its own `.status` at runtime whatever ServiceAccount it runs as.

///

## The credentials Secret

The mover reads the repository password, and any object-store keys, from a Secret mounted into the Job with `envFrom`. **`envFrom` is namespace-local**: it can only reference a Secret in the Job's own namespace. So the credential Secret must exist in the **workload** namespace. You have two ways to get it there:

| Repository kind                      | Self-managed (default)                                                                       | Projection (recommended for shared repos)                                                                       |
| ------------------------------------ | -------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `Repository` (namespaced)            | Nothing extra; the repo and its Secret are already in the workload namespace.               | Not needed (no-op): the Secret is already where the mover runs.                                                 |
| `ClusterRepository` (cluster-scoped) | Place a Secret of the same name in **each** workload namespace that backs up to it.          | Set `credentialProjection.enabled: true` on the `SnapshotPolicy`/`Restore`/`Maintenance` that uses it.            |

/// tip | Don't hand-copy Secrets: turn on projection

If you run a shared `ClusterRepository` across more than a namespace or two, **use credential projection** instead of replicating the Secret by hand. It's one field on the consumer, meaning the `SnapshotPolicy`, `Restore` or `Maintenance`, and you never touch the credential Secret in a workload namespace again. It's **off by default**, because a namespace is a trust boundary and copying across one is opt-in. But for the multi-namespace shared-repo case it's the intended path. See [below](#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos).

///

### Let Kopiur project the credentials Secret (recommended for shared repos)

Set `spec.credentialProjection.enabled: true` on the **consumer**, meaning the `SnapshotPolicy`, and also `Restore` and `Maintenance`, not on the repository. That way the namespace owner opts in, rather than the shared repository pushing its credentials everywhere. Before each mover run, Kopiur reads the referenced repository's credential Secrets from their source namespace and writes a copy into the mover Job's namespace, so the `envFrom` resolves without you placing anything there:

```yaml
--8<-- "deploy/examples/credential-projection-consumer.yaml"
```

Apply it with `kubectl apply -f`. For the full three-part bundle, meaning the
`ClusterRepository` owner gate plus the consuming policy, see
[Example 11: Credential projection](examples.md#example-11--credential-projection).

`Snapshot`s produced from this config, whether manual, scheduled, or discovered, inherit the setting.

/// warning | The repository owner must also allow it (`credentialProjection.allowed`)

For a shared `ClusterRepository`, the consumer's `enabled: true` is **necessary but not sufficient**. Projection into a foreign namespace is **fail-closed**. It needs all three:

1. The repository owner sets `credentialProjection.allowed: true` on the `ClusterRepository` (default **false**).
2. The consumer sets `credentialProjection.enabled: true` (above).
3. The operator has the cluster-wide `secrets` RBAC (`features.credentialProjection.enabled`, below).

A namespaced `Repository` has no such gate. Its repo and Secret co-reside, so projection there is a same-namespace no-op. See [Repositories → credentialProjection.allowed](repositories.md#credentialprojectionallowed--the-owner-gate-for-shared-creds).

///

How the projected copies behave:

- **A copy lives only as long as a mover Job can read it.** Once the run that needed it has finished, kopiur deletes the copy. It does not leave live repository credentials sitting in your app namespaces. The copy does carry an `ownerReference` to its consuming CR, but treat that as a backstop, not the cleanup. A `Snapshot` is retained for your whole retention window, because it owns the kopia snapshot, so waiting for garbage collection would mean waiting months. Copies are reclaimed when a run ends, when a consumer disables projection, and when a backend re-config drops a source Secret. A [periodic sweep](#the-object-sweep) catches anything the reconciler missed.
- **One stable copy per consuming CR.** The copy's name embeds the consumer, not the run: `<snapshot>-creds-N` for a backup, `<restore>-restore-creds-N`, `<maintenance>-maint-creds-N`, `<policy>-vfy-creds-N` for verification, and `<snapshot>-pin-creds-N` or `<snapshot>-delete-creds-N` for the auxiliary movers. Every run of that CR refreshes the **same** Secret in place, so recurring consumers such as a `Maintenance` cron or a verification tier never accumulate copies.
- **Always fresh.** The copy is re-read from source on every run, so rotating the source Secret takes effect on the next backup. There is no long-lived shadow copy to drift.
- **The deletion path projects for itself, against the opt-in the run used.** A backup's copy is reclaimed the moment that run ends, so a `deletionPolicy: Delete` finalizer firing weeks later has nothing to reuse. It projects its own `<snapshot>-delete-creds-N`, reclaimed as soon as the finalizer clears. The opt-in that authorizes that is recorded into `status.resolved.credentialProjection` when the backup runs, so deleting the `SnapshotPolicy` first does **not** strand the `Snapshot`. The recorded value wins over the live recipe, because this path's job is to reproduce the conditions the run executed under. The repository owner's `credentialProjection.allowed` is still resolved **live** and still fails closed, so a recorded value cannot re-open consent that was withdrawn. See [Backups → what `Delete` needs to succeed](backups.md#what-delete-needs-to-succeed).
- **A no-op when not needed.** For a namespaced `Repository` whose Secret already lives in the workload namespace, projection copies nothing. It just verifies the Secret is present, exactly like the self-managed path. It only copies for the genuine cross-namespace case.
- **Labeled kopiur-managed.** Copies carry the labels `app.kubernetes.io/managed-by=kopiur`, `app.kubernetes.io/component=credentials`, and `kopiur.home-operations.com/creds-scope=cr`, which is the stable-name marker. They also carry the annotations `kopiur.home-operations.com/projected-from`, naming the source, and `kopiur.home-operations.com/projected-at`, recording when the last run wrote it. Don't edit them by hand.

/// tip | Watch `kopiur_projected_secrets_live`

This gauge is the number of projected copies alive right now. In a healthy cluster it hovers around the number of runs in flight and returns to roughly zero between them. If it climbs day over day, copies are not being reclaimed. That is the shape of the bug fixed in 0.7.3, and `deriv(kopiur_projected_secrets_live[24h]) > 0` is the alert for it coming back.

///

/// warning | Projection needs the operator's `secrets` create/patch/delete RBAC

To write Secrets into workload namespaces, and to clean its own copies up again, the operator needs cluster-wide `secrets` `create`, `patch` and `delete`. The Helm value `features.credentialProjection.enabled`, **off by default**, grants it. Projection is itself opt-in per consumer, so the chart withholds this broader RBAC until you set `features.credentialProjection.enabled: true`. The trade-off is that `create` cannot be scoped to a Secret name, so the operator can write a Secret in any namespace it manages. While the flag stays at the default `false`, `secrets` access is read-only, and a projection-enabled `SnapshotPolicy`, `Restore` or `Maintenance` surfaces an actionable `403` telling you to enable it. A projected copy in namespace `X` is readable by anything that can read Secrets in `X`, exactly as it would be if you placed it there yourself. See [Feature permissions](feature-permissions.md) for the full mapping of flag to feature.

///

### Or manage the Secret yourself

This is the self-managed default: you place the credential Secret in each workload namespace yourself. The declarative path is a plain `Secret` manifest applied into the workload namespace. It uses the same name and keys the repository's `auth.secretRef` and `encryption.passwordSecretRef` reference, but lives where the mover runs:

```yaml
--8<-- "deploy/examples/workload-credential-secret.yaml"
```

```console
$ kubectl apply -f deploy/examples/workload-credential-secret.yaml
```

You don't have to hand-author it, though. The intended paths for a shared `ClusterRepository` are:

- **[Credential projection](#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos)**, the recommendation. One field on the consumer, and the operator copies the Secret in for you, fresh each run. No manifest to maintain per namespace.
- A **secret-sync controller** such as [External Secrets](https://external-secrets.io), [Reflector](https://github.com/emberstack/kubernetes-reflector), or `kubernetes-replicator`, to mirror the source Secret into each workload namespace from your secret store.

/// note | Ad-hoc one-liner

For a quick, one-off copy from the operator namespace, with no manifest and no sync controller, pipe the live Secret through `sed` to rewrite its namespace and re-apply:

```console
$ kubectl get secret kopia-rustfs-creds -n kopiur-system -o yaml \
    | sed 's/namespace: kopiur-system/namespace: media/' \
    | kubectl apply -n media -f -
```

Prefer projection or a sync controller for anything you'll maintain. This one drifts the moment the source rotates.

///

When the Secret is missing, the `Snapshot` does **not** silently hang. The Secret can be missing because projection is off and you haven't placed it, or because projection is on but the **source** Secret doesn't exist. Either way the `Snapshot` stays `Pending` and reports exactly what's wrong:

```console
$ kubectl get snapshots my-backup -n media \
    -o jsonpath='{.status.conditions[?(@.type=="CredentialsAvailable")].message}'
credentials Secret `kopia-rustfs-creds` does not exist in namespace `media`,
where the mover Job runs and loads it via envFrom — Kubernetes envFrom is
namespace-local and cannot read a Secret from another namespace. The referenced
ClusterRepository `rustfs-primary` keeps that Secret in namespace `kopiur-system`...
Fix: create a Secret named `kopia-rustfs-creds` in namespace `media`...
```

Place the Secret and the condition clears to `CredentialsAvailable=True` on the next reconcile. The backup then proceeds.

## Privileged movers

By default movers run unprivileged. Some workloads need an elevated mover, most commonly a **root** mover (`spec.mover.securityContext.runAsUser: 0`) to read files an unprivileged user can't. Because the controller mints a ServiceAccount in the workload namespace, a tenant with access there could reuse it to run pods at the mover's privilege. Granting that is therefore a **per-namespace admin decision**, gated by an annotation, exactly like VolSync's `volsync.backube/privileged-movers`.

For the full mover `securityContext` surface, covering the hardened default, setting or inheriting the UID and GID, and the complex cases, see [The mover security context](security-context.md).

The gate applies to **every** kind that runs a mover: a `SnapshotPolicy`'s `spec.mover`, a `Restore`'s `spec.mover`, and a `Maintenance`'s `spec.mover` alike. It also applies to a context **inherited** from a workload pod through `inheritSecurityContextFrom`, because the resolved context is what gets checked, so an inherited-root mover is gated too. If `spec.mover` requests privilege and the namespace has **not** opted in, the `Snapshot`, `Restore` or `Maintenance` is refused with a clear condition. "Requests privilege" means any of `runAsUser: 0`, `privileged: true`, `allowPrivilegeEscalation: true`, added Linux capabilities, `runAsNonRoot: false`, or `privilegedMode: true`.

```console
$ kubectl get snapshots my-backup -n media \
    -o jsonpath='{.status.conditions[?(@.type=="MoverPermitted")]}'
{"type":"MoverPermitted","status":"False","reason":"PrivilegedMoverNotPermitted",
 "message":"SnapshotPolicy `my-config` requests a privileged mover ... namespace
 `media` has not opted in ... kubectl annotate namespace media
 kopiur.home-operations.com/privileged-movers=true ..."}
```

A cluster admin opts the namespace in by applying the annotated `Namespace`:

```yaml
--8<-- "deploy/examples/privileged-mover-namespace.yaml"
```

```console
$ kubectl apply -f deploy/examples/privileged-mover-namespace.yaml
```

Or imperatively: `kubectl annotate namespace media kopiur.home-operations.com/privileged-movers=true`.

On the next reconcile `MoverPermitted` clears to `True` and the privileged mover runs. To revoke, remove the annotation, or drop the elevated `securityContext` from the `SnapshotPolicy`, `Restore` or `Maintenance`.

/// tip | Prefer unprivileged when you can

Reach for a privileged mover only when a workload genuinely needs it, for example an app that writes files as root. Many sources back up fine unprivileged, and an unprivileged mover keeps the minted ServiceAccount's blast radius minimal. Before going root, try matching the mover's UID and GID to the data owner. See [Permissions, UID & GID](permissions.md).

///

## Putting it together: a ClusterRepository backup in a workload namespace

To back up a PVC in `media` to a shared `ClusterRepository` whose Secret lives in `kopiur-system`, with a root mover:

1. **Credentials**: place the repo Secret in `media`, or turn on [credential projection](#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) and skip this step:
    ```console
    $ kubectl apply -f deploy/examples/workload-credential-secret.yaml
    ```
    (See [the manifest](#or-manage-the-secret-yourself); or copy ad-hoc with `kubectl get secret … -o yaml | sed … | kubectl apply -f -`.)
2. **Privilege opt-in**, only if the mover runs as root:
    ```console
    $ kubectl apply -f deploy/examples/privileged-mover-namespace.yaml
    ```
    (Or imperatively: `kubectl annotate namespace media kopiur.home-operations.com/privileged-movers=true`.)
3. **Apply** your `SnapshotPolicy` and `Snapshot`, or a `SnapshotSchedule`, in `media`. The controller mints the `kopiur-mover` ServiceAccount and RoleBinding, both gates pass, and the mover Job runs.
4. **Watch it**:
    ```console
    $ kubectl get snapshots -n media -w        # Pending → Running → Succeeded
    ```

## Try it end-to-end

Prove the privileged-mover gate from a clean slate. You get a namespace that has **opted in**, a PVC seeded with **root-owned `0600`** files, which is data an unprivileged mover can't read, and a **root** `SnapshotPolicy`. The backup goes green and the mover pod really runs as UID `0`, and it would have been refused without the opt-in.

This is one apply-ready bundle, [`deploy/examples/tryit/movers-privileged.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/movers-privileged.yaml), containing the opted-in `media` `Namespace`, a PVC, a seed Job, an S3 `Repository`, a root-mover `SnapshotPolicy`, and a manual `Snapshot`.

The opt-in is the piece everything else depends on. Without it the root Snapshot is refused with `MoverPermitted=False`:

```yaml
--8<-- "deploy/examples/tryit/movers-privileged.yaml:namespace"
```

The seed writes root-owned, owner-only files, the case that actually needs a root mover:

```yaml
--8<-- "deploy/examples/tryit/movers-privileged.yaml:seed"
```

And the policy runs the mover as root, with `runAsUser: 0` and `runAsNonRoot: false`:

```yaml
--8<-- "deploy/examples/tryit/movers-privileged.yaml:policy"
```

**1. Fill in the credentials** (`AWS_*` keys + a `KOPIA_PASSWORD`) in the `secret` section, then apply the whole bundle: namespace, PVC, seed Job, Secret, Repository, and SnapshotPolicy in one shot:

```console
$ kubectl apply -f deploy/examples/tryit/movers-privileged.yaml
```

**2. Wait for the repository and the seed**:

```console
$ kubectl -n media wait --for=condition=Ready repository/primary --timeout=2m
$ kubectl -n media wait --for=condition=complete job/seed-app-data --timeout=2m
```

**3. Take the backup.** The `Snapshot` uses `generateName`, so `create` it, not `apply`:

```console
$ kubectl create -f deploy/examples/tryit/movers-privileged.yaml
snapshot.kopiur.home-operations.com/app-data-manual-abc12 created

$ kubectl -n media wait --for=jsonpath='{.status.phase}'=Succeeded \
    snapshot/app-data-manual-abc12 --timeout=5m
```

**4. Prove it (deep).** Confirm the privilege gate passed, the run succeeded, and the mover pod really ran as root:

```console
# the gate cleared because the namespace opted in:
$ kubectl -n media get snapshot app-data-manual-abc12 \
    -o jsonpath='{.status.conditions[?(@.type=="MoverPermitted")].status}{"\n"}'
True

# the mover Job is named after the Snapshot CR (no -snap suffix):
$ kubectl -n media get snapshot app-data-manual-abc12 -o jsonpath='{.status.job.name}{"\n"}'
app-data-manual-abc12

# its pod, by the standard Job-managed selector:
$ kubectl -n media get pods --selector=job-name=app-data-manual-abc12

# the mover container's effective UID — 0 (root):
$ kubectl -n media get pod <mover-pod> \
    -o jsonpath='{.spec.containers[0].securityContext.runAsUser}{"\n"}'
0
```

/// note | Illustrative names

`app-data-manual-abc12` and `<mover-pod>` stand in for the server-generated names your run gets. Substitute the names `kubectl create` and `kubectl get pods` print.

///

**See it refuse without the opt-in.** Remove the annotation and re-run to watch the gate close:

```console
$ kubectl annotate namespace media kopiur.home-operations.com/privileged-movers-
$ kubectl create -f deploy/examples/tryit/movers-privileged.yaml
$ kubectl -n media get snapshot <new-name> \
    -o jsonpath='{.status.conditions[?(@.type=="MoverPermitted")]}{"\n"}'
{"type":"MoverPermitted","status":"False","reason":"PrivilegedMoverNotPermitted", ...}
```

Re-add the annotation, either by re-applying the bundle's `namespace` section or with `kubectl annotate … =true`, and the blocked Snapshot proceeds within seconds. No re-apply needed.

## Extra pod labels & annotations

Cluster machinery frequently keys off pod metadata that Kopiur has no field of its own for: a queueing system's queue name, a `NetworkPolicy` or monitoring selector, a service-mesh exclusion. `moverDefaults.podLabels` and `moverDefaults.podAnnotations` on the `Repository` or `ClusterRepository` put arbitrary keys on **every** mover that repository spawns:

```yaml
spec:
    moverDefaults:
        podLabels:
            kueue.x-k8s.io/queue-name: backups
            egress-to-object-store: "true"
        podAnnotations:
            sidecar.istio.io/inject: "false"
```

Both maps merge **under** Kopiur's own metadata, so a key Kopiur sets always wins. Nothing you put here can break the selectors the controller counts and reaps by. A key that silently lost would mean your manifest claims a label the pod never carries, so instead of dropping such keys at render time, admission **rejects** them: keys under `kopiur.home-operations.com/` and the exact key `app.kubernetes.io/managed-by`.

/// note | Labels reach the Job too; annotations are pod-template-only

`podLabels` are mirrored onto the `Job` object *and* its pod template, because label selectors are how you find a workload's Jobs from the outside. `podAnnotations` are applied to the **pod template only**. That asymmetry is deliberate. The canonical annotation is a sidecar-injection opt-out such as `sidecar.istio.io/inject: "false"`, `linkerd.io/inject: disabled`, or `vault.hashicorp.com/agent-inject: "false"`, and that only means anything on the pod a mesh's webhook actually sees. It matters for movers specifically: a mover is a short-lived batch pod, and an injected sidecar that never exits keeps its Job running forever.

///

/// note | A `kubectl kopiur browse` session takes the annotations but not the labels

An interactive [browse/serve session](server.md) pod applies `podAnnotations` and deliberately ignores `podLabels`. The reason each way round is the same one. An injected sidecar matters *more* for a session than for a batch mover, because it would outlive the session's TTL and leave a pod running after your command returned. Meanwhile the canonical `podLabels` value routes work through a batch queueing system, and a human waiting at a terminal must not be queued behind a nightly backup window.

///

### Putting movers under a queueing system (Kueue)

`podLabels` is how you hand mover Jobs to [Kueue](https://kueue.sigs.k8s.io/) or a similar batch admission controller:

```yaml
spec:
    moverDefaults:
        podLabels:
            kueue.x-k8s.io/queue-name: backups # a LocalQueue in the mover's namespace
```

This composes correctly with a repository's own [`concurrency.maxConcurrentJobs`](repositories.md#concurrency--cap-the-mover-jobs-one-repository-runs-at-once). Kueue admits work by flipping `spec.suspend` on the Job, and a **suspended Job has no pod and occupies no slot** in Kopiur's pool. Without that rule the two systems would deadlock each other: Kopiur would park new runs behind Jobs that Kueue was holding, forever.

/// warning | `manageJobsWithoutQueueName` will swallow every mover, labeled or not

If your Kueue is configured with `manageJobsWithoutQueueName: true`, it suspends **every** Job in scope. That includes the mover Jobs Kopiur creates for repositories where you never set a queue-name label, and the ones you did not intend to queue. Those Jobs then wait for a Kueue admission that may never come. Because a suspended Job holds no slot, Kopiur sees no queue and reports nothing wrong: backups simply never start.

The `Snapshot` stays `Pending` with its Job created but suspended. Either leave `manageJobsWithoutQueueName: false`, the default, and label deliberately, or make sure every repository whose movers land in Kueue's scope sets a `kueue.x-k8s.io/queue-name` that resolves to a real `LocalQueue` with quota.

///

The complete tuned repository, combining concurrency, `scheduleDefaults`, and these labels, is [example 43](repositories.md#a-tuned-repository-end-to-end).

## Run artifacts & cleanup

A mover run is exactly **one Kubernetes object: the `Job`**. The controller embeds the run's instructions, the *work spec*, as serialized JSON directly in the Job's pod environment as `KOPIUR_WORK_SPEC`. There is no per-run ConfigMap, Secret, or other sidecar object. That means:

- **One clock cleans up everything.** The Job, and its pod, lives until its `ttlSecondsAfterFinished`, 1 hour by default. You can tune that per recipe with `spec.mover.ttlSecondsAfterFinished` or repository-wide with `spec.moverDefaults.ttlSecondsAfterFinished`. Kopiur never deletes a finished Job early, because the pod logs are your debugging record, and `kubectl kopiur logs` resolves them through the Job for as long as the TTL keeps it around.
- **Everything the controller told the mover is inspectable in one place.** `kubectl get job <name> -o yaml` shows the pod template *and* the exact work spec the mover ran, for the Job's whole lifetime. The work spec never contains credentials; those reach the mover through `envFrom` Secret references.

The one exception is repository **bootstrap and probe** runs, which additionally create a small result `ConfigMap`, named the same as the Job, that the mover writes its outcome into. That is needed because the controller may read the result after the Job is already gone. It is one fixed-name object per repository, consumed and deleted by the controller, so it cannot accumulate.

/// note | Why not a ConfigMap?
Operator versions up to 0.7.0 mounted the work spec from a per-run ConfigMap. The Job self-reaped through its TTL, but ConfigMaps have no TTL mechanism and were owner-referenced to long-lived CRs. A `Snapshot` is the durable record of a backup, and a `SnapshotPolicy` never goes away, so one ConfigMap leaked per run, forever. Issue #224 reported 605 of them in one cluster. Embedding the spec in the Job removes the second object, and with it the entire leak class.
///

### The object sweep

One leader-only background sweep reclaims objects a run no longer needs. It is a **steady-state safety net**, not just an upgrade tool. The reconcilers reclaim their own objects as each run ends, and the sweep catches whatever they missed: a controller that crashed mid-cleanup, a `Snapshot` deleted while the operator was down, or an RBAC grant that was briefly absent.

It handles three things:

- **Projected credential copies of finished runs.** A copy is only needed while its mover Job can load it through `envFrom`. Once the owning `Snapshot` has reached `Succeeded` or `Failed` and no live Job still references the copy, it is deleted. The copies of long-lived consumers, such as a `Maintenance` cron or a verification tier, are **not** touched between their runs. Those are already one per CR and are refreshed in place.
- **Per-run projected credential Secrets** (versions ≤ 0.7.1). Older versions named copies after each mover *run*, as `<job>-creds-N`, so recurring consumers accumulated one live credential copy per run. The sweep deletes kopiur-managed, `projected-from`-annotated Secrets that lack the stable-name marker, are loaded by no live mover Job, and are past the minimum age. The kopia web-UI credential mirrors and user Secrets can never match those conditions, so they are never touched.
- **Per-run work-spec ConfigMaps** (versions ≤ 0.7.0). Work-spec ConfigMaps, identified by the data key `work-spec.json`, with **no same-named Job** and past the minimum age, are deleted. Bootstrap result ConfigMaps, and anything a live legacy run still mounts, are never touched.

Deleting Secrets depends on the `features.credentialProjection.enabled` RBAC grant. If you used projection and later disabled the flag, the sweep logs an actionable warning naming it instead of deleting.

Backlogs drain automatically on upgrade, with no `kubectl` cleanup needed. Two environment variables tune it, set through the chart's controller `extraEnv`:

| Variable | Default | Meaning |
| --- | --- | --- |
| `KOPIUR_WORK_SPEC_SWEEP_INTERVAL_SECS` | `21600` (6h) | Sweep cadence; `0` disables the sweep entirely. |
| `KOPIUR_WORK_SPEC_SWEEP_MIN_AGE_SECS` | `3600` (1h) | Minimum object age before it may be reaped. Credential copies additionally have a hard 15-minute floor, so setting this to `0` cannot expose the window in which a copy exists but its Job does not yet. |

Each pass increments `kopiur_work_spec_cms_swept_total`, `kopiur_projected_secrets_swept_total`, and `kopiur_creds_secrets_reaped_total{by="sweep"}` on `/metrics`. That last one is worth an alert. In steady state the reconciler should reclaim a copy long before the sweep sees it, so a **sustained** `by="sweep"` rate means the reconciler's own cleanup has stopped firing. Don't sum it with `by="terminal"`; keeping the two apart is the whole point.

## Troubleshooting

The mover preconditions surface on the `Snapshot` or `Restore` status as conditions **and** as `Warning` Events, visible in `kubectl describe`. So you never have to read controller logs to find out why a backup didn't start.

| Symptom                                                                         | Condition / Event                                         | Cause                                                           | Fix                                                               |
| ------------------------------------------------------------------------------- | --------------------------------------------------------- | --------------------------------------------------------------- | ----------------------------------------------------------------- |
| Backup stuck `Pending`, no Job                                                  | `CredentialsAvailable=False` / `MissingCredentialsSecret` | The credential Secret isn't in the workload namespace.          | Create the Secret there (replicate it for a `ClusterRepository`). |
| Backup stuck `Pending`, no Job                                                  | `MoverPermitted=False` / `PrivilegedMoverNotPermitted`    | The mover requests privilege but the namespace hasn't opted in. | Annotate the namespace, or drop the elevated `securityContext`.   |
| Job created but pod never appears, `FailedCreate: serviceaccount ... not found` | (pre-fix only)                                            | The mover ServiceAccount isn't in the namespace.                | Upgrade the operator; it now mints the ServiceAccount automatically. |

/// info | Where to look

- `kubectl describe snapshot <name> -n <ns>`, or `restore` or `maintenance`, shows conditions **and** Events in one place.
- `kubectl get serviceaccount,rolebinding -n <ns> -l app.kubernetes.io/component=mover` confirms the mover RBAC was minted.
- Find the mover Job, then its pod. For a `Snapshot` the Job name is in its status: `kubectl get snapshot <name> -n <ns> -o jsonpath='{.status.job.name}'`. For a `Restore` the mover Job is named after the `Restore` itself. Either way, list the pod with the standard Job-managed selector: `kubectl get pods -n <ns> --selector=job-name=<job-name>`. To list **all** of a policy's snapshot mover Jobs and pods at once, use the policy label: `kubectl get jobs,pods -n <ns> -l kopiur.home-operations.com/config=<policy-name>`.

///

## Quick reference

| Thing                              | Value                                                                                                                 |
| ---------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| Minted mover ServiceAccount / role | `kopiur-mover` (release-prefixed)                                                                                     |
| Privileged-mover opt-in annotation | `kopiur.home-operations.com/privileged-movers: "true"` (on the **Namespace**)                                         |
| Credentials condition              | `CredentialsAvailable` (reason `MissingCredentialsSecret`)                                                            |
| Privilege condition                | `MoverPermitted` (reason `PrivilegedMoverNotPermitted`)                                                               |
| Operator RBAC needed to mint       | `serviceaccounts: [create, get]`, `rolebindings: [get, create, update, patch]`, `namespaces: [get]` (cluster install) |
