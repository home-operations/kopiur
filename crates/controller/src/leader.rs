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
//! - **Renew deadline < lease duration.** A leader that cannot renew abdicates
//!   once [`RENEW_DEADLINE`] (10s) passes without a successful write — always
//!   BEFORE any standby may consider the 15s Lease expired, so the "both sides
//!   reconcile" window cannot exist. Renew attempts retry on the short
//!   [`RETRY_PERIOD`] within that deadline, so one transient blip never
//!   abdicates.
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
//! - [`spawn_renewal`]'s task completes **only when leadership is lost**; the
//!   caller treats that as fatal and exits (restart re-enters the election).
//!   Failing fast beats a split-brain double-reconcile: a duplicated mover Job
//!   is a real cost, a pod restart is not.
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

use crate::config::LeaderElection;

/// How long a held Lease is honored without an observed change before another
/// replica may claim it. Upstream (client-go / controller-runtime) default.
pub const LEASE_DURATION: Duration = Duration::from_secs(15);
/// How long the leader tolerates failed renews before abdicating. Strictly
/// less than [`LEASE_DURATION`]: the deposed leader must stop reconciling
/// BEFORE any standby may consider the Lease expired.
pub const RENEW_DEADLINE: Duration = Duration::from_secs(10);
/// Cadence at which a healthy leader re-stamps `renewTime`. Comfortably inside
/// [`RENEW_DEADLINE`] so a single missed tick still leaves retry room.
pub const RENEW_PERIOD: Duration = Duration::from_secs(5);
/// Cadence for standby re-checks and for renew retries after a failure.
pub const RETRY_PERIOD: Duration = Duration::from_secs(2);

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
) -> kube::Result<bool> {
    let now = Timestamp::now();
    match observed {
        None => {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(lease_name.to_string()),
                    ..Default::default()
                },
                spec: Some(claimed_spec(identity, now, 1)),
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
            match decide(holder, identity, observed_unchanged_for, LEASE_DURATION) {
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
                    lease.spec = Some(claimed_spec(identity, now, transitions));
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

fn claimed_spec(identity: &str, now: Timestamp, transitions: i32) -> LeaseSpec {
    LeaseSpec {
        holder_identity: Some(identity.to_string()),
        lease_duration_seconds: Some(LEASE_DURATION.as_secs() as i32),
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
pub async fn acquire(client: &Client, cfg: &LeaderElection, identity: &str) -> Acquired {
    let api: Api<Lease> = Api::namespaced(client.clone(), &cfg.namespace);
    tracing::info!(
        lease = %cfg.lease_name,
        namespace = %cfg.namespace,
        identity,
        "leader election enabled; campaigning"
    );
    let mut observation = Observation::new();
    let mut standby_logged = false;
    loop {
        let attempt = async {
            let observed = api.get_opt(&cfg.lease_name).await?;
            let unchanged_for = observation.track(lease_view(&observed));
            act_on_observation(&api, &cfg.lease_name, identity, observed, unchanged_for).await
        };
        match attempt.await {
            Ok(true) => {
                tracing::info!(lease = %cfg.lease_name, identity, "elected leader");
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
        tokio::time::sleep(RETRY_PERIOD).await;
    }
}

/// Keep renewing the Lease we hold. The returned task completes **only when
/// leadership is lost** — a verified foreign holder, or [`RENEW_DEADLINE`]
/// elapsing without a successful renew (we can no longer prove we are sole
/// leader, and must stop BEFORE the 15s Lease can expire for anyone else).
/// The caller must treat completion as fatal (exit and re-elect on restart) —
/// continuing to reconcile without the Lease is a split-brain.
pub fn spawn_renewal(
    client: Client,
    cfg: LeaderElection,
    identity: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let api: Api<Lease> = Api::namespaced(client, &cfg.namespace);
        let mut observation = Observation::new();
        let mut last_ok = Instant::now();
        let mut delay = RENEW_PERIOD;
        loop {
            tokio::time::sleep(delay).await;
            let attempt = async {
                let observed = api.get_opt(&cfg.lease_name).await?;
                let unchanged_for = observation.track(lease_view(&observed));
                let holder = observed
                    .as_ref()
                    .and_then(|l| l.spec.as_ref())
                    .and_then(|s| s.holder_identity.clone());
                let won =
                    act_on_observation(&api, &cfg.lease_name, &identity, observed, unchanged_for)
                        .await?;
                Ok::<_, kube::Error>((won, holder))
            };
            // The attempt itself is deadline-bounded: against a
            // connected-but-STALLED apiserver (the OOM-flap signature) an
            // unbounded await would wedge here forever — never reaching the
            // deadline check below — while in HA a standby claims the expired
            // Lease after LEASE_DURATION: split-brain double-reconcile. A
            // stalled attempt is a failed renew; by the time the timeout fires
            // (sleep + RENEW_DEADLINE since last_ok) the deadline has
            // necessarily passed, so the Err arm abdicates.
            let attempt = tokio::time::timeout(RENEW_DEADLINE, attempt);
            match attempt.await.unwrap_or_else(|_elapsed| {
                Err(kube::Error::Service(
                    format!(
                        "lease renew attempt stalled past the renew deadline ({}s) — the API \
                         server accepted the connection but never answered",
                        RENEW_DEADLINE.as_secs()
                    )
                    .into(),
                ))
            }) {
                Ok((true, _)) => {
                    last_ok = Instant::now();
                    delay = RENEW_PERIOD;
                }
                // Rejected write. Loss is VERIFIED, not inferred: only an
                // observed foreign holder ends leadership — a CAS 409 while the
                // Lease still names us is a spurious concurrent write (e.g. a
                // `kubectl annotate` on the Lease) and the renew just retries.
                Ok((false, holder)) => match holder.as_deref() {
                    Some(h) if h == identity => {
                        tracing::warn!(
                            lease = %cfg.lease_name,
                            "lease renew hit a concurrent write while we still hold it; retrying"
                        );
                        delay = RETRY_PERIOD;
                    }
                    _ => {
                        tracing::error!(
                            lease = %cfg.lease_name,
                            identity = %identity,
                            holder = holder.as_deref().unwrap_or("<none>"),
                            "leader lease lost to another replica"
                        );
                        return;
                    }
                },
                Err(e) => {
                    // Transient renew failures are tolerated only inside the
                    // renew deadline; past it we abdicate — strictly before any
                    // standby may consider the (longer) lease duration expired.
                    if last_ok.elapsed() > RENEW_DEADLINE {
                        tracing::error!(
                            error = %e,
                            lease = %cfg.lease_name,
                            "could not renew the leader lease within the renew deadline; \
                             abdicating"
                        );
                        return;
                    }
                    tracing::warn!(error = %e, lease = %cfg.lease_name, "lease renew failed; retrying");
                    delay = RETRY_PERIOD;
                }
            }
        }
    })
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
    match released.await {
        Ok(true) => tracing::info!(lease = %cfg.lease_name, "released leader lease on shutdown"),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(error = %e, lease = %cfg.lease_name, "could not release leader lease")
        }
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
        // The no-split-brain invariant: a leader abdicates (RENEW_DEADLINE)
        // strictly before any standby may claim (LEASE_DURATION), with room
        // for at least one retry cycle in between.
        assert!(RENEW_DEADLINE < LEASE_DURATION);
        assert!(RENEW_DEADLINE.as_secs() + RETRY_PERIOD.as_secs() <= LEASE_DURATION.as_secs());
        assert!(RENEW_PERIOD < RENEW_DEADLINE);
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
            let handle = spawn_renewal(
                client,
                LeaderElection {
                    lease_name: "kopiur-leader".to_string(),
                    namespace: "test-ns".to_string(),
                },
                ME.to_string(),
            );
            tokio::time::timeout(Duration::from_secs(60), handle)
                .await
                .expect("renewal task must complete once the lease is foreign-held")
                .expect("renewal task must not panic");
        }

        #[tokio::test(start_paused = true)]
        async fn renewal_survives_a_spurious_conflict_while_still_holding() {
            // A CAS 409 while the Lease still names US (someone touched the
            // object between our GET and PUT) must NOT depose the leader — the
            // loop retries. Proven by the task still running after several
            // renew cycles of GET→409.
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = mock_client(
                vec![
                    ("GET", StatusCode::OK, lease_json(ME, 1, 7)),
                    ("PUT", StatusCode::CONFLICT, status_json(409, "Conflict")),
                ],
                log.clone(),
            );
            let handle = spawn_renewal(
                client,
                LeaderElection {
                    lease_name: "kopiur-leader".to_string(),
                    namespace: "test-ns".to_string(),
                },
                ME.to_string(),
            );
            let outcome = tokio::time::timeout(Duration::from_secs(120), handle).await;
            assert!(
                outcome.is_err(),
                "the renewal task must keep retrying (not abdicate) while the Lease names us"
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
        async fn renewal_abdicates_when_the_apiserver_stalls_instead_of_hanging() {
            // regression (apiserver-outage incident): the renew ATTEMPT itself
            // had no timeout — against a connected-but-stalled apiserver the
            // `attempt.await` wedged forever, so the deadline check after it
            // never ran. Leadership was never abdicated while, in HA, a
            // standby claimed the expired Lease after 15s: split-brain
            // double-reconcile. A stalled attempt must count as a failed renew
            // and abdicate at the deadline.
            let handle = spawn_renewal(
                hanging_client(),
                LeaderElection {
                    lease_name: "kopiur-leader".to_string(),
                    namespace: "test-ns".to_string(),
                },
                ME.to_string(),
            );
            tokio::time::timeout(Duration::from_secs(60), handle)
                .await
                .expect(
                    "a stalled renew attempt must abdicate at the renew deadline, not hang \
                     leadership forever",
                )
                .expect("renewal task must not panic");
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
            release(
                &client,
                &LeaderElection {
                    lease_name: "kopiur-leader".to_string(),
                    namespace: "test-ns".to_string(),
                },
                ME,
            )
            .await;
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
            release(
                &client,
                &LeaderElection {
                    lease_name: "kopiur-leader".to_string(),
                    namespace: "test-ns".to_string(),
                },
                ME,
            )
            .await;
            assert!(
                !log.lock().unwrap().iter().any(|(m, _)| m == "PUT"),
                "we must never clear a Lease another replica holds"
            );
        }
    }
}
