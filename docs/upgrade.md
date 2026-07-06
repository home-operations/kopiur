# Upgrading Kopiur

This page covers upgrading the operator across releases. Most upgrades are a
routine `helm upgrade` (or a Flux/Argo reconcile) — bump the chart version, roll
the Deployments, done. The one exception so far is **0.5.x → 0.6.0**, which moves
the CRDs between two Helm mechanisms and needs one deliberate step to avoid data
loss. Read that section before you cross it.

## Upgrading 0.5.x → 0.6.0 (one-time CRD migration)

/// danger | 0.5.x → 0.6.0 is a breaking upgrade — read this first
Two things change at once on this crossing, and both can bite:

- **The CRDs are pruned and re-installed.** The old release-owned CRDs are deleted and
  re-created, which **cascade-deletes every `kopiur.home-operations.com` object** —
  your `Repository`, `ClusterRepository`, `Snapshot`, `Restore`, `SnapshotPolicy`,
  `SnapshotSchedule`, `Maintenance`, and `RepositoryReplication` resources.
- **The Helm values were restructured** to the org operator-chart shape, so your 0.5.x
  values do **not** apply unchanged.

Both are **one-time**, on this specific crossing; upgrades from 0.6.0 onward are
routine. Follow the numbered steps below **before** you upgrade — the CRD pin only
works while you are still on 0.5.x.
///

### Why this happens

In **0.5.x** the 8 CRDs were rendered as ordinary Helm templates, so they were
**owned by the Helm release**. In **0.6.0** they moved into Helm's special `crds/`
directory (see [CRD lifecycle](install.md#crd-lifecycle) for the steady-state
contract), which Helm installs on `helm install` but never tracks as part of the
release.

Because 0.6.0 no longer renders the CRDs as release-owned templates, a `helm upgrade`
(or a Flux reconcile) sees them **leave the release manifest** and **prunes them** —
and deleting a CRD deletes every custom resource of that kind. Helm then installs the
`crds/`-directory copies, but that path only runs on a fresh install, so the net
effect is the CRDs (and your CRs) getting removed and re-installed.

/// note | Your backups themselves are safe
Only the **Kubernetes CR objects** are affected. The **kopia snapshots in your
repository are not touched** — the backup data is intact. GitOps re-applies the CRs
straight from Git (see [recovery](#recovery--if-you-already-upgraded)); non-GitOps
users can re-adopt the existing snapshots via a discovered `Restore`.
///

### The upgrade, step by step

Do all of this **while you are still on 0.5.x** — the CRD pin in step 2 only takes
effect if it is in place before the upgrade runs.

**1. Confirm you are on 0.5.x.**

```bash
helm list -n kopiur-system            # the CHART column reads kopiur-0.5.x
# GitOps instead of the Helm CLI:
flux get helmrelease kopiur -n kopiur-system
```

**2. Pin the CRDs so the upgrade cannot delete them.** Annotating the live CRDs with
`helm.sh/resource-policy: keep` makes Helm **skip the prune**, so the CRDs — and every
CR they hold — survive; the new `crds/`-directory install then finds them already
present and does nothing.

```bash
# Run BEFORE upgrading / reconciling to 0.6.0
for crd in $(kubectl get crd -l app.kubernetes.io/part-of=kopiur -o name); do
  kubectl annotate "$crd" helm.sh/resource-policy=keep --overwrite
done
```

If the label selector returns nothing on your install, annotate the eight CRDs by name
instead:

```bash
for name in repositories clusterrepositories snapshotpolicies snapshots \
            snapshotschedules restores maintenances repositoryreplications; do
  kubectl annotate "crd/${name}.kopiur.home-operations.com" \
    helm.sh/resource-policy=keep --overwrite
done
```

Confirm the annotation landed before going further — every row should end in `keep`:

```bash
kubectl get crd -l app.kubernetes.io/part-of=kopiur \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.metadata.annotations.helm\.sh/resource-policy}{"\n"}{end}'
```

**3. Migrate your Helm values to the 0.6.0 layout.** 0.6.0 **restructured the chart
values** (the other half of the breaking change). Notably: the old `controller.*` block
flattened to the top level, `grafanaDashboard` moved under `monitoring.dashboards`, the
`installCRDs` toggle was removed, and new top-level keys (`mover`, `resources`,
`replicaCount`, `podDisruptionBudget`, probes, scheduling, …) were added. Your 0.5.x
values will **not** map 1:1, and a key the new schema doesn't recognize is silently
ignored — so an unmigrated value quietly reverts to its default. Rebuild your values
against [Helm chart values](configuration.md) and the
[chart README](https://github.com/home-operations/kopiur/blob/main/deploy/helm/kopiur/README.md)
before upgrading.

**4. Upgrade to 0.6.0.**

With the Helm CLI, pass your migrated values file:

```bash
helm upgrade kopiur deploy/helm/kopiur -n kopiur-system -f my-values.yaml
```

With Flux/Argo, bump the chart to `0.6.0` (e.g. `spec.chart.spec.version: 0.6.0` on the
`HelmRelease`) **together with** the migrated values, commit, then reconcile:

```bash
flux reconcile helmrelease kopiur -n kopiur-system --with-source
```

**5. Verify nothing was lost.** The CRD count should still be 8, and every resource
should still be there:

```bash
kubectl get crd -l app.kubernetes.io/part-of=kopiur   # still 8
kubectl get repositories,clusterrepositories,snapshots,restores,snapshotpolicies,snapshotschedules,maintenances -A
```

**6. (Optional) leave or remove the `keep` annotation.** It is harmless to leave — it
only stops Helm from ever deleting the CRDs, which is exactly what the 0.6.0 `crds/`
directory already guarantees. To drop it:

```bash
for crd in $(kubectl get crd -l app.kubernetes.io/part-of=kopiur -o name); do
  kubectl annotate "$crd" helm.sh/resource-policy-
done
```

### Recovery — if you already upgraded

If you crossed to 0.6.0 without pinning the CRDs, the CRDs were pruned and re-installed
and your CRs went with them. The backup data in kopia is untouched (see the note
above) — you only need to re-create the Kubernetes objects. **GitOps users recover
cleanly, because the CRs live in Git.**

**Flux.** Force a full reinstall of the release, then reconcile the sources that own
your CRs. `--force --reset` re-runs `helm install`, which re-creates the CRDs from the
chart's `crds/` directory:

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
Each `Snapshot` CR owns its kopia snapshot through a **finalizer**, so a deleted
`Snapshot` sits in `Terminating` until the finalizer clears rather than vanishing
immediately. You may have to reconcile the `HelmRelease` **more than once** while the
finalizers settle. Track progress with `kubectl get snapshots -A` before assuming a
reconcile is complete.
///

**Argo.** Same idea: sync the CRD application first (a `CreateReplace` sync policy
re-creates the `crds/`-shipped CRDs), then sync the applications that own the CRs. See
[GitOps with Kopiur](gitops.md) for the CRD sync-wave guidance.

**Helm CLI (no GitOps).** A `helm upgrade` will **not** re-create the CRDs — Helm never
touches `crds/` on upgrade — so re-create them by hand, then re-apply your CR manifests
from wherever you keep them:

```bash
kubectl apply --server-side -f deploy/crds/
kubectl apply -f <your-repository-and-snapshot-manifests>
```

CRs that only ever existed in the cluster (never captured in a manifest) can't be
restored as objects — but the underlying kopia snapshots are still in the repository,
so you can re-adopt them with a discovered `Restore` (see the discovered-restore
example in [Examples](examples.md)).

## See also

- [Installing Kopiur → CRD lifecycle](install.md#crd-lifecycle) — the steady-state
  `crds/`-directory contract and how to apply schema changes on later upgrades.
- [GitOps with Kopiur](gitops.md) — CRD sync waves, `CreateReplace`, and the Flux/Argo
  reconcile model.
