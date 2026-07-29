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

Fields like `copyMethod` (`Snapshot`), `mode` (`ReadWrite`), `onNamespaceDelete`
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

### Mass-deletion protection (cascade guard + breaker + batching)

Three composable mechanisms guard against a bulk-deletion incident (ADR-0006) —
spanning `Snapshot`, `SnapshotSchedule`, `Repository`/`ClusterRepository` — while
keeping Kopiur's own retention pruning unaffected:

- **`ScheduleDeletePolicy` is deliberately 2-variant** (`Retain`/`Delete`), not a reuse
  of the 3-variant `DeletionPolicy` (`Delete`/`Retain`/`Orphan`). An `Orphan` in cascade
  position would differ from `Retain` only in per-CR event/metric bookkeeping — a
  per-CR "orphaned" event/metric fired for every produced Snapshot in the cascade, vs.
  one quiet retain — a distinction with no operational value. Reusing `DeletionPolicy`
  would make that non-difference representable and force every match arm to decide
  what an `Orphan` cascade even means; the narrower enum makes it unrepresentable
  instead.
- **`pruned-by` distinguishes operator lifecycle from external deletion**, not
  `DeletionPolicy` or any other spec field, because the SAME `deletionPolicy: Delete`
  Snapshot must be handled differently depending on WHO is deleting it: Kopiur's own
  GFS retention and `failedJobsHistoryLimit` pruning must keep working — unthrottled —
  during an incident the breaker is actively holding, while an external actor deleting
  the same shaped CR is exactly what the breaker exists to gate. The annotation is
  stamped immediately before the operator's own delete call rather than inferred from
  context, so the finalizer never has to guess; any missing or unrecognized value
  defaults to EXTERNAL (fail-safe — a bug that fails to stamp it makes a Kopiur prune
  look external, never the reverse).
- **The mass-deletion ack is a VALUED timestamp, not a presence-only flag** (unlike
  `allow-identity-change`, consumed once at a single admission instant). The breaker's
  annotation is read on every reconcile of a live repository, so a presence-only ack —
  set once to release a wave — would silently and *permanently* disarm the breaker the
  moment anyone applied it, including a value left in Git. A timestamp instead answers
  "I approve everything pending as of THIS instant": a later wave has later
  `deletionTimestamp`s the same ack value doesn't cover, so the breaker re-arms for it
  automatically without anyone removing the old annotation. This is also why a held
  wave never auto-releases on its own — not when time passes, not when the pending
  count later drops back below threshold by itself: nothing substitutes for an
  explicit human ack: it is the only thing that ever clears a hold.
- **Deletions execute as a per-repository BATCH, CREATEd (never SSA-applied), with NO
  `ttlSecondsAfterFinished`.** One mover Job connects once and deletes every member's
  kopia manifest, replacing an earlier one-Job-per-Snapshot design that let a single
  cascade turn into hundreds of concurrent connects against one backend (the
  motivating incident). `CREATE`, not `apply`, makes the deterministic
  member-set-derived Job name double as a single-flight lock: a sibling reconcile's
  `create` for the same member set 409s harmlessly against the Job already launched,
  so two racing reconciles can never enroll a member twice. No TTL means the
  dispatcher reaps a terminal batch Job EXPLICITLY, only once every member has
  actually drained (a SUCCEEDED Job is deleted only once no covered member still
  holds its cleanup finalizer) — an unconditional TTL could otherwise reap the Job
  (and its `delete-members` audit trail) before a member's own reconcile observed the
  success and released its finalizer.
- **The delete-Job concurrency cap (`KOPIUR_MAX_CONCURRENT_DELETE_JOBS`) defaults to
  UNCAPPED (`0`), an opt-in backstop, not the primary defense.** Batching itself — one
  Job per repository per accumulation window rather than one per Snapshot — is what
  keeps a bulk deletion from overwhelming a backend; an operator-wide concurrency cap
  layered on top would let one slow or failing repository's batch Jobs
  head-of-line-block every OTHER repository's deletions behind the same global limit —
  exactly the blast-radius coupling a per-repository mechanism should avoid. The cap
  exists for an operator who wants an extra global throttle on top, not as the
  mechanism doing the real work.
- **The pending COUNT is inclusive; the fire SET is exclusive** — two intentionally
  opposite polarities over the same pending `Snapshot`s. The breaker counts a
  maximally-inclusive set (an unpinned or possibly-cascade-guarded CR is
  over-counted, never dropped) because over-counting only trips the breaker earlier —
  the fail-safe direction for a count. The set a reconcile actually FIRES into a batch
  delete Job is the opposite: maximally exclusive, because an over-included member
  there is an irreversible `kopia snapshot delete`, so the fail-safe direction is
  UNDER-fire + requeue. A breaker-exempt trigger (an operator prune, or an acked older
  wave) therefore narrows its fire set past the count — dropping breaker-HELD
  externals, `onNamespaceDelete: Orphan` members in a terminating namespace,
  schedule-owned members while the schedule store is still cold, and unpinned PEERS
  (whose manifest ids must not ride an unrelated repository's batch) — while every one
  of those still counts toward the breaker. An excluded member is never lost: it
  drains via its own reconcile's self-fire once it is genuinely eligible.

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
- **`deletionProtection.threshold`** — the mass-deletion circuit breaker (ADR-0006);
  default 10, `0` disables. See [Mass-deletion protection](#mass-deletion-protection-cascade-guard--breaker--batching) above.
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

- **`copyMethod` default `Snapshot`** — `Snapshot` (point-in-time CSI `VolumeSnapshot`
  staging) is the default because it is crash-consistent: kopia reads a frozen capture
  instead of a live, possibly-mid-write PVC, which matters most for databases and other
  stateful apps (ADR-0005 §1 originally proposed this default). It requires the CSI
  snapshot stack + a `VolumeSnapshotClass` for the source's driver; `Direct` (read the
  live PVC, no CSI required) remains available and is the right choice for non-CSI/static
  sources — set it explicitly. If the CSI stack is missing under the `Snapshot` default,
  the operator fails loud with a pointer to install the stack or set `copyMethod:
  Direct` (see `crates/controller/src/io/staging.rs`). (Earlier releases defaulted to
  `Direct` for backward-compat with the field's pre-wiring behavior; the reasoning for
  the flip is preserved in full in the `default_copy_method` fn doc.)
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
- **`onScheduleDelete`** (`spec`) / **`pruned-by`** (annotation) — the mass-deletion
  protection surface (ADR-0006): the stamped cascade policy the finalizer consults when
  the owning schedule is gone, and the discriminator that tells the finalizer an
  operator prune from an external deletion. See
  [Mass-deletion protection](#mass-deletion-protection-cascade-guard--breaker--batching)
  above.

### SnapshotSchedule

- **`policyRef` XOR `policySelector`** — exactly one (webhook + CRD validation);
  `policySelector` fans out to many policies in the namespace (mirrors `pvcSelector`).
- **`failedJobsHistoryLimit`** — bounds *failed* child `Snapshot`s only. There is **no**
  `successfulJobsHistoryLimit`: retention is GFS-only (ADR-0003 §4.4).
- **`deletion.onScheduleDelete`** — the schedule-cascade guard (ADR-0006), default
  `Retain`. Propagated to existing produced `Snapshot`s on edit (skipping any child
  already `Terminating`). See
  [Mass-deletion protection](#mass-deletion-protection-cascade-guard--breaker--batching)
  above.
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
  backend (webhook-enforced). Its `auth.secretRef` carries the destination backend's
  own access credentials. `kopia repository sync-to` copies blobs verbatim, so the
  mirror always shares the source's format and password — there is no destination
  password knob. Because one mover pod touches both backends, the destination Secret
  must co-reside in the CR's namespace and is delivered under a `KOPIUR_DEST_` env
  prefix, then remapped for the `sync-to` subprocess so source/destination keys of the
  same name (`AWS_*`, …) never collide.
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

## Controller resilience under API-server outage

A production control plane was OOM-killed ~5 times in a row (July 2026); each flap
the controller exhausted its fd table (`EMFILE`) within ~11s of startup, its
`/healthz` listener stopped accepting, the kubelet restarted it, and the restart's
re-list re-ran the storm — the election Lease reached `leaseTransitions=80`. The
amplification chain: watcher re-lists re-drive every primary × referent fan-out
(one ClusterRepository event enqueues every SnapshotSchedule cluster-wide) ×
**unbounded** reconcile concurrency × ~3–5 API reads per reconcile × one spawned
Warning-Event POST per failure × 30s connect timeouts pinning an fd per blackholed
SYN × no `NOFILE` handling.

The fix package (see
[watch-and-reconcile → API-outage resilience](watch-and-reconcile.md#api-outage-resilience)
for the per-defense detail): a **bounded-by-default reconcile concurrency cap**
(`reconcileConcurrency: 8` per controller — unlike `maxConcurrentDeleteJobs`,
where batching is the primary protection and uncapped is safe, reconcile
concurrency had NO other bound), paired with a kopia subprocess timeout so a hung
backend can't starve a capped controller; transport-error Event suppression plus a
16-permit/10s bound on failure publishes; deterministic [30s, 60s) jitter on the
transient requeue; 5s connect / 305s read client timeouts (exec streams exempt);
a deadline on each leader renew attempt; a fail-closed streamingLists probe; and
an `RLIMIT_NOFILE` soft→hard raise.

**Deliberately unchanged:** hooks staying inline on the reconcile slot (detaching
them would stretch the app's quiesce window across requeues — the slot-hold is
bounded by the hook timeout, the kopia timeout, and cap sizing); and the axum
accept loop (axum 0.8 already sleeps 1s and retries on `EMFILE`, matching the
incident logs' cadence).

`exit-on-lost-lease` was also listed here as deliberately unchanged. Issue #319
showed why that was too broad — see below.

## Leader election: the renew budget (#319)

The "deadline on each leader renew attempt" above was necessary but wrong on its
own, and the follow-up is worth recording because the failure was subtle.

`RENEW_DEADLINE` was used as **both** the per-attempt `timeout` **and** the
abdication budget, with the inter-attempt `sleep(RENEW_PERIOD)` charged against
that same budget. A stalled attempt therefore tripped its timeout at
`RENEW_PERIOD + RENEW_DEADLINE`, which is unconditionally past the budget — so
the `delay = RETRY_PERIOD` retry branch was **dead code for every slow failure**.
Fast failures (403, connection refused) retried; slow ones abdicated on the first
occurrence. The failure mode the loop handled worst was the common one.

In production that was ~15 process suicides a day off ordinary API latency, each
costing a full cluster-wide informer re-LIST — which is itself load, making the
next stall likelier.

Two fixes, and **both are required**:

1. **The budget is a window, not an attempt timeout.** A round opens a
   `RENEW_DEADLINE` window, retries every `RETRY_PERIOD` inside it, and bounds
   each attempt by `min(RENEW_ATTEMPT_TIMEOUT, remaining)` so no attempt can
   outspend the budget it draws from nor swallow it whole. The inter-round sleep
   sits outside the window. This is client-go's `wait.Until(renewRound,
   RetryPeriod)` shape. `RENEW_PERIOD` dropped 5s → 2s so worst-case abdication
   (12s) has real margin under `LEASE_DURATION` (15s); a `const` assertion now
   enforces that rather than a test.

2. **Election traffic gets its own client.** This is the load-bearing half, and
   the non-obvious one: `tokio::time::timeout` only drops the request *future* —
   it does **not** evict the wedged HTTP/2 connection from hyper's pool, so
   retries would go straight back onto it. Only a *transport* error evicts a
   connection. Lease traffic now rides a dedicated `kube::Client` whose short
   `read_timeout` (5s, vs the shared client's watch-sized 305s) produces exactly
   that error. `RENEW_ATTEMPT_TIMEOUT` is deliberately larger so the transport
   error wins the race.

Supporting evidence for (2): during the incident the operator logged 90 "the API
server accepted the connection but never answered" abdications in six days, while
the apiserver's own APF wait histogram showed **zero** requests waiting >5s in any
priority level over the same window. The requests never reached it. The log
message has been reworded accordingly — it was blaming the wrong component.

Separately, Kopiur's ServiceAccount lives outside `kube-system`, so the built-in
`system-leader-election` FlowSchema does not match it and its lease renewals fall
into `workload-low` alongside every other ServiceAccount's bulk traffic. The
chart now ships an opt-out `FlowSchema` putting `leases` get/create/update into
the guaranteed `leader-election` priority level.

**Exit-on-lost-lease, revisited.** Losing the Lease is now two cases, not one.
`LeadershipLost::ToPeer` — a foreign holder was *observed* — still exits; there is
nothing to re-take. `LeadershipLost::RenewFailed` means only that contact was
lost, and the process now checks whether it is the sole live controller pod
(self-derived label selector, `pod-template-hash` excluded so a mid-rollout peer
counts). If it is provably alone there is nobody to split-brain with, so it
re-campaigns in place and keeps its informer caches warm. Anything ambiguous —
more than one pod, a failed list, no labels — exits. The chart default is
`replicaCount: 1`, where the old unconditional exit bought no split-brain
protection and cost a full cold start every time.

### Breaking the restart→re-LIST→restart loop

Each abdication cost a full cold start, and a cold start is itself the load that
makes the next stall likelier. kube-rs shares no informers between `Controller`s,
so every `.owns`/`.watches` is its own LIST+WATCH — Kopiur registers ~50 of them
in cluster scope, all firing at once, with no client-side rate limiting anywhere
(kube-rs ships none). Three changes cut what a restart costs:

- **`ListSemantic::Any` on every watcher config** (`controllers::watcher_config`).
  kube-runtime defaults to `MostRecent`, so with streaming off each initial LIST
  was an etcd quorum read rather than a watch-cache read — ~50 of them at once.
  Safe because reconcilers are level-triggered and idempotent and the watch
  stream immediately corrects any staleness; this is what client-go's informers
  do by default.
- **The apiserver version probe now fails OPEN on a transport failure.** It ran
  moments after winning the Lease, so the restart most likely to hit a failing
  probe was one caused by congestion — and failing closed put every watcher on
  the *more* expensive paged path at exactly that moment. A pre-1.32 ANSWER still
  fails closed; that is a real version fact, a timeout is not. The probe is also
  bounded now.
- **The sweep's Snapshot read is paginated** (`sweep::list_all_snapshots`). It was
  one unbounded, unpaginated, cluster-wide full-object LIST running 60s after
  every start. Paginated rather than `.limit()`ed on purpose: completeness is
  load-bearing (`finalizer_holding_snapshot_uids` SPARES batch delete Jobs, so a
  silently truncated read would reap Jobs that are still needed).

Also fixed: the standalone Maintenance informer built its own
`WatcherConfig::default()` and so silently opted out of `streamingLists`. Both it
and the controller fan-out now share `controllers::watcher_config`.

**Not done, deliberately.** Deduplicating the watchers themselves (7× `Repository`,
7× `ClusterRepository`) needs kube's shared streams, which are behind the
`unstable-runtime-subscribe` feature. Opting a backup operator into an explicitly
unstable API is a decision worth taking on its own merits, not as a rider on an
availability fix. `ListSemantic::Any` removes the dominant per-LIST cost in the
meantime.

## See also

- [CRD reference](../reference/crds/index.md) — the per-field user documentation.
- [API conventions](api-conventions.md) — the encoding rules these decisions follow.
- [ADRs](../adr/0003-kopiur-rust-operator.md) — the full decision records.
