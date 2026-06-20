# ClusterRepository

A cluster-scoped, shared kopia repository operated by a platform team and
referenceable from allow-listed namespaces. It has the same storage surface as a
[Repository](repository.md) (backend, encryption, create, moverDefaults, catalog),
plus a tenancy gate (`allowedNamespaces`) and per-namespace identity expressions
(`identityDefaults`). For the terse type/default table see the
[field reference](../../field-reference.md); for how-to guidance see
[Repositories](../../repositories.md).

!!! important "Every Secret reference needs an explicit namespace"
    A `ClusterRepository` is cluster-scoped, so it has no namespace of its own to
    resolve references against. Every Secret/config reference — in `backend`,
    `encryption`, `server`, anywhere — **must carry an explicit `namespace`**. The
    type system cannot express this, so it is webhook-enforced: a reference without
    a namespace is rejected on apply.

## `spec`

Fields shared with `Repository` — `backend`, `create`, `moverDefaults`, `catalog`,
`maintenance`, `onNamespaceDelete`, `mode`, `suspend`, `health` — behave exactly as
on the [Repository](repository.md) page. The differences and additions:

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
`server`, `conditions`) with one addition:

- `allowedNamespaceCount` — number of namespaces currently resolved by
  `spec.allowedNamespaces`; also the `Namespaces` print column.
