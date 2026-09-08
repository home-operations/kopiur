# Upgrading Kopiur

This page covers upgrading the operator across releases. Most upgrades are a routine `helm upgrade`, or a Flux or Argo reconcile: bump the chart version, roll the Deployments, done. The one exception so far is **0.5.x → 0.6.0**, which moves the CustomResourceDefinitions (CRDs) between two Helm mechanisms and needs one deliberate step to avoid data loss. Read that section before you cross it.

## Per-repository mover concurrency: what the first reconcile after the upgrade does

The release that adds [`concurrency.maxConcurrentJobs`](repositories.md#concurrency--cap-the-mover-jobs-one-repository-runs-at-once) needs **no action**. With no cap set anywhere, which is the default, the gate costs one branch, performs no extra API calls, and changes nothing about backups. The notes below matter only once you set a cap, and only across the upgrade itself.

### Pre-upgrade mover Jobs are invisible to the counter

The pool is counted with a single label selector on `kopiur.home-operations.com/repo-pool`, and the previous version never stamped that label. So a mover Job that was already running when you upgraded **does not count** against its repository's cap. For as long as those Jobs run, the operator can briefly over-admit: you might see four movers against a `maxConcurrentJobs: 3` repository until the pre-upgrade ones finish.

That is the deliberate choice, not an oversight. The alternative, treating an unlabeled Job as belonging to every repository, would count each one against every cap at once and park the entire fleet on upgrade. Over-admitting for one run window is strictly the better failure. It resolves itself with no intervention, because every Job created after the upgrade carries the label.

### In-flight pre-upgrade Jobs just drain — you do not have to quiesce first

A `Job`'s pod template is immutable, so a natural worry is that the operator will try to stamp the new pool label onto a mover Job that is already running, and be refused with a `422`.

It does not. Every pooled spawn path checks whether its mover Job already exists and **returns before touching it**. The operator never re-applies an existing Job's pod template, on this upgrade or any other. A Job created by the previous version therefore runs to completion exactly as it is, unlabeled and, per the note above, uncounted. Its successor is created fresh with the new template. There is no error to see, nothing to retry, and no reason to drain your schedules before upgrading.

### A leader failover has the same one-window shape

The gate counts the pool by listing Jobs, and there is a gap between deciding to admit a run and that run's Job becoming visible to the next list. Within one operator process that gap is closed exactly: the leader keeps an in-memory record of the admissions it has granted, so two runs that start at the same instant cannot both read an empty pool. Only the leader reconciles, so there is never a second replica whose record this would have to agree with.

A **leader failover** resets that record. The new leader begins from the listed Jobs alone, so for the length of one reconcile pass it can admit a run whose predecessor's Job the API has not published yet. That is the same brief over-admission as the pre-upgrade case above, bounded to the moment of the handover, and self-correcting on the next pass. As there, no cap is ever *under*-run: the record only ever adds to what the list shows, so losing it can never park a repository that has room.

## Admission-only: jitter and deadline rules (re-apply only)

Two rules now **tighten fields that already shipped**. The admission webhook enforces them, and the reconcilers deliberately do **not** re-check them:

- A `jitter` window over **24h** is rejected on `SnapshotSchedule.spec.schedule`, `SnapshotPolicy.spec.verification.{quick,deep}`, `Maintenance.spec.schedule.{quick,full}`, and both replication kinds' `spec.schedule`. Jitter is a spread *within* a cron period, not a schedule offset. For `Maintenance` and `RepositoryReplication` the *parse* is new too: an unparseable window used to be accepted and then silently degrade to no jitter at reconcile.
- A **negative** `startingDeadlineSeconds` on a `SnapshotSchedule` is rejected. It is not "no deadline". The miss check is `now - slot > deadline`, so a negative value marks every slot expired the instant it fires, and the schedule silently skips every run forever while reporting itself healthy. Omit the field for no deadline. `0` is legitimate.

**Nothing breaks on upgrade.** A stored object carrying either value keeps reconciling exactly as it did before. A tightened rule that ran in the reconciler would stop backups on objects nobody touched, which is precisely the failure mode this split exists to prevent. The rejection lands on the next `kubectl apply`, or Flux or Argo reconcile, that writes that object.

So the practical shape is this: **a GitOps repo containing one of these values will fail to apply after the upgrade, while the running cluster carries on**. Grep your manifests before you roll:

```console
$ grep -rn 'jitter:' path/to/manifests   # anything over 24h
$ grep -rn 'startingDeadlineSeconds: -' path/to/manifests
```

The webhook's rejection names the exact field and value, so a surprise here is a clear message rather than a mystery.

/// note | The repository-level `scheduleDefaults.jitter` is not in this bucket

The same 24h cap applies to a `Repository` or `ClusterRepository`'s new `spec.scheduleDefaults.jitter`, but it needs none of the care above. The admission-only split exists to protect objects that were stored *before* a rule tightened. This field is brand new, so no stored repository can be carrying a value it rejects. There is nothing to grep for.

///

## After 0.10.5: the `NoAdoptableHistory` warning no longer exists (no action needed)

Versions after 0.10.5 remove the `NoAdoptableHistory` Warning Event outright. There is nothing to apply: no schema change, no flag, no RBAC. The operator simply stops emitting it.

**What changed.** A `SnapshotPolicy` with no history of its own, pointed at a repository that already holds `origin: discovered` snapshots belonging to something else, used to get a `NoAdoptableHistory` Warning Event on **every** reconcile, roughly every five minutes, until its own first backup landed. On a daily cron that is days. And that is the ordinary shape of a shared repository: retire one app, whose snapshots stay as discovered rows because the deletion cascade defaults to `Retain`, then add a different app against the same `ClusterRepository`. The new app's policy then warns indefinitely about history that was never meant for it.

**Why it was removed rather than tuned.** The event existed to catch one disaster-recovery case: a rebuilt policy whose identity no longer matches the history it was meant to inherit. But a kopia identity is a pure function of inputs you author in Git. Those are the policy's name and namespace, `spec.identity`, the repository's `identityDefaults` including `cluster`, labels and annotations, the PVC name, and `sourcePathOverride`. Nothing about the live cluster feeds it, so a GitOps re-apply cannot drift an identity on its own; every mismatch begins as a human edit to a manifest. The event could not tell that rare case apart from the common shared-repository one, and in practice it taught people to set `spec.adoption: Ignore` to make the noise stop, which disables the very adoption it was meant to protect.

**Where the signal went.** From "wait for a warning that it didn't work" to "check that it did":

- `status.adoption.lastScanMatched` and `status.adoption.lastScanUnmatched` on the `SnapshotPolicy` say how many discovered rows matched this policy's identity at the last adoption pass, and how many didn't. Both absent means no pass has run yet; `0`/`0` means the catalog was empty. A multi-repository policy sums across its ready targets.
- `kubectl kopiur snapshots list -n <ns> --origin discovered --repository <repo>` lists the history in that repository that no live policy has claimed. It should be empty after a complete disaster recovery.
- `status.adoption.totalAdopted` is omitted when unset, so **empty output means adoption never ran**, not `0`.

Those are the checks in [Scenario 10 → verification checklist](scenarios/dr-with-replicated-repository.md#verification-checklist) and [Troubleshooting → adoption didn't happen](troubleshooting.md#adoption-didnt-happen).

**If you set `spec.adoption: Ignore`, or `spec.catalog.adoption: Ignore` on the repository, solely to silence this warning, you can remove it now.** The warning was the defect; adoption itself was working correctly. Before you do, re-read the retention hazard in [Scenario 10](scenarios/dr-with-replicated-repository.md#hazards-to-review-before-you-apply). An adopted snapshot comes under the policy's GFS retention, and that is the one substantive reason to keep `Ignore`.

## 0.10.0: SnapshotReplication, multi-repository fan-out, new RBAC, and a new Snapshot phase

Four things to know before you cross this release.

### Apply the CRDs before rolling the operator

Three schema changes ride this release, and **Helm's `crds/` directory is install-only**. A plain `helm upgrade` never updates CRD schemas, as described in [CRD lifecycle](install.md#crd-lifecycle), so on a helm-CLI install you must apply them yourself:

```bash
kubectl apply --server-side -f deploy/crds/
helm upgrade kopiur ...
```

GitOps tooling with a `CreateReplace` CRD policy, meaning Flux or Argo, upgrades them for you. Here is what breaks, per change, if you skip this:

- **`Snapshot.status.phase` gains `Unchanged`** (see [#351](https://github.com/home-operations/kopiur/issues/351)). The mover writes the phase directly onto the status subresource. So a mover image that knows the new value, talking to an apiserver whose CRD does not, will have its status PATCH **rejected by schema validation**.
- **`SnapshotPolicy.spec.repositories`**, the multi-repository fan-out from [#368](https://github.com/home-operations/kopiur/issues/368). With the old CRD the apiserver **prunes** the unknown field from every applied object, and admission then refuses the policy: neither `repository` nor `repositories` is set, so the exactly-one-of rule fails. That is loud, not silent, but the fix is the CRD apply above, not a manifest change. `kubectl kopiur doctor` flags a live `snapshotpolicies` CRD whose schema is missing the field.
- **The new `snapshotreplications` CRD** for [snapshot replication](snapshot-replication.md) simply does not exist until applied. `kubectl apply` of a `SnapshotReplication` fails with "no matches for kind".

### The webhook now covers `snapshotreplications`

The chart's `ValidatingWebhookConfiguration` and `MutatingWebhookConfiguration` gained the `snapshotreplications` resource, and `helm upgrade` applies both. If you maintain webhook configurations out of band, add the resource to each. Otherwise `SnapshotReplication` objects are admitted with **no validation or defaulting**.

### Non-Helm installs need the new RBAC

Re-apply `deploy/rbac/*.yaml` if you apply Role-Based Access Control (RBAC) directly rather than through the chart. There are two additions:

- **`groupBy: VolumeGroupSnapshot`** ([#346](https://github.com/home-operations/kopiur/issues/346)) needs `patch` on `groupsnapshot.storage.k8s.io/volumegroupsnapshots`, because the members of one expansion converge the shared capture by server-side apply, which is a PATCH. It also needs read on the cluster-scoped `volumegroupsnapshotclasses`. Without them, group staging fails with opaque 403s. Nothing else needs these: a policy without a `pvcSelector`, or with `groupBy: None`, never touches that API group.
- **The dedicated snapshot-replication mover Role** (`kopiur-snapshot-replication-mover`, plus its ServiceAccount and binding). Replication movers create the copy `Snapshot` CRs themselves, so they run under their own narrowly-scoped Role: get, list, create, patch and delete on `snapshots`, plus status patch. That is deliberately **not** granted to the ordinary backup or restore mover. Without it, every `SnapshotReplication` run fails with 403s on `snapshots`.

## After 0.7.0: per-run work-spec ConfigMaps no longer exist (no action needed)

Versions after 0.7.0 fix a resource leak. Every mover run, whether backup, restore, verify, replication, maintenance or pin, left its `work-spec.json` ConfigMap behind forever, and clusters with hourly schedules accumulated hundreds. The fix is structural, and both halves are automatic:

- The controller now **embeds the work spec in the mover Job's pod env** as `KOPIUR_WORK_SPEC`. A run is one object, and no per-run ConfigMap is created at all. The Job still lingers to its `ttlSecondsAfterFinished`, 1h by default, so pod logs and `kubectl kopiur logs` are unaffected, and `kubectl get job <name> -o yaml` now shows the full run spec in one place. Repository bootstrap and probe keep one fixed-name result ConfigMap per repository, consumed and deleted by the controller.
- A periodic **legacy sweep**, by default every 6h over ConfigMaps older than 1h with no matching Job, deletes the work-spec ConfigMaps accumulated by earlier versions. Existing clusters converge on their own after the upgrade, with no `kubectl` cleanup required. Watch the backlog drain through the `kopiur_work_spec_cms_swept_total` counter.

If any tooling of yours read the per-run ConfigMaps, read the Job's `KOPIUR_WORK_SPEC` env instead. Details and tuning knobs: [Movers → Run artifacts & cleanup](movers.md#run-artifacts--cleanup).

## After 0.7.1: recurring consumers stop accumulating credential copies (no action needed)

Versions after 0.7.1 fix one half of the credential-projection leak on `spec.credentialProjection`. Projected Secret copies were named after each mover **run**, so recurring consumers such as a `Maintenance` cron or `SnapshotPolicy` verification left one live copy of the repository credentials behind per run, owned by the long-lived CR and never deleted. Copies now use a **stable per-CR name**, for example `<maintenance>-maint-creds-0`, refreshed in place on every run. A periodic sweep deletes the per-run copies earlier versions accumulated.

The chart's `features.credentialProjection.enabled` grant gained the `secrets` **delete** verb to back this cleanup, and `helm upgrade` applies it. If you used projection on an old version and later turned the flag **off**, the sweep cannot delete the leftovers. It logs a warning naming the flag, so either re-enable it once, or delete the copies by hand.

## After 0.7.2: **backups** stop accumulating credential copies (no action needed)

The 0.7.1 fix above did not cover the highest-frequency consumer, so you may have watched your Secret count keep climbing. The stable name is `<snapshot>-creds-0`. But a `Snapshot` **is** the per-run object, so "one copy per CR" was still one live copy of your repository password and backend keys per backup, per namespace, retained for the entire GFS window because the `Snapshot` CR owns the kopia snapshot through a finalizer. A stable name bounds copies per CR; it cannot bound them per run. The lifetime was the bug, not the name.

Versions after 0.7.2 reclaim a copy when the run that needed it finishes. Both halves are automatic and need no `kubectl`:

- Each `Snapshot` reclaims its own copies at its terminal phase, once its mover Job can no longer schedule a pod that would read them.
- The periodic sweep reclaims the copies of any already-finished `Snapshot`, which is what drains the backlog. Most of it goes within ten minutes of the rollout, because terminal Snapshots re-reconcile on that cadence. The rest goes on the next sweep pass.

Watch it land on `/metrics`. **`kopiur_projected_secrets_live`** is the live count of projected copies. It should fall to roughly the number of runs in flight and then stay flat across your backup window, instead of stepping up once per run. `deriv(kopiur_projected_secrets_live[24h]) > 0` is the alert if this ever regresses. Details: [Movers → The object sweep](movers.md#the-object-sweep).

`runJob` hooks also now get a default `ttlSecondsAfterFinished` of 1h when your `jobSpec` omits one, so a completed hook Job and its Pod no longer linger for the whole retention window. Set the field explicitly to keep them longer.

## Upgrading 0.5.x → 0.6.0 (one-time CRD migration)

/// danger | 0.5.x → 0.6.0 is a breaking upgrade, read this first

Two things change at once on this crossing, and both can bite:

- **The CRDs are pruned and re-installed.** The old release-owned CRDs are deleted and re-created, which **cascade-deletes every `kopiur.home-operations.com` object**: your `Repository`, `ClusterRepository`, `Snapshot`, `Restore`, `SnapshotPolicy`, `SnapshotSchedule`, `Maintenance`, and `RepositoryReplication` resources.
- **The Helm values were restructured** to the org operator-chart shape, so your 0.5.x values do **not** apply unchanged.

Both are **one-time**, on this specific crossing. Upgrades from 0.6.0 onward are routine. Follow the numbered steps below **before** you upgrade: the CRD pin only works while you are still on 0.5.x.

///

### Why this happens

In **0.5.x** the 8 CRDs were rendered as ordinary Helm templates, so they were **owned by the Helm release**. In **0.6.0** they moved into Helm's special `crds/` directory, which Helm installs on `helm install` but never tracks as part of the release. See [CRD lifecycle](install.md#crd-lifecycle) for the steady-state behavior.

Because 0.6.0 no longer renders the CRDs as release-owned templates, a `helm upgrade`, or a Flux reconcile, sees them **leave the release manifest** and **prunes them**. Deleting a CRD deletes every custom resource of that kind. Helm then installs the `crds/`-directory copies, but that path only runs on a fresh install. So the net effect is the CRDs, and your custom resources, getting removed and re-installed.

/// note | Your backups themselves are safe

Only the **Kubernetes CR objects** are affected. The **kopia snapshots in your repository are not touched**, so the backup data is intact. GitOps re-applies the CRs straight from Git, as described under [recovery](#recovery--if-you-already-upgraded). Non-GitOps users can re-adopt the existing snapshots through a discovered `Restore`.

///

### The upgrade, step by step

Do all of this **while you are still on 0.5.x**. The CRD pin in step 2 only takes effect if it is in place before the upgrade runs.

**1. Confirm you are on 0.5.x.**

```bash
helm list -n kopiur-system            # the CHART column reads kopiur-0.5.x
# GitOps instead of the Helm CLI:
flux get helmrelease kopiur -n kopiur-system
```

**2. Pin the CRDs so the upgrade cannot delete them.** Annotating the live CRDs with `helm.sh/resource-policy: keep` makes Helm **skip the prune**, so the CRDs, and every CR they hold, survive. The new `crds/`-directory install then finds them already present and does nothing. The CRDs carry no distinguishing label, so select them by their API group:

```bash
# Run BEFORE upgrading / reconciling to 0.6.0
for crd in $(kubectl get crd -o name | grep kopiur.home-operations.com); do
  kubectl annotate "$crd" helm.sh/resource-policy=keep --overwrite
done
```

Prefer an explicit list? Annotate the eight CRDs by name. It is equivalent, and it errors loudly if one is missing rather than silently matching nothing:

```bash
for name in repositories clusterrepositories snapshotpolicies snapshots \
            snapshotschedules restores maintenances repositoryreplications; do
  kubectl annotate "crd/${name}.kopiur.home-operations.com" \
    helm.sh/resource-policy=keep --overwrite
done
```

Confirm the annotation landed before going further. Every kopiur CRD must show `keep`:

```bash
kubectl get crd -o custom-columns='NAME:.metadata.name,KEEP:.metadata.annotations.helm\.sh/resource-policy' \
  | grep -E 'NAME|kopiur.home-operations.com'
```

/// note | Flux / Argo: the pin survives reconciles

The `keep` annotation is read from the **live** CRD at the instant Helm would prune it, and Flux's helm-controller, like Argo, runs the same Helm engine. So the pin is honored under GitOps exactly as on the CLI. Helm's three-way merge only strips fields it previously set, so a reconcile will not remove an annotation you added out of band.

- If you run helm-controller **drift detection** (`spec.driftDetection.mode: enabled`), it can revert that out-of-band annotation while you are still on 0.5.x. Leave drift detection off for the cutover, or add the annotation immediately before you trigger the reconcile.
- You do **not** need to change `spec.upgrade.crds` for safety, because the pinned CRDs persist whatever its value. Set it to `CreateReplace` if you also want the **0.6.0 CRD schema** applied on upgrade. That is safe, because replacing a CRD keeps its CRs. Otherwise apply `deploy/crds/` yourself (see [CRD lifecycle](install.md#crd-lifecycle)).

///

**3. Migrate your Helm values to the 0.6.0 layout.** 0.6.0 **restructured the chart values**, which is the other half of the breaking change. Notably: the old `controller.*` block flattened to the top level, `grafanaDashboard` moved under `monitoring.dashboards`, the `installCRDs` toggle was removed, and new top-level keys were added (`mover`, `resources`, `replicaCount`, `podDisruptionBudget`, probes, scheduling, and more). Your 0.5.x values will **not** map one-to-one, and a key the new schema doesn't recognize is silently ignored, so an unmigrated value quietly reverts to its default. Rebuild your values against [Helm chart values](configuration.md) and the [chart README](https://github.com/home-operations/kopiur/blob/main/deploy/helm/kopiur/README.md) before upgrading.

**4. Upgrade to 0.6.0.**

With the Helm CLI, pass your migrated values file:

```bash
helm upgrade kopiur oci://ghcr.io/home-operations/charts/kopiur --version 0.6.0 -n kopiur-system -f my-values.yaml
```

With Flux or Argo, bump the chart to `0.6.0`, for example `spec.chart.spec.version: 0.6.0` on the `HelmRelease`, **together with** the migrated values. Commit, then reconcile:

```bash
flux reconcile helmrelease kopiur -n kopiur-system --with-source
```

**5. Verify nothing was lost.** The CRD count should still be 8, and every resource should still be there:

```bash
kubectl get crd | grep kopiur.home-operations.com   # expect 8 lines
kubectl get repositories,clusterrepositories,snapshots,restores,snapshotpolicies,snapshotschedules,maintenances -A
```

**6. (Optional) leave or remove the `keep` annotation.** It is harmless to leave. It only stops Helm from ever deleting the CRDs, which is exactly what the 0.6.0 `crds/` directory already guarantees. To drop it:

```bash
for crd in $(kubectl get crd -o name | grep kopiur.home-operations.com); do
  kubectl annotate "$crd" helm.sh/resource-policy-
done
```

### Recovery — if you already upgraded

If you crossed to 0.6.0 without pinning the CRDs, the CRDs were pruned and re-installed, and your CRs went with them. The backup data in kopia is untouched, as the note above explains. You only need to re-create the Kubernetes objects. **GitOps users recover cleanly, because the CRs live in Git.**

**Flux.** Force a full reinstall of the release, then reconcile the sources that own your CRs. `--force --reset` re-runs `helm install`, which re-creates the CRDs from the chart's `crds/` directory:

```bash
# Re-create the CRDs by forcing a reinstall of the HelmRelease:
flux reconcile helmrelease kopiur -n kopiur-system --force --reset

# Re-apply the Kustomization that defines your Repository / ClusterRepository
# (substitute your own Kustomization name + namespace; may need running twice):
flux reconcile kustomization <repo-kustomization> -n <ns> --with-source

# Then, per app, re-apply the Kustomization that defines its Snapshot / SnapshotPolicy
# / Restore CRs:
flux reconcile kustomization <app> -n <app-namespace> --with-source
```

/// warning | Watch the finalizers

Each `Snapshot` CR owns its kopia snapshot through a **finalizer**, so a deleted `Snapshot` sits in `Terminating` until the finalizer clears rather than vanishing immediately. You may have to reconcile the `HelmRelease` **more than once** while the finalizers settle. Track progress with `kubectl get snapshots -A` before assuming a reconcile is complete.

///

**Argo.** Same idea: sync the CRD application first, where a `CreateReplace` sync policy re-creates the `crds/`-shipped CRDs, then sync the applications that own the CRs. See [GitOps with Kopiur](gitops.md) for the CRD sync-wave guidance.

**Helm CLI (no GitOps).** A `helm upgrade` will **not** re-create the CRDs, because Helm never touches `crds/` on upgrade. Re-create them by hand, then re-apply your CR manifests from wherever you keep them:

```bash
kubectl apply --server-side -f deploy/crds/
kubectl apply -f <your-repository-and-snapshot-manifests>
```

CRs that only ever existed in the cluster, and were never captured in a manifest, can't be restored as objects. But the underlying kopia snapshots are still in the repository, so you can re-adopt them with a discovered `Restore`. See the discovered-restore example in [Examples](examples.md).

## See also

- [Installing Kopiur → CRD lifecycle](install.md#crd-lifecycle): the steady-state
  `crds/`-directory behavior, and how to apply schema changes on later upgrades.
- [GitOps with Kopiur](gitops.md): CRD sync waves, `CreateReplace`, and the Flux/Argo
  reconcile model.
