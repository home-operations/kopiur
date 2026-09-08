# ClusterRepository

A `ClusterRepository` is a shared kopia repository that lives outside any namespace. A platform team operates it, and namespaces you allow can reference it.

It has the same storage surface as a [Repository](repository.md), including `backend`, `encryption`, `create`, `moverDefaults`, `scheduleDefaults` and `catalog`. On top of that it adds a tenancy gate, `allowedNamespaces`, and per-namespace identity expressions, `identityDefaults`.

For the short type-and-default table see the [field reference](../../field-reference.md). For task guidance see [Repositories](../../repositories.md).

/// warning | Every Secret reference needs an explicit namespace

A `ClusterRepository` is cluster-scoped, so it has no namespace of its own to resolve references against. Every Secret or config reference **must carry an explicit `namespace`**, in `backend`, in `encryption`, in `server`, everywhere. The type system cannot express that rule, so the webhook enforces it: a reference without a namespace is rejected when you apply it.

///

## `spec`

These fields behave exactly as they do on the [Repository](repository.md) page: `backend`, `create`, `moverDefaults`, `scheduleDefaults`, `concurrency`, `catalog`, `maintenance`, `onNamespaceDelete`, `mode`, `suspend`, `health` and `parameters`.

`parameters.epoch` is worth a note on a shared repository. It describes the repository itself, so declare it on the cluster that owns the repository. Two clusters declaring different values will fight over them, and a `mode: ReadOnly` consumer is rejected for declaring any.

The rest of this page covers the differences and the additions.

/// note | What `concurrency.maxConcurrentJobs` counts under a namespaced install

Kopiur enforces the cap by listing this repository's in-flight pooled mover Jobs, and that list only covers the namespaces the operator watches.

Under an `installScope: namespaced` install, a `ClusterRepository`'s cap therefore bounds the pooled Jobs **in the watched namespace**, not in every namespace that uses the repository. That is the only answer available: a cluster-wide list under a namespaced install's `Role` RBAC is a permanent 403 that would wedge the reconcile. It is also the same scoping every other reconcile-time list already uses. A cluster-scoped install, which is the default, counts every namespace.

///

### `encryption`

The repository password as a Secret reference. Because the object is cluster-scoped, the reference **must** carry an explicit `namespace`.

### `allowedNamespaces`

The tenancy gate: which namespaces may reference this repository. The webhook checks it on every consumer object. Set exactly one of:

- `list: [...]` names namespaces explicitly.
- `selector: {...}` matches namespaces by label, using a `LabelSelector`.
- `all: true` allows every namespace. It must be `true`; `false` means nothing and the webhook rejects it.

The number of namespaces this currently resolves to appears in `status.allowedNamespaceCount` and in the `Namespaces` print column.

### `identityDefaults`

CEL expressions that derive a consumer's kopia identity when a `SnapshotPolicy` does not override it. Kopiur evaluates them at admission.

Each `*Expr` returns a string and is evaluated against `namespace`, `policyName`, `labels` and `annotations`, taken from the consuming `SnapshotPolicy`'s metadata. The expressions run sandboxed with no I/O and are validated at admission, so a typo or an out-of-scope variable is rejected when you apply.

- `hostnameExpr` is the CEL expression for the kopia identity hostname, for example `"namespace"`.
- `usernameExpr` is the CEL expression for the kopia identity username, for example `"namespace + '-' + policyName"`.

### `server`

An optional kopia web UI server. Adding the block turns it on. Because the object is cluster-scoped, the target `namespace` is required. See [Server](../../server.md).

### `maintenance`

Managed by default, the same as on the namespaced kind. Because `Maintenance` is itself namespaced, `maintenance.namespace` picks where the owned `Maintenance` object lands. It defaults to the operator's namespace. See [Maintenance](../../maintenance.md).

### `seed`

The same block as on the [Repository](repository.md#seed) page, plus these cluster-scoped resolution rules:

- The seeding bootstrap Job runs in **the namespace this repository's own credentials resolve in**. That is the operator's namespace, unless `encryption.passwordSecretRef.namespace` points somewhere else. A blob seed's `from.backend` credential Secret is loaded with `envFrom`, which only reads the local namespace, so the Secret must live in **that** namespace. A seed `secretRef` that names a namespace is rejected at admission, because a cluster-scoped spec cannot know the right one.
- A migrate seed's `from.repository` with no `namespace` resolves in the operator's namespace, the same rule every other cluster-scoped reference follows. Set it explicitly whenever the source lives anywhere else.
- An armed seed makes the object hold its cleanup finalizer, so that a deletion reconcile runs at all. That reconcile then tries to delete the in-flight `<name>-discovery` Job, but it does not block or retry, and a deletion is never stuck waiting on the cleanup. A Job that already finished is an ordinary miss. A failed delete is logged as a warning. If the Job's namespace cannot be resolved at all, the operator warns and tells you to delete the Job by hand. This exists because a namespaced Job cannot carry a cluster-scoped ownerReference, so nothing else would ever clean up a 24-hour seeding Job whose owning object is gone.

### `credentialProjection`

The repository owner's gate on projecting this repository's credential Secrets into another namespace.

It is **off by default** (`credentialProjection.allowed: false`). A consumer's own `credentialProjection.enabled` is necessary but not enough: the `ClusterRepository` owner must also allow it, and operator RBAC must permit it. If any of the three is missing, projection does not happen. A namespaced `Repository` has no such gate, because projection there copies a Secret into the namespace it already lives in.

## `status`

Status mirrors [Repository](repository.md) status: `phase`, `observedGeneration`, `resolvedCredentialVersion`, `uniqueId`, `backend`, `storageStats`, `catalog`, `seed`, `server` and `conditions`. There is one addition:

- `allowedNamespaceCount` is the number of namespaces `spec.allowedNamespaces` currently resolves to. It is also the `Namespaces` print column.

`uniqueId` behaves exactly as it does on a namespaced `Repository`. It is set on the first successful bootstrap, and once set, Kopiur will never create a fresh empty repository at this backend. A wiped backend parks at `Failed` with reason `RepositoryReinitializeBlocked`.

## Annotations

The same set as on [Repository](repository.md#annotations). Only the way you apply them differs: a `ClusterRepository` is cluster-scoped, so the commands Kopiur prints in its condition messages and events carry **no `-n`**:

```console
$ kubectl annotate clusterrepository shared \
    kopiur.home-operations.com/allow-reinitialize=$(kubectl get clusterrepository shared -o jsonpath='{.status.uniqueId}')
```

See [Deliberately re-initialize a wiped repository](../../repository-health.md#deliberately-re-initialize-a-wiped-repository).
