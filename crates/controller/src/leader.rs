//! Lease-based leader election (`--leader-elect`).
//!
//! kube-rs has no built-in election, so this is the standard Kubernetes
//! `coordination.k8s.io/v1` Lease protocol, hand-rolled thin over a **pure,
//! unit-tested decision** ([`decide`]): a replica claims the Lease when it is
//! unheld or has gone unchanged past the lease duration, renews it on a fixed
//! cadence while it leads, and stands by otherwise. Writes are compare-and-swap
//! (`replace` with the observed `resourceVersion`; create races surface as
//! 409s), so two replicas can never both conclude they won the same
//! observation.
//!
//! Three deliberate properties, each matching client-go's leaderelection:
//!
//! - **Skew-immune expiry.** A foreign Lease is judged expired by how long WE
//!   have observed it unchanged (a local, monotonic clock via
//!   [`Observation`]), never by comparing the holder's wall-clock `renewTime`
//!   against ours — inter-node clock skew must not let a standby steal a live
//!   leader's Lease. The cost: a fresh process waits out one full lease
//!   duration before claiming a stale Lease (client-go does the same).
//! - **Renew window < lease duration.** [`RENEW_DEADLINE`] (10s) is a budget
//!   for a whole ROUND of attempts, not one attempt's timeout: a round retries
//!   on [`RETRY_PERIOD`] until it succeeds or the window closes, and each
//!   attempt is bounded by what is LEFT of the window so it can never outspend
//!   the budget it draws from. Only a full window with no successful write
//!   abdicates — always BEFORE any standby may consider the 15s Lease expired,
//!   so the "both sides reconcile" window cannot exist. The `RENEW_PERIOD`
//!   sleep between successful rounds sits deliberately OUTSIDE the budget;
//!   folding it in is what made one 10s hiccup fatal in #319.
//! - **Loss is verified, not inferred.** A rejected renew (CAS 409) re-observes
//!   the Lease before concluding anything: if the holder is still us it was a
//!   spurious concurrent write (someone `kubectl annotate`d the Lease) and the
//!   renew is simply retried; only an observed foreign holder ends leadership.
//!
//! Lifecycle contract:
//! - [`acquire`] blocks until this replica holds the Lease. The caller starts
//!   reconcilers only after it returns — a standby replica serves probes and
//!   `/metrics` but reconciles nothing. A 403 (missing leases RBAC) **degrades
//!   to running without election** with a loud error instead of crash-looping:
//!   every already-released chart stamps `--leader-elect=true` while granting
//!   no leases RBAC, so a fatal 403 would break plain image-only upgrades —
//!   and degrading is exactly the previous release's (no-election) behavior.
//! - [`spawn_renewal`]'s task completes **only when leadership is lost**, and
//!   reports WHICH way via [`LeadershipLost`]. The caller must stop reconciling
//!   immediately either way; what it does next depends on the variant.
//!   [`LeadershipLost::ToPeer`] means a peer verifiably leads, so the only
//!   correct move is to exit. [`LeadershipLost::RenewFailed`] means we merely
//!   lost contact, so [`reconfirm`] gets one chance — inside the remaining
//!   margin, never past it — to PROVE the Lease never left our hands, and the
//!   process keeps its informer caches warm instead of paying a full cold-start
//!   re-LIST. Failing that it exits. A duplicated mover Job is a real cost; so
//!   is a restart storm that re-LISTs the whole cluster fifteen times a day.
//! - [`release`] clears the holder on graceful shutdown so the successor
//!   claims the Lease immediately instead of waiting out the full duration on
//!   every rolling upgrade.

use std::time::Duration;

// tokio's Instant, not std's: identical monotonic semantics at runtime, but
// paused-test-aware — the renewal-deadline tests drive it with virtual time.
use tokio::time::Instant;

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, ObjectMeta, PostParams};

use crate::config::{LeaderElection, LeaseTimings};

/// How long a held Lease is honored without an observed change before another
/// replica may claim it. Upstream (client-go / controller-runtime) default.
pub const LEASE_DURATION: Duration = Duration::from_secs(15);
/// The renew WINDOW: how long a leader keeps retrying a failed renew before
/// abdicating. Strictly less than [`LEASE_DURATION`] — the deposed leader must
/// stop reconciling BEFORE any standby may consider the Lease expired.
///
/// This is a budget for a whole round of attempts, NOT a single attempt's
/// timeout. See [`renew_round`] for why conflating the two is a bug.
pub const RENEW_DEADLINE: Duration = Duration::from_secs(10);
/// Cadence at which a healthy leader re-stamps `renewTime`, slept BETWEEN
/// successful rounds and deliberately outside the [`RENEW_DEADLINE`] budget.
pub const RENEW_PERIOD: Duration = Duration::from_secs(2);
/// Cadence for standby re-checks and for renew retries inside a round.
pub const RETRY_PERIOD: Duration = Duration::from_secs(2);
/// Cap on ONE renew attempt, independent of how much of the window is left.
///
/// Without it a hung attempt swallows the entire [`RENEW_DEADLINE`] budget and
/// the round makes exactly one connection attempt — useless precisely when it
/// matters, because replacing a wedged connection requires trying again.
///
/// Must exceed [`crate::config::KUBE_CLIENT_ELECTION_READ_TIMEOUT`] so a real
/// transport error wins this race: a transport error EVICTS the poisoned
/// connection from hyper's pool, whereas this timeout only drops the request
/// future and leaves the connection sitting there to swallow the retry too.
pub const RENEW_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(6);
/// Bound on one campaign attempt in [`acquire`]. Not a correctness bound the
/// way [`RENEW_DEADLINE`] is — nothing is at risk while we are not leading —
/// but without it a stalled apiserver holds the pod not-ready for the client's
/// full read timeout (minutes) instead of retrying on the standby cadence.
pub const CAMPAIGN_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the best-effort [`release`]. Deliberately short: release runs in the
/// SIGTERM path, and overrunning `terminationGracePeriodSeconds` gets the
/// process SIGKILLed with the Lease still held — the successor then waits out a
/// full [`LEASE_DURATION`], which is the exact stall release exists to prevent.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

// The no-split-brain invariant, enforced at compile time rather than left to a
// test: worst-case abdication is one full RENEW_PERIOD sleep plus one full
// RENEW_DEADLINE window, and that must land strictly inside LEASE_DURATION or a
// standby could claim the Lease while the old leader is still reconciling.
const _: () = assert!(
    RENEW_PERIOD.as_millis() + RENEW_DEADLINE.as_millis() < LEASE_DURATION.as_millis(),
    "RENEW_PERIOD + RENEW_DEADLINE must be < LEASE_DURATION (see the module docs)"
);
const _: () = assert!(
    RETRY_PERIOD.as_millis() < RENEW_DEADLINE.as_millis(),
    "RETRY_PERIOD must fit inside the renew window or a round gets one attempt"
);
const _: () = assert!(
    RENEW_ATTEMPT_TIMEOUT.as_millis() < RENEW_DEADLINE.as_millis(),
    "one attempt must not be able to consume the whole renew window"
);
const _: () = assert!(
    RENEW_ATTEMPT_TIMEOUT.as_millis()
        > crate::config::KUBE_CLIENT_ELECTION_READ_TIMEOUT.as_millis(),
    "a transport error must beat this timeout — only the former evicts the connection"
);

/// What this replica should do about the Lease it just observed. Pure output
/// of [`decide`]; the IO layer maps it onto create/replace calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The Lease is unheld or expired: claim it (bumping `leaseTransitions`).
    Claim,
    /// We already hold it: re-stamp `renewTime`.
    Renew,
    /// Someone else holds a live Lease: wait and re-check.
    Standby,
}

/// The election decision, pure over the observed Lease state so the whole
/// protocol's correctness is table-testable without a cluster.
///
/// `observed_unchanged_for` is how long this process has watched the Lease's
/// `(holder, renewTime)` stay identical, measured on OUR monotonic clock
/// ([`Observation`]) — deliberately not the holder's wall-clock `renewTime`,
/// which clock skew could make look arbitrarily stale (or fresh).
pub fn decide(
    holder: Option<&str>,
    identity: &str,
    observed_unchanged_for: Duration,
    lease_duration: Duration,
) -> Decision {
    match holder {
        Some(h) if h == identity => Decision::Renew,
        Some(_) => {
            if observed_unchanged_for > lease_duration {
                Decision::Claim
            } else {
                Decision::Standby
            }
        }
        None => Decision::Claim,
    }
}

/// The `(holder, renewTime)` view of a Lease this process last saw, plus WHEN
/// (monotonic) it last saw it change — the skew-immune expiry clock.
#[derive(Debug)]
struct Observation {
    seen: Option<(Option<String>, Option<Timestamp>)>,
    since: Instant,
}

impl Observation {
    fn new() -> Self {
        Observation {
            seen: None,
            since: Instant::now(),
        }
    }

    /// Record the current view; returns how long it has been unchanged.
    fn track(&mut self, current: (Option<String>, Option<Timestamp>)) -> Duration {
        if self.seen.as_ref() != Some(&current) {
            self.seen = Some(current);
            self.since = Instant::now();
        }
        self.since.elapsed()
    }
}

/// One decision → CAS-write attempt against an already-observed Lease.
/// `Ok(true)` iff this replica holds the Lease after the attempt. Conflicts
/// (another writer won the same race) are `Ok(false)`, not errors — the loop
/// re-observes and decides again.
async fn act_on_observation(
    api: &Api<Lease>,
    lease_name: &str,
    identity: &str,
    observed: Option<Lease>,
    observed_unchanged_for: Duration,
    timings: LeaseTimings,
) -> kube::Result<bool> {
    let now = Timestamp::now();
    match observed {
        None => {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(lease_name.to_string()),
                    ..Default::default()
                },
                spec: Some(claimed_spec(identity, now, 1, timings.lease_duration)),
            };
            match api.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(true),
                // Another replica created it between our get and create.
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                Err(e) => Err(e),
            }
        }
        Some(mut lease) => {
            let spec = lease.spec.clone().unwrap_or_default();
            let holder = spec.holder_identity.as_deref();
            match decide(
                holder,
                identity,
                observed_unchanged_for,
                timings.lease_duration,
            ) {
                Decision::Standby => Ok(false),
                Decision::Renew => {
                    let mut renewed = spec;
                    renewed.renew_time = Some(MicroTime(now));
                    lease.spec = Some(renewed);
                    match api
                        .replace(lease_name, &PostParams::default(), &lease)
                        .await
                    {
                        Ok(_) => Ok(true),
                        Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                        Err(e) => Err(e),
                    }
                }
                Decision::Claim => {
                    let transitions = spec.lease_transitions.unwrap_or(0) + 1;
                    lease.spec = Some(claimed_spec(
                        identity,
                        now,
                        transitions,
                        timings.lease_duration,
                    ));
                    match api
                        .replace(lease_name, &PostParams::default(), &lease)
                        .await
                    {
                        Ok(_) => {
                            tracing::info!(
                                lease = lease_name,
                                identity,
                                transitions,
                                "claimed leader lease"
                            );
                            Ok(true)
                        }
                        // Lost the claim race to another replica.
                        Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                        Err(e) => Err(e),
                    }
                }
            }
        }
    }
}

/// The observed `(holder, renewTime)` view used by [`Observation::track`].
fn lease_view(lease: &Option<Lease>) -> (Option<String>, Option<Timestamp>) {
    match lease.as_ref().and_then(|l| l.spec.as_ref()) {
        Some(spec) => (
            spec.holder_identity.clone(),
            spec.renew_time.as_ref().map(|t| t.0),
        ),
        None => (None, None),
    }
}

fn claimed_spec(
    identity: &str,
    now: Timestamp,
    transitions: i32,
    lease_duration: Duration,
) -> LeaseSpec {
    LeaseSpec {
        holder_identity: Some(identity.to_string()),
        // Saturating, never `as`: an unchecked cast of an oversized duration
        // wraps to a negative or unrelated value, and the Lease would then
        // advertise expiry semantics this process does not enforce.
        // `LeaseTimings::validate` already caps this well inside i32 — belt and
        // braces, because the failure is silent and the field is a safety input
        // for every OTHER client reading the Lease.
        lease_duration_seconds: Some(i32::try_from(lease_duration.as_secs()).unwrap_or(i32::MAX)),
        acquire_time: Some(MicroTime(now)),
        renew_time: Some(MicroTime(now)),
        lease_transitions: Some(transitions),
        ..Default::default()
    }
}

/// Outcome of [`acquire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    /// This replica holds the Lease; the caller must run [`spawn_renewal`].
    Leading,
    /// The operator lacks leases RBAC (403): running WITHOUT election, loudly.
    /// This is upgrade compatibility, not a feature — every already-released
    /// chart stamps `--leader-elect=true` with no leases RBAC, so a fatal 403
    /// would crash-loop plain image-only upgrades. Degrading reproduces the
    /// previous release's exact (no-election) behavior.
    Degraded,
}

/// Block until this replica holds the election Lease (or the RBAC to elect is
/// missing — see [`Acquired::Degraded`]). Other API errors are transient (API
/// server restart, network) and retried on the standby cadence.
pub async fn acquire(
    client: &Client,
    cfg: &LeaderElection,
    identity: &str,
    metrics: Option<&crate::metrics::Metrics>,
) -> Acquired {
    let api: Api<Lease> = Api::namespaced(client.clone(), &cfg.namespace);
    tracing::info!(
        lease = %cfg.lease_name,
        namespace = %cfg.namespace,
        identity,
        "leader election enabled; campaigning"
    );
    // Publish 0 while campaigning so a standby reports "not leader" rather than
    // an absent series — `absent()` cannot tell a standby from a dead pod.
    if let Some(metrics) = metrics {
        metrics.set_leader(false);
    }
    let mut observation = Observation::new();
    let mut standby_logged = false;
    loop {
        let attempt = async {
            let observed = api.get_opt(&cfg.lease_name).await?;
            let unchanged_for = observation.track(lease_view(&observed));
            act_on_observation(
                &api,
                &cfg.lease_name,
                identity,
                observed,
                unchanged_for,
                cfg.timings,
            )
            .await
        };
        // Bounded like the renew attempts: an unbounded await here inherits the
        // shared client's watch-sized read timeout, so a stalled apiserver would
        // pin a starting replica in not-ready for minutes rather than letting it
        // retry on the standby cadence.
        let outcome = match tokio::time::timeout(CAMPAIGN_TIMEOUT, attempt).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                tracing::warn!(
                    lease = %cfg.lease_name,
                    timeout_secs = CAMPAIGN_TIMEOUT.as_secs(),
                    "lease check produced no response before the attempt deadline; retrying"
                );
                tokio::time::sleep(cfg.timings.retry_period).await;
                continue;
            }
        };
        match outcome {
            Ok(true) => {
                tracing::info!(lease = %cfg.lease_name, identity, "elected leader");
                if let Some(metrics) = metrics {
                    metrics.set_leader(true);
                }
                return Acquired::Leading;
            }
            Ok(false) => {
                if !standby_logged {
                    tracing::info!(
                        lease = %cfg.lease_name,
                        "another replica leads; standing by (probes stay up, reconcilers idle)"
                    );
                    standby_logged = true;
                }
            }
            Err(kube::Error::Api(e)) if e.code == 403 => {
                tracing::error!(
                    lease = %cfg.lease_name,
                    namespace = %cfg.namespace,
                    "leader election is enabled but the operator cannot access the Lease (403): \
                     the ServiceAccount needs get/create/update on coordination.k8s.io leases \
                     (upgrade the chart, which grants this when controller.leaderElection.enabled \
                     is true, or disable leader election). RUNNING WITHOUT LEADER ELECTION — safe \
                     at one replica; at more than one, every replica reconciles concurrently"
                );
                return Acquired::Degraded;
            }
            Err(e) => {
                tracing::warn!(error = %e, lease = %cfg.lease_name, "lease check failed; retrying");
            }
        }
        tokio::time::sleep(cfg.timings.retry_period).await;
    }
}

/// Why leadership ended. The two cases have genuinely different remedies —
/// a peer is already leading (nothing to re-take) versus we simply lost contact
/// (the Lease may well still be ours) — so they are separate variants and the
/// caller matches exhaustively rather than treating "not leading" as one blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeadershipLost {
    /// The Lease verifiably no longer names us: a foreign `holderIdentity` was
    /// OBSERVED, or another replica won the create race on a deleted Lease.
    /// Someone else leads; this replica must stop and stay stopped.
    ToPeer {
        /// The observed foreign `holderIdentity`, or `None` when the Lease was
        /// recreated by a peer we never got to see.
        holder: Option<String>,
    },
    /// A full [`RENEW_DEADLINE`] window closed without a successful write. We
    /// do not know who holds the Lease — only that we can no longer PROVE we
    /// do, which is reason enough to stop reconciling.
    RenewFailed {
        /// How many attempts the round made before the window closed.
        attempts: u32,
        /// The last failure seen, for the abdication log.
        last_error: String,
        /// The instant a standby could FIRST claim this Lease: one
        /// `lease_duration` after our last successful renew, because that renew
        /// reset every observer's staleness clock.
        ///
        /// This is the hard edge of the no-split-brain invariant. Everything
        /// this replica does before it — including [`reconfirm`] — is provably
        /// exclusive; anything after it is not. Note how little is left: the
        /// renew round already spent `renew_period + renew_deadline` of the
        /// budget, so the remainder is only the const-asserted MARGIN (~3s at
        /// the defaults), not a fresh interval.
        safe_until: Instant,
    },
}

/// Outcome of one [`renew_round`].
enum RoundOutcome {
    /// `renewTime` was re-stamped; leadership continues.
    Renewed,
    Lost(LeadershipLost),
}

/// One renew round: keep attempting until the Lease is re-stamped or the
/// [`RENEW_DEADLINE`] window closes. Mirrors client-go's
/// `PollImmediateUntil(RetryPeriod, tryAcquireOrRenew, timeoutCtx(RenewDeadline))`.
///
/// **The window is a budget for the round, and each attempt is bounded by what
/// is LEFT of it.** Getting this wrong is how #319 happened: the previous
/// implementation used `RENEW_DEADLINE` as both the per-attempt timeout and the
/// abdication budget, and charged the inter-attempt sleep against that same
/// budget — so a stalled attempt tripped the timeout at `RENEW_PERIOD +
/// RENEW_DEADLINE`, which is unconditionally past the budget. The retry branch
/// was unreachable for every slow failure, and one 10-second hiccup killed the
/// process. Fast failures retried; slow ones never did.
///
/// Note what this alone does NOT fix: dropping a request future (which is all a
/// `tokio::time::timeout` does) leaves a wedged HTTP/2 connection sitting in
/// hyper's pool, so a retry would go straight back onto it. Retrying only helps
/// because the election rides a dedicated client whose short `read_timeout`
/// turns a stalled connection into a transport error, and a transport error is
/// what actually evicts the connection. See `startup.rs`.
async fn renew_round(
    api: &Api<Lease>,
    lease_name: &str,
    identity: &str,
    observation: &mut Observation,
    timings: LeaseTimings,
    safe_until: Instant,
    metrics: Option<&crate::metrics::Metrics>,
) -> RoundOutcome {
    let window_closes = Instant::now() + timings.renew_deadline;
    let mut attempts: u32 = 0;
    let mut last_error = String::from("no attempt completed");

    loop {
        let remaining = window_closes.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return RoundOutcome::Lost(LeadershipLost::RenewFailed {
                attempts,
                last_error,
                safe_until,
            });
        }
        attempts += 1;

        match renew_attempt(
            api,
            lease_name,
            identity,
            observation,
            remaining,
            timings,
            metrics,
        )
        .await
        {
            AttemptOutcome::Renewed => return RoundOutcome::Renewed,
            AttemptOutcome::LostToPeer { holder } => {
                return RoundOutcome::Lost(LeadershipLost::ToPeer { holder });
            }
            AttemptOutcome::Retryable(why) => {
                tracing::warn!(lease = %lease_name, reason = %why, "lease renew failed; retrying");
                last_error = why;
            }
        }

        // Retry cadence, clamped so a sleep can never overrun the window.
        let remaining = window_closes.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            tokio::time::sleep(timings.retry_period.min(remaining)).await;
        }
    }
}

/// What one renew attempt concluded. Separated from [`renew_round`] so the
/// per-attempt classification is a flat, exhaustive match instead of nested
/// arms inside the retry loop.
enum AttemptOutcome {
    /// `renewTime` re-stamped; the round is done.
    Renewed,
    /// The Lease verifiably names someone else — terminal, not retryable.
    LostToPeer { holder: Option<String> },
    /// Anything the round should try again inside its window, carrying the
    /// description for the abdication log.
    Retryable(String),
}

/// One bounded GET-then-CAS attempt at re-stamping the Lease, timed for
/// `kopiur_leader_renew_duration_seconds`.
async fn renew_attempt(
    api: &Api<Lease>,
    lease_name: &str,
    identity: &str,
    observation: &mut Observation,
    remaining: Duration,
    timings: LeaseTimings,
    metrics: Option<&crate::metrics::Metrics>,
) -> AttemptOutcome {
    let attempt = async {
        let observed = api.get_opt(lease_name).await?;
        let unchanged_for = observation.track(lease_view(&observed));
        let holder = observed
            .as_ref()
            .and_then(|l| l.spec.as_ref())
            .and_then(|s| s.holder_identity.clone());
        let won =
            act_on_observation(api, lease_name, identity, observed, unchanged_for, timings).await?;
        Ok::<_, kube::Error>((won, holder))
    };

    // Two bounds, and both matter. `remaining` stops an attempt outspending the
    // budget it draws from; RENEW_ATTEMPT_TIMEOUT stops a single hung attempt
    // swallowing the whole budget, which would leave the round with one
    // connection attempt and no way to replace a wedged connection.
    let bound = RENEW_ATTEMPT_TIMEOUT.min(remaining);
    let started = Instant::now();
    let outcome = tokio::time::timeout(bound, attempt).await;

    // Every attempt is timed, successful or not: a renew-latency histogram
    // creeping toward the deadline is the leading indicator #319 had no way to
    // surface. `reason` is a closed set — it is a metric label.
    let reason = match &outcome {
        Ok(Ok((true, _))) => None,
        Ok(Ok((false, _))) => Some("conflict"),
        Ok(Err(_)) => Some("transport"),
        Err(_) => Some("stalled"),
    };
    if let Some(metrics) = metrics {
        metrics.record_leader_renew(started.elapsed().as_secs_f64(), reason);
    }

    match outcome {
        Ok(Ok((true, _))) => AttemptOutcome::Renewed,
        // Rejected write. Loss is VERIFIED, not inferred: only an observed
        // foreign holder ends leadership — a CAS 409 while the Lease still names
        // us is a spurious concurrent write (someone `kubectl annotate`d it).
        Ok(Ok((false, holder))) => match holder.as_deref() {
            Some(h) if h == identity => {
                AttemptOutcome::Retryable("concurrent write while we still hold the Lease".into())
            }
            _ => AttemptOutcome::LostToPeer { holder },
        },
        Ok(Err(e)) => AttemptOutcome::Retryable(e.to_string()),
        Err(_elapsed) => AttemptOutcome::Retryable(format!(
            "no response within {}s",
            bound.as_secs_f32().round()
        )),
    }
}

/// Keep renewing the Lease we hold. The returned task completes **only when
/// leadership is lost**, and says which way (see [`LeadershipLost`]).
///
/// The caller must stop reconciling the moment this resolves — continuing
/// without the Lease is a split-brain double-reconcile.
pub fn spawn_renewal(
    client: Client,
    cfg: LeaderElection,
    identity: String,
    metrics: Option<crate::metrics::Metrics>,
) -> tokio::task::JoinHandle<LeadershipLost> {
    tokio::spawn(async move {
        let api: Api<Lease> = Api::namespaced(client, &cfg.namespace);
        let mut observation = Observation::new();
        // We were just elected, so the Lease is fresh as of now. Every observer's
        // staleness clock resets on each successful renew, so this is what the
        // "earliest a peer may claim" edge is measured from.
        let mut last_renewed = Instant::now();
        loop {
            // Between SUCCESSFUL rounds only, and deliberately OUTSIDE the
            // renew window: charging this sleep against the abdication budget
            // is precisely what made the previous implementation abdicate on
            // its first slow attempt.
            tokio::time::sleep(cfg.timings.renew_period).await;

            match renew_round(
                &api,
                &cfg.lease_name,
                &identity,
                &mut observation,
                cfg.timings,
                last_renewed + cfg.timings.lease_duration,
                metrics.as_ref(),
            )
            .await
            {
                RoundOutcome::Renewed => last_renewed = Instant::now(),
                RoundOutcome::Lost(lost) => {
                    if let Some(metrics) = &metrics {
                        metrics.set_leader(false);
                    }
                    match &lost {
                        LeadershipLost::ToPeer { holder } => tracing::error!(
                            lease = %cfg.lease_name,
                            identity = %identity,
                            holder = holder.as_deref().unwrap_or("<none>"),
                            "leader lease lost to another replica"
                        ),
                        LeadershipLost::RenewFailed {
                            attempts,
                            last_error,
                            ..
                        } => tracing::error!(
                            lease = %cfg.lease_name,
                            identity = %identity,
                            attempts,
                            window_secs = cfg.timings.renew_deadline.as_secs(),
                            last_error = %last_error,
                            "could not renew the leader lease within the renew window; abdicating"
                        ),
                    }
                    return lost;
                }
            }
        }
    })
}

/// Outcome of [`reconfirm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconfirmed {
    /// The Lease still named us and has been re-stamped. Our exclusive hold was
    /// never broken, so the reconcilers that kept running were never concurrent
    /// with a peer.
    StillOurs,
    /// Continuous ownership could NOT be proven. The caller must exit.
    Lost(String),
}

/// After a failed renew round, re-confirm that this replica **never stopped**
/// holding the Lease, and re-stamp it.
///
/// Deliberately NOT [`acquire`]. Acquire is for a process that holds nothing and
/// runs nothing, so standing by when a peer leads is correct there. On this path
/// the reconcilers are STILL RUNNING — standing by would mean reconciling
/// underneath whoever now holds the Lease, an unbounded split brain rather than
/// a transient one.
///
/// **The proof.** Any takeover must overwrite `holderIdentity`, and this replica
/// writes nothing between losing contact and this call. So observing our own
/// identity still on the Lease *proves* no peer claimed in the interval — it is
/// not an inference from replica counts or elapsed time. Identity is the pod
/// name, so two live pods cannot collide on it. Anything else — a foreign
/// holder, an unheld Lease, a deleted Lease — is unprovable and therefore fatal.
///
/// Because the proof does not depend on how many replicas exist, this is correct
/// under HA too; it needs no replica-count precondition. An earlier revision
/// gated it on a "sole replica" pod count, which was both weaker (point-in-time:
/// a peer can start the instant after it answers) and actively harmful (its GET
/// + LIST spent the very margin below).
///
/// **`safe_until` is a hard edge, not a budget to restart.** It is the instant a
/// standby could first claim — one `lease_duration` after our last SUCCESSFUL
/// renew. The failed renew round already consumed `renew_period +
/// renew_deadline` of that, so what remains is only the const-asserted margin
/// (~3s at the defaults). Measuring a fresh `lease_duration` from *now* instead
/// would run ~12s past the point the invariant protects — reconciling while a
/// peer legitimately leads, which is precisely what the whole margin exists to
/// make impossible.
pub async fn reconfirm(
    client: &Client,
    cfg: &LeaderElection,
    identity: &str,
    safe_until: Instant,
) -> Reconfirmed {
    let api: Api<Lease> = Api::namespaced(client.clone(), &cfg.namespace);
    let give_up_at = safe_until;
    let mut last_error = String::from("no attempt completed");

    loop {
        let remaining = give_up_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Reconfirmed::Lost(format!(
                "ran out of margin before the Lease could be re-reached ({last_error}); past \
                 this point a standby may legitimately claim it, so continuing to reconcile \
                 could not be proven exclusive"
            ));
        }

        let attempt = async {
            let observed = api.get_opt(&cfg.lease_name).await?;
            let Some(mut lease) = observed else {
                return Ok::<_, kube::Error>(Some(Reconfirmed::Lost(
                    "the Lease no longer exists; a peer may have recreated and claimed it"
                        .to_string(),
                )));
            };
            let mut spec = lease.spec.clone().unwrap_or_default();
            match spec.holder_identity.as_deref() {
                Some(h) if h == identity => {}
                other => {
                    return Ok(Some(Reconfirmed::Lost(format!(
                        "the Lease now names {}; it left our hands while we were out of contact",
                        other.unwrap_or("<nobody>")
                    ))));
                }
            }
            // Still ours: re-stamp under the observed resourceVersion. A 409
            // means someone wrote concurrently — re-observe rather than assume.
            spec.renew_time = Some(MicroTime(Timestamp::now()));
            lease.spec = Some(spec);
            match api
                .replace(&cfg.lease_name, &PostParams::default(), &lease)
                .await
            {
                Ok(_) => Ok(Some(Reconfirmed::StillOurs)),
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(None),
                Err(e) => Err(e),
            }
        };

        match tokio::time::timeout(RENEW_ATTEMPT_TIMEOUT.min(remaining), attempt).await {
            Ok(Ok(Some(outcome))) => return outcome,
            // 409: re-observe on the next pass.
            Ok(Ok(None)) => last_error = "concurrent write on the Lease".to_string(),
            Ok(Err(e)) => last_error = e.to_string(),
            Err(_elapsed) => last_error = "no response".to_string(),
        }

        let remaining = give_up_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            continue;
        }
        tokio::time::sleep(cfg.timings.retry_period.min(remaining)).await;
    }
}

/// Best-effort graceful release on shutdown: clear `holderIdentity` (CAS) so
/// the successor's very next poll sees an unheld Lease and claims it
/// immediately, instead of every rolling upgrade stalling reconciliation for
/// the full lease duration. Failures are logged and swallowed — the process is
/// exiting either way, and the Lease then just ages out as usual.
pub async fn release(client: &Client, cfg: &LeaderElection, identity: &str) {
    let api: Api<Lease> = Api::namespaced(client.clone(), &cfg.namespace);
    let released = async {
        let Some(mut lease) = api.get_opt(&cfg.lease_name).await? else {
            return Ok::<_, kube::Error>(false);
        };
        let holder = lease
            .spec
            .as_ref()
            .and_then(|s| s.holder_identity.as_deref());
        if holder != Some(identity) {
            return Ok(false);
        }
        let mut spec = lease.spec.clone().unwrap_or_default();
        spec.holder_identity = None;
        spec.renew_time = None;
        lease.spec = Some(spec);
        api.replace(&cfg.lease_name, &PostParams::default(), &lease)
            .await?;
        Ok(true)
    };
    match tokio::time::timeout(RELEASE_TIMEOUT, released).await {
        Ok(Ok(true)) => {
            tracing::info!(lease = %cfg.lease_name, "released leader lease on shutdown")
        }
        Ok(Ok(false)) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, lease = %cfg.lease_name, "could not release leader lease")
        }
        Err(_elapsed) => tracing::warn!(
            lease = %cfg.lease_name,
            timeout_secs = RELEASE_TIMEOUT.as_secs(),
            "releasing the leader lease timed out; leaving it to age out (the successor waits \
             out the lease duration instead of taking over immediately)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "kopiur-abc";
    const OTHER: &str = "kopiur-xyz";

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn unheld_lease_is_claimed() {
        assert_eq!(decide(None, ME, secs(0), LEASE_DURATION), Decision::Claim);
    }

    #[test]
    fn own_lease_is_renewed() {
        assert_eq!(
            decide(Some(ME), ME, secs(0), LEASE_DURATION),
            Decision::Renew
        );
        // Even one we somehow let go stale: still ours to re-stamp.
        assert_eq!(
            decide(Some(ME), ME, secs(9999), LEASE_DURATION),
            Decision::Renew
        );
    }

    #[test]
    fn fresh_foreign_lease_means_standby() {
        assert_eq!(
            decide(Some(OTHER), ME, secs(1), LEASE_DURATION),
            Decision::Standby
        );
        // Boundary: unchanged for exactly lease-duration is NOT yet expired.
        assert_eq!(
            decide(Some(OTHER), ME, LEASE_DURATION, LEASE_DURATION),
            Decision::Standby
        );
    }

    #[test]
    fn foreign_lease_unchanged_past_duration_is_claimed() {
        assert_eq!(
            decide(Some(OTHER), ME, LEASE_DURATION + secs(1), LEASE_DURATION),
            Decision::Claim
        );
    }

    #[test]
    fn expiry_is_observation_based_not_wall_clock() {
        // The skew-immunity property in one line: a foreign lease we have only
        // JUST observed is honored no matter what its wall-clock renewTime
        // claims — decide() never even sees a wall-clock timestamp.
        assert_eq!(
            decide(Some(OTHER), ME, secs(0), LEASE_DURATION),
            Decision::Standby
        );
    }

    #[test]
    fn renew_deadline_leaves_a_margin_before_lease_expiry() {
        // The no-split-brain invariant. The composed WORST CASE is the one that
        // matters and the one the old assertions missed: a leader can sleep a
        // full RENEW_PERIOD and then burn a full RENEW_DEADLINE window before
        // abdicating, so it is that SUM — not RENEW_DEADLINE alone — that has to
        // land inside LEASE_DURATION. At the old 5s RENEW_PERIOD the sum was
        // exactly 15s, i.e. zero margin against a standby's claim.
        let worst_case_abdication = RENEW_PERIOD + RENEW_DEADLINE;
        assert!(
            worst_case_abdication < LEASE_DURATION,
            "worst-case abdication {worst_case_abdication:?} must be < {LEASE_DURATION:?}"
        );
        // (also enforced at compile time by the const assertions on the
        // constants — this test states the reasoning the assertions encode.)
        assert!(RENEW_DEADLINE < LEASE_DURATION);
        assert!(RETRY_PERIOD < RENEW_DEADLINE);
        // A round must fit more than one attempt, or the window is decorative.
        assert!(
            RENEW_DEADLINE.as_secs_f64() / RETRY_PERIOD.as_secs_f64() >= 4.0,
            "the renew window must allow at least ~4 attempts"
        );
    }

    #[test]
    fn observation_tracks_changes_and_staleness() {
        let mut obs = Observation::new();
        let view_a = (Some(OTHER.to_string()), None);
        let first = obs.track(view_a.clone());
        assert!(first < secs(1), "a fresh observation starts the clock");
        // Unchanged view: the clock keeps running (not reset).
        let second = obs.track(view_a);
        assert!(second >= first);
        // Changed view (renewTime moved): the clock resets.
        let view_b = (Some(OTHER.to_string()), Some(Timestamp::now()));
        let reset = obs.track(view_b);
        assert!(reset < secs(1), "an observed change resets the clock");
    }

    // --- the IO protocol (act_on_observation / spawn_renewal) against a mock
    // API server: a `tower::service_fn` Client returning canned responses and
    // recording every request, so the CAS/409 paths — the part of an election
    // that actually prevents split-brain — are proven without a cluster. ---

    mod protocol {
        use super::*;
        use std::sync::{Arc, Mutex};

        use http::{Request, Response, StatusCode};
        use k8s_openapi::jiff::SignedDuration;
        use kube::client::Body;

        /// One recorded API call: method + a JSON view of the body (empty for GET).
        type Recorded = (String, serde_json::Value);

        /// A kube `Client` whose responses are canned per (method) and whose
        /// requests are recorded for assertion. `responses` maps the HTTP
        /// method to (status, body-JSON).
        fn mock_client(
            responses: Vec<(&'static str, StatusCode, serde_json::Value)>,
            log: Arc<Mutex<Vec<Recorded>>>,
        ) -> Client {
            let responses = Arc::new(responses);
            let svc = tower::service_fn(move |req: Request<Body>| {
                let responses = responses.clone();
                let log = log.clone();
                async move {
                    let method = req.method().as_str().to_string();
                    let bytes = http_body_util::BodyExt::collect(req.into_body())
                        .await
                        .expect("collect request body")
                        .to_bytes();
                    let body_json: serde_json::Value =
                        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                    log.lock().unwrap().push((method.clone(), body_json));
                    let (_, status, body) = responses
                        .iter()
                        .find(|(m, _, _)| *m == method)
                        .unwrap_or_else(|| panic!("unexpected {method} request in mock"));
                    let resp = Response::builder()
                        .status(*status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(body).unwrap()))
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            Client::new(svc, "test-ns")
        }

        /// Like [`mock_client`], but the responses listed for a given method are
        /// consumed IN ORDER, the last one repeating forever — and each response
        /// may be preceded by a stall.
        ///
        /// [`mock_client`] can only express a permanent condition ("every PUT
        /// 409s"). The interesting cases for a renew loop are the transient ones:
        /// a conflict that clears, or an apiserver that goes quiet for a few
        /// seconds and then answers. Those are exactly the failures the loop is
        /// supposed to ride out, and they are unrepresentable without ordering.
        fn scripted_client(
            responses: Vec<(&'static str, Duration, StatusCode, serde_json::Value)>,
            log: Arc<Mutex<Vec<Recorded>>>,
        ) -> Client {
            let responses = Arc::new(responses);
            let calls: Arc<Mutex<std::collections::HashMap<String, usize>>> =
                Arc::new(Mutex::new(std::collections::HashMap::new()));
            let svc = tower::service_fn(move |req: Request<Body>| {
                let responses = responses.clone();
                let calls = calls.clone();
                let log = log.clone();
                async move {
                    let method = req.method().as_str().to_string();
                    let bytes = http_body_util::BodyExt::collect(req.into_body())
                        .await
                        .expect("collect request body")
                        .to_bytes();
                    let body_json: serde_json::Value =
                        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                    log.lock().unwrap().push((method.clone(), body_json));

                    let for_method: Vec<_> =
                        responses.iter().filter(|(m, ..)| *m == method).collect();
                    assert!(
                        !for_method.is_empty(),
                        "unexpected {method} request in scripted mock"
                    );
                    let nth = {
                        let mut calls = calls.lock().unwrap();
                        let n = calls.entry(method).or_insert(0);
                        let this = *n;
                        *n += 1;
                        this
                    };
                    let (_, stall, status, body) = for_method[nth.min(for_method.len() - 1)];

                    if !stall.is_zero() {
                        tokio::time::sleep(*stall).await;
                    }
                    let resp = Response::builder()
                        .status(*status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(body).unwrap()))
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            Client::new(svc, "test-ns")
        }

        fn lease_json(holder: &str, renewed_secs_ago: i64, transitions: i32) -> serde_json::Value {
            let renew = Timestamp::now() - SignedDuration::from_secs(renewed_secs_ago);
            serde_json::json!({
                "apiVersion": "coordination.k8s.io/v1",
                "kind": "Lease",
                "metadata": { "name": "kopiur-leader", "namespace": "test-ns",
                              "resourceVersion": "42" },
                "spec": {
                    "holderIdentity": holder,
                    "leaseDurationSeconds": 15,
                    // Serialize through MicroTime so the wire format is exactly
                    // what the real API server emits (and kube parses).
                    "renewTime": serde_json::to_value(MicroTime(renew)).unwrap(),
                    "leaseTransitions": transitions,
                }
            })
        }

        fn status_json(code: u16, reason: &str) -> serde_json::Value {
            serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "message": reason, "reason": reason, "code": code
            })
        }

        fn lease_api(client: &Client) -> Api<Lease> {
            Api::namespaced(client.clone(), "test-ns")
        }

        fn typed_lease(v: serde_json::Value) -> Lease {
            serde_json::from_value(v).expect("lease JSON deserializes")
        }

        #[tokio::test]
        async fn renew_stamps_renew_time_and_keeps_holder_and_transitions() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let held = lease_json(ME, 9, 3);
            let client = mock_client(vec![("PUT", StatusCode::OK, held.clone())], log.clone());
            let won = act_on_observation(
                &lease_api(&client),
                "kopiur-leader",
                ME,
                Some(typed_lease(held)),
                Duration::from_secs(0),
                LeaseTimings::default(),
            )
            .await
            .expect("renew must not error");
            assert!(won, "the current holder keeps leadership on renew");

            let log = log.lock().unwrap();
            let (_, put_body) = log
                .iter()
                .find(|(m, _)| m == "PUT")
                .expect("renew must write the lease");
            let spec = &put_body["spec"];
            assert_eq!(
                spec["holderIdentity"], ME,
                "renew must not steal the holder"
            );
            assert_eq!(
                spec["leaseTransitions"], 3,
                "renew must not bump transitions"
            );
            // The renewTime must have been re-stamped (newer than the 9s-old one).
            let renewed: Timestamp = spec["renewTime"]
                .as_str()
                .expect("renewTime written")
                .parse()
                .expect("renewTime parses");
            assert!(
                Timestamp::now().duration_since(renewed) < SignedDuration::from_secs(5),
                "renewTime must be freshly stamped"
            );
        }

        #[tokio::test]
        async fn claim_of_expired_foreign_lease_bumps_transitions() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let stale = lease_json(OTHER, 60, 4);
            let client = mock_client(vec![("PUT", StatusCode::OK, stale.clone())], log.clone());
            let won = act_on_observation(
                &lease_api(&client),
                "kopiur-leader",
                ME,
                Some(typed_lease(stale)),
                // Observed unchanged past the lease duration → claimable.
                LEASE_DURATION + Duration::from_secs(1),
                LeaseTimings::default(),
            )
            .await
            .expect("claim must not error");
            assert!(won, "an expired foreign lease is claimable");

            let log = log.lock().unwrap();
            let (_, put_body) = log.iter().find(|(m, _)| m == "PUT").expect("claim writes");
            assert_eq!(put_body["spec"]["holderIdentity"], ME);
            assert_eq!(
                put_body["spec"]["leaseTransitions"], 5,
                "a takeover must record the transition"
            );
        }

        #[tokio::test]
        async fn fresh_foreign_lease_stands_by_without_writing() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(vec![], log.clone());
            let won = act_on_observation(
                &lease_api(&client),
                "kopiur-leader",
                ME,
                Some(typed_lease(lease_json(OTHER, 1, 1))),
                Duration::from_secs(1),
                LeaseTimings::default(),
            )
            .await
            .expect("standby must not error");
            assert!(!won, "a freshly-observed foreign lease is honored");
            assert!(
                log.lock().unwrap().is_empty(),
                "standby must be read-only — writing would churn the holder's lease"
            );
        }

        #[tokio::test]
        async fn create_race_409_is_standby_not_error() {
            // No lease observed; another replica creates it first; our create
            // 409s. That is a lost race (Ok(false)), never an error.
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(
                vec![(
                    "POST",
                    StatusCode::CONFLICT,
                    status_json(409, "AlreadyExists"),
                )],
                log.clone(),
            );
            let won = act_on_observation(
                &lease_api(&client),
                "kopiur-leader",
                ME,
                None,
                Duration::from_secs(0),
                LeaseTimings::default(),
            )
            .await
            .expect("a lost create race must not error");
            assert!(!won);
        }

        #[tokio::test]
        async fn cas_conflict_on_claim_is_standby_not_error() {
            // Two replicas observe the same expired lease; the loser's replace
            // (CAS on resourceVersion) 409s — Ok(false), re-observe next tick.
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(
                vec![("PUT", StatusCode::CONFLICT, status_json(409, "Conflict"))],
                log.clone(),
            );
            let won = act_on_observation(
                &lease_api(&client),
                "kopiur-leader",
                ME,
                Some(typed_lease(lease_json(OTHER, 60, 4))),
                LEASE_DURATION + Duration::from_secs(1),
                LeaseTimings::default(),
            )
            .await
            .expect("a lost CAS race must not error");
            assert!(!won);
        }

        #[tokio::test(start_paused = true)]
        async fn renewal_task_completes_when_the_lease_is_stolen() {
            // The renewal loop's contract: completion == VERIFIED loss. The
            // observed Lease names a foreign holder, so the task must end (the
            // caller exits the process on it). Paused tokio time drives the
            // RENEW_PERIOD sleep instantly.
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(
                vec![("GET", StatusCode::OK, lease_json(OTHER, 1, 7))],
                log.clone(),
            );
            let handle = spawn_renewal(client, election_cfg(), ME.to_string(), None);
            tokio::time::timeout(Duration::from_secs(60), handle)
                .await
                .expect("renewal task must complete once the lease is foreign-held")
                .expect("renewal task must not panic");
        }

        /// The edge `reconfirm` is really given in production: our last
        /// successful renew was one `RENEW_PERIOD + RENEW_DEADLINE` round ago,
        /// so only the const-asserted MARGIN is left — not a fresh interval.
        fn margin_edge() -> Instant {
            Instant::now() + (LEASE_DURATION - (RENEW_PERIOD + RENEW_DEADLINE))
        }

        fn election_cfg() -> LeaderElection {
            LeaderElection {
                lease_name: "kopiur-leader".to_string(),
                namespace: "test-ns".to_string(),
                timings: LeaseTimings::default(),
            }
        }

        #[tokio::test(start_paused = true)]
        async fn renewal_survives_a_spurious_conflict_while_still_holding() {
            // A CAS 409 while the Lease still names US (someone touched the
            // object between our GET and PUT) must NOT depose the leader — the
            // round retries inside its window and the next attempt succeeds.
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = scripted_client(
                vec![
                    ("GET", Duration::ZERO, StatusCode::OK, lease_json(ME, 1, 7)),
                    // The first PUT conflicts; every later one succeeds.
                    (
                        "PUT",
                        Duration::ZERO,
                        StatusCode::CONFLICT,
                        status_json(409, "Conflict"),
                    ),
                    ("PUT", Duration::ZERO, StatusCode::OK, lease_json(ME, 0, 7)),
                ],
                log.clone(),
            );
            let handle = spawn_renewal(client, election_cfg(), ME.to_string(), None);
            assert!(
                tokio::time::timeout(Duration::from_secs(120), handle)
                    .await
                    .is_err(),
                "a conflict that clears must not depose the leader"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn renewal_abdicates_when_conflicts_never_let_us_re_stamp() {
            // The other half of the conflict story, and a DELIBERATE behavior
            // change: an unending 409 loop is not survivable. While every CAS is
            // rejected our `renewTime` never advances, so a standby will claim
            // the Lease at LEASE_DURATION — retrying forever (the old behavior)
            // meant reconciling straight into the split brain the deadline
            // exists to prevent. Abdicate inside the window instead.
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(
                vec![
                    ("GET", StatusCode::OK, lease_json(ME, 1, 7)),
                    ("PUT", StatusCode::CONFLICT, status_json(409, "Conflict")),
                ],
                log.clone(),
            );
            let handle = spawn_renewal(client, election_cfg(), ME.to_string(), None);
            let lost = tokio::time::timeout(Duration::from_secs(60), handle)
                .await
                .expect("an unending conflict must abdicate, not spin forever")
                .expect("renewal task must not panic");
            assert!(
                matches!(lost, LeadershipLost::RenewFailed { .. }),
                "an unending conflict is a failed renew, not a verified peer takeover: {lost:?}"
            );
        }

        /// A client whose responses never arrive — a connected-but-stalled
        /// apiserver (the OOM-flap signature), as opposed to one that refuses.
        fn hanging_client() -> Client {
            let svc = tower::service_fn(move |_req: Request<Body>| async move {
                std::future::pending::<()>().await;
                unreachable!("the hanging mock never responds");
                #[allow(unreachable_code)]
                Ok::<Response<Body>, std::convert::Infallible>(
                    Response::builder().body(Body::empty()).unwrap(),
                )
            });
            Client::new(svc, "test-ns")
        }

        #[tokio::test(start_paused = true)]
        async fn renewal_survives_a_stall_shorter_than_the_renew_window() {
            // THE #319 REGRESSION TEST. A single slow-but-recoverable API call
            // must not depose the leader.
            //
            // The pre-fix loop used RENEW_DEADLINE as both the per-attempt
            // timeout and the abdication budget, and charged the inter-attempt
            // sleep against that same budget — so a stalled attempt tripped the
            // timeout at RENEW_PERIOD + RENEW_DEADLINE, which is unconditionally
            // past the budget, and the retry branch was dead code for every slow
            // failure. In production that meant ~15 process suicides a day off
            // ordinary API latency.
            //
            // The scenario, timed against the OLD constants (RENEW_PERIOD 5s,
            // one 10s attempt, budget measured from `last_ok`):
            //   t=5   attempt starts, 5s after the last success
            //   t=12  the slow call finally fails
            //         last_ok.elapsed() = 12 > RENEW_DEADLINE(10) -> ABDICATE
            //   t=14  ...the retry that would have succeeded never happened
            // and against the fixed loop (window opened at the round start,
            // per-attempt cap, sleep outside the budget):
            //   t=2   round opens, window [2,12]
            //   t=8   attempt 1 hits RENEW_ATTEMPT_TIMEOUT, still 4s of budget
            //   t=10  attempt 2 succeeds -> leadership retained
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = scripted_client(
                vec![
                    // One slow failure...
                    (
                        "GET",
                        Duration::from_secs(7),
                        StatusCode::INTERNAL_SERVER_ERROR,
                        status_json(500, "InternalError"),
                    ),
                    // ...then a healthy apiserver again.
                    ("GET", Duration::ZERO, StatusCode::OK, lease_json(ME, 1, 7)),
                    ("PUT", Duration::ZERO, StatusCode::OK, lease_json(ME, 0, 7)),
                ],
                log.clone(),
            );
            let handle = spawn_renewal(client, election_cfg(), ME.to_string(), None);
            assert!(
                tokio::time::timeout(Duration::from_secs(120), handle)
                    .await
                    .is_err(),
                "a stall shorter than the renew window must be retried, not abdicated"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn renewal_abdicates_when_the_apiserver_stalls_instead_of_hanging() {
            // regression (apiserver-outage incident): the renew ATTEMPT itself
            // had no timeout — against a connected-but-stalled apiserver the
            // `attempt.await` wedged forever, so the deadline check after it
            // never ran. Leadership was never abdicated while, in HA, a
            // standby claimed the expired Lease after 15s: split-brain
            // double-reconcile. A permanently stalled attempt must count as a
            // failed renew and abdicate when the window closes.
            //
            // The bound is TIGHT on both sides. The old 60s ceiling would have
            // sat quietly through a regression to 50s — and abdicating EARLY is
            // its own bug (it throws away leadership the window was meant to
            // protect), so the floor matters just as much.
            let start = Instant::now();
            let handle = spawn_renewal(hanging_client(), election_cfg(), ME.to_string(), None);
            let lost = tokio::time::timeout(
                RENEW_PERIOD + RENEW_DEADLINE + Duration::from_secs(1),
                handle,
            )
            .await
            .expect(
                "a stalled renew attempt must abdicate when the renew window closes, not \
                     hang leadership forever",
            )
            .expect("renewal task must not panic");
            let took = start.elapsed();
            assert!(
                took >= RENEW_DEADLINE,
                "abdicated after {took:?} — earlier than the {RENEW_DEADLINE:?} window it is \
                 supposed to spend retrying first"
            );
            assert!(
                matches!(lost, LeadershipLost::RenewFailed { .. }),
                "a stall is a failed renew, not a verified peer takeover: {lost:?}"
            );
            // And the window must have been SPENT retrying, not burned in one
            // attempt that happened to be bounded by the whole budget.
            let LeadershipLost::RenewFailed { attempts, .. } = lost else {
                unreachable!("asserted above")
            };
            assert!(
                attempts >= 2,
                "only {attempts} attempt in a {RENEW_DEADLINE:?} window — one hung attempt \
                 swallowed the whole budget, so nothing ever gets a second connection"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn reconfirm_accepts_only_a_lease_that_still_names_us() {
            // The proof the re-campaign path rests on: a takeover must overwrite
            // holderIdentity, and we write nothing while out of contact, so our
            // own identity still being there means no peer claimed.
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(
                vec![
                    ("GET", StatusCode::OK, lease_json(ME, 12, 7)),
                    ("PUT", StatusCode::OK, lease_json(ME, 0, 7)),
                ],
                log.clone(),
            );
            assert_eq!(
                reconfirm(&client, &election_cfg(), ME, margin_edge()).await,
                Reconfirmed::StillOurs
            );
            // And it must actually re-stamp, not just look: a reconfirm that
            // reads without writing leaves the Lease ageing out underneath us.
            let log = log.lock().unwrap();
            assert!(
                log.iter().any(|(m, _)| m == "PUT"),
                "reconfirm must re-stamp renewTime, not merely observe: {log:?}"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn reconfirm_gives_up_when_a_peer_holds_the_lease() {
            // THE REVIEW FIX (#324). `sole_replica` is point-in-time, so a peer
            // can start right after it answers — a rollout does exactly that.
            // The old path called `acquire`, which STANDS BY behind a peer, so
            // the deposed leader would have sat in standby while its reconcilers
            // kept running: an unbounded split brain, not a transient one.
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(
                vec![("GET", StatusCode::OK, lease_json(OTHER, 1, 8))],
                log.clone(),
            );
            let outcome = reconfirm(&client, &election_cfg(), ME, margin_edge()).await;
            assert!(
                matches!(outcome, Reconfirmed::Lost(_)),
                "a foreign holder must be fatal, never a standby: {outcome:?}"
            );
            // Read-only: it must not try to steal the Lease back.
            let log = log.lock().unwrap();
            assert!(
                !log.iter().any(|(m, _)| m == "PUT"),
                "reconfirm must never write over a peer's Lease: {log:?}"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn reconfirm_gives_up_on_an_unheld_or_absent_lease() {
            // Unheld is just as unprovable as foreign-held: someone could have
            // claimed AND released while we were blind.
            let log = Arc::new(Mutex::new(Vec::new()));
            let unheld = serde_json::json!({
                "apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
                "metadata": { "name": "kopiur-leader", "namespace": "test-ns",
                              "resourceVersion": "42" },
                "spec": { "leaseDurationSeconds": 15, "leaseTransitions": 9 }
            });
            let client = mock_client(vec![("GET", StatusCode::OK, unheld)], log.clone());
            assert!(matches!(
                reconfirm(&client, &election_cfg(), ME, margin_edge()).await,
                Reconfirmed::Lost(_)
            ));

            let client = mock_client(
                vec![("GET", StatusCode::NOT_FOUND, status_json(404, "NotFound"))],
                Arc::new(Mutex::new(Vec::new())),
            );
            assert!(matches!(
                reconfirm(&client, &election_cfg(), ME, margin_edge()).await,
                Reconfirmed::Lost(_)
            ));
        }

        #[tokio::test(start_paused = true)]
        async fn reconfirm_never_outlives_the_margin() {
            // THE SECOND REVIEW FIX (#324). An earlier revision gave reconfirm a
            // FRESH lease_duration measured from now. That runs ~12s past the
            // point a standby may legitimately claim — the const-asserted margin
            // guarantees abdication at RENEW_PERIOD + RENEW_DEADLINE precisely
            // BECAUSE a peer cannot claim until LEASE_DURATION, and restarting
            // the clock threw that guarantee away while reconcilers kept running.
            //
            // What is actually left is the margin, and nothing more.
            let margin = LEASE_DURATION - (RENEW_PERIOD + RENEW_DEADLINE);
            let start = Instant::now();
            let outcome = tokio::time::timeout(
                LEASE_DURATION,
                reconfirm(&hanging_client(), &election_cfg(), ME, start + margin),
            )
            .await
            .expect("reconfirm must be bounded, not hang while reconcilers run");
            assert!(matches!(outcome, Reconfirmed::Lost(_)), "{outcome:?}");

            let spent = start.elapsed();
            assert!(
                spent >= margin,
                "gave up after {spent:?} without spending its {margin:?} margin"
            );
            assert!(
                spent < LEASE_DURATION - (RENEW_PERIOD + RENEW_DEADLINE) + RETRY_PERIOD,
                "reconfirm ran {spent:?}, past the {margin:?} margin — a standby may already \
                 have claimed the Lease while our reconcilers were still running"
            );
        }

        #[test]
        fn the_margin_is_what_reconfirm_may_spend() {
            // Pin the relationship the fix rests on, so a future timing change
            // cannot silently hand reconfirm a budget it is not entitled to.
            let worst_case_abdication = RENEW_PERIOD + RENEW_DEADLINE;
            let margin = LEASE_DURATION - worst_case_abdication;
            assert!(
                !margin.is_zero(),
                "no margin left for reconfirm; it must then never run at all"
            );
            assert!(
                worst_case_abdication + margin <= LEASE_DURATION,
                "renew round + reconfirm must not outlive the lease duration"
            );
        }

        #[tokio::test]
        async fn release_clears_our_holder_identity() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let held = lease_json(ME, 1, 3);
            let client = mock_client(
                vec![
                    ("GET", StatusCode::OK, held.clone()),
                    ("PUT", StatusCode::OK, held),
                ],
                log.clone(),
            );
            release(&client, &election_cfg(), ME).await;
            let log = log.lock().unwrap();
            let (_, put_body) = log
                .iter()
                .find(|(m, _)| m == "PUT")
                .expect("release must write the lease");
            assert!(
                put_body["spec"]["holderIdentity"].is_null(),
                "release must clear the holder so the successor claims immediately"
            );
        }

        #[tokio::test]
        async fn release_of_a_foreign_lease_is_a_no_op() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(
                vec![("GET", StatusCode::OK, lease_json(OTHER, 1, 3))],
                log.clone(),
            );
            release(&client, &election_cfg(), ME).await;
            assert!(
                !log.lock().unwrap().iter().any(|(m, _)| m == "PUT"),
                "we must never clear a Lease another replica holds"
            );
        }
    }
}
