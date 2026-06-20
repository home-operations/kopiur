# CRD design rationale

Why the CRD fields are shaped the way they are. This is the *why* that used to live
in the Rust doc comments (and therefore in the generated CRD `description` text). The
descriptions themselves are now one-liners; the field-by-field user guidance lives in
the [CRD reference](../reference/crds/index.md), and the cross-cutting design reasons
live here for contributors.

The code is the source of truth for current behavior — this page records intent. ADR
section refs (e.g. `ADR-0005 §5`) point at [the ADRs](../adr/0003-kopiur-rust-operator.md)
for the full decision record.

## Recurring patterns

### Sub-objects over leaf fields

Every credential / policy / identity / schedule / health surface is a **struct, not a
bare `bool`/`string`/`enum`**, so a future field slots in without an API break
(ADR §4.11). Examples: `encryption` (room for rotation/`previousPasswords`),
`credentialProjection` (key remapping, copy-name template, immutability),
`sourceColocation` (custom hostname label key), `health` (future health knobs),
`create.ecc`, `server.auth.generate` (username/rotation), `populator: {}` (an empty
object whose mere presence selects the mode, leaving room for future populator knobs).

### Externally-tagged enums (discriminated unions)

`backend`, `source`, `target`, `allowedNamespaces`, `inheritSecurityContextFrom`,
`server.auth`, hooks, and the cache/repo volume types are **externally-tagged** enums
(`backend: { s3: {...} }`), never `#[serde(tag = "kind")]`. Internally-tagged enums
break Kubernetes structural-schema generation — kube's rewriter hoists the `oneOf`
branch properties and panics on the differing tag property. External tagging keeps full
type-safety *and* generates a valid CRD: "exactly one of" is unrepresentable as two,
and reconcilers `match` exhaustively so a new variant cannot compile until every handler
accounts for it.

### Not `Eq`, only `PartialEq`

Several types are `PartialEq` but not `Eq` because they transitively embed `k8s-openapi`
types that are `PartialEq`-only: `LabelSelector` (via `pvcSelector`, `policySelector`,
`workloadSelector`, `AllowedNamespaces::Selector`), `JobSpec` (via `hooks`,
`RunJobHook`), `ResourceRequirements`, `SecurityContext`, `PodSecurityContext`. Reuse
these types; don't re-invent them, and don't add `Eq`.

### Materialized defaults vs `skip_serializing_if`

Fields like `copyMethod` (`Direct`), `mode` (`ReadWrite`), `onNamespaceDelete`
(`Orphan`), `concurrencyPolicy` (`Forbid`), `runOnCreate` (`false`), and
`fromPolicy.offset` (`0`) carry a **real OpenAPI `default:`** in the schema rather than
being `Option` + `skip_serializing_if`. A named `default_*` fn backs **both**
`#[serde(default = …)]` and `#[schemars(default = …)]` — that pairing is what makes
schemars 1 emit a real `default:` in the generated CRD. The value then materializes into
the stored object and `kubectl explain`, and GitOps engines stop diff-thrashing on a
controller-set value (a bare value, not `Option`, so `skip_serializing_if` can never
drop it).

### CEL cost budgets (`maxItems` / `maxLength`)

`SnapshotPolicy.spec.sources` carries `#[schemars(length(max = 100))]` and
`source.sourcePathOverride` carries `length(max = 4096)` **so the apiserver can bound
the cost of the per-item exactly-one-of `x-kubernetes-validations` rule** on `Source`.
CEL rule cost is `rule_cost × maxItems`; without a bound the apiserver assumes a huge
array/string and rejects the CRD as over budget. 100 sources / 4096-byte paths are far
past any real use. The exactly-one-of rule is written as an integer **sum of
`has()`-ternaries** rather than `filter().size()` precisely to stay inside that budget.

### The hooks `jobSpec` is `x-kubernetes-preserve-unknown-fields`

schemars inlines the *entire* structural schema of an embedded `k8s-openapi` type.
The hooks `JobSpec` (`RunJobHook.jobSpec`) drags in the full `PodSpec`: inlined, it
made a single `SnapshotPolicy` CRD ~1.2 MB — ~85% of the whole bundle — which bloats
Helm releases and breaks large-CRD apply paths, most sharply client-side
`kubectl apply`, whose `last-applied-configuration` annotation has a hard 256 KB
limit (a 1.2 MB CRD can't be applied at all).

So `RunJobHook.jobSpec` carries
`#[schemars(schema_with = "crate::schema::preserve_unknown_object")]`, which renders
it as `{ type: object, x-kubernetes-preserve-unknown-fields: true }` instead of the
inlined schema. That alone takes the bundle from ~1.9 MB to ~665 KB (gzipped
259 KB → ~92 KB) and `snapshotpolicies` from ~1.26 MB to ~76 KB.

The trade-off is contained: the **Rust field stays a concrete typed `JobSpec`**, so
kube still deserializes into it (scalar type errors are still rejected at admission),
and the apiserver structurally validates the *actual hook Job* when the controller
creates it. Only **apply-time** structural validation of the `jobSpec` is deferred —
a malformed hook spec fails at Job creation rather than at `SnapshotPolicy` apply.

This is scoped to the `jobSpec` on purpose. The other embedded `core/v1` objects —
mover/server `securityContext`, `podSecurityContext`, `resources`, `affinity` — are
**left inlined** so the apiserver keeps validating them at apply time: each adds only
tens of KB (the bundle is fine at ~92 KB gzipped), and for a backup operator the
early, structural validation of security/resource settings is worth more than the
saving. A regression test in `crates/xtask/tests/crds.rs`
(`hooks_job_spec_renders_as_preserve_unknown_not_inlined`) guards both the
preserve-unknown rendering of `jobSpec` and a size ceiling on `snapshotpolicies`.

### Immutability transition rules

`Repository`/`ClusterRepository` `create.{splitter,hash,encryption,ecc}` carry CRD
`x-kubernetes-validations` transition rules (apiserver + CI, §7/§15) complementing the
webhook. Each leaf is **`has()`-guarded on both `self` and `oldSelf`**: a CEL field
access on an absent optional key raises "no such key", which fails the *whole* rule →
422 on *every* update, which would block the controller's own finalizer/status writes.
The common `create: {enabled: true}` case (no algorithm fields) must reconcile, not
wedge. The `encryption` password-Secret reference is deliberately **not** locked: kopia
fixes only the resolved password *value* into the repo format, never the Secret
name/key, so a rename with identical content must pass (locking it broke GitOps). See
`validate::diff_immutable_repo_fields`.

---

## Per-CRD notes

### Repository

- **`encryption`** — password Secret ref is not locked by the immutability rules (see
  above); only the resolved *value* is fixed in the kopia format.
- **`moverDefaults`** — inherited by *every* mover (bootstrap/backup/restore/maintenance),
  overridable per-recipe via `mover`, merged field-wise (ADR-0004 §1/§2). Absorbed the
  former `cacheDefaults` (now `moverDefaults.cache`).
- **`onNamespaceDelete`** — **breaking** default change (ADR-0005 §5): default `Orphan`
  means `kubectl delete ns` no longer destroys snapshots. Materialized `default: Orphan`.
- **`mode`** — ADR-0005 §11; materialized `default: ReadWrite`.
- **`health.indexBlobWarnThreshold`** — absent ⇒ `DEFAULT_INDEX_BLOB_WARN_THRESHOLD`
  (1000); `0` is the **disable sentinel** (not fall-back-to-default); negative rejected
  by the webhook. The default/disable semantics live in the pure
  `resolve_index_blob_warn_threshold` fn (shared by webhook/controller/tests).
- **`status.resolvedCredentialVersion`** — the password Secret's `resourceVersion`; the
  terminal-failure hard-stop reopens when it changes, so editing Secret *content* (which
  doesn't bump `metadata.generation`) re-triggers a connect instead of parking the repo
  `Failed` forever.
- **`status.storageStats.indexBlobCount`** — observed at bootstrap; an unbounded climb
  means maintenance isn't keeping up (raises `IndexBlobHealth`).

### ClusterRepository

- **Cluster scope** — every Secret/config ref (`backend`/`encryption`/`server`) MUST
  carry an explicit `namespace` (webhook-enforced; the type system can't express it).
- **`allowedNamespaces`** — externally-tagged (`list`/`selector`/`all`); `all` must be
  `true` (`false` rejected). Not `Eq` (embeds `LabelSelector`).
- **`identityDefaults`** — CEL `*Expr` (ADR-0004 §5), evaluated at admission against
  `namespace`/`policyName`/`labels`/`annotations`; sandboxed, no I/O; a typo / out-of-scope
  variable is rejected on apply.
- **`maintenance.namespace`** — `Maintenance` is namespaced, so this selects where the
  owned CR lands (defaults to the operator namespace).
- **`credentialProjection`** — repository-**owner** gate (ADR-0005 §8), breaking,
  default-off (`allowed: false`), fail-closed: consumer opt-in + this gate + operator
  RBAC all required. A sub-object, distinct from the consumer-side
  `credentialProjection.enabled`. A namespaced `Repository` has no such gate
  (same-namespace projection is a no-op).

### SnapshotPolicy

- **`copyMethod` default `Direct`** — `Direct` (read the live PVC) is the default for
  backward-compat and portability: it was the behavior in effect before `copyMethod` was
  wired (the field was inert) and works on any storage with no CSI snapshot stack.
  ADR-0005 §1 originally proposed `Snapshot` as the default, but defaulting to it would
  silently break every existing policy / non-CSI source on upgrade. (The
  backward-compat reasoning is preserved in full in the `default_copy_method` fn doc.)
- **`groupBy`** — must be set explicitly; a silent per-PVC fallback would produce
  inconsistent backups (a data-integrity hazard, ADR §4.9). Multi-PVC fan-out +
  VolumeGroupSnapshot is **not yet wired** (single-PVC staging only today).
- **`hooks.runJob`** — the `RunJobHook` is `Box`ed because it embeds a `JobSpec` (~2 KB);
  `Box<T>` is transparent to serde.
- **`verification`** — `successExpr` is a CEL bool predicate over
  `stats{files,bytes,errors}` / `snapshot` / `restored{files,checksumMatches}`, validated
  at admission (`validate_success_expr`).

### Snapshot

- **`deletionPolicy`** — origin-aware effective default: `Delete` for scheduled/manual,
  forced `Retain` for discovered (a discovered `Snapshot` has an empty spec). ADR §4.5.
- **`pin`** — exempts the snapshot from GFS retention; the reconciler reconciles kopia's
  pin state against `spec.pin` and never spawns a redundant pin op.
- **`status.stats.filesFailed`** — kopia's `rootEntry.summ.errors`: source entries kopia
  **excluded** because it couldn't read them. Present and `> 0` only when an
  `errorHandling.ignore*Errors` policy let the snapshot complete despite unreadable files
  — i.e. the backup is **incomplete**. Backs the `SecurityContextCompatible` condition
  (positive-only, never a heuristic guess).
- **`status.hooks`** — each hook list runs exactly once per Snapshot, across requeues and
  pod restarts (quiesce/resume has side effects).
- **`status.staged`** — the CSI staging objects are recorded once, reused across retries,
  reaped on the terminal transition, never double-created.

### SnapshotSchedule

- **`policyRef` XOR `policySelector`** — exactly one (webhook + CRD validation);
  `policySelector` fans out to many policies in the namespace (mirrors `pvcSelector`).
- **`failedJobsHistoryLimit`** — bounds *failed* child `Snapshot`s only. There is **no**
  `successfulJobsHistoryLimit`: retention is GFS-only (ADR-0003 §4.4).
- **`schedule.{runOnCreate,concurrencyPolicy}`** — materialized OpenAPI defaults (see the
  pattern above); `Forbid` skips a firing rather than letting runs pile up.
- **`status.lastSchedule.at`** — accepts the `scheduledAt` alias on the wire (serde
  `alias`).

### Restore

- **`target` required** — ADR-0005 §9 removed the empty-`target` form; a `Restore` with
  no `target` now fails deserialization. Exactly one of `pvc`/`pvcRef`/`populator`
  (operator-authored CEL `x-kubernetes-validations` + webhook). `source` XOR is
  webhook-enforced.
- **`populator: {}`** — an empty sub-object whose presence selects passive-populator mode
  (claimed via a PVC's `spec.dataSourceRef`); `inheritSecurityContextFrom` is rejected
  with it (no workload pod exists at provision time).
- **`fromPolicy.offset`** — materialized `default: 0` (see the defaults pattern).
- **`status.resolved`** — `resolution` is a closed enum so the decision is one
  exhaustively-matched value, **pinned once** at first resolution and never re-resolved:
  a later-appearing snapshot can never silently retarget an already-provisioned volume
  (ADR §4.6). A legacy pin written before `resolution` existed leaves it `None` with
  `kopiaSnapshotID` set, read as `Snapshot` (stale-id self-heal).
- **source/target semantics** — `repository` is derived from `source` unless
  `source.identity` (which has no CR to derive it from, so `spec.repository` is required);
  `onMissingSnapshot` defaults `Fail` for explicit sources, `Continue` for `fromPolicy`
  (deploy-or-restore). `fsGroup` lives on the pod-level context so a fresh restore volume
  is group-writable for an unprivileged mover.

### Maintenance

- **`ownership`** — at most one `Maintenance` may own a repository at a time (a lease).
  `takeoverPolicy` (`Never`/`PromptCondition`/`Force`) governs what a second `Maintenance`
  does when it finds a foreign owner. The lease identity can be a stale ephemeral
  bootstrap-pod identity; a stuck owner is resolved with `takeoverPolicy: Force` once.
- **`RepositoryMaintenanceSpec`** — default-managed: when absent or `enabled: true` the
  reconciler creates and owns a `Maintenance` CR. An externally-authored `Maintenance`
  is always honored (never duplicated), even with `enabled: false`. `namespace` is
  ClusterRepository-only.
- **`status.full.lastContentReclaimedBytes`** — the only place storage reclamation is
  surfaced.
- **`status.<kind>.lastHandledAt`** — records the most recent cron slot whose Job
  finished, *including* a yield to a foreign lease holder (which deliberately does not
  move `lastRunAt`), so a handled slot never re-fires after its Job self-reaps.

### RepositoryReplication

- **`destination`** — externally-tagged `Backend`, must **differ** from the source
  backend (webhook-enforced).
- **`destinationEncryption`** — `kopia repository sync-to` copies blobs verbatim, so the
  destination must share the source's format; omit this to reuse the source password (a
  true mirror).
- **`mover`** — inherits the source repository's `moverDefaults`.

## Shared types

- **`BackendAuth.workloadIdentity`** — empty `--access-key=` flags engage kopia's ambient
  credential chain; the user-created, cloud-federated ServiceAccount (IRSA / EKS Pod
  Identity / AKS Workload Identity / GKE WI) must pre-exist with the right cloud
  annotation — the operator preflights and binds the mover role to it, but **never
  creates it**. Azure additionally requires `storageAccount`. A `RepositoryReplication`
  whose source and destination are the same cloud kind must not mix static and
  workload-identity auth (the replication pod's env would leak the static keys into the
  ambient chain).
- **`MoverSpec` / `MoverDefaults` merge** — `hardened ⊂ moverDefaults ⊂ recipe.mover`,
  merged field-wise (ADR-0004 §2). A partial override can only **tighten** the hardened
  baseline (it never drops `drop:[ALL]` / seccomp). This closes drift between the
  maintenance/backup/restore movers and the bootstrap-mover gap.
- **`inheritSecurityContextFrom`** — externally-tagged: `workloadSelector` (copy UID/GID
  from a label-selected workload; valid on backup or restore) or `pvcConsumer`
  (auto-derive from the pod mounting the source PVC; backup-source only — rejected on
  `Restore`/`Maintenance`, which have no backup source).
- **`server`** — the kopia web-UI server is read-write-delete and holds the repository
  decryption key; read-only is enforced at the connection level (`spec.server.readOnly`).
  On filesystem backends the repo PVC must be `ReadWriteMany`; `fsGroup` is a no-op on
  NFS. `ServerStatus.namespace` is load-bearing (a cluster-scoped owner has no implicit
  namespace).
- **`FailurePolicy.podStartupDeadlineSeconds`** — fails a mover that can't *start*
  (`CreateContainerConfigError`/`ImagePullBackOff`/`Unschedulable`); a wedged pod never
  reaches a terminal phase and `backoffLimit` never trips, so this is the only backstop.

## See also

- [CRD reference](../reference/crds/index.md) — the per-field user documentation.
- [API conventions](api-conventions.md) — the encoding rules these decisions follow.
- [ADRs](../adr/0003-kopiur-rust-operator.md) — the full decision records.
