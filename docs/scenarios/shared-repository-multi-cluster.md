# Scenario 09 — Share one repository across clusters

**Several clusters back up to (or restore from) the SAME kopia repository at
the same time** — a platform team running one S3 bucket for every cluster's
backups, an active-active pair, or a warm DR-standby cluster that also reads
from the primary's history. Unlike [migrating an app](migrate-across-clusters.md),
where the app runs in exactly one place before and after, this is a genuinely
**concurrent** shape: more than one cluster's operator writes to (or reads
from) the same physical repository, on an ongoing basis.

## Why this needs its own knob

Kopia's own identity model (`username@hostname:path`) defaults `hostname` to
the **namespace**. Point two clusters at the same bucket without anything
else and every same-named namespace (`billing` on cluster "east", `billing`
on cluster "west") writes under the **identical** kopia identity — three
things break at once:

| Collision surface | Without `identityDefaults.cluster` | With it set |
| --- | --- | --- |
| **Identity dedup / retention** | Both clusters' `Snapshot` CRs for `billing` share one kopia source; anything that matches a snapshot by path alone (retention, pin/unpin re-matching, a `Snapshot` CR's own deletion) can't tell which cluster's snapshot is which. | Each cluster's default hostname becomes `<namespace>.<cluster>` — distinct identities, so retention, pin/unpin re-matching, and CR deletion never cross-match another cluster's snapshot of the same path. |
| **Catalog cross-materialization** | Every cluster's catalog scan sees the OTHER cluster's snapshots as unrecognized, same-namespace history and would materialize them as its own `discovered` `Snapshot` CRs — noise at best, a misleading duplicate at worst. | `catalog.foreignSnapshots` classifies a `.<cluster>` suffix that isn't this cluster's own: `Ignore` (default) drops those rows — still counted in `status.catalog.foreignSnapshotCount`, never silently invisible — `Fallback` collects them into one namespace instead. |
| **Maintenance lease** | The default-managed `Maintenance`'s lease derives from the repository name alone, byte-identical on every cluster — there is no way for the yield/claim logic to tell "mine" from "the other cluster's" claim. | The lease is cluster-qualified (`kopiur/<cluster>/...`); each cluster gets a distinct lease and kopia owner, so exactly one cluster's `Maintenance` actually claims and runs. See [Maintenance → Ownership and shared repositories](../maintenance.md#ownership-and-shared-repositories). |

`identityDefaults.cluster` (an RFC 1123 label, at most 32 characters, no
dots) is the single field that fixes all three at once — see
[Repositories → `identityDefaults.cluster`](../repositories.md#identitydefaultscluster--sharing-one-repository-across-clusters)
for the field itself.

## Greenfield setup — two supported shapes

Starting a shared repository from scratch, pick one of two shapes:

### Shape A — active-active, both clusters write

Every cluster gets its own `ClusterRepository` (or `Repository`) object
pointing at the same bucket, each with a **distinct** `identityDefaults.cluster`,
and maintenance enabled on exactly **one** of them:

```yaml
--8<-- "deploy/examples/scenarios/09-shared-repository-multi-cluster.yaml"
```

### Shape B — one primary writes, a secondary is read-only

If only ONE cluster should ever back up (a true DR-standby that only restores,
or a read replica), skip the second cluster's own maintenance/writer role
entirely: set [`mode: ReadOnly`](../repositories.md#mode--readwrite-or-readonly)
on the secondary's repository object instead of also making it a writer.

```yaml
spec:
    mode: ReadOnly # this cluster only ever restores; never backs up, never bootstraps write
    identityDefaults:
        cluster: west # still set, so any catalog/placement work stays correct
```

A `ReadOnly` repository never launches a backup mover, never stamps or
restamps a maintenance owner (M6's ReadOnly owner-stamp gating — a read-only
consumer connecting read-write to self-heal an owner was exactly the bug this
closes), and skips maintenance projection. It can still discover and restore
the primary's snapshots.

/// tip | Recommended on any shared repository

Set `catalog.periodicRefresh: true` (off by default) on every cluster sharing
the repository, so each one keeps discovering the others' snapshots as they
write, instead of only re-scanning on its own next spec change.

///

## Turning it on for a repository already in production

This is the higher-stakes path: one cluster already has a live repository
with real snapshot history, and you're adding one or more peers. **The order
below is load-bearing** — each step exists to close a specific window the
step before it would otherwise leave open.

### Step 0 — upgrade every cluster's operator (and CRDs) FIRST

`identityDefaults.cluster` is a new field. Upgrade the operator **and** mover
image on **every** cluster that will share the repository before you set it
anywhere. A `helm upgrade` bumps the running images but does **not** touch
the CRDs — Helm's special `crds/` chart directory is only ever applied by
`helm install` (see [Installation → CRD lifecycle](../install.md#crd-lifecycle))
— so on a helm-CLI install you must also re-apply the CRDs by hand:

```console
$ kubectl apply --server-side -f deploy/crds/
```

Verify the field actually resolves on **every** cluster before proceeding:

```console
$ kubectl explain clusterrepository.spec.identityDefaults.cluster
    # or: kubectl explain repository.spec.identityDefaults.cluster
```

If this errors or omits `cluster`, that cluster's CRD is still stale — fix it
before touching any repository's spec. A cluster with the old CRD would
silently strip `identityDefaults.cluster` at admission (structural-schema
pruning of an unrecognized field), and you'd have no error to tell you the
flip didn't take.

### Step 1 — pick names and the ONE maintenance cluster, before any identity change

Decide, in order:

1. Every cluster's `cluster` name (an RFC 1123 label, ≤ 32 chars, no dots).
2. Which ONE cluster owns maintenance going forward.

Then, on every **non-owner** cluster's repository, **before** touching
`identityDefaults` at all:

```yaml
spec:
    maintenance:
        enabled: false # do this BEFORE the identity flip, not after
```

And remove `ownership.takeoverPolicy: Force` from every cluster except the
one true owner, if any of them had it set. Doing this first means that when
the identity flip (next step) changes the maintenance lease format, there is
only ever ONE cluster racing to claim it — never a multi-way scramble during
the transition itself. See [Maintenance → pick ONE owner](../maintenance.md#sharing-one-repositorys-maintenance-across-clusters-pick-one-owner)
for the full rationale.

### Step 2 — annotate, then set `cluster`

On the repository that already has history (and on every peer being added),
in the SAME apply:

```yaml
metadata:
    annotations:
        kopiur.home-operations.com/allow-identity-change: "intentional"
spec:
    identityDefaults:
        cluster: east
```

Setting `identityDefaults.cluster` for the first time on a repository whose
consumers already have history IS an identity change — the webhook rejects it
fleet-wide without the annotation (see [Repositories → `identityDefaults.cluster`](../repositories.md#identitydefaultscluster--sharing-one-repository-across-clusters)
and [Backups → identity](../backups.md#identity--what-kopia-records-usernamehostnamepath)).

/// warning | `foreignSnapshots` is REQUIRED, explicitly, if you already have a `fallbackNamespace`

If `catalog.fallbackNamespace` was already configured (e.g. from adopting a
repository before it had a cluster identity), the validator now **requires**
`catalog.foreignSnapshots` to be set explicitly in the same edit — either
`Ignore` or `Fallback`. This is deliberate: adopting a cluster identity must
never silently repurpose or silently keep an existing fallback collector
without you saying which behavior you actually want. Choosing `Ignore` never
touches kopia data — any discovered `Snapshot` CR the fallback namespace was
previously holding for what now classifies as another cluster's snapshot is
simply **expired** (the CR is deleted; discovered rows are forced
`deletionPolicy: Retain`, so the underlying kopia snapshot is untouched
either way) on this same re-scan, since it no longer matches a placeable
classification.

///

This spec edit is itself a change, so it triggers an immediate re-scan (no
need to separately bump `catalog.retain`/wait for `periodicRefresh`) — the
same deterministic-rescan-on-spec-change behavior any other catalog config
edit gets.

### Step 3 — what changes on the very next backup

- **Re-pin.** Every consumer `SnapshotPolicy`'s `status.resolved.identity.hostname`
  updates to `<namespace>.<cluster>` on its next reconcile (identity
  re-resolves from the live repository — it was never frozen at admission).
- **New lineage.** The next `Snapshot` this policy takes writes under that new
  hostname — a different kopia source than everything before it.
- **Content-dedup, not a re-upload.** Kopia deduplicates by content hash
  across the WHOLE repository regardless of identity, so the first post-flip
  snapshot still dedups against everything already uploaded — this is a
  metadata re-pin, not a fresh full backup.
- **Legacy `Snapshot` CRs keep pruning normally.** Kopiur's own GFS retention
  selects over every `Snapshot` CR labeled to a policy regardless of which
  identity each one recorded, so pre-flip CRs are not abandoned by their own
  policy's retention — they continue aging out exactly as before. See the
  residual window below for the multi-cluster-specific hazard this does
  **not** cover.

### Step 4 — the residual window (and how to close it)

Before the flip, if more than one cluster was ALREADY (unsafely) writing
under the same bare-namespace identity, each cluster still holds its own
local `Snapshot` CRs pointing into that one shared, pre-migration lineage.
Every cluster's GFS retention prunes its **own** CRs independently — so until
every cluster's pre-migration CRs have aged out of their **own** GFS windows,
more than one cluster can still independently decide to delete a kopia
snapshot from that shared pre-migration history. This is exactly the
[identity dedup / retention hazard](#why-this-needs-its-own-knob) the flip
fixes going forward — it just doesn't retroactively fix history that
predates it.

**Mitigation:** on every cluster except the one you want to keep pruning that
old shared lineage, flip the legacy (pre-flip) `Snapshot` CRs to
`deletionPolicy: Orphan` — the finalizer then removes only the Kubernetes
object when GFS ages them out, never touching the kopia snapshot:

```console
$ kubectl get snapshot -n billing -l kopiur.home-operations.com/config=postgres-data -o json \
  | jq -r '.items[] | select((.status.snapshot.identity.hostname // "") | contains(".") | not) | .metadata.name' \
  | xargs -r -n1 kubectl patch snapshot -n billing --type=merge -p '{"spec":{"deletionPolicy":"Orphan"}}'
```

(The filter selects rows whose recorded identity hostname has no `.` — the
pre-cluster-identity, bare-namespace form — so only PRE-flip CRs are touched;
run it once, right after the flip, before any new-identity CRs exist.)

### Step 5 — maintenance's lease upgrades itself

The owner cluster's managed `Maintenance` upgrades its lease to the
cluster-qualified format on its very next claim, automatically: the operator
records the pre-cluster lease as a recognized `ownerAliases` entry, so the
run recognizes kopia's already-recorded owner as itself, claims cleanly, and
re-stamps it to the new format. No manual `takeoverPolicy: Force` is needed
for this step specifically — see
[Maintenance → self-healing a stale owner](../maintenance.md#self-healing-a-stale-owner).

### Step 6 — reaching the old history afterward

The pre-flip lineage doesn't disappear — restore from it at any time via the
raw identity, whether or not its `Snapshot` CR (or catalog row) still exists:

```yaml
--8<-- "deploy/examples/13-restore-by-identity.yaml"
```

## Edge notes

- **The fork guard's window.** Like the per-policy guard, the repository-edit
  guard can only fire on an UPDATE (it diffs against `oldObject`); a delete +
  re-apply of the repository is a CREATE with no prior object to diff, so it
  bypasses the guard by design. This is a known, accepted limitation of
  admission-time guards in general — don't rely on delete + re-apply as a way
  around the annotation.
- **Explicit `spec.identity` policies keep the ORIGINAL hazard.** A
  `SnapshotPolicy` that pins its own `identity.{username,hostname}` explicitly
  never consults the repository's `identityDefaults` at all — an explicit
  override always wins. If two clusters both hand-pin the SAME identity, the
  cluster flip does nothing for them; give each cluster's pinned identity its
  own suffix by hand instead.
- **A custom, dotted `hostnameExpr` forgoes catalog placement — restores
  still work.** `classify_hostname` splits any hostname at its FIRST `.` and
  compares the suffix to `cluster` — it has no idea whether the prefix is a
  real namespace. A hand-written `hostnameExpr` that embeds a dot for its own
  reasons (not the `<namespace>.<cluster>` convention) will misclassify under
  this scheme, and the catalog's placement pass will likely give up on it
  (fallback or unplaced) — but [`Restore.spec.source.identity`](../restores.md#identity--a-raw-kopia-identity)
  matches the raw `username@hostname:path` directly and is unaffected. Prefer
  the default `<namespace>.<cluster>` scheme (no `hostnameExpr` at all) unless
  you have a specific reason not to.
- **Verification goes quiet for one cycle, not forever.** `verification.deep`
  restores the **latest snapshot for the policy's currently resolved
  identity** — if a scheduled deep verify lands in the gap between the flip
  and the first post-flip backup, it targets the new identity and finds
  nothing yet to restore. Expect (at most) one harmless miss until the first
  new-lineage snapshot completes; `verification.quick` and the overall
  verification gate are unaffected (they don't reset on an identity change).
- **Stale legacy kopia policy objects are cosmetic.** Kopiur pins kopia's own
  native retention to effectively-infinite per identity it manages
  (`kopia policy set`, scoped to that identity) — after a flip, the OLD
  identity's policy object just becomes unused clutter in `kopia policy list`.
  It's harmless to leave; `kopia policy delete <old-username>@<old-hostname>`
  (via `kubectl exec` into any mover/kopia-shell pod connected to the
  repository) tidies it up if you care.

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

- [Repositories → `identityDefaults.cluster`](../repositories.md#identitydefaultscluster--sharing-one-repository-across-clusters) — the field itself, and the identity-change warning.
- [Backups → identity](../backups.md#identity--what-kopia-records-usernamehostnamepath) — how identity resolves live, and both fork guards.
- [Maintenance → Ownership and shared repositories](../maintenance.md#ownership-and-shared-repositories) — the lease format table, `ownerAliases`, and self-healing rules in full.
- [Migrate an app across clusters or namespaces](migrate-across-clusters.md) — the one-time-move cousin of this scenario.
- [Troubleshooting → `LeaseHeldByOther` on a shared repository](../troubleshooting.md#leaseheldbyother-on-a-repository-shared-across-clusters--expected-or-stale) — telling expected from stale.
- [`deploy/examples/13-restore-by-identity.yaml`](../examples.md#example-13--restore-by-raw-kopia-identity) — restoring old-lineage history by raw identity.
