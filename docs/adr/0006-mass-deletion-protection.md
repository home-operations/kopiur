# ADR-0006 — Mass-Deletion Protection

- **Status:** Accepted / Implemented
- **Date:** 2026-07-17
- **Deciders:** kopiur maintainers
- **Builds on:** ADR-0005 §5 (namespace-deletion cascade policy), ADR-0003 §4.5 (`Snapshot` lifecycle = CR lifecycle, finalizer + `deletionPolicy`)
- **Scope:** `kopiur.home-operations.com/v1alpha1` — additive fields (`SnapshotSchedule.spec.deletion`, `Snapshot.spec.onScheduleDelete`, `Repository`/`ClusterRepository.spec.deletionProtection`) + a controller-internal execution change (batched deletes)

## Context

A production cluster hit an incident where `kro` (Kubernetes Resource
Orchestrator), which was managing a `SnapshotSchedule` as part of a composed
resource graph, flapped and repeatedly deleted-and-recreated that
`SnapshotSchedule`. Each cycle caused Kopiur to fan out roughly 600–700
concurrent `kopia snapshot delete` Jobs against one repository:
each time the schedule disappeared, Kubernetes' ownerReference GC removed its
produced `Snapshot` CRs, and each CR's own `deletionPolicy: Delete` (the correct,
intentional default for a scheduled backup) ran its own per-CR delete Job exactly
as designed. Every individual deletion was authorized by its own CR — the bug was
architectural, not a validation gap: nothing distinguished "a schedule's children
were deleted because someone tore down the schedule" from "a schedule's children
were deleted because retention pruned them", and nothing bounded how many
concurrent delete Jobs one repository could be asked to absorb at once. The
backend degraded under the connect storm, and recovering required manual
intervention.

ADR-0005 §5 already established the precedent that a backup tool must not make a
bulk-deletion trigger (there, `kubectl delete ns`) a silent data-loss event by
default. This incident showed the precedent was incomplete: `onNamespaceDelete`
only covers the namespace axis. A schedule deletion is a second, independent axis
with the same hazard, and neither axis had any protection against *volume* — a
single authorized action (however it happened) fanning out into hundreds of
Jobs and thousands of deletes with no circuit breaker at all.

## Decision

Three independent, composable mechanisms, none of which weakens another:

### 1. The `pruned-by` discriminator

Every `Snapshot` Kopiur deletes as part of its own lifecycle (GFS retention,
`failedJobsHistoryLimit` pruning) is stamped with
`kopiur.home-operations.com/pruned-by: retention|failed-history` **immediately
before** the delete call. The finalizer treats a stamped deletion as the
operator's own action; anything unstamped (missing, or an unrecognized value) is
EXTERNAL, fail-safe. This is the load-bearing distinction the incident lacked:
without it, a breaker (below) that halts external deletions would *also* halt
Kopiur's own retention the moment a wave trips it, and a cascade guard (below)
couldn't tell "the schedule pruned its own stale failed runs" from "the schedule
was deleted out from under its children."

### 2. The schedule-deletion cascade guard

`SnapshotSchedule.spec.deletion.onScheduleDelete: Retain | Delete` (default
`Retain`) is stamped onto every produced `Snapshot` at creation time
(`spec.onScheduleDelete`) and propagated to already-produced children when
edited. When a produced `Snapshot` is deleted EXTERNALLY (not `pruned-by`) while
its owning `SnapshotSchedule` is gone, replaced (deleted-and-recreated, a
different UID), or terminating, and its effective `deletionPolicy` is `Delete`,
the guard downgrades that deletion to retain: the finalizer releases without
calling `kopia snapshot delete`, a `SnapshotRetainedOnScheduleDelete` Warning
Event fires, and the catalog rediscovers the kopia snapshot as
`origin: discovered` on its next scan. Setting `onScheduleDelete: Delete` opts a
schedule's cascade back in — that deletion still passes through the breaker
below, since it remains an external, destructive deletion.

`ScheduleDeletePolicy` is a dedicated 2-variant enum (`Retain`/`Delete`), not a
reuse of the 3-variant `DeletionPolicy` — see
[design rationale](../dev/design-rationale.md#mass-deletion-protection-cascade-guard--breaker--batching)
for why an `Orphan` cascade variant would add a distinction without a difference.

### 3. The per-repository mass-deletion breaker

`Repository`/`ClusterRepository.spec.deletionProtection.threshold` (default 10,
`0` disables) bounds the volume of pending EXTERNAL, destructive Snapshot
deletions (`deletionTimestamp` set, effective `deletionPolicy: Delete`, not
`pruned-by`) a repository will absorb before requiring a human ack. At or above
threshold, every such pending deletion is HELD: `Snapshot.status.conditions`
gets `DeletionHeld=True` (reason `MassDeletionBreaker`) with a message carrying
the exact release command and the RFC3339 value to copy; the repository gets a
`MassDeletionHeld` condition; a `SnapshotDeletionHeld` Warning Event fires on
each Snapshot's transition into held. Release is a **valued** timestamp
annotation —
`kubectl annotate repository/<name> kopiur.home-operations.com/allow-mass-deletion="<value>" --overwrite`
— not a presence-only flag: a deletion releases iff its own `deletionTimestamp`
is at or before the acknowledged value, so acknowledging today's wave does not
silently disarm the breaker for a wave next month. The controller clamps a
future-dated value to now and ignores an unparseable one (fail-safe, with an
`InvalidMassDeletionAck` Warning). The breaker is deliberately axis-independent:
it also holds a namespace-teardown cascade (`onNamespaceDelete: Delete`) that
crosses the threshold, and a per-CR `skip-snapshot-cleanup` escape hatch
overrides it, same as it overrides everything else.

Kopiur's own retention/history prunes are never held — see the `pruned-by`
discriminator above.

### 4. Batched execution

Deletions for one repository are aggregated (a ~10-second quiet window, up to
200 members) into ONE mover Job — `snapdel-*`, op label
`snapshot-delete-batch` — that connects once and deletes every member's kopia
manifest, replacing the earlier one-Job-per-`Snapshot` design that let a single
cascade turn into hundreds of concurrent connects (the incident above). A lone
deletion is a batch of one and still waits out the quiet window (documented
~10s added latency). The batch Job is CREATEd (never SSA-applied — the
deterministic member-set name is itself a single-flight lock) and carries no
`ttlSecondsAfterFinished`; the dispatcher reaps a terminal batch Job explicitly,
once every member has actually drained its own finalizer. An optional cluster-
wide cap, `KOPIUR_MAX_CONCURRENT_DELETE_JOBS` (Helm
`controller.maxConcurrentDeleteJobs`), defaults to **uncapped** (`0`) — batching
itself is the primary defense, and a global cap is an opt-in backstop, not
Kopiur's answer to head-of-line blocking one repository's slow deletes behind
every other repository's.

Full rationale for each of the five design choices above:
[design rationale → Mass-deletion protection](../dev/design-rationale.md#mass-deletion-protection-cascade-guard--breaker--batching).

## Consequences

**Positive.** The exact incident this ADR is named for can no longer repeat: a
flapping schedule no longer cascades into deleting kopia data by default
(mechanism 2), a repository can no longer be asked to absorb an unbounded wave of
deletes without a human in the loop (mechanism 3), and even an *acknowledged* or
*retention-driven* wave executes as a small number of batched connects rather
than hundreds of concurrent ones (mechanism 4). Kopiur's own GFS retention and
failed-history pruning are provably unaffected by any of this (mechanism 1),
so hardening against an external incident never degrades routine space
reclamation.

**Costs.** Two new annotations to know
(`pruned-by` — read-only/informational; `allow-mass-deletion` — the operator
lever), one new sub-object per repository kind, one new sub-object on
`SnapshotSchedule`, and up to ~10 seconds of added latency on every deletion
(including a single manual `kubectl delete snapshot`) from the quiet window.

**Residual/accepted.** A wave that stays *just under* threshold for its entire
accumulation window — e.g. 9 pending deletions that each individually complete
before a 10th ever arrives — never trips the breaker, by design: the breaker
counts a point-in-time pending total, not a rate. The quiet window (mechanism 4)
narrows this in practice, since concurrent arrivals within ~10 seconds pool into
the same pending count rather than draining independently, but a sufficiently
slow trickle can still stay under threshold indefinitely. This is accepted as
the correct trade-off for a mechanism whose entire purpose is to never block
legitimate low-volume churn — see `threshold: 0` (below) for repositories that
need a different answer entirely.

## Upgrade notes

- **Pre-upgrade fleets are auto-protected.** `onScheduleDelete` absent resolves
  to `Retain` and `deletionProtection.threshold` absent resolves to 10 — every
  existing `SnapshotSchedule`/`Repository`/`ClusterRepository` gets the fail-safe
  behavior with no manifest change required.
- **The breaker is ON by default at threshold 10.** A repository with
  legitimately higher external-deletion churn (many tenants each pruning by
  hand, a bulk migration) should raise `deletionProtection.threshold`, or set it
  to `0`, before that churn happens — not after it's already HELD.
- **An in-flight prune wave from an older operator version, upgraded mid-wave,
  auto-reclassifies or needs one ack.** A wave the *new* operator did not
  itself stamp `pruned-by` (because it started under a version that predates
  this feature) is read as external and counted toward the breaker like any
  other external deletion; a genuinely operator-driven wave still in flight
  typically re-stamps and self-resolves on its very next prune pass, but a
  wave that trips the breaker before that happens needs one `allow-mass-
  deletion` ack to drain.
- **Per-CR delete Jobs are replaced by `snapdel-*` batch Jobs.** Any dashboard
  or alert matching on the old `{snapshot-name}-delete` Job naming must be
  updated to match `snapdel-*` / the `kopiur.home-operations.com/op=snapshot-delete-batch`
  label instead.
- **New metrics, conditions, and events** — see
  [observability → deletion metrics](../dev/observability.md) and
  [Repositories → deletionProtection](../repositories.md#deletionprotection--the-mass-deletion-circuit-breaker)
  for the full list.

## Alternatives considered

- **Rate-limiting deletions instead of a pending-count breaker.** Rejected as
  the primary mechanism — a rate limit adds latency to every deletion (not just
  a bulk one) and still requires deciding a threshold; a pending-count breaker
  only engages when a wave is actually forming, and batching (mechanism 4)
  already bounds the connect rate.
- **A cluster-wide deletion cap instead of per-repository.** Rejected — a
  cluster-wide limit couples every repository's blast radius together; a slow
  or failing repository would throttle deletions for every OTHER repository
  sharing the same global counter. Kept per-repository, with an opt-in global
  backstop (`KOPIUR_MAX_CONCURRENT_DELETE_JOBS`) for operators who want one.
- **Reusing `DeletionPolicy` for the schedule-cascade knob.** Rejected — see
  the `ScheduleDeletePolicy` design-rationale entry: an `Orphan` cascade variant
  would be representable but never actually differ in behavior from `Retain`.
- **A presence-only ack annotation (mirroring `allow-identity-change`).**
  Rejected — that annotation is consumed once at a single admission instant;
  the breaker's ack is read continuously by a running controller, so a
  presence-only flag would permanently disarm the breaker the first time it was
  applied. A valued timestamp scopes the approval to the wave pending when it
  was written.
- **Auto-releasing a held wave after some backoff.** Rejected — the entire
  point of the breaker is that a wave this size requires a human to look at it;
  auto-release on a timer would make the breaker a delay, not a gate.
