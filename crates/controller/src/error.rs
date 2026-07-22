//! Controller error type and the kind-aware `error_policy` (ADR §5.2).
//!
//! Errors are classified [`ErrorClass::Transient`] (kopia subprocess, API
//! server, webhook outage — short retry) or [`ErrorClass::Structural`] (a bug
//! in the CRD/our logic — long retry, clamped at 5 min). The generic
//! [`error_policy_for`] logs, increments `controller_reconcile_errors_total`, and
//! requeues accordingly. Each per-CRD controller wires it via a tiny closure
//! that supplies the `kind` label.

use std::time::Duration;

use kube::runtime::controller::Action;

use crate::context::Context;

/// Backoff for transient errors (kopia / API server / webhook). ADR §5.2.
pub const TRANSIENT_BACKOFF: Duration = Duration::from_secs(30);
/// Maximum backoff for structural errors, clamped per ADR §5.2.
pub const STRUCTURAL_BACKOFF: Duration = Duration::from_secs(300);
/// Heartbeat interval for *terminal* errors — a permanent failure (e.g. filesystem
/// `PermissionDenied`, wrong repository password) that will not succeed on retry
/// without a spec change. We do NOT use a pure [`Action::await_change`] because the
/// kube maintainers warn it can miss updates on a watch desync; instead we requeue
/// on a long, deliberately quiet interval. The reconciler's terminal gate makes the
/// wake a no-op (it re-checks `observedGeneration` and returns without backend IO or
/// an error log) until the spec actually changes.
pub const TERMINAL_HEARTBEAT: Duration = Duration::from_secs(1800);

/// All errors a reconciler can surface. The exhaustive `class()` mapping is
/// what drives requeue timing — a new variant must be classified to compile.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A `kube::Client` API call failed (GET/PATCH/CREATE/DELETE). Transient.
    #[error("kube api error: {0}")]
    Kube(#[from] kube::Error),

    /// A kopia subprocess invocation failed. Transient (repo may be offline).
    #[error("kopia error: {0}")]
    Kopia(#[from] kopiur_kopia::KopiaError),

    /// Defensive re-validation (via `api::validate`) found a spec problem.
    /// Structural: the object will not reconcile until the user fixes it.
    #[error("validation error: {0}")]
    Validation(String),

    /// A required referenced object (Repository, SnapshotPolicy, …) was not
    /// found. Transient: it may appear shortly (GitOps apply ordering).
    #[error("missing dependency: {0}")]
    MissingDependency(String),

    /// Blocked on a grant an admin applies out-of-band on ANOTHER object (e.g.
    /// the namespace `privileged-movers` opt-in annotation). The granting object
    /// is watched, so the grant re-enqueues the blocked CR the moment it lands —
    /// the requeue is only a watch-desync backstop, so it runs on the slow
    /// structural cadence instead of hot-looping every 30s until a human acts.
    #[error("blocked on an out-of-band grant: {0}")]
    BlockedOnGrant(String),

    /// JSON (de)serialization of a spec/status/work-spec failed. Structural.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The mover Job builder refused ([`kopiur_mover::jobs::BuildJobError`]):
    /// either the work spec can't be JSON-encoded (structural) or it exceeds
    /// what a pod env var can carry (a pathological recipe — terminal until the
    /// spec shrinks, so it maps to Validation semantics via the error message).
    #[error("mover Job build failed: {0}")]
    BuildJob(#[from] kopiur_mover::jobs::BuildJobError),

    /// A cron expression failed to parse at scheduling time. Structural.
    #[error("invalid schedule: {0}")]
    InvalidSchedule(String),

    /// An object lacked a field the reconciler requires (e.g. `.metadata.name`).
    /// Structural.
    #[error("invariant violated: {0}")]
    Invariant(String),

    /// Self-managed webhook TLS setup failed on the **cluster IO** side: reading/
    /// writing the serving Secret or injecting `caBundle` into a webhook
    /// configuration ([`crate::webhook_tls`]). Transient — the webhook config or
    /// namespace may not exist yet at boot, and the periodic reconcile retries.
    /// Never fatal to the controller (degrade-not-crash): admission simply stays
    /// untrusted until it succeeds.
    ///
    /// Deliberately a `String`: each call site wraps a `kube::Error` with its own
    /// what/why context (which Secret, which configuration), and that composed
    /// message is the useful payload — a `{context, source}` struct would add
    /// ceremony without adding information. Failures from the pure cert-minting
    /// layer are the typed [`Error::WebhookCert`] instead.
    #[error("webhook TLS setup failed: {0}")]
    WebhookSetup(String),

    /// The pure cert-minting layer failed to produce the CA or serving leaf
    /// ([`crate::webhook_tls::CertError`]). Transient like [`Error::WebhookSetup`]
    /// (the periodic reconcile retries; admission stays untrusted, the controller
    /// never crashes), but typed so the `rcgen` source chain stays inspectable.
    #[error("webhook TLS setup failed: could not mint or resolve the CA/serving certificate: {0}")]
    WebhookCert(#[from] crate::webhook_tls::CertError),
}

/// How a reconcile error should be re-driven — the classification that picks the
/// requeue behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Short retry (kopia/API/webhook outage, missing dependency, a *retryable*
    /// kopia class).
    Transient,
    /// Long retry, clamped (a spec/logic bug needing human intervention).
    Structural,
    /// A permanent failure that will not succeed on retry without a spec change
    /// (a *non-retryable* kopia class: `PermissionDenied`, `AuthFailure`,
    /// `AccessDenied`, `NotFound`, `Unknown`). Re-driven only on a long quiet
    /// heartbeat so we stop hammering the backend and flooding the logs.
    Terminal,
}

impl ErrorClass {
    /// The metric label string.
    pub fn label(self) -> &'static str {
        match self {
            ErrorClass::Transient => "transient",
            ErrorClass::Structural => "structural",
            ErrorClass::Terminal => "terminal",
        }
    }

    /// The requeue [`Action`] for this class. kube applies the returned delay
    /// verbatim (no implicit backoff), so this is the single source of cadence.
    pub fn action(self) -> Action {
        match self {
            ErrorClass::Transient => Action::requeue(TRANSIENT_BACKOFF),
            ErrorClass::Structural => Action::requeue(STRUCTURAL_BACKOFF),
            ErrorClass::Terminal => Action::requeue(TERMINAL_HEARTBEAT),
        }
    }
}

impl Error {
    /// Classify this error. Exhaustive `match` — a new variant cannot compile
    /// until it is given a class (the type-safety thesis, ADR §5.5).
    ///
    /// A kopia error is split on its own retry hint
    /// ([`kopiur_kopia::KopiaErrorClass::is_retryable`]): a transient backend blip
    /// (unreachable, locked) is [`Transient`](ErrorClass::Transient) and worth a
    /// 30 s retry, but a permanent failure (permission denied, wrong password) is
    /// [`Terminal`](ErrorClass::Terminal) — retrying it on a tight loop only spams.
    pub fn class(&self) -> ErrorClass {
        match self {
            Error::Kube(_)
            | Error::MissingDependency(_)
            | Error::WebhookSetup(_)
            | Error::WebhookCert(_) => ErrorClass::Transient,
            Error::Kopia(e) => {
                if e.class().is_retryable() {
                    ErrorClass::Transient
                } else {
                    ErrorClass::Terminal
                }
            }
            Error::Validation(_)
            | Error::BlockedOnGrant(_)
            | Error::Serialization(_)
            | Error::BuildJob(_)
            | Error::InvalidSchedule(_)
            | Error::Invariant(_) => ErrorClass::Structural,
        }
    }

    /// Whether publishing a failure Event for this error is futile because the
    /// SAME kube client the Recorder rides cannot currently complete requests.
    ///
    /// During an apiserver outage every reconcile fails with a transport error
    /// at once; publishing a Warning about "the API server is unreachable" TO
    /// the unreachable API server only opens another doomed socket per failure
    /// (one of the fd-exhaustion amplifiers in the apiserver-outage incident).
    /// Everything that is NOT a kube transport failure keeps publishing — the
    /// Event is the user-visible breadcrumb, and for those the client works.
    /// Exhaustive `match` (ADR §5.5): a new variant cannot compile unclassified.
    pub fn event_publish_futile(&self) -> bool {
        match self {
            Error::Kube(e) => kube_error_is_transport(e),
            Error::Kopia(_)
            | Error::Validation(_)
            | Error::MissingDependency(_)
            | Error::BlockedOnGrant(_)
            | Error::Serialization(_)
            | Error::BuildJob(_)
            | Error::InvalidSchedule(_)
            | Error::Invariant(_)
            | Error::WebhookSetup(_)
            | Error::WebhookCert(_) => false,
        }
    }
}

/// Deterministic per-object transient requeue in
/// [[`TRANSIENT_BACKOFF`], 2×[`TRANSIENT_BACKOFF`]), seeded by a stable FNV
/// hash of `kind/namespace/name` ([`kopiur_api::jitter_offset`] — the repo's
/// croner-H convention: NO RNG, identical across restarts and HA replicas).
///
/// Why: a flat 30s requeue re-synchronizes every object that failed together
/// (an apiserver outage fails them ALL together) into one 30s retry wave.
/// Spreading OBJECTS across the window — rather than re-rolling per retry —
/// breaks the wave while keeping a given object's cadence stable and
/// debuggable. Scope honesty: watch-driven re-triggers (e.g. the re-list after
/// a watcher reconnects) override any requeue because the scheduler keeps the
/// EARLIER deadline, so this de-synchronizes steady-state REPEAT failures; the
/// recovery stampede itself is bounded by the reconcile-concurrency cap.
/// Structural/Terminal cadences stay fixed: structural failures wait on a
/// human spec fix (they don't correlate into waves) and terminal wakes are
/// no-op heartbeats behind the terminal gate.
pub(crate) fn jittered_transient_requeue(
    kind: &str,
    namespace: Option<&str>,
    name: &str,
) -> Duration {
    let seed = format!("{kind}/{}/{name}", namespace.unwrap_or(""));
    // slot_start_unix = 0: there is no schedule slot here; the seed alone
    // spreads objects. Whole-second resolution → 30 buckets over [30s, 60s).
    TRANSIENT_BACKOFF + kopiur_api::jitter_offset(&seed, 0, TRANSIENT_BACKOFF)
}

/// Transport/client-infrastructure failures, where an Event POST would ride
/// the same broken path the failing call just used. Exhaustive over the
/// `kube::Error` variants compiled under this workspace's kube features
/// (`client`, `rustls-tls`, `ws`) — `kube::Error` is not `#[non_exhaustive]`,
/// so a kube bump that adds a variant forces a classification decision here
/// instead of silently defaulting.
fn kube_error_is_transport(err: &kube::Error) -> bool {
    match err {
        // Futile: the client cannot complete ANY request right now — a dead or
        // unreachable endpoint (`Service` is the incident's
        // "ServiceError: client error (Connect)"), a connection that died
        // mid-stream, an unusable TLS/auth/config layer.
        kube::Error::Service(_)
        | kube::Error::HyperError(_)
        | kube::Error::ReadEvents(_)
        | kube::Error::RustlsTls(_)
        | kube::Error::TlsRequired
        | kube::Error::Auth(_)
        | kube::Error::InferConfig(_)
        | kube::Error::InferKubeconfig(_)
        | kube::Error::ProxyProtocolUnsupported { .. }
        | kube::Error::ProxyProtocolDisabled { .. } => true,
        // Publishable: the apiserver ANSWERED (`Api` — any code, including a
        // 429/500/503 from a half-alive control plane) or the failure is local
        // to THIS request's payload/URL — the transport itself works, so the
        // Warning Event both can and should reach the user.
        kube::Error::Api(_)
        | kube::Error::FromUtf8(_)
        | kube::Error::LinesCodecMaxLineLengthExceeded
        | kube::Error::HttpError(_)
        | kube::Error::SerdeError(_)
        | kube::Error::BuildRequest(_)
        | kube::Error::Discovery(_)
        | kube::Error::UpgradeConnection(_) => false,
    }
}

/// Result alias for reconcile functions.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Shared `error_policy` body: log, record the metric, surface the failure as
/// a Warning Event on the failing object, and requeue by class.
///
/// Each controller passes its CRD `kind` label so the metric is correctly
/// attributed, and the object so the Event has a `regarding` reference. This
/// keeps one classification/backoff/visibility policy across all reconcilers
/// (ADR §5.2): every kind's failures are visible in `kubectl get events`, not
/// only the ones with bespoke in-reconcile publishes.
///
/// The Event publish is fire-and-forget on the runtime (`error_policy` is
/// sync) and best-effort; repeats of the same failure aggregate into a single
/// Event object via the Recorder's dedup (see [`crate::io::event_ref`] for why
/// the reference must not carry a `resourceVersion`). Outside a tokio runtime
/// (pure unit tests) the publish is skipped — degrade, never panic.
pub fn error_policy_for<K>(kind: &str, obj: &K, err: &Error, ctx: &Context) -> Action
where
    K: kube::Resource<DynamicType = ()>,
{
    let class = err.class();
    ctx.metrics.record_error(kind, class.label());
    tracing::warn!(
        kind = kind,
        class = class.label(),
        error = %err,
        "reconcile error; requeueing"
    );
    if err.event_publish_futile() {
        // The client's transport is down: an Event POST would only open
        // another doomed socket against the same dead endpoint. Count the
        // drop so an outage is visible on /metrics even without Events.
        ctx.metrics.record_failure_event_dropped("transport");
        tracing::debug!(
            kind = kind,
            "skipping failure Event: kube transport error — the publish would ride the same \
             dead connection"
        );
    } else {
        let event = crate::io::reconcile_failure_event(err, crate::io::operator_uid());
        let regarding = crate::io::event_ref(obj);
        crate::io::try_spawn_failure_publish(
            ctx.metrics.clone(),
            ctx.recorder.clone(),
            regarding,
            event,
        );
    }
    // Exhaustive on class (no `_ =>`): Transient gets the per-object jitter,
    // the other cadences stay exactly `class.action()` — see
    // [`jittered_transient_requeue`] for why only Transient is spread.
    match class {
        ErrorClass::Transient => {
            let meta = obj.meta();
            Action::requeue(jittered_transient_requeue(
                kind,
                meta.namespace.as_deref(),
                meta.name.as_deref().unwrap_or(""),
            ))
        }
        ErrorClass::Structural | ErrorClass::Terminal => class.action(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use kopiur_kopia::{KopiaError, KopiaErrorClass};

    #[test]
    fn kube_and_missing_are_transient() {
        assert_eq!(
            Error::MissingDependency("repo".into()).class(),
            ErrorClass::Transient
        );
    }

    #[test]
    fn blocked_on_grant_is_structural_not_a_hot_loop() {
        // Regression: the privileged-mover namespace opt-in used to be
        // MissingDependency (Transient), so a blocked Snapshot re-checked —
        // and re-logged the refusal — every 30s, forever. The Namespace watch
        // now delivers the grant immediately; the requeue is a slow backstop.
        let err = Error::BlockedOnGrant("namespace `app` has not opted in".into());
        assert_eq!(err.class(), ErrorClass::Structural);
        assert!(err.to_string().contains("out-of-band grant"));
        assert!(err.to_string().contains("namespace `app` has not opted in"));
    }

    #[test]
    fn webhook_setup_is_transient() {
        // Webhook-TLS setup retries (the config/namespace may not exist yet at
        // boot); it must never hard-stop the controller.
        assert_eq!(
            Error::WebhookSetup("no such config".into()).class(),
            ErrorClass::Transient
        );
    }

    #[test]
    fn webhook_cert_is_transient_and_keeps_the_source_chain() {
        // The typed cert-minting failure classifies exactly like WebhookSetup
        // (retry, degrade-not-crash) but preserves the rcgen source error.
        let err = Error::WebhookCert(crate::webhook_tls::CertError::Generate(
            rcgen::Error::CouldNotParseCertificate,
        ));
        assert_eq!(err.class(), ErrorClass::Transient);
        assert!(err.to_string().contains("could not mint"));
        assert!(
            std::error::Error::source(&err).is_some(),
            "the CertError source chain must stay inspectable"
        );
    }

    #[test]
    fn kopia_class_follows_retryability() {
        // A retryable kopia class (backend unreachable / locked) is Transient —
        // worth a 30 s retry.
        let retryable = KopiaError::NonZeroExit {
            args: "repository connect".into(),
            code: Some(1),
            class: KopiaErrorClass::RepositoryUnavailable,
            stderr_tail: "dial tcp: connection refused".into(),
        };
        assert_eq!(Error::Kopia(retryable).class(), ErrorClass::Transient);

        // A non-retryable kopia class (filesystem permission denied — the reported
        // bug) is Terminal: hard-stop, do NOT requeue on a tight loop.
        let terminal = KopiaError::NonZeroExit {
            args: "repository connect".into(),
            code: Some(1),
            class: KopiaErrorClass::PermissionDenied,
            stderr_tail: "open /repo/.shards.tmp.deadbeef: permission denied".into(),
        };
        assert_eq!(Error::Kopia(terminal).class(), ErrorClass::Terminal);

        // EmptyOutput maps to Unknown, which is non-retryable → Terminal.
        let unknown = KopiaError::EmptyOutput {
            context: "x".into(),
        };
        assert_eq!(Error::Kopia(unknown).class(), ErrorClass::Terminal);
    }

    #[test]
    fn validation_and_schedule_are_structural() {
        assert_eq!(
            Error::Validation("bad".into()).class(),
            ErrorClass::Structural
        );
        assert_eq!(
            Error::InvalidSchedule("nope".into()).class(),
            ErrorClass::Structural
        );
        assert_eq!(
            Error::Invariant("no name".into()).class(),
            ErrorClass::Structural
        );
    }

    // NOTE: a typo'd KOPIUR_HTTP_ADDR is no longer an `Error` variant — the
    // address is validated by the clap value parser at startup, before any
    // reconciler exists. Its actionable-message contract is asserted in
    // `crate::config::tests::http_addr_invalid_value_fails_loudly_with_an_actionable_message`.

    #[test]
    fn action_cadence_per_class() {
        assert_eq!(
            ErrorClass::Transient.action(),
            Action::requeue(Duration::from_secs(30))
        );
        assert_eq!(
            ErrorClass::Structural.action(),
            Action::requeue(Duration::from_secs(300))
        );
        // The whole point of the fix: a terminal error does NOT requeue at the
        // transient 30 s cadence — it goes quiet on the 30 min heartbeat.
        let terminal = ErrorClass::Terminal.action();
        assert_eq!(terminal, Action::requeue(Duration::from_secs(1800)));
        assert_ne!(terminal, Action::requeue(Duration::from_secs(30)));
    }

    // --- transient requeue jitter (apiserver-outage incident): a flat 30s
    // requeue re-synchronized every failed object into one retry wave. The
    // jitter is DETERMINISTIC (FNV — the croner-H convention: no RNG, stable
    // across restarts) and spread per OBJECT across [30s, 60s), so a given
    // object's cadence stays debuggable while the fleet de-synchronizes.
    // Scope honesty: watch-driven re-triggers (recovery re-lists) override any
    // requeue — the scheduler keeps the EARLIER deadline — so this shapes
    // steady-state REPEAT failures; the recovery stampede is bounded by the
    // reconcile-concurrency cap, not by jitter. ---

    #[test]
    fn transient_requeue_is_jittered_within_one_to_two_backoffs() {
        for (kind, ns, name) in [
            ("Snapshot", Some("media"), "qbittorrent-b2-20260721"),
            ("SnapshotSchedule", Some("default"), "paperless-b2"),
            ("SnapshotPolicy", Some("media"), "bazarr-b2"),
            ("ClusterRepository", None, "nas"),
        ] {
            let d = jittered_transient_requeue(kind, ns, name);
            assert!(
                d >= TRANSIENT_BACKOFF && d < TRANSIENT_BACKOFF * 2,
                "{kind}/{ns:?}/{name}: {d:?} must land in [30s, 60s)"
            );
        }
    }

    #[test]
    fn transient_jitter_is_deterministic_per_object() {
        // HA-replica/restart stability, same as croner-H: identical inputs →
        // identical requeue, every time.
        let a = jittered_transient_requeue("Snapshot", Some("media"), "seerr");
        let b = jittered_transient_requeue("Snapshot", Some("media"), "seerr");
        assert_eq!(a, b);
    }

    #[test]
    fn transient_jitter_spreads_across_objects() {
        let distinct: std::collections::BTreeSet<Duration> = (0..100)
            .map(|i| jittered_transient_requeue("Snapshot", Some("media"), &format!("app-{i}")))
            .collect();
        assert!(
            distinct.len() > 10,
            "100 objects must spread across well over 10 of the 30 one-second \
             buckets, got {}",
            distinct.len()
        );
    }

    // --- regression (apiserver-outage EMFILE fix): `error_policy_for` used to
    // fire-and-forget a Warning-Event POST for EVERY failed reconcile — even
    // when the failure itself was the kube client's transport being down, so
    // each failure spawned another socket-opening request against the dead
    // apiserver. Transport-level failures must suppress the (futile) publish;
    // API-level answers (403/429/503 from a live apiserver) must keep it. ---

    #[test]
    fn transport_level_kube_errors_suppress_the_failure_event() {
        // The incident signature: `ServiceError: client error (Connect)`.
        let connect = Error::Kube(kube::Error::Service("client error (Connect)".into()));
        assert!(connect.event_publish_futile());
        // A connection that died mid-stream is the same broken transport.
        let read = Error::Kube(kube::Error::ReadEvents(std::io::Error::other(
            "connection reset by peer",
        )));
        assert!(read.event_publish_futile());
    }

    #[test]
    fn api_level_kube_errors_still_publish() {
        // The apiserver ANSWERED — an Event publish is possible and valuable
        // (during a half-alive apiserver these are exactly the breadcrumbs a
        // user greps for). 403: RBAC gap; 503: overloaded but alive.
        for code in [403u16, 429, 503] {
            let err = Error::Kube(kube::Error::Api(Box::new(kube::core::Status {
                code,
                ..Default::default()
            })));
            assert!(
                !err.event_publish_futile(),
                "Api({code}) must keep publishing"
            );
        }
    }

    #[test]
    fn non_kube_errors_always_publish() {
        // Everything that is not a kube transport failure reached the error
        // policy over a WORKING client — the Event is the user-visible surface.
        for err in [
            Error::MissingDependency("repo `x` not found".into()),
            Error::Validation("bad spec".into()),
            Error::BlockedOnGrant("namespace not opted in".into()),
            Error::InvalidSchedule("bad cron".into()),
            Error::Invariant("no name".into()),
            Error::WebhookSetup("no config".into()),
        ] {
            assert!(!err.event_publish_futile(), "{err} must keep publishing");
        }
        let kopia = Error::Kopia(KopiaError::NonZeroExit {
            args: "repository connect".into(),
            code: Some(1),
            class: KopiaErrorClass::RepositoryUnavailable,
            stderr_tail: "dial tcp: connection refused".into(),
        });
        assert!(!kopia.event_publish_futile());
    }

    // -- the policy itself, against a request-recording mock client --

    use std::sync::{Arc, Mutex};

    use http::{Request, Response, StatusCode};
    use kube::Client;
    use kube::client::Body;

    /// Recorded API call: HTTP method + path.
    type Recorded = (String, String);

    /// A kube `Client` that records every request and answers POSTs by echoing
    /// the posted body back (a created Event parses as itself), everything
    /// else with 200 `{}`.
    fn recording_client(log: Arc<Mutex<Vec<Recorded>>>) -> Client {
        let svc = tower::service_fn(move |req: Request<Body>| {
            let log = log.clone();
            async move {
                let method = req.method().as_str().to_string();
                let path = req.uri().path().to_string();
                let bytes = http_body_util::BodyExt::collect(req.into_body())
                    .await
                    .expect("collect request body")
                    .to_bytes();
                log.lock().unwrap().push((method.clone(), path));
                let (status, body) = if method == "POST" {
                    (StatusCode::CREATED, bytes.to_vec())
                } else {
                    (StatusCode::OK, b"{}".to_vec())
                };
                let resp = Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        Client::new(svc, "test-ns")
    }

    fn test_obj(name: &str) -> k8s_openapi::api::core::v1::ConfigMap {
        k8s_openapi::api::core::v1::ConfigMap {
            metadata: kube::core::ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some("test-ns".to_string()),
                uid: Some("00000000-0000-0000-0000-000000000000".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Let the (current-thread) runtime drive any spawned publish to
    /// completion; deterministic — if nothing was spawned, nothing can run.
    async fn drain_spawned(log: &Arc<Mutex<Vec<Recorded>>>) {
        for _ in 0..64 {
            if !log.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn error_policy_skips_event_post_for_transport_errors() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let ctx = crate::context::Context::test_context(recording_client(log.clone()));
        let err = Error::Kube(kube::Error::Service("client error (Connect)".into()));
        let _ = error_policy_for("Snapshot", &test_obj("storm-a"), &err, &ctx);
        drain_spawned(&log).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "a transport-level failure must not spawn an Event POST over the same dead transport"
        );
    }

    #[tokio::test]
    async fn error_policy_publishes_event_post_for_api_errors() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let ctx = crate::context::Context::test_context(recording_client(log.clone()));
        let err = Error::Kube(kube::Error::Api(Box::new(kube::core::Status {
            code: 503,
            ..Default::default()
        })));
        let _ = error_policy_for("Snapshot", &test_obj("alive-a"), &err, &ctx);
        drain_spawned(&log).await;
        let log = log.lock().unwrap();
        assert_eq!(
            log.len(),
            1,
            "an API-level failure must publish exactly one Warning Event; got {log:?}"
        );
        assert_eq!(log[0].0, "POST");
        assert!(log[0].1.contains("/events"), "{}", log[0].1);
    }

    #[tokio::test]
    async fn error_policy_transient_action_differs_across_objects() {
        // THE wave-breaking regression test: two different objects failing
        // transiently at the same instant must NOT be requeued for the same
        // instant again. (Before the fix both got a flat 30s.)
        let ctx = crate::context::Context::test_context(recording_client(Arc::new(Mutex::new(
            Vec::new(),
        ))));
        // A transport error, which also skips the Event publish — the Action
        // is the entire observable output here.
        let err = Error::Kube(kube::Error::Service("client error (Connect)".into()));
        let a = error_policy_for("Snapshot", &test_obj("qbittorrent-b2"), &err, &ctx);
        let b = error_policy_for("Snapshot", &test_obj("photos-b2"), &err, &ctx);
        assert_ne!(
            a, b,
            "two objects' transient requeues must land in different jitter buckets"
        );
    }

    #[tokio::test]
    async fn error_policy_structural_and_terminal_cadence_unchanged() {
        // Jitter is a Transient-only concern: structural failures need a human
        // spec fix (they don't correlate into waves) and terminal wakes are
        // no-op heartbeats behind the terminal gate.
        let ctx = crate::context::Context::test_context(recording_client(Arc::new(Mutex::new(
            Vec::new(),
        ))));
        let structural = error_policy_for(
            "SnapshotPolicy",
            &test_obj("bad-spec"),
            &Error::Validation("bad".into()),
            &ctx,
        );
        assert_eq!(structural, Action::requeue(STRUCTURAL_BACKOFF));
        let terminal = error_policy_for(
            "SnapshotPolicy",
            &test_obj("perm-denied"),
            &Error::Kopia(KopiaError::NonZeroExit {
                args: "repository connect".into(),
                code: Some(1),
                class: KopiaErrorClass::PermissionDenied,
                stderr_tail: "permission denied".into(),
            }),
            &ctx,
        );
        assert_eq!(terminal, Action::requeue(TERMINAL_HEARTBEAT));
    }
}
