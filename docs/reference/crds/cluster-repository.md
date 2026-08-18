# ClusterRepository

A cluster-scoped, shared kopia repository operated by a platform team and
referenceable from allow-listed namespaces. It has the same storage surface as a
[Repository](repository.md) (backend, encryption, create, moverDefaults,
scheduleDefaults, catalog), plus a tenancy gate (`allowedNamespaces`) and
per-namespace identity expressions (`identityDefaults`). For the terse
type/default table see the [field reference](../../field-reference.md); for
how-to guidance see [Repositories](../../repositories.md).

/// warning | Every Secret reference needs an explicit namespace

A `ClusterRepository` is cluster-scoped, so it has no namespace of its own to
resolve references against. Every Secret/config reference — in `backend`,
`encryption`, `server`, anywhere — **must carry an explicit `namespace`**. The
type system cannot express this, so it is webhook-enforced: a reference without
a namespace is rejected on apply.

///

## `spec`

Fields shared with `Repository` — `backend`, `create`, `moverDefaults`,
`scheduleDefaults`, `catalog`, `maintenance`, `onNamespaceDelete`, `mode`,
`suspend`, `health`, `parameters` — behave exactly as on the
[Repository](repository.md) page. (`parameters.epoch` is worth calling out for a
shared repository: it describes the repository itself, so declare it on the cluster
that owns it — two clusters declaring different values will fight over them, and a
`mode: ReadOnly` consumer is rejected for declaring any.)
The differences and additions:

### `encryption`

The repository password as a Secret reference. Because the CR is cluster-scoped, the
reference **must** carry an explicit `namespace`.

### `allowedNamespaces`

The tenancy gate: which namespaces are permitted to reference this repository,
webhook-enforced on every consumer CR. Exactly one of:

- `list: [...]` — explicit namespace names.
- `selector: {...}` — match namespaces by label (a `LabelSelector`).
- `all: true` — allow all namespaces. Must be `true`; `false` is meaningless and
  rejected by the webhook.

The number of namespaces this currently resolves to is surfaced as
`status.allowedNamespaceCount` and the `Namespaces` print column.

### `identityDefaults`

CEL expressions evaluated at admission to derive a consumer's kopia identity when a
`SnapshotPolicy` doesn't override it. Each `*Expr` returns a string and is evaluated
against `namespace`, `policyName`, `labels`, and `annotations` (the consuming
`SnapshotPolicy`'s metadata). The expressions are sandboxed with no I/O and validated
at admission, so a typo or out-of-scope variable is rejected on apply.

- `hostnameExpr` — CEL expression for the kopia identity hostname (e.g. `"namespace"`).
- `usernameExpr` — CEL expression for the kopia identity username
  (e.g. `"namespace + '-' + policyName"`).

### `server`

Optional kopia web-UI server. Because the CR is cluster-scoped, the target
`namespace` is required. Presence enables it. See [Server](../../server.md).

### `maintenance`

Default-managed like the namespaced kind, but since `Maintenance` is itself
namespaced, `maintenance.namespace` selects where the owned `Maintenance` CR lands
(defaulting to the operator's namespace). See [Maintenance](../../maintenance.md).

### `seed`

Same block as on the [Repository](repository.md#seed), with the cluster-scoped
resolution rules:

- The seeding bootstrap Job runs in **the namespace the repository's own
  credentials resolve in** — the operator's namespace, unless
  `encryption.passwordSecretRef.namespace` pins another. A blob seed's
  `from.backend` credential Secret is loaded with `envFrom` (namespace-local), so
  it must live in **that** namespace; a seed `secretRef` that pins a namespace is
  rejected at admission, because a cluster-scoped spec cannot name the right one.
- A migrate seed's `from.repository` with no `namespace` resolves in the
  operator's namespace — the same rule every other cluster-scoped reference
  follows. Set it explicitly whenever the source lives anywhere else.
- An armed seed makes the CR hold its cleanup finalizer, so that a deletion
  reconcile runs at all. That reconcile then deletes the in-flight
  `<name>-discovery` Job **best-effort** — it does not block or retry, and a
  deletion is never wedged on the cleanup: a Job that already finished is an
  ordinary miss, a failed delete is logged as a warning, and if the Job's
  namespace cannot be resolved at all the operator warns and tells you to delete
  the Job by hand. This exists because a namespaced Job cannot carry a
  cluster-scoped ownerReference, so nothing else would ever reap a 24 h seeding
  Job whose CR is gone.

### `credentialProjection`

The repository-owner gate for projecting this repository's credential Secret(s) into
a foreign consumer namespace. **Default off** (`credentialProjection.allowed:
false`): a consumer's own `credentialProjection.enabled` is necessary but not
sufficient — the `ClusterRepository` owner must also allow it, and operator RBAC must
permit it (fail-closed). A namespaced `Repository` has no such gate, because
projection there is a same-namespace no-op.

## `status`

Mirrors [Repository](repository.md) status (`phase`, `observedGeneration`,
`resolvedCredentialVersion`, `uniqueId`, `backend`, `storageStats`, `catalog`,
`seed`, `server`, `conditions`) with one addition:

- `allowedNamespaceCount` — number of namespaces currently resolved by
  `spec.allowedNamespaces`; also the `Namespaces` print column.
