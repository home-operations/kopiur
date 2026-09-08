# Scenario 09 — Share one repository across clusters

**Several clusters back up to, or restore from, the SAME kopia repository at the same time.** A platform team running one S3 bucket for every cluster's backups. An active-active pair. A warm DR-standby cluster that also reads from the primary's history.

This is different from [migrating an app](migrate-across-clusters.md), where the app runs in exactly one place before and after. Here more than one cluster's operator writes to, or reads from, the same physical repository, on an ongoing basis.

## Why this needs its own knob

Kopia's own identity model is `username@hostname:path`, and `hostname` defaults to the **namespace**. Point two clusters at the same bucket with nothing else set, and every same-named namespace writes under the **identical** kopia identity. `billing` on cluster "east" and `billing` on cluster "west" collide. Three things break at once:

| Collision surface | Without `identityDefaults.cluster` | With it set |
| --- | --- | --- |
| **Identity dedup / retention** | Both clusters' `Snapshot` CRs for `billing` share one kopia source. Anything that matches a snapshot by path alone (retention, pin/unpin re-matching, a `Snapshot` CR's own deletion) can't tell which cluster's snapshot is which. | Each cluster's default hostname becomes `<namespace>.<cluster>`. The identities are distinct, so retention, pin/unpin re-matching, and CR deletion never cross-match another cluster's snapshot of the same path. |
| **Catalog cross-materialization** | Every cluster's catalog scan sees the OTHER cluster's snapshots as unrecognized, same-namespace history, and would turn them into its own `discovered` `Snapshot` CRs. That is noise at best, and a misleading duplicate at worst. | `catalog.foreignSnapshots` classifies a `.<cluster>` suffix that isn't this cluster's own. `Ignore` is the default and drops those rows, while still counting them in `status.catalog.foreignSnapshotCount` so they are never silently invisible. `Fallback` collects them into one namespace instead. |
| **Maintenance lease** | The default-managed `Maintenance`'s lease derives from the repository name alone, so it is the same string on every cluster. The yield/claim logic then has no way to tell "mine" from "the other cluster's" claim. | The lease is cluster-qualified as `kopiur/<cluster>/...`. Each cluster gets a distinct lease and kopia owner, so exactly one cluster's `Maintenance` actually claims and runs. See [Maintenance → Ownership and shared repositories](../maintenance.md#ownership-and-shared-repositories). |

`identityDefaults.cluster` is the single field that fixes all three at once. It takes an RFC 1123 label, at most 32 characters, with no dots. See [Repositories → `identityDefaults.cluster`](../repositories.md#identitydefaultscluster--sharing-one-repository-across-clusters) for the field itself.

## Greenfield setup — two supported shapes

Starting a shared repository from scratch, pick one of two shapes:

### Shape A — active-active, both clusters write

Every cluster gets its own `ClusterRepository` (or `Repository`) object pointing at the same bucket, each with a **distinct** `identityDefaults.cluster`, and maintenance enabled on exactly **one** of them:

```yaml
--8<-- "deploy/examples/scenarios/09-shared-repository-multi-cluster.yaml"
```

### Shape B — one primary writes, a secondary is read-only

If only ONE cluster should ever back up, such as a true DR standby that only restores, or a read replica, skip the second cluster's maintenance and writer role entirely. Set [`mode: ReadOnly`](../repositories.md#mode--readwrite-or-readonly) on the secondary's repository object instead of also making it a writer.

```yaml
spec:
    mode: ReadOnly # this cluster only ever restores; never backs up, never bootstraps write
    identityDefaults:
        cluster: west # still set, so any catalog/placement work stays correct
```

A `ReadOnly` repository never launches a backup mover, never stamps or restamps a maintenance owner, and skips maintenance projection. It can still discover and restore the primary's snapshots.

That owner-stamp gating landed in M6 to close a specific bug: a read-only consumer would connect read-write to self-heal an owner it should never have touched.

/// tip | Recommended on any shared repository

Set `catalog.periodicRefresh: true`, which is off by default, on every cluster sharing the repository. Each cluster then keeps discovering the others' snapshots as they are written, instead of only re-scanning on its own next spec change.

///

## Turning it on for a repository already in production

This is the higher-stakes path: one cluster already has a live repository with real snapshot history, and you're adding one or more peers.

**Follow the steps in order.** Each one closes a specific window that the step before it would otherwise leave open.

### Step 0 — upgrade every cluster's operator (and CRDs) FIRST

`identityDefaults.cluster` is a new field. Upgrade the operator **and** mover image on **every** cluster that will share the repository before you set it anywhere.

A `helm upgrade` bumps the running images but does **not** touch the CRDs, because Helm only ever applies its special `crds/` chart directory during `helm install`. See [Installation → CRD lifecycle](../install.md#crd-lifecycle). So on a helm-CLI install you must also re-apply the CRDs by hand:

```console
$ kubectl apply --server-side -f deploy/crds/
```

Verify the field actually resolves on **every** cluster before proceeding:

```console
$ kubectl explain clusterrepository.spec.identityDefaults.cluster
    # or: kubectl explain repository.spec.identityDefaults.cluster
```

If this errors or omits `cluster`, that cluster's CRD is still stale. Fix it before touching any repository's spec. A cluster with the old CRD would silently strip `identityDefaults.cluster` at admission, because the structural schema prunes fields it doesn't recognize, and you'd have no error telling you the change didn't take.

### Step 1 — pick names and the ONE maintenance cluster, before any identity change

Decide, in order:

1. Every cluster's `cluster` name (an RFC 1123 label, at most 32 characters, no dots).
2. Which ONE cluster owns maintenance going forward.

Then, on every **non-owner** cluster's repository, **before** touching `identityDefaults` at all:

```yaml
spec:
    maintenance:
        enabled: false # do this BEFORE the identity flip, not after
```

Also remove `ownership.takeoverPolicy: Force` from every cluster except the one true owner, if any of them had it set.

Doing this first means that when the identity flip in the next step changes the maintenance lease format, there is only ever ONE cluster racing to claim it, never a multi-way scramble during the transition. See [Maintenance → pick ONE owner](../maintenance.md#sharing-one-repositorys-maintenance-across-clusters-pick-one-owner) for the full reasoning.

### Step 2 — annotate, then set `cluster`

On the repository that already has history, and on every peer being added, in the SAME apply:

```yaml
metadata:
    annotations:
        kopiur.home-operations.com/allow-identity-change: "intentional"
spec:
    identityDefaults:
        cluster: east
```

Setting `identityDefaults.cluster` for the first time on a repository whose consumers already have history IS an identity change. The webhook rejects it fleet-wide without the annotation. See [Repositories → `identityDefaults.cluster`](../repositories.md#identitydefaultscluster--sharing-one-repository-across-clusters) and [Backups → identity](../backups.md#identity--what-kopia-records-usernamehostnamepath).

/// warning | `foreignSnapshots` is REQUIRED, explicitly, if you already have a `fallbackNamespace`

If `catalog.fallbackNamespace` was already configured, for instance from adopting a repository before it had a cluster identity, the validator now **requires** `catalog.foreignSnapshots` to be set explicitly in the same edit. Choose either `Ignore` or `Fallback`.

This is deliberate: adopting a cluster identity must never silently repurpose, or silently keep, an existing fallback collector without you saying which behavior you actually want.

Choosing `Ignore` never touches kopia data. Any discovered `Snapshot` CR the fallback namespace was holding for what now classifies as another cluster's snapshot is simply **expired** on this same re-scan, since it no longer matches a placeable classification. Expiring deletes the CR. Discovered rows are forced to `deletionPolicy: Retain`, so the underlying kopia snapshot is untouched either way.

///

This spec edit is itself a change, so it triggers an immediate re-scan. You don't need to separately bump `catalog.retain` or wait for `periodicRefresh`. Any other catalog config edit gets the same deterministic rescan-on-spec-change behavior.

### Step 3 — what changes on the very next backup

- **The resolved identity updates.** Every consumer `SnapshotPolicy`'s `status.resolved.identity.hostname` becomes `<namespace>.<cluster>` on its next reconcile. Identity re-resolves from the live repository; it was never frozen at admission.
- **New lineage.** The next `Snapshot` this policy takes writes under that new hostname, which is a different kopia source than everything before it.
- **Content dedup, not a re-upload.** Kopia deduplicates by content hash across the WHOLE repository regardless of identity, so the first post-change snapshot still dedups against everything already uploaded. This is a metadata change, not a fresh full backup.
- **Legacy `Snapshot` CRs keep pruning normally.** Kopiur's own GFS retention selects over every `Snapshot` CR labeled to a policy, regardless of which identity each one recorded. So pre-change CRs are not abandoned by their own policy's retention; they continue aging out exactly as before. See the residual window below for the multi-cluster hazard this does **not** cover.

### Step 4 — the residual window (and how to close it)

Before the change, if more than one cluster was ALREADY writing unsafely under the same bare-namespace identity, each cluster still holds its own local `Snapshot` CRs pointing into that one shared, pre-migration lineage.

Every cluster's GFS retention prunes its **own** CRs independently. So until every cluster's pre-migration CRs have aged out of their **own** GFS windows, more than one cluster can still independently decide to delete a kopia snapshot from that shared pre-migration history.

This is exactly the [identity dedup and retention hazard](#why-this-needs-its-own-knob) the change fixes going forward. It just doesn't retroactively fix history that predates it.

**Mitigation:** on every cluster except the one you want to keep pruning that old shared lineage, flip the legacy pre-change `Snapshot` CRs to `deletionPolicy: Orphan`. The finalizer then removes only the Kubernetes object when GFS ages them out, and never touches the kopia snapshot:

```console
$ kubectl get snapshot -n billing -l kopiur.home-operations.com/config=postgres-data -o json \
  | jq -r '.items[] | select((.status.snapshot.identity.hostname // "") | contains(".") | not) | .metadata.name' \
  | xargs -r -n1 kubectl patch snapshot -n billing --type=merge -p '{"spec":{"deletionPolicy":"Orphan"}}'
```

The filter selects rows whose recorded identity hostname has no `.`, which is the pre-cluster-identity, bare-namespace form. Only PRE-change CRs are touched. Run it once, right after the change, before any new-identity CRs exist.

### Step 5 — maintenance's lease upgrades itself

The owner cluster's managed `Maintenance` upgrades its lease to the cluster-qualified format on its very next claim, automatically.

The operator records the pre-cluster lease as a recognized `ownerAliases` entry. The run therefore recognizes kopia's already-recorded owner as itself, claims cleanly, and re-stamps the lease to the new format. You do not need `takeoverPolicy: Force` for this step. See [Maintenance → self-healing a stale owner](../maintenance.md#self-healing-a-stale-owner).

### Step 6 — reaching the old history afterward

The pre-change lineage doesn't disappear. Restore from it at any time via the raw identity, whether or not its `Snapshot` CR or catalog row still exists:

```yaml
--8<-- "deploy/examples/13-restore-by-identity.yaml"
```

## Edge notes

- **The fork guard only fires on an update.** Like the per-policy guard, the repository-edit guard diffs against `oldObject`, so it can only fire on an UPDATE. Deleting and re-applying the repository is a CREATE with no prior object to diff, so it bypasses the guard. That is a known, accepted limitation of admission-time guards in general. Don't rely on delete-and-re-apply as a way around the annotation.
- **Explicit `spec.identity` policies keep the ORIGINAL hazard.** A `SnapshotPolicy` that pins its own `identity.username` and `identity.hostname` never consults the repository's `identityDefaults` at all, because an explicit override always wins. If two clusters both hand-pin the SAME identity, setting `cluster` does nothing for them. Give each cluster's pinned identity its own suffix by hand instead.
- **A custom, dotted `hostnameExpr` gives up catalog placement, but restores still work.** `classify_hostname` splits any hostname at its FIRST `.` and compares the suffix to `cluster`. It has no idea whether the prefix is a real namespace. A hand-written `hostnameExpr` that embeds a dot for its own reasons, rather than following the `<namespace>.<cluster>` convention, will be misclassified, and the catalog's placement pass will likely give up on it as fallback or unplaced. [`Restore.spec.source.identity`](../restores.md#identity--a-raw-kopia-identity) matches the raw `username@hostname:path` directly and is unaffected. Prefer the default `<namespace>.<cluster>` scheme, meaning no `hostnameExpr` at all, unless you have a specific reason not to.
- **Verification goes quiet for one cycle, not forever.** `verification.deep` restores the **latest snapshot for the policy's currently resolved identity**. If a scheduled deep verify lands in the gap between the identity change and the first backup after it, it targets the new identity and finds nothing yet to restore. Expect at most one harmless miss until the first new-lineage snapshot completes. `verification.quick` and the overall verification gate are unaffected, because they don't reset on an identity change.
- **Stale legacy kopia policy objects are cosmetic.** Kopiur pins kopia's own native retention to effectively-infinite for each identity it manages, using `kopia policy set` scoped to that identity. After an identity change, the OLD identity's policy object becomes unused clutter in `kopia policy list`. It's harmless to leave. `kopia policy delete <old-username>@<old-hostname>`, run through `kubectl exec` into any mover or kopia-shell pod connected to the repository, tidies it up if you care.

## Verification checklist

```console
# 1. The consumer's resolved identity carries the new suffix:
$ kubectl get snapshotpolicy postgres-data -n billing \
    -o jsonpath='{.status.resolved.identity.hostname}'
billing.east

# 2. Exactly one cluster's Maintenance shows itself as the owner; the OWNER
#    column is the cluster-qualified lease (kopiur/<cluster>/...), and the
#    others show LeaseOwned=False, reason=LeaseHeldByOther (or don't exist at
#    all, if maintenance.enabled: false there):
$ kubectl get maintenance -A

# 3. Peers' snapshots are being counted (not silently dropped or duplicated):
$ kubectl get clusterrepository shared-primary \
    -o jsonpath='{.status.catalog.foreignSnapshotCount}'

# 4. The CRD schema is current on every cluster (step 0, re-checked):
$ kubectl explain clusterrepository.spec.identityDefaults.cluster
```

## See also

- [Repositories → `identityDefaults.cluster`](../repositories.md#identitydefaultscluster--sharing-one-repository-across-clusters): the field itself, and the identity-change warning.
- [Backups → identity](../backups.md#identity--what-kopia-records-usernamehostnamepath): how identity resolves live, and both fork guards.
- [Maintenance → Ownership and shared repositories](../maintenance.md#ownership-and-shared-repositories): the lease format table, `ownerAliases`, and self-healing rules in full.
- [Migrate an app across clusters or namespaces](migrate-across-clusters.md): the one-time-move cousin of this scenario.
- [Troubleshooting → `LeaseHeldByOther` on a shared repository](../troubleshooting.md#leaseheldbyother-on-a-repository-shared-across-clusters--expected-or-stale): telling expected from stale.
- [`deploy/examples/13-restore-by-identity.yaml`](../examples.md#example-13--restore-by-raw-kopia-identity): restoring old-lineage history by raw identity.
