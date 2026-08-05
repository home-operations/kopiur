# ADR-0007 — Repository Circuit Breaker

- **Status:** Accepted / Implemented
- **Date:** 2026-08-04
- **Deciders:** kopiur maintainers
- **Builds on:** ADR-0005 §4 (the opt-in backend health probe), ADR-0006 (the
  mass-deletion breaker precedent: a single fault must not fan out into
  unbounded per-CR work), ADR-0003 §4.5 (repository as a first-class resource
  every consumer gates on)
- **Scope:** `kopiur.home-operations.com/v1alpha1` — additive fields
  (`Repository`/`ClusterRepository.spec.health.probe.onFailure`) + two schema
  **default flips** on existing fields (`probe.enabled` now defaults `true`,
  `onFailure` defaults `Degrade`) + controller behavior changes (phase-aware
  probe failure, strict-retry recovery loop, a `Restore` readiness gate,
  missed-slot re-pinning)

## Context

Issue #345: a homelab's Garage (S3) backend went down for several hours. Every
`Repository` pointing at it had bootstrapped long ago and sat `Ready`; the
health probe existed (ADR-0005 §4) but was **opt-in and alert-only**, and this
fleet — like almost every real fleet — had never enabled it. So the readiness
gate (`repository_ready`) stayed open the whole outage: every schedule tick
minted a `Snapshot`, every `Snapshot` launched a mover Job, every Job burned its
`backoffLimit` against a dead endpoint and died. The user came back to **53
`Failed` Snapshot CRs and 23 dead mover Jobs** against one repository — the
exact "storm of pods that each only fail on `kopia repository connect`" the
fail-fast gate was designed to prevent, defeated because nothing ever flipped
the phase the gate reads.

The pre-existing design had documented this as a "one-Job detection window":
a failed backup stamps a `reverify-requested-at` nudge, the repository
re-probes, and a *terminal* verdict flips it `Failed`. Two gaps made the
incident worse than one Job:

1. **A retryable outage never flipped the phase.** The reverify classified the
   connect failure as transient and left the repository `Ready`, so the gate
   never closed — one doomed Job per schedule tick, not one per outage.
2. **The nudge itself was asymmetric and the schedule's slot pin could wedge**
   (fixed as this feature's M1 groundwork; see
   `crates/controller/src/snapshot_schedule.rs::SlotDisposition` for the
   missed-deadline re-pin).

ADR-0006 already establishes the shape of the answer: when a fault would fan
out into unbounded per-CR work, put a breaker between the trigger and the
fan-out. Here the fault is "the backend is down", the fan-out is "one Failed CR
plus one dead Job per schedule tick per consumer kind".

## Decision

### 1. The probe is ON by default, and its default action is to pause

`spec.health.probe.enabled` now schema-defaults to `true`
(`crates/api/src/repository.rs::default_health_probe_enabled`): every
`Repository`/`ClusterRepository` re-connects its backend on the probe cadence
(default `interval: 30m`, min `30s`; `failureThreshold` default `3`) unless
explicitly opted out with `enabled: false`. A new field,
`probe.onFailure: Degrade | Alert` (default `Degrade`), decides what sustained
failure *does*:

- **`Degrade`** (default) — past `failureThreshold` consecutive failed
  connects the repository moves to phase **`Degraded`** with
  `BackendReachable=False` and `Ready=False`: the circuit breaker opens.
- **`Alert`** — the pre-#345 contract, now the opt-out: the repository stays
  `Ready`; only the `BackendReachable` condition, a Warning Event, and
  `kopiur_repository_health_probe_failures` fire.

The cost of default-on is one short connect Job per repository per 30 minutes
(object-store/server/volume-backed backends; bare-path filesystem repos connect
in-process). To avoid a thundering first probe after upgrade, a successful
bootstrap/strict connect **seeds the probe clock**
(`crates/controller/src/health.rs::success_fold`): a repository that has never
probed gets its `lastProbeAt` stamped by the connect that just succeeded, so the
first real probe lands one full interval later, not immediately.

### 2. One sensor: every bootstrap-connect verdict

The breaker has exactly **one** sensor — the outcome of a bootstrap-shaped
`kopia repository connect`, whether launched as a periodic probe or as a strict
(re-)bootstrap. Consumers (a failed backup, replication, maintenance) only
**nudge** the repository to re-probe sooner (the existing
`reverify-requested-at` mechanism, now symmetric); their own failures never
increment the streak directly. This is deliberate: a mover can fail for reasons
that say nothing about the backend — a broken *source* PVC, a full scratch
volume, an app-level hook error — and a breaker fed by consumer failures would
pause a healthy repository over a sick workload. Only a failed *connect*,
observed by the repository's own probe/bootstrap machinery
(`crates/controller/src/health.rs::reconcile_probe_failure`), counts.
`status.health.consecutiveProbeFailures` accumulates across probe **and**
strict-retry failures, so the streak is also the outage-duration signal.

### 3. Open, half-open, close

- **Open** (`crates/controller/src/health.rs::breaker_verdict` /
  `probe_failure_phase`): at `failureThreshold` under `Degrade`, phase →
  `Degraded`, `BackendReachable=False`, `Ready=False`, a
  `kopiur_repository_breaker_trips_total` increment and a Warning Event — on
  the **transition only**. Every consumer gate closes: `Snapshot`,
  `SnapshotPolicy`, `Maintenance`, `RepositoryReplication` already gated on
  `Ready`; **`Restore` now does too**
  (`crates/controller/src/restore/mod.rs`), closing the one write path that
  previously launched movers against a dead backend. Gated work parks
  (`Pending`, `Ready` reason `RepositoryNotReady`) — deferred, never refused —
  and repository→pending-consumer watch mappers
  (`crates/controller/src/watch.rs`) re-reconcile parked CRs promptly on
  recovery instead of waiting out a requeue.
- **Phase stability while open.** A `Degraded` repository holds `Degraded`
  through its retry cycles (`crates/controller/src/health.rs::launch_phase`):
  no `Degraded`→`Initializing` flapping, so `for:`-clause alerting and the
  parked-work gates see one stable open state.
- **Half-open** — the strict retry loop: the repository recycles its bootstrap
  Job and re-connects on an exponential launch-side holdoff, 120s doubling to a
  600s cap (`health::strict_retry_backoff` / `strict_retry_holdoff`, stamped
  in status so a restart cannot reset the clock). Worst case ~144 connect Jobs
  per day of outage — bounded, and each is a short connect, not a backup.
- **Close** — **any** successful connect (probe or strict) heals: streak
  cleared, `BackendReachable=True`, phase `Ready`
  (`health::success_fold`). Recovery is fully automatic; there is no ack.

### 4. The strict-verdict reroute is narrowed to `RepositoryUnavailable` only

Previously any failed strict bootstrap on a once-bootstrapped repository parked
terminal `Failed` (kstatus `Stalled`). Now exactly the
`RepositoryUnavailable` verdict class — connection refused/timeout, DNS, the
transport-shaped failures an outage produces — reroutes into the `Degraded`
retry loop (`crates/controller/src/repository.rs::recycle_bootstrap_outage`;
verdict classification in `crates/kopia/src/error.rs`). `Locked`,
`SourceError`, auth/password failures keep their terminal `Failed`: those need
a human (a wrong password will not fix itself, and hammering a locked
repository is harmful), and blurring them into "retrying" would hide an
actionable error behind a spinner.

### 5. `Vanished` escalates through the breaker to terminal — never a recreate

A probe that finds the backend reachable but the repository **absent**
(`RepositoryVanished`) opens the breaker like any other failure (pausing is
equally right either way), and the strict re-check then confirms:
`RepositoryNotInitialized` on a repository with a pinned `uniqueId` is terminal
`Failed`. The ADR-0005 invariant is untouched: kopiur **never** auto-recreates
a once-`Ready` repository — `create.enabled` governs the first bootstrap only,
and a wipe always ends in a loud terminal state for a human, not a silent
fresh-empty-repo.

### 6. kstatus: `Degraded` is `Reconciling`, not `Stalled`

`crates/controller/src/io/finalizer.rs::ready_outcome_for_phase` maps
`Degraded` → `Reconciling`. For Flux/kstatus tooling that means `flux wait` /
health checks **wait** on a breaker-open repository rather than failing the
Kustomization: the state is self-healing by construction, which is exactly what
`Reconciling` means. `Failed` remains `Stalled`.

### 7. Missed-slot semantics per `concurrencyPolicy`

While the breaker is open, a `SnapshotSchedule` keeps ticking:

- **`Forbid`** (default): the first slot's `Snapshot` parks `Pending` and
  counts as the active run, so later slots `Wait` — parked work is bounded at
  **one**, and on recovery that pinned stale slot fires exactly once (the
  catch-up), after which the cadence is normal again.
- **`Allow`**: one `Pending` per slot, by that policy's declared overlap
  contract — choosing `Allow` is choosing unbounded overlap.
- `startingDeadlineSeconds` now actually works for a due-but-expired slot: it
  is **skipped and re-pinned forward** (CronJob semantics,
  `snapshot_schedule.rs::SlotDisposition::SkipExpired`) instead of wedging the
  reconciler in a 1-second requeue loop on a past slot — the M1 fix that makes
  "bounded parked work" true even for schedules that set a deadline.

### 8. Observability

New metrics (`crates/controller/src/metrics.rs`):
`kopiur_repository_breaker_trips_total{kind,namespace,name,probe_kind}`
(transition-only counter),
`kopiur_repository_consecutive_backend_failures{kind,namespace,name}`
(store-backed; emitted whenever health status exists, so the post-recovery `0`
is visible), `kopiur_repository_breaker_open_since_timestamp_seconds` (series
exists **only** while open — `time() - metric` is the open duration), and
`kopiur_snapshot_gated{namespace,policy}` (parked-Pending population; drains to
absence on recovery). Helm ships two rules
(`deploy/helm/kopiur/templates/prometheusrule.tpl`):
`KopiurRepositoryBreakerOpen` (warning, 15m) and `KopiurSnapshotsGated` (info,
30m), plus dashboard panels (`deploy/dashboards/kopiur.json`).

## Consequences

**Positive.** The #345 incident cannot repeat: an outage now costs at most the
in-flight backup plus the one doomed Job launched inside the detection window —
then the breaker opens, work parks `Pending` (visible, bounded, never lost),
`kubectl get repository` says `Degraded`, alerts name the breaker, and when the
backend returns everything heals and catches up with **zero human actions**.
Detection is proactive (default-on probe) instead of next-backup-reactive, so
an outage between backups is seen within one probe interval.

**Costs.**

- One connect Job per repository per probe interval (default 30m), for every
  object-store/server/volume-backed repository in the fleet, forever — the
  price of watching. Tune `interval` up for metered backends, or
  `enabled: false` to opt a repository out entirely (which also disables the
  breaker: the probe is its only sensor).
- Repositories can now leave `Ready` **without any spec change** — a behavior
  change GitOps/status tooling must expect (mitigated by the
  `Degraded`→`Reconciling` kstatus mapping).
- A backup window can be missed deliberately: while open, nothing even tries.
  Better one missed window than a stream of dead Jobs, but users who truly
  prefer try-anyway set `onFailure: Alert`.

**Residual/accepted.**

- **The detection window is small, not zero.** A backup launched between the
  outage's start and the threshold-crossing probe still fails — one doomed Job
  per outage per repository, same as before; the breaker removes the
  *per-tick* pile-up, not the first casualty.
- **Alert-mode + `spec.preflight` composes sharply.** A `SnapshotPolicy`
  preflight using `repository.backendReachable` will hold backups `Pending`
  and then **`Failed`** once `preflight.timeout` elapses — so an Alert-mode
  fleet with that preflight still accumulates bounded `Failed` CRs during an
  outage. Accepted: both knobs are explicit user configuration, and the
  preflight timeout is the user's own bound.
- **`Allow` schedules park one `Pending` per slot** during an outage, by
  contract (see §7).

## Upgrade notes

- **The CRD default materializes into stored objects.** `probe.enabled` and
  `onFailure` are schema defaults: the apiserver stamps `enabled: true` (and
  `onFailure: Degrade` where a probe block exists) into stored objects on
  their next write. A GitOps repo that had written `spec.health.probe:
  { interval: … }` without `enabled` will see a server-side diff
  (`enabled: true` appearing) — expected, one-time, and semantically what was
  already happening.
- **Behavior change: repositories can leave `Ready` without a spec change.**
  Anything that treated `phase == Ready` as a permanent post-bootstrap state
  (custom dashboards, scripts) must now handle `Degraded`.
- **First probe shortly after upgrade for never-probed repositories** — the
  probe-clock seeding (§1) only helps repositories whose upgrade-era reconcile
  performs a successful connect/finalize first; a long-idle repository probes
  within its first interval after the new operator starts. Fleet-wide that is
  one wave of short connect Jobs.
- **Restoring the old behavior** is two lines per repository:
  `health: { probe: { onFailure: Alert } }` (alert-only, never pauses) or
  `health: { probe: { enabled: false } }` (no probe, no breaker — back to
  next-backup-reactive detection).
- **New metrics/alerts** — see §8; the Helm PrometheusRule and dashboard ship
  updated in the same release.

## Alternatives considered

- **A second counter/probe dedicated to the breaker** (keep the alert probe
  as-was, add a breaker-specific health check). Rejected — two sensors over
  the same backend drift, double the Job cost, and invite the question of
  which one the gate believes. One sensor, one streak, one verdict
  (`health.rs` keeps the whole state machine in one place, exhaustively
  matched).
- **Feeding the breaker from consumer (mover) failures.** Rejected — a broken
  source PVC or hook would trip a repository-wide pause over a single sick
  workload. Consumers nudge the sensor; only connect verdicts count (§2).
- **A manual ack to close, à la ADR-0006's mass-deletion breaker.** Rejected —
  that breaker guards an *irreversible destructive* fan-out, so a human in the
  loop is the point. This breaker guards a *pause*: non-destructive,
  self-healing, and an ack requirement would turn every transient NAS reboot
  into a paging event with a manual step.
- **Gating at the schedule tick instead of the repository** (don't mint
  `Snapshot` CRs while the repository is down). Rejected — a
  `SnapshotSchedule`'s `policySelector` can span policies on *different*
  repositories, so the tick has no single repository to gate on; and a parked
  `Pending` CR is honest, visible state (`kopiur_snapshot_gated`) that
  launches itself on recovery, whereas an un-minted slot is invisible. The
  `Forbid` bound already keeps parked volume at one.
- **Terminal `Failed` on outage (the old strict-verdict behavior) with better
  docs.** Rejected — `Failed`/`Stalled` needs a spec/Secret change to retry,
  which turns every outage into a manual recovery; and it is a lie: nothing is
  wrong with the *spec*. `Degraded`/`Reconciling` with automatic retry states
  the truth.
