# Upgrading Kopiur

This page covers upgrading the operator across releases. Most upgrades are a
routine `helm upgrade` (or a Flux/Argo reconcile) — bump the chart version, roll
the Deployments, done. The one exception so far is **0.5.x → 0.6.0**, which moves
the CRDs between two Helm mechanisms and needs one deliberate step to avoid data
loss. Read that section before you cross it.

## After 0.10.5: the `NoAdoptableHistory` warning no longer exists (no action needed)

Versions after 0.10.5 remove the `NoAdoptableHistory` Warning Event outright. There is nothing to apply — no schema change, no flag, no RBAC. The operator simply stops emitting it.

**What changed.** A `SnapshotPolicy` with no history of its own, pointed at a repository that already holds `origin: discovered` snapshots belonging to something else, used to get a `NoAdoptableHistory` Warning Event on **every** reconcile — roughly every five minutes — until its own first backup landed, which on a daily cron is days. That is the ordinary shape of a shared repository: retire one app (the deletion cascade defaults to `Retain`, so its snapshots stay as discovered rows), add a different app against the same `ClusterRepository`, and the new app's policy warns indefinitely about history that was never meant for it.

**Why it was removed rather than tuned.** The event existed to catch one disaster-recovery case: a rebuilt policy whose identity no longer matches the history it was meant to inherit. But a kopia identity is a pure function of Git-authored inputs — the policy's name and namespace, `spec.identity`, the repository's `identityDefaults` (including `cluster`), labels and annotations, the PVC name, `sourcePathOverride`. Nothing about the live cluster feeds it, so a GitOps re-apply cannot drift an identity on its own; every mismatch begins as a human edit to a manifest. The event could not tell that rare case apart from the common shared-repository one, and in practice it taught people to set `spec.adoption: Ignore` to make the noise stop — disabling the very adoption it was meant to protect.

**Where the signal went.** From "wait for a warning that it didn't work" to "check that it did":

- `status.adoption.lastScanMatched` / `status.adoption.lastScanUnmatched` on the `SnapshotPolicy` — how many discovered rows matched this policy's identity at the last adoption pass, and how many didn't. Both absent means no pass has run yet; `0`/`0` means the catalog was empty. A multi-repository policy sums across its ready targets.
- `kubectl kopiur snapshots list -n <ns> --origin discovered --repository <repo>` — the history in that repository no live policy has claimed. Empty after a complete DR.
- `status.adoption.totalAdopted` — omitted when unset, so **empty output means adoption never ran**, not `0`.

Those are the checks in [Scenario 10 → verification checklist](scenarios/dr-with-replicated-repository.md#verification-checklist) and [Troubleshooting → adoption didn't happen](troubleshooting.md#adoption-didnt-happen).

**If you set `spec.adoption: Ignore` (or `spec.catalog.adoption: Ignore` on the repository) solely to silence this warning, you can remove it now.** The warning was the defect; adoption itself was working correctly. Before you do, re-read the retention hazard in [Scenario 10](scenarios/dr-with-replicated-repository.md#hazards-to-review-before-you-apply) — an adopted snapshot comes under the policy's GFS retention, and that is the one substantive reason to keep `Ignore`.

## 0.10.0: SnapshotReplication, multi-repository fan-out, new RBAC, and a new Snapshot phase

Four things to know before you cross this release.

### Apply the CRDs before rolling the operator

Three schema changes ride this release, and **Helm's `crds/` directory is
install-only** — a plain `helm upgrade` never updates CRD schemas (see
[CRD lifecycle](install.md#crd-lifecycle)), so on a helm-CLI install you must
apply them yourself:

```bash
kubectl apply --server-side -f deploy/crds/
helm upgrade kopiur ...
```

(GitOps tooling with a `CreateReplace` CRD policy — Flux, Argo — upgrades them
for you.) What breaks, per change, if you skip this:

- **`Snapshot.status.phase` gains `Unchanged`** (see
  [#351](https://github.com/home-operations/kopiur/issues/351)). The mover
  writes the phase directly onto the status subresource, so a mover image that
  knows the new value talking to an apiserver whose CRD does not will have its
  status PATCH **rejected by schema validation**.
- **`SnapshotPolicy.spec.repositories`** (multi-repository fan-out,
  [#368](https://github.com/home-operations/kopiur/issues/368)): with the old
  CRD the apiserver **prunes** the unknown field from every applied object, and
  admission then refuses the policy — neither `repository` nor `repositories`
  is set, the exactly-one-of rule fails. Loud, not silent — but the fix is the
  CRD apply above, not the manifest. `kubectl kopiur doctor` flags a live
  `snapshotpolicies` CRD whose schema is missing the field.
- **The new `snapshotreplications` CRD**
  ([snapshot replication](snapshot-replication.md)) simply does not exist until
  applied — `kubectl apply` of a `SnapshotReplication` fails with "no matches
  for kind".

### The webhook now covers `snapshotreplications`

The chart's `ValidatingWebhookConfiguration` and `MutatingWebhookConfiguration`
gained the `snapshotreplications` resource — `helm upgrade` applies both. If
you maintain webhook configurations out-of-band, add the resource to each, or
`SnapshotReplication` objects are admitted with **no validation or defaulting**.

### Non-Helm installs need the new RBAC

Re-apply `deploy/rbac/*.yaml` if you apply RBAC directly rather than through
the chart. Two additions:

- **`groupBy: VolumeGroupSnapshot`** ([#346](https://github.com/home-operations/kopiur/issues/346)):
  `patch` on `groupsnapshot.storage.k8s.io/volumegroupsnapshots` (the members
  of one expansion converge the shared capture by server-side apply, which is a
  PATCH) and read on the cluster-scoped `volumegroupsnapshotclasses`. Without
  them, group staging fails with opaque 403s. Nothing else needs these: a
  policy without a `pvcSelector`, or with `groupBy: None`, never touches that
  API group.
- **The dedicated snapshot-replication mover Role** (`kopiur-snapshot-replication-mover`,
  plus its ServiceAccount/binding): replication movers create the copy
  `Snapshot` CRs themselves, so they run under their own narrowly-scoped Role
  (get/list/create/patch/delete on `snapshots` + status patch) — deliberately
  **not** granted to the ordinary backup/restore mover. Without it, every
  `SnapshotReplication` run fails with 403s on `snapshots`.

## After 0.7.0: per-run work-spec ConfigMaps no longer exist (no action needed)

Versions after 0.7.0 fix a resource leak: every mover run (backup, restore,
verify, replication, maintenance, pin) left its `work-spec.json` ConfigMap
behind forever — clusters with hourly schedules accumulated hundreds. The fix
is structural, and both halves are automatic:

- The controller now **embeds the work spec in the mover Job's pod env**
  (`KOPIUR_WORK_SPEC`) — a run is one object, and no per-run ConfigMap is
  created at all. The Job still lingers to its `ttlSecondsAfterFinished`
  (default 1h) — pod logs and `kubectl kopiur logs` are unaffected, and
  `kubectl get job <name> -o yaml` now shows the full run spec in one place.
  (Repository bootstrap/probe keeps one fixed-name result ConfigMap per
  repository, consumed and deleted by the controller.)
- A periodic **legacy sweep** (default: every 6h, ConfigMaps older than 1h with
  no matching Job) deletes the work-spec ConfigMaps accumulated by earlier
  versions, so existing clusters converge on their own after the upgrade — no
  `kubectl` cleanup required. Watch the backlog drain via the
  `kopiur_work_spec_cms_swept_total` counter.

If any tooling of yours read the per-run ConfigMaps, read the Job's
`KOPIUR_WORK_SPEC` env instead. Details and tuning knobs:
[Movers → Run artifacts & cleanup](movers.md#run-artifacts--cleanup).

## After 0.7.1: recurring consumers stop accumulating credential copies (no action needed)

Versions after 0.7.1 fix one half of the credential-projection leak
(`spec.credentialProjection`): projected Secret copies were named after each
mover **run**, so recurring consumers — a `Maintenance` cron, `SnapshotPolicy`
verification — left one live copy of the repository credentials behind per run,
owned by the long-lived CR and never deleted. Copies now use a **stable per-CR
name** (e.g. `<maintenance>-maint-creds-0`), refreshed in place on every run, and
a periodic sweep deletes the per-run copies earlier versions accumulated.

The chart's `features.credentialProjection.enabled` grant gained the `secrets`
**delete** verb to back this cleanup — `helm upgrade` applies it. If you used
projection on an old version and later turned the flag **off**, the sweep cannot
delete the leftovers; it logs a warning naming the flag (re-enable it once, or
delete the copies by hand).

## After 0.7.2: **backups** stop accumulating credential copies (no action needed)

The 0.7.2 fix above did not cover the highest-frequency consumer, and you may
have watched your Secret count keep climbing. The stable name is
`<snapshot>-creds-0` — and a `Snapshot` **is** the per-run object, so "one copy
per CR" was still one live copy of your repository password and backend keys per
backup, per namespace, retained for the entire GFS window because the `Snapshot`
CR owns the kopia snapshot via a finalizer. A stable name bounds copies per CR;
it cannot bound them per run. The lifetime was the bug, not the name.

Versions after 0.7.2 reclaim a copy when the run that needed it finishes. Both
halves are automatic and need no `kubectl`:

- Each `Snapshot` reclaims its own copies at its terminal phase, once its mover
  Job can no longer schedule a pod that would read them.
- The periodic sweep reclaims the copies of any already-finished `Snapshot`,
  which is what drains the backlog. Most of it goes within ten minutes of the
  rollout (terminal Snapshots re-reconcile on that cadence); the rest goes on the
  next sweep pass.

Watch it land on `/metrics`: **`kopiur_projected_secrets_live`** is the live
count of projected copies. It should fall to roughly the number of runs in flight
and then stay flat across your backup window instead of stepping up once per run.
`deriv(kopiur_projected_secrets_live[24h]) > 0` is the alert if this ever
regresses. Details: [Movers → The object sweep](movers.md#the-object-sweep).

`runJob` hooks also now get a default `ttlSecondsAfterFinished` (1h) when your
`jobSpec` omits one, so a completed hook Job and its Pod no longer linger for the
whole retention window. Set the field explicitly to keep them longer.

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
present and does nothing. The CRDs carry no distinguishing label, so select them by
their API group:

```bash
# Run BEFORE upgrading / reconciling to 0.6.0
for crd in $(kubectl get crd -o name | grep kopiur.home-operations.com); do
  kubectl annotate "$crd" helm.sh/resource-policy=keep --overwrite
done
```

Prefer an explicit list? Annotate the eight CRDs by name — equivalent, and it errors
loudly if one is missing rather than silently matching nothing:

```bash
for name in repositories clusterrepositories snapshotpolicies snapshots \
            snapshotschedules restores maintenances repositoryreplications; do
  kubectl annotate "crd/${name}.kopiur.home-operations.com" \
    helm.sh/resource-policy=keep --overwrite
done
```

Confirm the annotation landed before going further — every kopiur CRD must show `keep`:

```bash
kubectl get crd -o custom-columns='NAME:.metadata.name,KEEP:.metadata.annotations.helm\.sh/resource-policy' \
  | grep -E 'NAME|kopiur.home-operations.com'
```

/// note | Flux / Argo: the pin survives reconciles
The `keep` annotation is read from the **live** CRD at the instant Helm would prune it,
and Flux's helm-controller (like Argo) runs the same Helm engine — so the pin is honored
under GitOps exactly as on the CLI. Helm's three-way merge only strips fields it
previously set, so a reconcile will not remove an annotation you added out of band.

- If you run helm-controller **drift detection** (`spec.driftDetection.mode: enabled`),
  it can revert that out-of-band annotation while you are still on 0.5.x. Leave drift
  detection off for the cutover, or add the annotation immediately before you trigger the
  reconcile.
- You do **not** need to change `spec.upgrade.crds` for safety — the pinned CRDs persist
  whatever its value. Set it to `CreateReplace` if you also want the **0.6.0 CRD schema**
  applied on upgrade (safe — replacing a CRD keeps its CRs); otherwise apply
  `deploy/crds/` yourself (see [CRD lifecycle](install.md#crd-lifecycle)).
///

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
helm upgrade kopiur oci://ghcr.io/home-operations/charts/kopiur --version 0.6.0 -n kopiur-system -f my-values.yaml
```

With Flux/Argo, bump the chart to `0.6.0` (e.g. `spec.chart.spec.version: 0.6.0` on the
`HelmRelease`) **together with** the migrated values, commit, then reconcile:

```bash
flux reconcile helmrelease kopiur -n kopiur-system --with-source
```

**5. Verify nothing was lost.** The CRD count should still be 8, and every resource
should still be there:

```bash
kubectl get crd | grep kopiur.home-operations.com   # expect 8 lines
kubectl get repositories,clusterrepositories,snapshots,restores,snapshotpolicies,snapshotschedules,maintenances -A
```

**6. (Optional) leave or remove the `keep` annotation.** It is harmless to leave — it
only stops Helm from ever deleting the CRDs, which is exactly what the 0.6.0 `crds/`
directory already guarantees. To drop it:

```bash
for crd in $(kubectl get crd -o name | grep kopiur.home-operations.com); do
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
