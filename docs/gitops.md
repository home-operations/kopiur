# GitOps with Kopiur

Kopiur is built to be a well-behaved Flux and Argo citizen. Every reconciled resource reports standard health, every object the operator creates is labeled and owned, the controller never writes back into your `spec`, and cross-field invariants are validated in the apiserver, so CI catches them too.

This page is the reference for those guarantees: `kubectl wait`, health checks, drift hygiene, and CI validation.

## kstatus conditions + `observedGeneration`

Every reconciled CRD exposes standard Kubernetes [`Condition`s](https://kubernetes.io/docs/reference/using-api/api-concepts/) and a `status.observedGeneration`, following the [kstatus](https://github.com/kubernetes-sigs/cli-utils/blob/master/pkg/kstatus/README.md) convention.

That makes the resources first-class for `kubectl wait`, for Flux `healthChecks` and `healthCheckExprs`, and for Argo CD health, with no custom health plugin.

| Condition      | Meaning |
| -------------- | --- |
| `Ready`        | The resource is reconciled and healthy (repository connected, schedule armed, restore complete, …). The one condition to gate on. |
| `Reconciling`  | The controller is actively working toward the desired state (transient). |
| `Stalled`      | Progress is blocked on something that won't resolve by retrying (e.g. a missing dependency, a terminal kopia error) — look at the message. |

`observedGeneration` is the `metadata.generation` the status reflects. When it lags `metadata.generation`, the controller has not caught up to your latest edit yet.

### `kubectl wait`

```console
# Block until a repository is connected before applying policies that use it
$ kubectl wait --for=condition=Ready repository/primary -n billing --timeout=120s

# Wait for a one-shot restore to finish
$ kubectl wait --for=condition=Ready restore/postgres-verify -n billing --timeout=30m
```

Each kind also has its own conditions alongside the kstatus three. A `Repository` adds `Connected` and `MaintenanceOwned`, a `SnapshotPolicy` adds `RepositoryReachable`, and a `Snapshot` adds `SnapshotCreated`. `kubectl describe` shows the full set with human-readable messages.

### Flux

```yaml
# A Kustomization can gate on a Repository going Ready
spec:
    healthChecks:
        - apiVersion: kopiur.home-operations.com/v1alpha1
          kind: Repository
          name: primary
          namespace: billing
    # …or the CEL form (Flux ≥ 2.x):
    healthCheckExprs:
        - apiVersion: kopiur.home-operations.com/v1alpha1
          kind: SnapshotPolicy
          inProgress: "status.conditions.filter(c, c.type == 'Reconciling').exists(c, c.status == 'True')"
          failed: "status.conditions.filter(c, c.type == 'Stalled').exists(c, c.status == 'True')"
          current: "status.conditions.filter(c, c.type == 'Ready').exists(c, c.status == 'True')"
```

### Dependency gating

Because the conditions are standard, dependents gate cleanly. A `SnapshotPolicy` in a tenant namespace will not usefully reconcile until its `Repository` is `Ready`, and you can express that with a Flux `dependsOn`, pointed at the Kustomization that contains the `Repository`, or with an Argo sync-wave.

## Materialized defaults — no diff-thrash

Fields whose default is unconditional now carry a real OpenAPI `default:`, and it is written into the stored object:

| Field | Materialized default |
| --- | --- |
| `SnapshotPolicy.spec.copyMethod` | `Snapshot` |
| `Restore.spec.source.fromPolicy.offset` | `0` |
| `SnapshotSchedule.spec.schedule.runOnCreate` | `false` |
| `SnapshotSchedule.spec.schedule.concurrencyPolicy` | `Forbid` |
| `Repository.spec.mode` / `ClusterRepository.spec.mode` | `ReadWrite` |
| `Repository.spec.onNamespaceDelete` / `ClusterRepository…` | `Orphan` |

They appear in `kubectl explain` and round-trip in the stored spec, so Flux and Argo do not report an `OutOfSync` diff against a value the controller set.

*Conditional* defaults, meaning identity and `policy.onMissingSnapshot`, stay resolved by the controller or webhook and pinned to **status**. They are never written into `spec`.

## Standing fields that act exactly once

Some fields are written for a situation that has not happened yet. They sit in Git forever without doing anything until it does.

They are safe to commit unconditionally, and they remove exactly the "fresh install or recovery?" branch that GitOps hates.

- **`Repository.spec.seed` and `ClusterRepository.spec.seed`** initialize the repository from a surviving replica. The seed is armed **only** while the repository has never been initialized, meaning `status.uniqueId` is unset, *and* the backend really is uninitialized.

  On every later reconcile it is a documented no-op that copies nothing and touches nothing, reporting `Seeded=True` with reason `AlreadyInitialized`. Commit it once and the same manifest set rebuilds the cluster on the day you need it; see [Scenario 10](scenarios/dr-with-replicated-repository.md). It writes only to `status.seed`, never back into your `spec`.

- **`Restore` with `target.populator: {}`** stays passive until a PVC claims it through `dataSourceRef`; see [deploy-or-restore](restores.md#deploy-or-restore-gitops).

  Its `waitTimeout` window opens when the restore can first proceed, not when you applied it, so a populator that sits in Git for months does not arrive with its window already spent. The same holds for apply ordering inside one commit: a `Restore` whose `Repository` or `SnapshotPolicy` has not landed yet parks in `Pending` with `RestoreReferentMissing`, its window still closed, and un-parks by itself once the referenced object appears.

`spec.seed` is also **mutable**. Editing it mid-seed lets the running Job finish, discards its result as stale, and relaunches for the live spec, so a GitOps change to the seed source is applied rather than ignored. Read [Repositories → editing `spec.seed` mid-flight](repositories.md#editing-specseed-mid-flight-and-suspend) before you repoint a migrate seed that has already started.

## Status-only writes

The controller writes only `.status`, and **never** your `spec`. The pinned identity lives in `status.resolved.identity`, and new fields default through OpenAPI rather than by mutating the spec.

A write-back into spec would make Argo or Flux permanently `OutOfSync`, so Kopiur does not do it. This holds in every code path, not just where it is convenient.

## managed-by + ownerReferences

Every object Kopiur creates carries `app.kubernetes.io/managed-by: kopiur` **and** an `ownerReference` to the custom resource that caused it:

- mover Jobs (backup/restore/maintenance/replication),
- the minted `kopiur-mover` ServiceAccount + RoleBinding,
- the cache PVC (when `cache.mode: Persistent`),
- CSI `VolumeSnapshot`s,
- the projected credential Secret (§8).

So Argo and Flux do not report them `OutOfSync` or prune them, and they garbage-collect with their owner. Tell your GitOps engine to ignore the children the operator manages:

```yaml title="Argo CD — resource.exclusions / ignore"
# argocd-cm: ignore kopiur-managed children by label
resource.customizations.ignoreDifferences.all: |
    managedFieldsManagers: [kopiur]
```

```yaml title="Flux — exclude by label in the Kustomization source, or"
# A Kustomization that only owns the kopiur CRs (not their children) needs no
# special config: the children carry ownerReferences and are not in Git.
```

## Suspend — one declarative pause

`suspend: true` is available consistently across `Repository`, `ClusterRepository`, `SnapshotPolicy`, `SnapshotSchedule` (as `schedule.suspend`), and `RepositoryReplication`.

Pause through Git, and see it in a `SUSPENDED` print column where one exists. There is no imperative `kubectl` dance.

## CRD-schema validation (`x-kubernetes-validations`)

Cross-field invariants are CEL rules written **into the CRD schema**, so the apiserver enforces them *and* so do `kubeconform` and `flux build` in CI. That shrinks the webhook, and it means a bad manifest fails your PR gate rather than production.

| Kind | Rule |
| --- | --- |
| `SnapshotPolicy` | each `source` is exactly one of `pvc`/`pvcSelector`/`nfs`. |
| `SnapshotSchedule` | exactly one of `policyRef` / `policySelector`. |
| `Restore` | exactly one of `target.pvc` / `target.pvcRef` / `target.populator`. |
| `Repository` / `ClusterRepository` | `create.{splitter,hash,encryption,ecc}` are immutable (transition rules). The `encryption.passwordSecretRef` reference is mutable — rename/repoint freely as long as it resolves to the same password value. |

/// tip | Validate before you push

```console
$ flux build kustomization apps --dry-run | kubeconform -strict -schema-location default \
    -schema-location 'https://…/kopiur.home-operations.com/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json'
```

The published CRD JSON schemas, on the home-operations schema server and in `datreeio/CRDs-catalog`, let `kubeconform` validate kopiur resources without a cluster, and make `# yaml-language-server: $schema=` editor hints resolve.

///

## Webhook failure mode

The admission webhook is scoped to kopiur kinds only, so a `failurePolicy: Fail` outage never wedges unrelated GitOps applies.

**CRD applies bypass the webhook**, so a CRD-only sync-wave or `dependsOn` still bootstraps during operator downtime. Apply CRDs in an early wave so a custom resource never races ahead of its CRD.

Two of the CRDs, `Repository` and `ClusterRepository`, now exceed the 256 KB cap on the `kubectl.kubernetes.io/last-applied-configuration` annotation used by client-side apply. Argo CD applications that manage the CRDs therefore need `syncOptions: [ServerSideApply=true]`. Flux's `kustomize-controller` already applies server-side by default.

## The populator cutover caveat

A bound PVC's `dataSourceRef` is **immutable**.

Migrating an existing app onto the volume-populator restore path, meaning `target.populator`, is therefore a snapshot-gated delete-and-repopulate, not a silent `git push`. See [Restores → deploy-or-restore](restores.md#deploy-or-restore-gitops). Back up, delete the old PVC, then let the populator-backed PVC provision. Plan the cutover deliberately.

## Upgrading across the 0.6.0 CRD move

Upgrading **from 0.5.x to 0.6.0** relocated the CRDs into Helm's `crds/` directory.

Under GitOps that crossing removes and re-installs the CRDs, which cascade-deletes your custom resources, unless you pin them first. Recovering afterwards is a reconcile from Git: reinstall the release to recreate the CRDs, then reconcile the apps that own the custom resources. The full pre-upgrade step and the Flux and Argo recovery sequence live on the [Upgrading](upgrade.md#upgrading-05x--060-one-time-crd-migration) page.

## See also

- [Upgrading](upgrade.md) covers the one-time 0.5.x to 0.6.0 CRD migration and recovery.
- [Restores → deploy-or-restore](restores.md#deploy-or-restore-gitops) is the one-bundle GitOps pattern.
- [Scenario 10 — DR from a replicated repository](scenarios/dr-with-replicated-repository.md) covers `spec.seed`, the standing field that rebuilds a repository from its mirror.
- [Field reference](field-reference.md) lists the conditions and status fields per kind.
- [Observability](dev/observability.md) covers metrics and the `resource_phase` gauge.
