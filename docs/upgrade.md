# Upgrading Kopiur

This page covers upgrading the operator across releases. Most upgrades are a
routine `helm upgrade` (or a Flux/Argo reconcile) — bump the chart version, roll
the Deployments, done. The one exception so far is **0.5.x → 0.6.0**, which moves
the CRDs between two Helm mechanisms and needs one deliberate step to avoid data
loss. Read that section before you cross it.

## Upgrading 0.5.x → 0.6.0 (one-time CRD migration)

/// danger | This upgrade removes and re-installs the CRDs
Crossing **0.5.x → 0.6.0** prunes the old, release-owned CRDs and **cascade-deletes
every `kopiur.home-operations.com` object** — your `Repository`,
`ClusterRepository`, `Snapshot`, `Restore`, `SnapshotPolicy`, `SnapshotSchedule`,
`Maintenance`, and `RepositoryReplication` resources. It is a **one-time** event on
this specific crossing; upgrades from 0.6.0 onward are safe. Do the pre-upgrade step
below **before** you upgrade.
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

### Safe path — pin the CRDs before you upgrade

While you are **still on 0.5.x**, annotate the live CRDs with
`helm.sh/resource-policy: keep`. Helm honors that annotation during the upgrade and
**skips the prune**, so the CRDs — and all your CRs — survive. The new
`crds/`-directory install then finds them already present and does nothing.

```console
# Run against your 0.5.x release, BEFORE upgrading / reconciling to 0.6.0
$ for crd in $(kubectl get crd -l app.kubernetes.io/part-of=kopiur -o name); do
    kubectl annotate "$crd" helm.sh/resource-policy=keep --overwrite
  done
```

If the label selector returns nothing on your install, annotate the eight CRDs by
name instead:

```console
$ for name in repositories clusterrepositories snapshotpolicies snapshots \
              snapshotschedules restores maintenances repositoryreplications; do
    kubectl annotate "crd/${name}.kopiur.home-operations.com" \
      helm.sh/resource-policy=keep --overwrite
  done
```

Now upgrade as usual (`helm upgrade …`, or bump the chart in Git and let Flux/Argo
reconcile). Afterwards, confirm the CRDs and your resources are still present:

```console
$ kubectl get crd -l app.kubernetes.io/part-of=kopiur
$ kubectl get repositories,snapshots,restores -A
```

### Recovery — if you already upgraded

If you crossed to 0.6.0 without pinning the CRDs, re-apply everything from Git. For
Flux, reinstall the release (which recreates the CRDs from the `crds/` directory),
then reconcile the repository and each app so their CRs come back:

```console
$ flux reconcile -n kopiur-system hr kopiur --force --reset          # recreate the CRDs
$ flux reconcile ks -n kopiur-system kopiur-repository --with-source  # may need running twice
$ flux reconcile ks -n <app-namespace> <app> --with-source           # per app: recreate its CRs
```

/// warning | Watch the finalizers
Each `Snapshot` CR owns its kopia snapshot through a **finalizer**, so a deleted
`Snapshot` sits in `Terminating` until the finalizer clears rather than vanishing
immediately. You may have to reconcile the `HelmRelease` **more than once** while the
finalizers settle. Track progress with `kubectl get snapshots -A` before assuming a
reconcile is complete.
///

Argo users apply the same idea: sync the CRD application first (a `CreateReplace` sync
policy recreates the `crds/`-shipped CRDs), then sync the applications that own the CRs.
See [GitOps with Kopiur](gitops.md) for the CRD sync-wave guidance.

## See also

- [Installing Kopiur → CRD lifecycle](install.md#crd-lifecycle) — the steady-state
  `crds/`-directory contract and how to apply schema changes on later upgrades.
- [GitOps with Kopiur](gitops.md) — CRD sync waves, `CreateReplace`, and the Flux/Argo
  reconcile model.
