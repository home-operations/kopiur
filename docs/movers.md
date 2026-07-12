# Movers, RBAC & credentials

Every backup, restore, snapshot deletion, and repository bootstrap runs in a short-lived Kubernetes **Job** called a **mover**. The mover is where kopia actually executes: it mounts your data, connects to the repository, and streams snapshots. Understanding _where_ a mover runs — and what it needs to be there — explains the two things you must get right for a backup to succeed: a **ServiceAccount** and a **credentials Secret**, both in the workload namespace.

/// info | The one rule to remember

A mover Job runs in the **same namespace as the data it backs up** — not in the operator's namespace. So everything the mover needs (its ServiceAccount and the repository's credential Secret) must exist **in that workload namespace**. Kopiur mints the ServiceAccount for you. The credential Secret is yours to place by default — but you don't have to: flip on [**credential projection**](#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) and Kopiur copies it into each mover namespace for you.

///

## Why movers run in the workload namespace

A backup reads a `PersistentVolumeClaim`, and a PVC is namespaced — a Job can only mount a PVC in its own namespace. So a `Snapshot` in namespace `media` runs its mover Job in `media`, even when the repository it targets is a cluster-scoped `ClusterRepository` whose definition and credentials live in `kopiur-system`.

That split is the source of the two requirements below.

## The mover ServiceAccount (minted for you)

The mover patches its owning `Snapshot`/`Restore` `.status`, so it needs a ServiceAccount with RBAC — and that SA must exist in the workload namespace. The operator's own ServiceAccount lives only in the operator namespace, so before each mover Job the controller **mints**, in the Job's namespace:

- a **`kopiur-mover` ServiceAccount**, and
- a **RoleBinding** tying it to the **`kopiur-mover`** ClusterRole.

/// info | Least privilege

The `kopiur-mover` role grants only what a mover actually uses: `patch` on the owning CRDs' `/status` subresource and on the bootstrap-result ConfigMap. It does **not** grant Secrets, Jobs, Pods, or PVCs — a far smaller surface than the operator's own role. A namespace tenant who can read that SA's token can do almost nothing with it (see [Privileged movers](#privileged-movers) for the one exception that needs your sign-off).

///

You don't create or manage these — they're applied idempotently on every reconcile, labelled `app.kubernetes.io/managed-by: kopiur`. To see them:

```console
$ kubectl get serviceaccount,rolebinding -n media -l app.kubernetes.io/component=mover
NAME                            SECRETS   AGE
serviceaccount/kopiur-mover     0         3m

NAME                                              ROLE                      AGE
rolebinding/kopiur-mover   ClusterRole/kopiur-mover   3m
```

/// note | Names are chart-derived

The names above assume the default Helm release. The chart passes the real names to the controller via `KOPIUR_MOVER_SERVICE_ACCOUNT` and `KOPIUR_MOVER_CLUSTERROLE`, so a release-prefixed install (e.g. `myrel-mover`) stays consistent.

///

/// note | Workload identity: your SA instead of the minted one

When a repository's cloud backend sets `auth.workloadIdentity.serviceAccountName`
([S3](backends/s3.md#workload-identity-irsa--eks-pod-identity) /
[Azure](backends/azure.md#workload-identity-aks) /
[GCS](backends/gcs.md#workload-identity-gke)), its mover Jobs run as **your**
federated ServiceAccount instead of the minted `kopiur-mover`. The controller
never creates or modifies that SA (its cloud annotations are your federation
contract) — it preflights its existence (a missing SA surfaces as
`CredentialsAvailable=False` naming it) and applies one extra RoleBinding,
`kopiur-mover-wi-<sa>`, tying your SA to the same least-privilege
`kopiur-mover` role: the mover still patches its own `.status` at runtime,
whatever SA it runs as.

///

## The credentials Secret

The mover reads the repository password (and any object-store keys) from a Secret, mounted into the Job with `envFrom`. **`envFrom` is namespace-local** — it can only reference a Secret in the Job's own namespace. So the credential Secret must exist in the **workload** namespace. You have two ways to get it there:

| Repository kind                      | Self-managed (default)                                                                       | Projection (recommended for shared repos)                                                                       |
| ------------------------------------ | -------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `Repository` (namespaced)            | Nothing extra — the repo and its Secret are already in the workload namespace.               | Not needed (no-op): the Secret is already where the mover runs.                                                 |
| `ClusterRepository` (cluster-scoped) | Place a Secret of the same name in **each** workload namespace that backs up to it.          | Set `credentialProjection.enabled: true` on the `SnapshotPolicy`/`Restore`/`Maintenance` that uses it.            |

/// tip | Don't hand-copy Secrets — turn on projection

If you run a shared `ClusterRepository` across more than a namespace or two, **use credential projection** instead of replicating the Secret by hand. It's one field on the consumer (`SnapshotPolicy`/`Restore`/`Maintenance`) and you never touch the credential Secret in a workload namespace again. It's **off by default** (a namespace is a trust boundary, so cross-namespace copying is opt-in), but for the multi-namespace shared-repo case it's the intended path — see [below](#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos).

///

### Let Kopiur project the credentials Secret (recommended for shared repos)

Set `spec.credentialProjection.enabled: true` on the **consumer** — the `SnapshotPolicy` (also on `Restore` and `Maintenance`), not the repository. The namespace owner opts in, rather than the shared repository pushing its creds everywhere. Before each mover run, Kopiur reads the referenced repository's credential Secret(s) from their source namespace and writes a copy into the mover Job's namespace — so the `envFrom` resolves without you placing anything there:

```yaml
--8<-- "deploy/examples/credential-projection-consumer.yaml"
```

Apply it with `kubectl apply -f`. For the full three-part bundle (the
`ClusterRepository` owner gate plus the consuming policy), see
[Example 11 — Credential projection](examples.md#example-11--credential-projection).

`Snapshot`s produced from this config (manual, scheduled, or discovered) inherit the setting.

/// warning | The repository owner must also allow it (`credentialProjection.allowed`)

For a shared `ClusterRepository`, the consumer's `enabled: true` is **necessary but not sufficient**. Projection into a foreign namespace is **fail-closed** — it needs all three:

1. The repository owner sets `credentialProjection.allowed: true` on the `ClusterRepository` (default **false**).
2. The consumer sets `credentialProjection.enabled: true` (above).
3. The operator has the cluster-wide `secrets` RBAC (`features.credentialProjection.enabled`, below).

A namespaced `Repository` has no such gate — its repo and Secret co-reside, so projection there is a same-namespace no-op. See [Repositories → credentialProjection.allowed](repositories.md#credentialprojectionallowed--the-owner-gate-for-shared-creds).

///

How the projected copies behave:

- **A copy lives only as long as a mover Job can read it.** Once the run that needed it has finished, kopiur deletes the copy — it does not leave live repository credentials sitting in your app namespaces. The copy does carry an `ownerReference` to its consuming CR, but treat that as a backstop, not the cleanup: a `Snapshot` is retained for your whole retention window (it owns the kopia snapshot), so waiting for garbage collection would mean waiting months. Copies are reclaimed when a run ends, when a consumer disables projection, and when a backend re-config drops a source Secret; a [periodic sweep](#the-object-sweep) catches anything the reconciler missed.
- **One stable copy per consuming CR.** The copy's name embeds the consumer, not the run: `<snapshot>-creds-N` for a backup, `<restore>-restore-creds-N`, `<maintenance>-maint-creds-N`, `<policy>-vfy-creds-N` for verification, `<snapshot>-pin-creds-N` / `<snapshot>-delete-creds-N` for the auxiliary movers. Every run of that CR refreshes the **same** Secret in place, so recurring consumers (a `Maintenance` cron, verification tiers) never accumulate copies.
- **Always fresh.** The copy is re-read from source on every run, so rotating the source Secret takes effect on the next backup. There is no long-lived shadow copy to drift.
- **A no-op when not needed.** For a namespaced `Repository` whose Secret already lives in the workload namespace, projection copies nothing — it just verifies the Secret is present, exactly like the self-managed path. It only copies for the genuine cross-namespace case.
- **Labeled kopiur-managed.** Copies carry `app.kubernetes.io/managed-by=kopiur`, `app.kubernetes.io/component=credentials`, and `kopiur.home-operations.com/creds-scope=cr` (the stable-name marker), plus `kopiur.home-operations.com/projected-from` (the source) and `kopiur.home-operations.com/projected-at` (when the last run wrote it) annotations — don't edit them by hand.

/// tip | Watch `kopiur_projected_secrets_live`

This gauge is the number of projected copies alive right now. In a healthy cluster it hovers around the number of runs in flight and returns to roughly zero between them. If it climbs day over day, copies are not being reclaimed — that is the shape of the bug fixed in 0.7.3, and `deriv(kopiur_projected_secrets_live[24h]) > 0` is the alert for it coming back.

///

/// warning | Projection needs the operator's `secrets` create/patch/delete RBAC

To write Secrets into workload namespaces (and clean its own copies up again), the operator needs cluster-wide `secrets` `create`/`patch`/`delete`. The Helm value `features.credentialProjection.enabled` (**off by default**) grants it. Projection is itself opt-in per-consumer, so the chart withholds this broader RBAC until you set `features.credentialProjection.enabled: true`. The trade-off: `create` cannot be scoped to a Secret name, so the operator can write a Secret in any namespace it manages. While it stays at the default `false`, `secrets` access is read-only — and a projection-enabled `SnapshotPolicy`/`Restore`/`Maintenance` surfaces an actionable `403` telling you to enable it. A projected copy in namespace `X` is readable by anything that can read Secrets in `X` — exactly as it would be if you placed it there yourself. See [Feature permissions](feature-permissions.md) for the full flag↔feature mapping.

///

### Or manage the Secret yourself

The self-managed default: place the credential Secret in each workload namespace yourself. The declarative path is a plain `Secret` manifest applied into the workload namespace — same name and keys the repository's `auth.secretRef`/`encryption.passwordSecretRef` reference, but living where the mover runs:

```yaml
--8<-- "deploy/examples/workload-credential-secret.yaml"
```

```console
$ kubectl apply -f deploy/examples/workload-credential-secret.yaml
```

You don't have to hand-author it, though. The intended paths for a shared `ClusterRepository` are:

- **[Credential projection](#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos)** (recommended) — one field on the consumer and the operator copies the Secret in for you, fresh each run. No manifest to maintain per namespace.
- A **secret-sync controller** — [External Secrets](https://external-secrets.io), [Reflector](https://github.com/emberstack/kubernetes-reflector), or `kubernetes-replicator` — to mirror the source Secret into each workload namespace from your secret store.

/// note | Ad-hoc one-liner

For a quick, one-off copy from the operator namespace (no manifest, no sync controller), pipe the live Secret through `sed` to rewrite its namespace and re-apply:

```console
$ kubectl get secret kopia-rustfs-creds -n kopiur-system -o yaml \
    | sed 's/namespace: kopiur-system/namespace: media/' \
    | kubectl apply -n media -f -
```

Prefer projection or a sync controller for anything you'll maintain — this drifts the moment the source rotates.

///

When the Secret is missing — projection off and you haven't placed it, or projection on but the **source** Secret doesn't exist — the `Snapshot` does **not** silently hang. It stays `Pending` and reports exactly what's wrong:

```console
$ kubectl get snapshots my-backup -n media \
    -o jsonpath='{.status.conditions[?(@.type=="CredentialsAvailable")].message}'
credentials Secret `kopia-rustfs-creds` does not exist in namespace `media`,
where the mover Job runs and loads it via envFrom — Kubernetes envFrom is
namespace-local and cannot read a Secret from another namespace. The referenced
ClusterRepository `rustfs-primary` keeps that Secret in namespace `kopiur-system`...
Fix: create a Secret named `kopia-rustfs-creds` in namespace `media`...
```

Place the Secret and the condition clears to `CredentialsAvailable=True` on the next reconcile; the backup proceeds.

## Privileged movers

By default movers run unprivileged. Some workloads need an elevated mover — most commonly a **root** mover (`spec.mover.securityContext.runAsUser: 0`) to read files an unprivileged user can't. Because the controller mints a ServiceAccount in the workload namespace, a tenant with access there could reuse it to run pods at the mover's privilege. Granting that is therefore a **per-namespace admin decision**, gated by an annotation — exactly like VolSync's `volsync.backube/privileged-movers`.

For the full mover `securityContext` surface — the hardened default, setting or inheriting the UID/GID, and the complex cases — see [The mover security context](security-context.md).

The gate applies to **every** kind that runs a mover — a `SnapshotPolicy`'s `spec.mover`, a `Restore`'s `spec.mover`, and a `Maintenance`'s `spec.mover` alike — including a context **inherited** from a workload pod via `inheritSecurityContextFrom` (the resolved context is what's checked, so an inherited-root mover is gated too). If `spec.mover` requests privilege (any of `runAsUser: 0`, `privileged: true`, `allowPrivilegeEscalation: true`, added Linux capabilities, `runAsNonRoot: false`, or `privilegedMode: true`) and the namespace has **not** opted in, the `Snapshot`/`Restore`/`Maintenance` is refused with a clear condition:

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

On the next reconcile `MoverPermitted` clears to `True` and the privileged mover runs. To revoke, remove the annotation (or drop the elevated `securityContext` from the `SnapshotPolicy`/`Restore`/`Maintenance`).

/// tip | Prefer unprivileged when you can

Reach for a privileged mover only when a workload genuinely needs it (e.g. an app that writes files as root). Many sources back up fine unprivileged, and an unprivileged mover keeps the minted ServiceAccount's blast radius minimal. Before going root, try matching the mover's UID/GID to the data owner — see [Permissions, UID & GID](permissions.md).

///

## Putting it together: a ClusterRepository backup in a workload namespace

To back up a PVC in `media` to a shared `ClusterRepository` whose Secret lives in `kopiur-system`, with a root mover:

1. **Credentials** — place the repo Secret in `media` (or turn on [credential projection](#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos) and skip this step):
    ```console
    $ kubectl apply -f deploy/examples/workload-credential-secret.yaml
    ```
    (See [the manifest](#or-manage-the-secret-yourself); or copy ad-hoc with `kubectl get secret … -o yaml | sed … | kubectl apply -f -`.)
2. **Privilege opt-in** (only if the mover runs as root):
    ```console
    $ kubectl apply -f deploy/examples/privileged-mover-namespace.yaml
    ```
    (Or imperatively: `kubectl annotate namespace media kopiur.home-operations.com/privileged-movers=true`.)
3. **Apply** your `SnapshotPolicy` + `Snapshot` (or `SnapshotSchedule`) in `media`. The controller mints `kopiur-mover` SA + RoleBinding, both gates pass, and the mover Job runs.
4. **Watch it**:
    ```console
    $ kubectl get snapshots -n media -w        # Pending → Running → Succeeded
    ```

## Try it end-to-end

Prove the privileged-mover gate from a clean slate: a namespace that has **opted in**, a PVC seeded with **root-owned `0600`** files (data an unprivileged mover can't read), and a **root** `SnapshotPolicy`. The backup goes green and the mover pod really runs as UID `0` — and would have been refused without the opt-in.

This is one apply-ready bundle, [`deploy/examples/tryit/movers-privileged.yaml`](https://github.com/home-operations/kopiur/blob/main/deploy/examples/tryit/movers-privileged.yaml): the opted-in `media` `Namespace`, a PVC, a seed Job, an S3 `Repository`, a root-mover `SnapshotPolicy`, and a manual `Snapshot`.

The opt-in is the load-bearing piece — without it the root Snapshot is refused with `MoverPermitted=False`:

```yaml
--8<-- "deploy/examples/tryit/movers-privileged.yaml:namespace"
```

The seed writes root-owned, owner-only files — the case that actually needs a root mover:

```yaml
--8<-- "deploy/examples/tryit/movers-privileged.yaml:seed"
```

And the policy runs the mover as root (`runAsUser: 0`, `runAsNonRoot: false`):

```yaml
--8<-- "deploy/examples/tryit/movers-privileged.yaml:policy"
```

**1. Fill in the credentials** (`AWS_*` keys + a `KOPIA_PASSWORD`) in the `secret` section, then apply the whole bundle — namespace, PVC, seed Job, Secret, Repository, and SnapshotPolicy in one shot:

```console
$ kubectl apply -f deploy/examples/tryit/movers-privileged.yaml
```

**2. Wait for the repository and the seed**:

```console
$ kubectl -n media wait --for=condition=Ready repository/primary --timeout=2m
$ kubectl -n media wait --for=condition=complete job/seed-app-data --timeout=2m
```

**3. Take the backup.** The `Snapshot` uses `generateName`, so `create` it (not `apply`):

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

Re-add the annotation (re-apply the bundle's `namespace` section, or `kubectl annotate … =true`) and the blocked Snapshot proceeds within seconds — no re-apply needed.

## Run artifacts & cleanup

A mover run is exactly **one Kubernetes object: the `Job`**. The controller embeds the run's instructions (the *work spec*, serialized JSON) directly in the Job's pod environment as `KOPIUR_WORK_SPEC` — there is no per-run ConfigMap, Secret, or other sidecar object. That means:

- **One clock cleans up everything.** The Job (and its pod) lives until its `ttlSecondsAfterFinished` — 1 hour by default, tunable per recipe (`spec.mover.ttlSecondsAfterFinished`) or repository-wide (`spec.moverDefaults.ttlSecondsAfterFinished`). Kopiur never deletes a finished Job early: the pod logs are your debugging record, and `kubectl kopiur logs` resolves them through the Job for as long as the TTL keeps it around.
- **The full controller→mover contract is inspectable in one place**: `kubectl get job <name> -o yaml` shows the pod template *and* the exact work spec the mover ran, for the Job's whole lifetime. The work spec never contains credentials — those reach the mover via `envFrom` Secret references.

The one exception is repository **bootstrap/probe** runs, which additionally create a small result `ConfigMap` (same name as the Job) the mover writes its outcome into — needed because the controller may read the result after the Job is already gone. It is one fixed-name object per repository, consumed and deleted by the controller; it cannot accumulate.

/// note | Why not a ConfigMap?
Operator versions up to 0.7.0 mounted the work spec from a per-run ConfigMap. The Job self-reaped via its TTL, but ConfigMaps have no TTL mechanism and were owner-referenced to long-lived CRs (a `Snapshot` is the durable record of a backup; a `SnapshotPolicy` never goes away) — so one ConfigMap leaked per run, forever (issue #224: 605 in one reported cluster). Embedding the spec in the Job removes the second object, and with it the entire leak class.
///

### The object sweep

One leader-only background sweep reclaims objects a run no longer needs. It is a **steady-state safety net**, not just an upgrade tool: the reconcilers reclaim their own objects as each run ends, and the sweep catches whatever they missed — a controller that crashed mid-cleanup, a `Snapshot` deleted while the operator was down, an RBAC grant that was briefly absent.

It handles three things:

- **Projected credential copies of finished runs.** A copy is only needed while its mover Job can load it via `envFrom`. Once the owning `Snapshot` has reached `Succeeded`/`Failed` and no live Job still references the copy, it is deleted. The copies of long-lived consumers (a `Maintenance` cron, a verification tier) are **not** touched between their runs — those are already one-per-CR and are refreshed in place.
- **Per-run projected credential Secrets** (versions ≤ 0.7.1): older versions named copies after each mover *run* (`<job>-creds-N`), so recurring consumers accumulated one live credential copy per run. Kopiur-managed, `projected-from`-annotated Secrets that lack the stable-name marker, are loaded by no live mover Job, and are past the minimum age are deleted. The kopia web-UI credential mirrors and user Secrets are excluded by construction.
- **Per-run work-spec ConfigMaps** (versions ≤ 0.7.0): work-spec ConfigMaps (data key `work-spec.json`) with **no same-named Job**, past the minimum age, are deleted. Bootstrap result ConfigMaps and anything a live legacy run still mounts are never touched.

Deleting Secrets rides the `features.credentialProjection.enabled` RBAC grant. If you used projection and later disabled the flag, the sweep logs an actionable warning naming it instead of deleting.

Backlogs drain automatically on upgrade — no `kubectl` cleanup needed. Two environment variables tune it (set via the chart's controller `extraEnv`):

| Variable | Default | Meaning |
| --- | --- | --- |
| `KOPIUR_WORK_SPEC_SWEEP_INTERVAL_SECS` | `21600` (6h) | Sweep cadence; `0` disables the sweep entirely. |
| `KOPIUR_WORK_SPEC_SWEEP_MIN_AGE_SECS` | `3600` (1h) | Minimum object age before it may be reaped. Credential copies additionally have a hard 15-minute floor, so setting this to `0` cannot expose the window in which a copy exists but its Job does not yet. |

Each pass increments `kopiur_work_spec_cms_swept_total`, `kopiur_projected_secrets_swept_total`, and `kopiur_creds_secrets_reaped_total{by="sweep"}` on `/metrics`. That last one is worth an alert: in steady state the reconciler should reclaim a copy long before the sweep sees it, so a **sustained** `by="sweep"` rate means the reconciler's own cleanup has stopped firing. (Don't sum it with `by="terminal"` — the split is the whole point.)

## Troubleshooting

The mover preconditions surface on the `Snapshot`/`Restore` status as conditions **and** as `Warning` Events (visible in `kubectl describe`), so you never have to read controller logs to find out why a backup didn't start.

| Symptom                                                                         | Condition / Event                                         | Cause                                                           | Fix                                                               |
| ------------------------------------------------------------------------------- | --------------------------------------------------------- | --------------------------------------------------------------- | ----------------------------------------------------------------- |
| Backup stuck `Pending`, no Job                                                  | `CredentialsAvailable=False` / `MissingCredentialsSecret` | The credential Secret isn't in the workload namespace.          | Create the Secret there (replicate it for a `ClusterRepository`). |
| Backup stuck `Pending`, no Job                                                  | `MoverPermitted=False` / `PrivilegedMoverNotPermitted`    | The mover requests privilege but the namespace hasn't opted in. | Annotate the namespace, or drop the elevated `securityContext`.   |
| Job created but pod never appears, `FailedCreate: serviceaccount ... not found` | (pre-fix only)                                            | The mover SA isn't in the namespace.                            | Upgrade the operator — it now mints the SA automatically.         |

/// info | Where to look

- `kubectl describe snapshot <name> -n <ns>` (or `restore`/`maintenance`) — conditions **and** Events in one place.
- `kubectl get serviceaccount,rolebinding -n <ns> -l app.kubernetes.io/component=mover` — confirm the mover RBAC was minted.
- Find the mover Job, then its pod. For a `Snapshot` the Job name is in its status: `kubectl get snapshot <name> -n <ns> -o jsonpath='{.status.job.name}'`. For a `Restore` the mover Job is named after the `Restore` itself. Either way, list the pod with the standard Job-managed selector: `kubectl get pods -n <ns> --selector=job-name=<job-name>`. To list **all** of a policy's snapshot mover Jobs/pods at once, use the policy label: `kubectl get jobs,pods -n <ns> -l kopiur.home-operations.com/config=<policy-name>`.

///

## Quick reference

| Thing                              | Value                                                                                                                 |
| ---------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| Minted mover ServiceAccount / role | `kopiur-mover` (release-prefixed)                                                                                     |
| Privileged-mover opt-in annotation | `kopiur.home-operations.com/privileged-movers: "true"` (on the **Namespace**)                                         |
| Credentials condition              | `CredentialsAvailable` (reason `MissingCredentialsSecret`)                                                            |
| Privilege condition                | `MoverPermitted` (reason `PrivilegedMoverNotPermitted`)                                                               |
| Operator RBAC needed to mint       | `serviceaccounts: [create, get]`, `rolebindings: [get, create, update, patch]`, `namespaces: [get]` (cluster install) |
