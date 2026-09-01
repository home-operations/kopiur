//! Per-repository mover-Job **pool membership**: which flavors of mover Job
//! count toward a repository's `spec.concurrency.maxConcurrentJobs`, and the
//! label that makes the pool countable with one selector.
//!
//! Membership is a policy decision, so it lives in exactly ONE place: the
//! exhaustive `match` in [`counts_toward_repo_pool`]. Every mover-spawning path
//! names its [`MoverJobKind`] there, so "does a pin Job compete with backups?"
//! has a single answer that a test can read off, rather than N label blocks
//! that can silently disagree. A new mover flavor cannot be added to
//! [`MoverJobKind`] without deciding — the `match` has no `_ =>` arm.
//!
//! The label itself is [`kopiur_api::consts::REPO_POOL_LABEL`], stamped through
//! `inputs.labels` so `build_job` mirrors it onto both the `Job` and its pod
//! template.
//!
//! The second half of this module is the **admission gate** the label makes
//! possible: [`observe_live_pool`] counts the pool with ONE selector,
//! [`pool_verdict`] — pure, exhaustive, no IO — decides whether a run may mint
//! its mover Job now or must park, and [`AdmissionLedger`] makes the
//! observe→decide→create sequence atomic so two simultaneous reconciles cannot
//! both read an empty pool. The pieces are deliberately split so every
//! interesting case is a table-driven unit test rather than a cluster fixture.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use k8s_openapi::api::batch::v1::Job;
use kube::Api;
use kube::api::ListParams;

use kopiur_api::common::RepositoryRef;
use kopiur_api::consts::{MANAGED_BY_LABEL, MANAGED_BY_VALUE, REPO_POOL_LABEL};

use crate::context::Context;
use crate::error::Result;
use crate::naming::repo_label;

/// Every flavor of mover `Job` kopiur spawns, as the pool-membership decision
/// sees them. One variant per job-build path.
///
/// Deliberately its own enum rather than a reuse of `OP_*` label constants: the
/// question here is "does this run compete for a repository's concurrency
/// budget", which is not the same partition as the op string the mover
/// dispatches on (a restore's direct and populator Jobs are one kind here; a
/// bootstrap and a catalog re-scan are one kind here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoverJobKind {
    /// `Snapshot` backup mover (`crate::snapshot`).
    Backup,
    /// `Restore` mover — both the direct and the populator Job, which share one
    /// build path (`crate::restore::run_restore_mover`).
    Restore,
    /// `RepositoryReplication` `kopia repository sync-to` mirror.
    RepositoryReplication,
    /// `SnapshotReplication` `kopia snapshot migrate` copy.
    SnapshotReplication,
    /// `Maintenance` quick/full run.
    Maintenance,
    /// `SnapshotPolicy` verification (quick or deep).
    Verification,
    /// `Snapshot` pin/unpin.
    Pin,
    /// Batched `snapdel-*` snapshot-delete Job.
    SnapshotDeleteBatch,
    /// Repository bootstrap / catalog re-scan (`<repo>-discovery`), for both
    /// repository kinds.
    Discovery,
    /// The CLI's read-only `kopiur browse` session pod.
    BrowseSession,
}

/// Does this flavor of mover Job count toward its repository's
/// `spec.concurrency.maxConcurrentJobs`?
///
/// **The whole decision table.** `true` for the flavors that do the bulk data
/// work a user throttles a backend against: backups, restores, and the SOURCE
/// side of either replication.
///
/// `false` for everything else, and each `false` is deliberate:
///
/// - **Maintenance** is the *cure* for an overloaded repository (it compacts
///   indexes and drops unreferenced blobs). Queuing it behind a saturated
///   backup pool would make a struggling repository permanently unmaintainable.
/// - **Verification** and **Pin** are cheap metadata operations; a pin is a
///   single manifest rewrite, and both are operator/housekeeping-driven rather
///   than user-scheduled load.
/// - **SnapshotDeleteBatch** is already single-flighted per repository by its
///   own dispatcher, and it *reduces* repository load; holding a delete behind
///   backups grows the backlog it exists to drain.
/// - **Discovery** (bootstrap / catalog re-scan) is what makes a repository
///   `Ready` in the first place. Gating it on a pool that only fills once the
///   repository IS ready would deadlock a fresh repository.
/// - **BrowseSession** is an interactive `kubectl kopiur browse` pod a human is
///   waiting on; parking it behind a nightly backup queue would read as a hung
///   command.
///
/// Exhaustive: a new [`MoverJobKind`] cannot compile until it is classified.
pub fn counts_toward_repo_pool(kind: MoverJobKind) -> bool {
    match kind {
        MoverJobKind::Backup
        | MoverJobKind::Restore
        | MoverJobKind::RepositoryReplication
        | MoverJobKind::SnapshotReplication => true,
        MoverJobKind::Maintenance
        | MoverJobKind::Verification
        | MoverJobKind::Pin
        | MoverJobKind::SnapshotDeleteBatch
        | MoverJobKind::Discovery
        | MoverJobKind::BrowseSession => false,
    }
}

/// The `(key, value)` [`REPO_POOL_LABEL`] entry for a run of `kind` against
/// `repo_ref` — or `None` when the flavor is excluded from the pool, so
/// `labels.extend(repo_pool_label(..))` leaves an excluded Job's metadata
/// byte-identical to what it carried before pooling existed.
///
/// **`repo_ref` must be NORMALIZED** — the ref a
/// `crate::io::ResolvedRepository::repository_ref` (equivalently
/// `kopiur_api::common::normalized_repository_ref`) produced, never a raw spec
/// ref. [`repo_label`] hashes `repository:{ns}/{name}`, so a `Repository` ref
/// carrying `namespace: None` hashes `repository:/{name}` and lands the run in
/// a pool of its own — splitting one repository's cap in two, silently. Every
/// caller therefore passes the ref it actually RESOLVED against.
pub fn repo_pool_label(kind: MoverJobKind, repo_ref: &RepositoryRef) -> Option<(String, String)> {
    counts_toward_repo_pool(kind).then(|| (REPO_POOL_LABEL.to_string(), repo_label(repo_ref)))
}

// --- the admission gate ------------------------------------------------------

/// What KIND of pooled run is asking to launch, from the gate's point of view.
///
/// Narrower than [`MoverJobKind`] on purpose: membership (which flavors OCCUPY a
/// slot) and admission (which flavors may be HELD from taking one) are different
/// questions with different answers, and collapsing them cost us the restore
/// guarantee once already. Every variant here is in the pool by
/// [`counts_toward_repo_pool`] — a flavor that is out of the pool never reaches
/// this gate at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PoolClass {
    /// A `Snapshot` backup run. Parkable.
    Backup,
    /// A `RepositoryReplication`/`SnapshotReplication` run reading the source
    /// repository. Parkable.
    Replication,
    /// A `Restore` run. **Never parked** — see [`pool_verdict`].
    Restore,
}

/// The two caps a pooled run is measured against: the repository's own
/// `spec.concurrency.maxConcurrentJobs` and the cluster-operator's
/// `KOPIUR_MAX_CONCURRENT_JOBS` backstop.
///
/// Both are `Option<NonZeroUsize>` because "uncapped" and "cap of zero" must not
/// be confusable at a call site: a `Some(0)` here would mean "admit nothing",
/// i.e. a repository that never runs another Job. `None` on BOTH is the default
/// install, and [`pool_live_counts`] short-circuits it without a LIST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolCaps {
    /// The per-repository cap (`spec.concurrency.maxConcurrentJobs`), the
    /// primary knob.
    pub repo: Option<NonZeroUsize>,
    /// The cluster-wide backstop (`KOPIUR_MAX_CONCURRENT_JOBS`), counted across
    /// EVERY repository.
    pub global: Option<NonZeroUsize>,
}

impl PoolCaps {
    /// Whether any cap is set at all. `false` ⇒ nothing to count, so the gate
    /// costs exactly one branch and no API call.
    pub fn is_uncapped(self) -> bool {
        self.repo.is_none() && self.global.is_none()
    }
}

/// The gate's answer for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolVerdict {
    /// Mint the mover Job now.
    Admit,
    /// Hold the run: no Job, no staging, no side effects. Carries the counts
    /// that produced the decision so the park message can name them.
    Park {
        /// Live pooled Jobs against THIS repository.
        repo_live: usize,
        /// Live pooled Jobs across every repository.
        global_live: usize,
    },
}

/// Decide whether a pooled run may launch. **Pure** — the counting is
/// [`pool_live_counts`]'s job — so the whole truth table is a unit test.
///
/// [`PoolClass::Restore`] is `Admit` unconditionally, at or over either cap. A
/// restore is a recovery in progress; queueing one behind routine backups is
/// exactly backwards. It still COUNTS (its Job carries the pool label), so a
/// running restore displaces backups rather than adding to them — which is the
/// behavior a cap is actually asked for.
///
/// For the parkable classes a cap that is SET and already met holds the run.
/// The two caps are independent and either alone is sufficient to park: the
/// per-repository cap protects one backend, the global backstop protects the
/// cluster, and a run must satisfy both.
///
/// Exhaustive over [`PoolClass`]: a new pooled work kind cannot compile until
/// its parkability is decided here.
pub fn pool_verdict(
    class: PoolClass,
    repo_live: usize,
    global_live: usize,
    caps: PoolCaps,
) -> PoolVerdict {
    match class {
        PoolClass::Restore => PoolVerdict::Admit,
        PoolClass::Backup | PoolClass::Replication => {
            let at_repo_cap = caps.repo.is_some_and(|c| repo_live >= c.get());
            let at_global_cap = caps.global.is_some_and(|c| global_live >= c.get());
            if at_repo_cap || at_global_cap {
                PoolVerdict::Park {
                    repo_live,
                    global_live,
                }
            } else {
                PoolVerdict::Admit
            }
        }
    }
}

/// Whether a pooled `Job` currently OCCUPIES a slot.
///
/// Two exclusions, both load-bearing:
///
/// - **Terminal** ([`crate::snapshot::job_terminal_state`] `is_some`): the work
///   is done, the Job is only waiting out its TTL. Counting it would make a cap
///   of 1 admit one run per TTL window instead of one at a time.
/// - **Suspended**: a Job with `spec.suspend: true` (or a `Suspended=True`
///   condition) has NO pod and is doing no work. Kueue and friends admit Jobs by
///   flipping exactly that field, so counting suspended Jobs would let a queueing
///   system's backlog fill kopiur's pool and deadlock both — kopiur would park
///   new runs behind Jobs that Kueue is holding, forever.
pub fn job_occupies_slot(job: &Job) -> bool {
    if crate::snapshot::job_terminal_state(job).is_some() {
        return false;
    }
    !job_is_suspended(job)
}

/// `spec.suspend: true`, or a `Suspended=True` status condition (what a
/// suspending controller writes once it has torn the pods down).
fn job_is_suspended(job: &Job) -> bool {
    if job.spec.as_ref().and_then(|s| s.suspend) == Some(true) {
        return true;
    }
    job.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| {
            cs.iter()
                .any(|c| c.type_ == "Suspended" && c.status == "True")
        })
}

/// Observe the live pooled mover Jobs behind `repo_pool_value` (a
/// [`repo_label`] value) — the counts plus the identity of every pooled Job the
/// LIST returned (see [`ObservedPool`]).
///
/// **Costs nothing when uncapped.** Both caps `None` returns an empty
/// [`ObservedPool`] without touching the apiserver — the default install must
/// not pay a LIST per reconcile for a feature it does not use. This mirrors the
/// batch-delete throttle's `throttle_live_count`.
///
/// ONE LIST serves both numbers: an exists-selector on [`REPO_POOL_LABEL`]
/// (plus the managed-by label, so a foreign Job that happens to carry the key
/// cannot inflate the count) returns every pooled Job in the watch scope, and
/// `repo_live` filters that set in memory on the label VALUE. Two selectors
/// would be two round-trips and, worse, two views of a moving cluster — a run
/// could be counted in one and not the other.
///
/// Jobs with no pool label are INVISIBLE here by construction. That is the
/// upgrade-skew contract: Jobs stamped by a kopiur that predates the label are
/// not counted, so an upgrade can briefly over-admit — strictly better than the
/// alternative, where unlabeled Jobs would be counted against every repository
/// at once and park the whole fleet.
///
/// **Scoped to [`Context::watch_scope`](crate::context::Context::watch_scope).**
/// The LIST only sees the namespaces this install watches, so under a
/// NAMESPACED install a `ClusterRepository`'s cap bounds the mover Jobs in the
/// watched namespace, not every namespace using that repository. That is the
/// only answer available — a cluster-wide LIST under a namespaced install's
/// Role RBAC is a permanent 403 that would wedge the reconcile — and it is the
/// same scoping the batch-delete throttle and every other reconcile-time LIST
/// already carries.
pub async fn observe_live_pool(
    ctx: &Context,
    repo_pool_value: &str,
    caps: PoolCaps,
) -> Result<ObservedPool> {
    if caps.is_uncapped() {
        return Ok(ObservedPool::default());
    }
    let selector = format!("{REPO_POOL_LABEL},{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}");
    let job_api: Api<Job> = crate::controllers::scoped_api(&ctx.client, &ctx.watch_scope);
    let jobs = job_api
        .list(&ListParams::default().labels(&selector))
        .await?
        .items;
    Ok(observe_pool(&jobs, repo_pool_value))
}

/// One LIST's worth of pool truth: the two counts [`pool_verdict`] is measured
/// against, plus the identity of every pooled Job the LIST actually returned.
///
/// `seen` is the half [`AdmissionLedger`] needs and the counts cannot supply. A
/// reservation exists only to cover the window between "this reconcile decided
/// to create a Job" and "that Job is visible to a LIST"; the moment the Job IS
/// visible, the observation already accounts for it and the reservation must
/// not be added on top. `seen` therefore holds EVERY pooled Job returned —
/// terminal and suspended ones included, which the counts deliberately exclude
/// — because a Job's mere existence is what proves its reservation is spent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedPool {
    /// Live pooled Jobs against THIS repository.
    pub repo_live: usize,
    /// Live pooled Jobs across every repository in the watch scope.
    pub global_live: usize,
    /// `namespace/name` of every pooled Job the LIST returned, whatever its
    /// state. Sorted (`BTreeSet`) so a `Debug` dump of a failing test is stable.
    pub seen: BTreeSet<String>,
}

/// The `namespace/name` identity a pooled `Job` is reserved and swept under.
///
/// One spelling, shared by [`observe_pool`] and every gate call site: two
/// spellings of "which Job is this" could drift, and a drifted key would make
/// the [`AdmissionLedger`] sweep miss — leaking a reservation until its RAII
/// guard drops.
pub fn job_key(namespace: &str, name: &str) -> String {
    format!("{namespace}/{name}")
}

/// The in-memory half of [`observe_live_pool`], split out so the filtering rules
/// are unit-testable without a cluster.
///
/// A Job with no [`REPO_POOL_LABEL`] contributes to NEITHER count and is not
/// `seen`. The LIST's exists-selector already excludes those, so this is
/// belt-and-braces — but it is the belt that keeps the upgrade-skew contract
/// true if this ever grows a store-backed caller whose input is not
/// pre-filtered.
pub fn observe_pool(jobs: &[Job], repo_pool_value: &str) -> ObservedPool {
    let mut observed = ObservedPool::default();
    for job in jobs {
        let Some(value) = job
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(REPO_POOL_LABEL))
        else {
            continue;
        };
        if let Some(name) = job.metadata.name.as_deref() {
            observed.seen.insert(job_key(
                job.metadata.namespace.as_deref().unwrap_or_default(),
                name,
            ));
        }
        if !job_occupies_slot(job) {
            continue;
        }
        observed.global_live += 1;
        if value == repo_pool_value {
            observed.repo_live += 1;
        }
    }
    observed
}

/// The counts alone, for the call sites and tests that do not need `seen`.
pub fn count_pool(jobs: &[Job], repo_pool_value: &str) -> (usize, usize) {
    let observed = observe_pool(jobs, repo_pool_value);
    (observed.repo_live, observed.global_live)
}

// --- the admission ledger: making observe→decide→create atomic ---------------

/// Reservations, `pool key → the {namespace}/{name} of each Job promised but not
/// yet observed`. A [`BTreeMap`]/[`BTreeSet`] rather than a hash map because the
/// contents are tiny (one entry per in-flight admission) and a stable ordering
/// makes a `Debug` dump of a failing test readable.
type Reservations = BTreeMap<String, BTreeSet<String>>;

/// Take the ledger lock, recovering from a poisoned mutex.
///
/// A poisoned lock here means some reconcile panicked while holding it. The
/// guarded value is a plain set of strings with no broken invariant to inherit,
/// and refusing to admit anything ever again would be a far worse failure than
/// carrying on, so the poison is deliberately absorbed.
fn lock_reservations(inner: &Mutex<Reservations>) -> std::sync::MutexGuard<'_, Reservations> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The in-process record of admissions this leader has GRANTED but whose Jobs a
/// LIST cannot see yet.
///
/// **Why this exists.** [`observe_live_pool`] and the Job `apply` that follows
/// it are separated by the whole launch path — mover-identity resolution,
/// credential projection, source staging — many API round-trips wide. Two
/// `Snapshot`s created a millisecond apart are reconciled concurrently
/// (`KOPIUR_RECONCILE_CONCURRENCY`, default 8); both LIST an empty pool, both
/// see `0 < 1`, both `Admit`, and a `maxConcurrentJobs: 1` repository runs two
/// movers. The LIST is a read of the past; the ledger is the record of the
/// decisions that have already been made against it. Decide and reserve happen
/// under ONE lock ([`AdmissionLedger::admit`]) — that atomicity is the fix.
///
/// **Why in-process state is sound.** Only the LEADER reconciles (leader
/// election), so there is never a second replica whose ledger this would have to
/// agree with. On failover the new leader starts EMPTY, which reopens exactly
/// the pre-fix window for the length of one reconcile pass — a bounded,
/// once-per-failover overshoot rather than a steady-state one. The observed LIST
/// remains the source of truth; the ledger only ever ADDS to it, so a lost
/// ledger can over-admit but never under-admit (it cannot park a repository that
/// has room).
///
/// **Why it cannot leak.** A granted reservation is handed back as an
/// [`AdmissionGuard`] whose `Drop` releases it, so every exit from the launch
/// path — Job applied, apply failed, mid-path error, early return — releases it
/// exactly once, including on an unwinding panic. On top of that, every pass
/// through [`admit`](Self::admit) sweeps any reservation whose Job the LIST can
/// now SEE: observed truth supersedes a promise, so even a reservation somehow
/// held past its Job's creation cannot park a repository forever.
///
/// Cheap to clone (one `Arc`), and never touched at all on the uncapped default
/// path — [`admit_or_park`] checks [`PoolCaps::is_uncapped`] before the lock.
#[derive(Debug, Clone, Default)]
pub struct AdmissionLedger {
    inner: Arc<Mutex<Reservations>>,
}

/// The [`AdmissionLedger`]'s answer for one run: [`pool_verdict`]'s decision
/// taken against `observed + reservations`, with the reservation attached.
#[derive(Debug)]
pub enum LedgerVerdict {
    /// Mint the mover Job now. `reservation` is `Some` for a class that
    /// occupies a slot the moment it is promised, and MUST be held until the
    /// Job exists; `None` for [`PoolClass::Restore`], which is admitted
    /// unconditionally and so has no decision to protect.
    Admit {
        /// The held reservation — drop it once the Job is applied (or the
        /// attempt failed).
        reservation: Option<AdmissionGuard>,
    },
    /// Hold the run. Carries the EFFECTIVE counts (observed plus outstanding
    /// reservations) that produced the decision, so the park message names the
    /// numbers the gate actually judged against.
    Park {
        /// Effective live pooled Jobs against THIS repository.
        repo_live: usize,
        /// Effective live pooled Jobs across every repository.
        global_live: usize,
    },
}

/// A held pool reservation. Releases it on `Drop`.
///
/// RAII rather than an explicit release call because the launch path has many
/// exits (a `?` on any of a dozen awaits, an early `return` on a missing
/// dependency, a panic) and each one must release. A forgotten release would
/// park the repository until the operator restarted.
#[derive(Debug)]
#[must_use = "the reservation is released when this guard drops; bind it for the \
              whole window between the admission and the Job's creation"]
pub struct AdmissionGuard {
    inner: Arc<Mutex<Reservations>>,
    pool_key: String,
    job_key: String,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        let mut held = lock_reservations(&self.inner);
        if let Some(pool) = held.get_mut(&self.pool_key) {
            pool.remove(&self.job_key);
            // Prune the empty set so an install with many repositories does not
            // accumulate one map entry per pool key it has ever admitted from.
            if pool.is_empty() {
                held.remove(&self.pool_key);
            }
        }
    }
}

impl AdmissionLedger {
    /// Decide and reserve **atomically**: sweep spent reservations, add the
    /// outstanding ones to `observed`, run [`pool_verdict`] over the sum, and —
    /// on `Admit` — record this run's reservation, all under one lock.
    ///
    /// `pool_key` is the [`repo_label`] value; `job_key` is [`job_key`] over the
    /// namespace and name of the Job this run is about to create. The Job key is
    /// the reservation's identity precisely because it is what the sweep can
    /// match against [`ObservedPool::seen`] — and it makes a repeated reserve for
    /// one run idempotent, since re-inserting the same key into the set is a
    /// no-op.
    ///
    /// Exhaustive over [`PoolClass`]: a new pooled work kind cannot compile
    /// until its reservation behavior is decided here.
    pub fn admit(
        &self,
        pool_key: &str,
        job_key: &str,
        observed: &ObservedPool,
        class: PoolClass,
        caps: PoolCaps,
    ) -> LedgerVerdict {
        let mut held = lock_reservations(&self.inner);
        sweep_observed(&mut held, &observed.seen);
        let repo_reserved = held.get(pool_key).map_or(0, BTreeSet::len);
        let global_reserved: usize = held.values().map(BTreeSet::len).sum();
        let repo_live = observed.repo_live + repo_reserved;
        let global_live = observed.global_live + global_reserved;
        match pool_verdict(class, repo_live, global_live, caps) {
            PoolVerdict::Park {
                repo_live,
                global_live,
            } => LedgerVerdict::Park {
                repo_live,
                global_live,
            },
            PoolVerdict::Admit => {
                let reservation = match class {
                    // A restore is admitted at or over every cap, so there is no
                    // decision for a reservation to protect. Reserving one would
                    // only make the NEXT backup's effective count double-charge
                    // a restore the LIST is about to show anyway.
                    PoolClass::Restore => None,
                    PoolClass::Backup | PoolClass::Replication => {
                        held.entry(pool_key.to_string())
                            .or_default()
                            .insert(job_key.to_string());
                        Some(AdmissionGuard {
                            inner: Arc::clone(&self.inner),
                            pool_key: pool_key.to_string(),
                            job_key: job_key.to_string(),
                        })
                    }
                };
                LedgerVerdict::Admit { reservation }
            }
        }
    }

    /// Outstanding reservations, `pool key → job keys`. Test-only: production
    /// code must read the ledger through [`admit`](Self::admit), which is the
    /// only place the decide-and-reserve pair is atomic.
    #[cfg(test)]
    fn outstanding(&self) -> Reservations {
        lock_reservations(&self.inner).clone()
    }
}

/// Drop every reservation whose Job the LIST can now SEE.
///
/// The defensive half of the leak story (the [`AdmissionGuard`] is the primary
/// one). A reservation is a promise that a Job is about to exist; once it DOES
/// exist, the observation counts it and the promise is spent. Sweeping on the
/// Job's existence — not its liveness — is deliberate: a Job already terminal
/// (or TTL-suspended) means the run it stood for is over, so its reservation is
/// stale either way.
fn sweep_observed(held: &mut Reservations, seen: &BTreeSet<String>) {
    if seen.is_empty() {
        return;
    }
    held.retain(|_, pool| {
        pool.retain(|job| !seen.contains(job));
        !pool.is_empty()
    });
}

/// The composed gate step: LIST the pool, then decide-and-reserve under the
/// ledger's lock.
///
/// The two halves are one function because separating them is exactly the bug
/// this closes — a caller that observed, then did something else, then decided,
/// would reopen the window. Every pooled gate goes through here.
///
/// **The uncapped default touches neither the apiserver nor the lock.** Both
/// caps `None` short-circuits to `Admit { reservation: None }` before the LIST,
/// so an install that never asked for a cap pays one branch.
pub async fn admit_or_park(
    ctx: &Context,
    pool_key: &str,
    job_key: &str,
    class: PoolClass,
    caps: PoolCaps,
) -> Result<LedgerVerdict> {
    if caps.is_uncapped() {
        return Ok(LedgerVerdict::Admit { reservation: None });
    }
    let observed = observe_live_pool(ctx, pool_key, caps).await?;
    Ok(ctx
        .pool_admissions
        .admit(pool_key, job_key, &observed, class, caps))
}

/// Whether `RepositorySlotAvailable` is present AND `False` on a run's status —
/// i.e. a park is currently standing and must be healed on admission.
///
/// "Only if PRESENT" is the load-bearing half, and it is why this is a
/// three-state question rather than a boolean: a run that was never parked must
/// not GROW a condition it never had, or every backup in every install that
/// never asked for a cap would gain a status field (and a `resourceVersion`
/// bump, and a GitOps diff). The `clear_source_pvc_gate_if_parked` discipline.
///
/// One definition, shared by the `Snapshot` gate and both replication gates: two
/// spellings of "is this parked?" could drift, and the one that drifted would
/// silently stop healing — leaving a launched run permanently reporting that it
/// is queued.
pub fn slot_gate_is_false(
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> bool {
    conditions.iter().any(|c| {
        c.type_ == kopiur_api::consts::REPOSITORY_SLOT_AVAILABLE_CONDITION && c.status == "False"
    })
}

/// How a repository is NAMED in a pool message: `namespace/name` for a
/// namespaced `Repository`, bare `name` for a cluster-scoped one.
///
/// The namespace is not decoration — two `Repository`s of one name in different
/// namespaces are different pools with different caps, so a message that dropped
/// it would send the reader to the wrong object.
pub fn repo_display(r: &RepositoryRef) -> String {
    match &r.namespace {
        Some(ns) => format!("{ns}/{}", r.name),
        None => r.name.clone(),
    }
}

/// The user-facing park message: which repository is saturated, by how much, and
/// the one thing people always ask next — whether a restore is stuck behind it.
///
/// Deliberately free of timestamps, attempt counters and Job names: this string
/// lands in a status condition that is re-written on every parked pass, so
/// anything volatile in it would bump `resourceVersion`, re-trigger the primary
/// watch and hot-loop the reconciler. The global clause is omitted entirely when
/// no global cap is set, rather than rendered as "unlimited" noise.
pub fn waiting_for_slot_message(
    repo_kind: &str,
    repo: &str,
    repo_live: usize,
    global_live: usize,
    caps: PoolCaps,
) -> String {
    let cap = match caps.repo {
        Some(c) => c.get().to_string(),
        // Parked on the global backstop alone: the repository itself is
        // uncapped, so quoting a per-repo denominator would be a lie.
        None => "unlimited".to_string(),
    };
    let global = match caps.global {
        Some(g) => format!(" (global {global_live}/{})", g.get()),
        None => String::new(),
    };
    format!(
        "waiting for a mover slot on {repo_kind} {repo}: {repo_live}/{cap} jobs running{global}; \
         restores are never held"
    )
}

/// Base wait before a parked run re-checks its repository's pool.
///
/// 30s matches the cadence a launched backup polls its Job at, so a freed slot
/// is picked up about as fast as it is released, without a busy loop against the
/// apiserver for a queue that may be minutes long.
///
/// This timer is the ONLY wake-up a parked run gets. A finishing mover Job is
/// owned by the CR that launched it, so its terminal event re-triggers only that
/// CR's reconcile — nothing pushes the runs QUEUED BEHIND it awake. Expect up to
/// [`pool_wait_requeue`]'s window of latency between a slot freeing and the next
/// run taking it.
pub const POOL_WAIT_REQUEUE: std::time::Duration = std::time::Duration::from_secs(30);

/// The parked-run requeue: [`POOL_WAIT_REQUEUE`] plus a deterministic per-object
/// offset over the same window, so `[30s, 60s)`.
///
/// **No RNG**, by the repo's croner-`H` convention: the offset is a hash of
/// `{kind}/{namespace}/{name}`, identical across restarts and across HA
/// replicas. Two things need that. A queue is by definition a set of objects
/// that all became eligible at once, so a flat requeue would re-synchronize the
/// whole queue into one 30s thundering herd against the apiserver every cycle.
/// And a leader failover must not reshuffle who wakes when — with a hash, the
/// new leader computes the same schedule the old one was running.
pub fn pool_wait_requeue(kind: &str, namespace: Option<&str>, name: &str) -> std::time::Duration {
    let seed = format!("{kind}/{}/{name}", namespace.unwrap_or(""));
    // slot_start_unix = 0: there is no schedule slot here, the seed alone
    // spreads objects — the `jittered_transient_requeue` shape.
    POOL_WAIT_REQUEUE + kopiur_api::jitter_offset(&seed, 0, POOL_WAIT_REQUEUE)
}

/// The message for the `True`/`SlotAcquired` heal written when a previously
/// parked run is admitted. Static (no counts): it is terminal for the park
/// episode, and a count in it would churn on every re-admission.
pub fn slot_acquired_message(repo_kind: &str, repo: &str) -> String {
    format!("holding a mover slot on {repo_kind} {repo}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::common::RepositoryKind;

    /// Every variant, so the truth-table tests below are provably exhaustive.
    /// Kept honest by [`assert_all_is_every_variant`], which destructures the
    /// enum exhaustively — a variant added to `MoverJobKind` but not to this
    /// list fails to COMPILE, rather than slipping through a length check.
    const ALL: &[MoverJobKind] = &[
        MoverJobKind::Backup,
        MoverJobKind::Restore,
        MoverJobKind::RepositoryReplication,
        MoverJobKind::SnapshotReplication,
        MoverJobKind::Maintenance,
        MoverJobKind::Verification,
        MoverJobKind::Pin,
        MoverJobKind::SnapshotDeleteBatch,
        MoverJobKind::Discovery,
        MoverJobKind::BrowseSession,
    ];

    const IN_POOL: &[MoverJobKind] = &[
        MoverJobKind::Backup,
        MoverJobKind::Restore,
        MoverJobKind::RepositoryReplication,
        MoverJobKind::SnapshotReplication,
    ];

    const OUT_OF_POOL: &[MoverJobKind] = &[
        MoverJobKind::Maintenance,
        MoverJobKind::Verification,
        MoverJobKind::Pin,
        MoverJobKind::SnapshotDeleteBatch,
        MoverJobKind::Discovery,
        MoverJobKind::BrowseSession,
    ];

    fn nas() -> RepositoryRef {
        RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: Some("backups".into()),
        }
    }

    /// The COMPILE-TIME half of the exhaustiveness guard.
    ///
    /// `ALL` is a hand-written list, so a length check alone proves nothing: a
    /// new `MoverJobKind` added to none of the three consts would keep
    /// `ALL.len() == IN_POOL.len() + OUT_OF_POOL.len()` true and sail through.
    /// This `match` has no `_ =>` arm and names each variant exactly as `ALL`
    /// does, so adding a variant to the enum breaks the BUILD here until it is
    /// listed — which is what forces it into a partition, and from there into
    /// [`counts_toward_repo_pool`]'s truth table below.
    fn assert_all_is_every_variant(k: MoverJobKind) {
        // Mirrors `ALL`, in order. Keep the two in sync.
        match k {
            MoverJobKind::Backup => assert_eq!(ALL[0], k),
            MoverJobKind::Restore => assert_eq!(ALL[1], k),
            MoverJobKind::RepositoryReplication => assert_eq!(ALL[2], k),
            MoverJobKind::SnapshotReplication => assert_eq!(ALL[3], k),
            MoverJobKind::Maintenance => assert_eq!(ALL[4], k),
            MoverJobKind::Verification => assert_eq!(ALL[5], k),
            MoverJobKind::Pin => assert_eq!(ALL[6], k),
            MoverJobKind::SnapshotDeleteBatch => assert_eq!(ALL[7], k),
            MoverJobKind::Discovery => assert_eq!(ALL[8], k),
            MoverJobKind::BrowseSession => assert_eq!(ALL[9], k),
        }
    }

    #[test]
    fn every_kind_is_covered_by_the_truth_tables() {
        // Compile-time: `ALL` really is every variant, in the stated order (a
        // new variant fails to compile in `assert_all_is_every_variant`; a
        // REORDERED or duplicated `ALL` fails the index assertions here).
        assert_eq!(ALL.len(), 10, "ALL and the destructure must stay in step");
        for k in ALL {
            assert_all_is_every_variant(*k);
        }
        // Run-time: each variant sits in exactly one partition, and
        // `counts_toward_repo_pool` agrees with that partition.
        assert_eq!(ALL.len(), IN_POOL.len() + OUT_OF_POOL.len());
        for k in ALL {
            assert!(
                IN_POOL.contains(k) ^ OUT_OF_POOL.contains(k),
                "{k:?} must be in exactly one partition"
            );
            assert_eq!(counts_toward_repo_pool(*k), IN_POOL.contains(k), "{k:?}");
        }
    }

    // --- presence: the four pooled job-build paths ------------------------

    #[test]
    fn backup_jobs_carry_the_pool_label_with_the_repo_label_value() {
        assert_eq!(
            repo_pool_label(MoverJobKind::Backup, &nas()),
            Some((REPO_POOL_LABEL.to_string(), repo_label(&nas()))),
        );
    }

    #[test]
    fn restore_jobs_carry_the_pool_label_with_the_repo_label_value() {
        assert_eq!(
            repo_pool_label(MoverJobKind::Restore, &nas()),
            Some((REPO_POOL_LABEL.to_string(), repo_label(&nas()))),
        );
    }

    #[test]
    fn repository_replication_jobs_carry_the_pool_label_with_the_repo_label_value() {
        assert_eq!(
            repo_pool_label(MoverJobKind::RepositoryReplication, &nas()),
            Some((REPO_POOL_LABEL.to_string(), repo_label(&nas()))),
        );
    }

    #[test]
    fn snapshot_replication_jobs_carry_the_pool_label_with_the_repo_label_value() {
        assert_eq!(
            repo_pool_label(MoverJobKind::SnapshotReplication, &nas()),
            Some((REPO_POOL_LABEL.to_string(), repo_label(&nas()))),
        );
    }

    #[test]
    fn every_pooled_kind_shares_one_value_so_one_selector_counts_the_pool() {
        // The point of the label: a backup, a restore and a replication against
        // the SAME repository must be countable with a single selector.
        let values: std::collections::BTreeSet<String> = IN_POOL
            .iter()
            .map(|k| repo_pool_label(*k, &nas()).expect("pooled").1)
            .collect();
        assert_eq!(values.len(), 1, "one repository ⇒ one pool value");
    }

    // --- absence: housekeeping / interactive Jobs stay out of the pool -----

    #[test]
    fn housekeeping_jobs_carry_no_pool_label_at_all() {
        // Regression pin: maintenance, verification, pin, the snapdel batch,
        // bootstrap/discovery and the browse session must not be countable in
        // the pool, and `None` keeps their Job metadata byte-identical.
        for kind in OUT_OF_POOL {
            assert_eq!(repo_pool_label(*kind, &nas()), None, "{kind:?}");
        }
    }

    // --- the normalization invariant --------------------------------------

    #[test]
    fn a_denormalized_ref_would_split_the_pool() {
        // Why every call site keys off the RESOLVED identity: the same
        // repository named without a namespace hashes to a DIFFERENT value.
        // This test documents the failure mode the callers avoid.
        let raw = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: None,
        };
        assert_ne!(
            repo_pool_label(MoverJobKind::Backup, &raw),
            repo_pool_label(MoverJobKind::Backup, &nas()),
        );
    }

    #[test]
    fn normalizing_a_namespaceless_spec_ref_rejoins_the_pool() {
        // The invariant the reconcilers rely on: a spec ref with
        // `namespace: None`, normalized against the owning CR's namespace,
        // produces the SAME pool value as the resolved/pinned identity.
        let raw = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: None,
        };
        let normalized = kopiur_api::common::normalized_repository_ref(&raw, "backups");
        assert_eq!(normalized, nas());
        assert_eq!(
            repo_pool_label(MoverJobKind::Backup, &normalized),
            repo_pool_label(MoverJobKind::Backup, &nas()),
        );
        assert_eq!(
            crate::naming::pinned_repo_key(&normalized),
            crate::naming::pinned_repo_key(&nas()),
        );
    }

    #[test]
    fn a_cluster_repository_pool_is_distinct_from_a_namespaced_one_of_the_same_name() {
        let cluster = RepositoryRef {
            kind: RepositoryKind::ClusterRepository,
            name: "nas".into(),
            namespace: None,
        };
        assert_ne!(
            repo_pool_label(MoverJobKind::Backup, &cluster),
            repo_pool_label(MoverJobKind::Backup, &nas()),
        );
    }

    #[test]
    fn two_namespaces_holding_a_repository_of_one_name_get_distinct_pools() {
        let other = RepositoryRef {
            namespace: Some("other".into()),
            ..nas()
        };
        assert_ne!(
            repo_pool_label(MoverJobKind::Backup, &other),
            repo_pool_label(MoverJobKind::Backup, &nas()),
        );
    }

    #[test]
    fn the_pool_label_value_is_a_valid_label_value() {
        // Kubernetes label values: <=63 chars, alphanumerics plus `-_.`, and
        // must start/end alphanumeric. A long repository name must not produce
        // a Job the apiserver rejects.
        let long = RepositoryRef {
            name: "n".repeat(120),
            ..nas()
        };
        let (_, value) = repo_pool_label(MoverJobKind::Backup, &long).expect("pooled");
        assert!(value.len() <= 63, "{} chars", value.len());
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        );
        assert!(value.starts_with(|c: char| c.is_ascii_alphanumeric()));
        assert!(value.ends_with(|c: char| c.is_ascii_alphanumeric()));
    }

    // --- the admission gate: pool_verdict truth table ----------------------

    /// Every [`PoolClass`], kept honest the same way `ALL` is: the destructure
    /// in [`assert_all_classes`] has no `_ =>` arm, so a new class fails to
    /// COMPILE until it is placed in the parkable/never-parked partition below.
    const ALL_CLASSES: &[PoolClass] = &[
        PoolClass::Backup,
        PoolClass::Replication,
        PoolClass::Restore,
    ];

    /// Classes the gate may hold.
    const PARKABLE: &[PoolClass] = &[PoolClass::Backup, PoolClass::Replication];

    /// Classes the gate must ALWAYS admit.
    const NEVER_PARKED: &[PoolClass] = &[PoolClass::Restore];

    fn assert_all_classes(c: PoolClass) {
        match c {
            PoolClass::Backup => assert_eq!(ALL_CLASSES[0], c),
            PoolClass::Replication => assert_eq!(ALL_CLASSES[1], c),
            PoolClass::Restore => assert_eq!(ALL_CLASSES[2], c),
        }
    }

    fn nz(n: usize) -> Option<std::num::NonZeroUsize> {
        std::num::NonZeroUsize::new(n)
    }

    fn repo_cap(n: usize) -> PoolCaps {
        PoolCaps {
            repo: nz(n),
            global: None,
        }
    }

    fn global_cap(n: usize) -> PoolCaps {
        PoolCaps {
            repo: None,
            global: nz(n),
        }
    }

    #[test]
    fn every_pool_class_is_partitioned_as_parkable_or_never_parked() {
        assert_eq!(
            ALL_CLASSES.len(),
            3,
            "ALL_CLASSES and the destructure must stay in step"
        );
        for c in ALL_CLASSES {
            assert_all_classes(*c);
            assert!(
                PARKABLE.contains(c) ^ NEVER_PARKED.contains(c),
                "{c:?} must be in exactly one partition"
            );
        }
        assert_eq!(ALL_CLASSES.len(), PARKABLE.len() + NEVER_PARKED.len());
    }

    #[test]
    fn an_uncapped_pool_admits_every_class_at_any_count() {
        // The default install: nothing is set, so nothing is ever held —
        // whatever the (never-counted, hence always 0) live numbers say.
        for c in ALL_CLASSES {
            assert_eq!(
                pool_verdict(*c, 0, 0, PoolCaps::default()),
                PoolVerdict::Admit,
                "{c:?}"
            );
            // Even fed absurd counts, an uncapped pool cannot park.
            assert_eq!(
                pool_verdict(*c, 999, 999, PoolCaps::default()),
                PoolVerdict::Admit,
                "{c:?}"
            );
        }
        assert!(PoolCaps::default().is_uncapped());
    }

    #[test]
    fn a_restore_is_admitted_at_and_over_every_cap() {
        // The guarantee the whole feature is sold on: a recovery never queues.
        for caps in [
            repo_cap(1),
            global_cap(1),
            PoolCaps {
                repo: nz(1),
                global: nz(1),
            },
        ] {
            for (repo_live, global_live) in [(1, 1), (5, 5), (100, 100)] {
                assert_eq!(
                    pool_verdict(PoolClass::Restore, repo_live, global_live, caps),
                    PoolVerdict::Admit,
                    "{caps:?} at {repo_live}/{global_live}"
                );
            }
        }
    }

    #[test]
    fn a_parkable_class_parks_at_the_repository_cap() {
        for c in PARKABLE {
            assert_eq!(
                pool_verdict(*c, 2, 2, repo_cap(2)),
                PoolVerdict::Park {
                    repo_live: 2,
                    global_live: 2
                },
                "{c:?}"
            );
        }
    }

    #[test]
    fn a_parkable_class_parks_on_the_global_cap_alone() {
        // No per-repository cap at all: the cluster-operator backstop is
        // sufficient on its own.
        for c in PARKABLE {
            assert_eq!(
                pool_verdict(*c, 0, 3, global_cap(3)),
                PoolVerdict::Park {
                    repo_live: 0,
                    global_live: 3
                },
                "{c:?}"
            );
        }
    }

    #[test]
    fn the_two_caps_are_independent() {
        let caps = PoolCaps {
            repo: nz(2),
            global: nz(5),
        };
        // Repo cap met, global far from it ⇒ park.
        assert!(matches!(
            pool_verdict(PoolClass::Backup, 2, 2, caps),
            PoolVerdict::Park { .. }
        ));
        // Global cap met, repo far from it ⇒ park.
        assert!(matches!(
            pool_verdict(PoolClass::Backup, 0, 5, caps),
            PoolVerdict::Park { .. }
        ));
        // Neither met ⇒ admit.
        assert_eq!(
            pool_verdict(PoolClass::Backup, 1, 4, caps),
            PoolVerdict::Admit
        );
    }

    #[test]
    fn the_boundary_is_at_the_cap_not_past_it() {
        // `live == cap` is FULL (the cap is a ceiling on concurrent Jobs, and
        // the run asking would be the cap+1'th). `live == cap - 1` has room.
        assert!(matches!(
            pool_verdict(PoolClass::Backup, 3, 0, repo_cap(3)),
            PoolVerdict::Park { .. }
        ));
        assert_eq!(
            pool_verdict(PoolClass::Backup, 2, 0, repo_cap(3)),
            PoolVerdict::Admit
        );
        // And over the cap (a race admitted two at once) still parks.
        assert!(matches!(
            pool_verdict(PoolClass::Backup, 4, 0, repo_cap(3)),
            PoolVerdict::Park { .. }
        ));
    }

    #[test]
    fn a_park_carries_the_counts_that_produced_it() {
        // The message names these numbers, so they must be the OBSERVED ones,
        // not re-derived later from a second (moved) view of the cluster.
        assert_eq!(
            pool_verdict(PoolClass::Replication, 7, 9, repo_cap(3)),
            PoolVerdict::Park {
                repo_live: 7,
                global_live: 9
            }
        );
    }

    // --- pool_live_counts filtering ----------------------------------------

    fn job(name: &str, pool: Option<&str>) -> Job {
        let mut j: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": { "name": name, "namespace": "backups" },
            "spec": { "template": { "spec": { "containers": [], "restartPolicy": "Never" } } },
        }))
        .expect("job fixture");
        if let Some(value) = pool {
            j.metadata
                .labels
                .get_or_insert_with(Default::default)
                .insert(REPO_POOL_LABEL.to_string(), value.to_string());
        }
        j
    }

    fn with_condition(mut j: Job, type_: &str, status: &str) -> Job {
        let conditions = serde_json::json!([{
            "type": type_,
            "status": status,
            "lastProbeTime": null,
            "lastTransitionTime": null,
        }]);
        let status_obj = serde_json::json!({ "conditions": conditions });
        j.status = Some(serde_json::from_value(status_obj).expect("job status"));
        j
    }

    #[test]
    fn a_running_pooled_job_occupies_a_slot() {
        assert!(job_occupies_slot(&job("live", Some("nas-abc"))));
    }

    #[test]
    fn a_terminal_job_does_not_occupy_a_slot() {
        // A Complete/Failed Job is only waiting out its TTL. Counting it would
        // turn a cap of 1 into "one run per TTL window".
        for (type_, status) in [("Complete", "True"), ("Failed", "True")] {
            let j = with_condition(job("done", Some("nas-abc")), type_, status);
            assert!(!job_occupies_slot(&j), "{type_}={status}");
        }
    }

    #[test]
    fn a_suspended_job_does_not_occupy_a_slot() {
        // Kueue admits Jobs by flipping spec.suspend; counting a suspended Job
        // would let its backlog fill kopiur's pool and deadlock both.
        let mut j = job("held", Some("nas-abc"));
        j.spec.as_mut().expect("spec").suspend = Some(true);
        assert!(!job_occupies_slot(&j));

        // The status-condition form, written once the pods are torn down.
        let j = with_condition(job("held2", Some("nas-abc")), "Suspended", "True");
        assert!(!job_occupies_slot(&j));

        // `Suspended=False` (resumed) is live again.
        let j = with_condition(job("resumed", Some("nas-abc")), "Suspended", "False");
        assert!(job_occupies_slot(&j));
    }

    #[test]
    fn another_repositorys_jobs_count_globally_but_not_per_repository() {
        let jobs = vec![
            job("mine-1", Some("nas-abc")),
            job("mine-2", Some("nas-abc")),
            job("theirs", Some("offsite-def")),
        ];
        assert_eq!(count_pool(&jobs, "nas-abc"), (2, 3));
        assert_eq!(count_pool(&jobs, "offsite-def"), (1, 3));
    }

    #[test]
    fn unlabeled_jobs_are_invisible_to_both_counts() {
        // Upgrade-skew pin: Jobs stamped by a kopiur that predates the pool
        // label contribute to NEITHER count. Brief over-admission during an
        // upgrade is the deliberate trade — the alternative (counting them
        // against every repository) would park the whole fleet.
        let jobs = vec![
            job("legacy", None),
            job("legacy-2", None),
            job("mine", Some("nas-abc")),
        ];
        assert_eq!(count_pool(&jobs, "nas-abc"), (1, 1));
    }

    #[test]
    fn terminal_and_suspended_jobs_are_excluded_from_both_counts() {
        let jobs = vec![
            job("live", Some("nas-abc")),
            with_condition(job("done", Some("nas-abc")), "Complete", "True"),
            with_condition(job("held", Some("offsite-def")), "Suspended", "True"),
        ];
        assert_eq!(count_pool(&jobs, "nas-abc"), (1, 1));
    }

    #[test]
    fn an_empty_pool_counts_zero() {
        assert_eq!(count_pool(&[], "nas-abc"), (0, 0));
    }

    // --- the observation's `seen` set --------------------------------------

    #[test]
    fn every_pooled_job_is_seen_whatever_its_state() {
        // `seen` is the LEDGER's input, not the cap's: a terminal or suspended
        // Job proves its reservation is spent even though it occupies no slot.
        // Getting this wrong would leave a spent reservation charged to the
        // repository until its RAII guard dropped.
        let jobs = vec![
            job("live", Some("nas-abc")),
            with_condition(job("done", Some("nas-abc")), "Complete", "True"),
            with_condition(job("held", Some("nas-abc")), "Suspended", "True"),
        ];
        let observed = observe_pool(&jobs, "nas-abc");
        assert_eq!((observed.repo_live, observed.global_live), (1, 1));
        assert_eq!(
            observed.seen,
            ["backups/live", "backups/done", "backups/held"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>(),
        );
    }

    #[test]
    fn an_unlabeled_job_is_neither_counted_nor_seen() {
        // The upgrade-skew contract extends to `seen`: an unlabeled Job is not
        // in the pool at all, so it must not sweep anybody's reservation either.
        let observed = observe_pool(&[job("legacy", None)], "nas-abc");
        assert_eq!(observed, ObservedPool::default());
    }

    #[test]
    fn a_job_of_another_repository_is_seen_globally() {
        // The sweep is global by construction: the ledger holds reservations for
        // every pool key, and one LIST covers them all.
        let observed = observe_pool(&[job("theirs", Some("offsite-def"))], "nas-abc");
        assert_eq!((observed.repo_live, observed.global_live), (0, 1));
        assert!(observed.seen.contains("backups/theirs"));
    }

    // --- the admission ledger ------------------------------------------------

    const POOL_A: &str = "nas-abc";
    const POOL_B: &str = "offsite-def";

    /// An observation of an EMPTY pool — the state both racing reconciles saw in
    /// the bug this ledger exists to close.
    fn empty() -> ObservedPool {
        ObservedPool::default()
    }

    fn admit_backup(
        ledger: &AdmissionLedger,
        pool: &str,
        job: &str,
        observed: &ObservedPool,
        caps: PoolCaps,
    ) -> LedgerVerdict {
        ledger.admit(pool, job, observed, PoolClass::Backup, caps)
    }

    fn is_admit(v: &LedgerVerdict) -> bool {
        matches!(v, LedgerVerdict::Admit { .. })
    }

    /// The park's `(repo_live, global_live)`, or `None` on an admission.
    ///
    /// A helper rather than `assert_eq!` on the verdict itself because
    /// [`LedgerVerdict`] deliberately is not `PartialEq`: it can carry an
    /// [`AdmissionGuard`], and a comparable reservation is a copyable one.
    fn park_counts(v: &LedgerVerdict) -> Option<(usize, usize)> {
        match v {
            LedgerVerdict::Admit { .. } => None,
            LedgerVerdict::Park {
                repo_live,
                global_live,
            } => Some((*repo_live, *global_live)),
        }
    }

    #[test]
    fn the_second_simultaneous_backup_parks_on_the_first_ones_reservation() {
        // THE REGRESSION. Both reconciles LIST an empty pool (the first one's Job
        // does not exist yet); without the ledger both would see `0 < 1` and both
        // would launch against a `maxConcurrentJobs: 1` repository.
        let ledger = AdmissionLedger::default();
        let caps = repo_cap(1);
        let first = admit_backup(&ledger, POOL_A, "ns/one", &empty(), caps);
        assert!(is_admit(&first));
        assert_eq!(
            park_counts(&admit_backup(&ledger, POOL_A, "ns/two", &empty(), caps)),
            // The park names the EFFECTIVE count: 0 observed + 1 reserved.
            Some((1, 1)),
        );
    }

    #[test]
    fn releasing_the_reservation_frees_the_slot_again() {
        let ledger = AdmissionLedger::default();
        let caps = repo_cap(1);
        let first = admit_backup(&ledger, POOL_A, "ns/one", &empty(), caps);
        assert!(is_admit(&first));
        assert_eq!(ledger.outstanding().len(), 1);
        drop(first);
        // The map entry is pruned, not left as an empty set.
        assert!(ledger.outstanding().is_empty());
        assert!(is_admit(&admit_backup(
            &ledger,
            POOL_A,
            "ns/two",
            &empty(),
            caps
        )));
    }

    #[test]
    fn a_park_reserves_nothing() {
        // A parked run holds no resources — that is the whole design — and a
        // reservation is a resource. If a park left one behind, a full pool
        // would never drain.
        let ledger = AdmissionLedger::default();
        let caps = repo_cap(1);
        let observed = ObservedPool {
            repo_live: 1,
            global_live: 1,
            seen: ["ns/running".to_string()].into_iter().collect(),
        };
        assert!(!is_admit(&admit_backup(
            &ledger, POOL_A, "ns/two", &observed, caps
        )));
        assert!(ledger.outstanding().is_empty());
    }

    #[test]
    fn re_reserving_one_run_is_idempotent() {
        // The reservation is keyed on the JOB, so a run that somehow reaches the
        // gate twice charges its repository once — never twice, which would park
        // the pool one slot early.
        let ledger = AdmissionLedger::default();
        let caps = repo_cap(2);
        let a = admit_backup(&ledger, POOL_A, "ns/one", &empty(), caps);
        let b = admit_backup(&ledger, POOL_A, "ns/one", &empty(), caps);
        assert!(is_admit(&a) && is_admit(&b));
        let outstanding = ledger.outstanding();
        assert_eq!(outstanding[POOL_A].len(), 1);
        // A third, DIFFERENT run still fits under the cap of 2 — it would not if
        // the two reserves for `ns/one` had each charged a slot.
        let third = admit_backup(&ledger, POOL_A, "ns/two", &empty(), caps);
        assert!(is_admit(&third));
    }

    #[test]
    fn a_reservation_whose_job_is_now_visible_is_not_counted_twice() {
        // The dedupe. Once the Job exists the observation counts it; adding the
        // reservation on top would charge one run two slots and park a
        // `maxConcurrentJobs: 2` repository after a single backup.
        let ledger = AdmissionLedger::default();
        let caps = repo_cap(2);
        let held = admit_backup(&ledger, POOL_A, "ns/one", &empty(), caps);
        assert!(is_admit(&held));
        let observed = ObservedPool {
            repo_live: 1,
            global_live: 1,
            seen: ["ns/one".to_string()].into_iter().collect(),
        };
        // BOUND, not a temporary: a verdict dropped at the end of its statement
        // releases its reservation immediately, which would make the assertion
        // below pass for the wrong reason.
        let second = admit_backup(&ledger, POOL_A, "ns/two", &observed, caps);
        assert!(is_admit(&second));
        // ...and the spent reservation was swept, not merely ignored.
        let outstanding = ledger.outstanding();
        assert_eq!(
            outstanding[POOL_A],
            ["ns/two".to_string()].into_iter().collect::<BTreeSet<_>>(),
        );
    }

    #[test]
    fn a_reservation_whose_job_finished_is_swept_even_though_it_is_not_live() {
        // The leak backstop: a Job that ran to completion is `seen` but not
        // counted, so a reservation still standing for it must be dropped or the
        // repository would be short one slot until the guard dropped.
        let ledger = AdmissionLedger::default();
        let caps = repo_cap(1);
        let held = admit_backup(&ledger, POOL_A, "ns/one", &empty(), caps);
        assert!(is_admit(&held));
        // The Job exists and has COMPLETED: zero live, but `seen`.
        let observed = ObservedPool {
            repo_live: 0,
            global_live: 0,
            seen: ["ns/one".to_string()].into_iter().collect(),
        };
        let second = admit_backup(&ledger, POOL_A, "ns/two", &observed, caps);
        assert!(is_admit(&second));
        let outstanding = ledger.outstanding();
        assert_eq!(
            outstanding[POOL_A],
            ["ns/two".to_string()].into_iter().collect::<BTreeSet<_>>(),
        );
    }

    #[test]
    fn a_reservation_in_one_pool_does_not_park_another_repository() {
        // The per-repository cap is per-repository: `repo_live` only sees this
        // pool key's reservations.
        let ledger = AdmissionLedger::default();
        let caps = repo_cap(1);
        let a = admit_backup(&ledger, POOL_A, "ns/a", &empty(), caps);
        let b = admit_backup(&ledger, POOL_B, "ns/b", &empty(), caps);
        assert!(is_admit(&a) && is_admit(&b));
    }

    #[test]
    fn reservations_across_repositories_add_up_for_the_global_backstop() {
        // The `the_env_backstop_serializes_across_repositories` scenario: two
        // UNCAPPED repositories under `KOPIUR_MAX_CONCURRENT_JOBS=1`. The second
        // run must see the first's reservation even though it is in another pool.
        let ledger = AdmissionLedger::default();
        let caps = global_cap(1);
        let first = admit_backup(&ledger, POOL_A, "ns/a", &empty(), caps);
        assert!(is_admit(&first));
        assert_eq!(
            park_counts(&admit_backup(&ledger, POOL_B, "ns/b", &empty(), caps)),
            // No per-repo reservation in POOL_B, but one globally.
            Some((0, 1)),
        );
    }

    #[test]
    fn a_restore_is_admitted_without_reserving() {
        // A restore is admitted at or over every cap, so it has no decision to
        // protect — and reserving one would double-charge the next backup, which
        // the LIST is about to count anyway.
        let ledger = AdmissionLedger::default();
        let verdict = ledger.admit(POOL_A, "ns/r", &empty(), PoolClass::Restore, repo_cap(1));
        assert!(matches!(
            verdict,
            LedgerVerdict::Admit { reservation: None }
        ));
        assert!(ledger.outstanding().is_empty());
    }

    #[test]
    fn a_replication_reserves_like_a_backup() {
        // Both parkable classes compete for the same budget, so both must hold
        // the window between the decision and the Job.
        let ledger = AdmissionLedger::default();
        let caps = repo_cap(1);
        let held = ledger.admit(POOL_A, "ns/repl", &empty(), PoolClass::Replication, caps);
        assert!(is_admit(&held));
        assert!(!is_admit(&admit_backup(
            &ledger,
            POOL_A,
            "ns/backup",
            &empty(),
            caps
        )));
    }

    #[test]
    fn every_pool_class_has_a_decided_reservation_behavior() {
        // The partition above (`PARKABLE`/`NEVER_PARKED`) is what the ledger's
        // exhaustive `match` keys off: a parkable class reserves, a never-parked
        // one does not. A new class cannot compile past that `match` undecided,
        // and this pins the two halves to each other.
        for c in ALL_CLASSES {
            let ledger = AdmissionLedger::default();
            let verdict = ledger.admit(POOL_A, "ns/x", &empty(), *c, repo_cap(1));
            let reserved = !ledger.outstanding().is_empty();
            assert!(is_admit(&verdict), "{c:?} at an empty pool must admit");
            assert_eq!(reserved, PARKABLE.contains(c), "{c:?}");
            assert_eq!(!reserved, NEVER_PARKED.contains(c), "{c:?}");
        }
    }

    /// Decide-and-reserve is ATOMIC: N runners racing an empty pool with a cap
    /// of 1 produce exactly one admission.
    ///
    /// A multi-thread runtime with a barrier, so the calls really do land
    /// together rather than being serialized by a current-thread scheduler —
    /// which is the shape of the production bug (`KOPIUR_RECONCILE_CONCURRENCY`
    /// runs the reconciles on a multi-thread runtime).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exactly_one_of_many_racing_runs_is_admitted() {
        const RUNNERS: usize = 16;
        let ledger = AdmissionLedger::default();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(RUNNERS));
        let admitted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(RUNNERS);
        for i in 0..RUNNERS {
            let ledger = ledger.clone();
            let barrier = barrier.clone();
            let admitted = admitted.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let verdict = ledger.admit(
                    POOL_A,
                    &format!("ns/run-{i}"),
                    &ObservedPool::default(),
                    PoolClass::Backup,
                    repo_cap(1),
                );
                match verdict {
                    LedgerVerdict::Admit { reservation } => {
                        admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // HOLD it, as the launch path does, until every runner
                        // has decided — releasing early would let a later runner
                        // legitimately take the freed slot and mask the race.
                        Some(reservation)
                    }
                    LedgerVerdict::Park { .. } => None,
                }
            }));
        }
        let mut guards = Vec::new();
        for h in handles {
            guards.push(h.await.expect("runner"));
        }
        assert_eq!(
            admitted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "maxConcurrentJobs=1 admitted more than one racing run",
        );
        drop(guards);
        assert!(ledger.outstanding().is_empty(), "every guard must release");
    }

    // --- the composed gate: the uncapped path is free -------------------------

    /// Records `"<METHOD> <path>"` per request; answers everything `404`. The
    /// same `tower::service_fn` harness the `snapshot_policy` tests use.
    fn recording_client(log: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> kube::Client {
        use http::Response;
        use kube::client::Body;
        let svc = tower::service_fn(move |req: http::Request<Body>| {
            let log = log.clone();
            async move {
                log.lock()
                    .expect("log")
                    .push(format!("{} {}", req.method(), req.uri().path()));
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(http::StatusCode::NOT_FOUND)
                        .header("content-type", "application/json")
                        .body(Body::from(b"{}".to_vec()))
                        .expect("response"),
                )
            }
        });
        kube::Client::new(svc, "default")
    }

    #[tokio::test]
    async fn the_uncapped_gate_lists_nothing_and_reserves_nothing() {
        // The default install's contract: no LIST, no lock, no ledger entry —
        // one branch. A regression here would put a Job LIST and a mutex on every
        // backup reconcile in every install that never asked for a cap.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx = Context::test_context(recording_client(log.clone()));
        let verdict = admit_or_park(
            &ctx,
            POOL_A,
            "ns/one",
            PoolClass::Backup,
            PoolCaps::default(),
        )
        .await
        .expect("uncapped admits");
        assert!(matches!(
            verdict,
            LedgerVerdict::Admit { reservation: None }
        ));
        assert!(
            log.lock().expect("log").is_empty(),
            "the uncapped gate made API calls: {:?}",
            log.lock().expect("log"),
        );
        assert!(ctx.pool_admissions.outstanding().is_empty());
    }

    // --- the park / heal messages -------------------------------------------

    #[test]
    fn the_park_message_names_the_repository_the_counts_and_the_restore_rule() {
        let msg = waiting_for_slot_message("Repository", "backups/nas", 2, 2, repo_cap(2));
        assert!(msg.contains("Repository backups/nas"), "{msg}");
        assert!(msg.contains("2/2 jobs running"), "{msg}");
        assert!(msg.contains("restores are never held"), "{msg}");
    }

    #[test]
    fn the_park_message_omits_the_global_clause_when_there_is_no_global_cap() {
        let msg = waiting_for_slot_message("Repository", "backups/nas", 2, 9, repo_cap(2));
        assert!(!msg.contains("global"), "{msg}");
    }

    #[test]
    fn the_park_message_includes_the_global_clause_when_a_backstop_is_set() {
        let caps = PoolCaps {
            repo: nz(2),
            global: nz(4),
        };
        let msg = waiting_for_slot_message("ClusterRepository", "nas", 2, 4, caps);
        assert!(msg.contains("(global 4/4)"), "{msg}");
    }

    #[test]
    fn a_repository_parked_only_by_the_backstop_reports_an_unlimited_denominator() {
        // Quoting a per-repo denominator here would be a lie: this repository
        // set no cap of its own, the cluster backstop held the run.
        let msg = waiting_for_slot_message("Repository", "backups/nas", 1, 3, global_cap(3));
        assert!(msg.contains("1/unlimited jobs running"), "{msg}");
        assert!(msg.contains("(global 3/3)"), "{msg}");
    }

    // --- requeue determinism ------------------------------------------------

    #[test]
    fn the_parked_requeue_is_deterministic_per_object() {
        // HA-safe: a failover must not reshuffle the queue's wake-up schedule.
        let a = pool_wait_requeue("Snapshot", Some("apps"), "db-1");
        let b = pool_wait_requeue("Snapshot", Some("apps"), "db-1");
        assert_eq!(a, b);
    }

    #[test]
    fn the_parked_requeue_lands_in_the_jitter_window() {
        for name in ["db-1", "db-2", "db-3", "web", "a-very-long-snapshot-name"] {
            let d = pool_wait_requeue("Snapshot", Some("apps"), name);
            assert!(d >= POOL_WAIT_REQUEUE, "{name}: {d:?}");
            assert!(d < POOL_WAIT_REQUEUE * 2, "{name}: {d:?}");
        }
    }

    #[test]
    fn different_objects_get_different_offsets() {
        // The whole point: a queue that became eligible together must not wake
        // together. Not every pair need differ, but the spread must be real.
        let offsets: std::collections::BTreeSet<_> = (0..20)
            .map(|i| pool_wait_requeue("Snapshot", Some("apps"), &format!("db-{i}")))
            .collect();
        assert!(offsets.len() > 5, "requeues barely spread: {offsets:?}");
    }

    #[test]
    fn the_requeue_seed_distinguishes_kind_and_namespace() {
        // A `Snapshot` and a `RepositoryReplication` of one name in one
        // namespace are different objects and must not be scheduled as if they
        // were the same. Asserted over a BATCH, not a single pair: the window
        // has only 30 whole-second buckets, so any two given seeds collide
        // ~3% of the time and a per-pair `assert_ne!` would be a coin flip.
        let names = ["nightly", "hourly", "weekly", "db", "media", "photos"];
        let differing_kind = names
            .iter()
            .filter(|n| {
                pool_wait_requeue("Snapshot", Some("apps"), n)
                    != pool_wait_requeue("RepositoryReplication", Some("apps"), n)
            })
            .count();
        assert!(
            differing_kind >= names.len() - 1,
            "the kind must be part of the seed, got {differing_kind}/{} differing",
            names.len()
        );
        let differing_ns = names
            .iter()
            .filter(|n| {
                pool_wait_requeue("Snapshot", Some("apps"), n)
                    != pool_wait_requeue("Snapshot", Some("other"), n)
            })
            .count();
        assert!(
            differing_ns >= names.len() - 1,
            "the namespace must be part of the seed, got {differing_ns}/{} differing",
            names.len()
        );
    }

    #[test]
    fn the_messages_are_free_of_volatile_content() {
        // Stable bytes while parked: a second identical pass must produce a
        // byte-identical condition, or the status patch re-triggers the watch.
        let caps = repo_cap(2);
        assert_eq!(
            waiting_for_slot_message("Repository", "backups/nas", 2, 2, caps),
            waiting_for_slot_message("Repository", "backups/nas", 2, 2, caps),
        );
        assert_eq!(
            slot_acquired_message("Repository", "backups/nas"),
            slot_acquired_message("Repository", "backups/nas"),
        );
    }
}
