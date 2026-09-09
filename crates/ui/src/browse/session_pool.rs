//! Bounding what browse can ask of the cluster: a semaphore over session-pod
//! creation, single-flight so concurrent clicks on the same snapshot start one
//! pod rather than several, and per-identity plus global caps on in-flight
//! `pods/exec` calls.
//!
//! # Why there is no session cache in here
//!
//! Browse sessions are shared with `kubectl kopiur browse`: the Job is the only
//! record of one, its name is derived deterministically from the repository, and
//! either front end may create or delete it at any moment. A map of "sessions I
//! believe exist" would therefore be a map of guesses — it would hand the SPA a
//! pod name that a `kubectl kopiur session end` deleted a second earlier, and
//! nothing in this process would ever learn otherwise. Every request instead
//! re-asks the apiserver with
//! [`find_session_job`](kopiur_ops::browse::session::find_session_job), under
//! the caller's own credentials, and this type holds only the *bounds*: how many
//! pods may be starting, and how many execs may be in flight.

use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use kube::ResourceExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use kopiur_api::common::RepositoryKind;
use kopiur_ui_model::views::SessionInfo;

use crate::api::problem::{ApiError, problem};
use crate::config::{SessionLimits, WireLimits};
use crate::metrics::UiMetrics;

/// Above this many per-identity semaphores, idle ones are pruned on the next
/// acquisition.
///
/// The map is keyed by impersonated username, so without a prune a long-running
/// UI in a large org accumulates one `Semaphore` per person who ever opened a
/// directory. The threshold only decides *when* the sweep runs; what it removes
/// is exact (an entry nobody holds a permit from and which is back at full
/// capacity), so a lower or higher value changes cost, never correctness.
const IDENTITY_SEMAPHORE_PRUNE_AT: usize = 64;

/// Which browse session a request wants: one warm pod per repository, in the
/// namespace the session runs in.
///
/// The repository — not the snapshot — is the unit, because one session pod
/// holds one repository connection open and can read every snapshot in it. Two
/// people browsing two snapshots of the same repository share a pod; the same
/// repository browsed from two namespaces does not, because the session Job
/// lives beside the `Snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKey {
    /// Namespace the session Job runs in (the `Snapshot`'s namespace).
    pub namespace: String,
    /// Which repository CRD kind holds the snapshot.
    pub kind: RepositoryKind,
    /// The repository object's own namespace (`None` for `ClusterRepository`).
    pub repo_namespace: Option<String>,
    /// The repository object's name.
    pub repo_name: String,
}

/// Hashed through [`RepositoryKind::kind_str`] because `RepositoryKind` derives
/// `PartialEq`/`Eq` but not `Hash`, and the CRD kinds are not this crate's to
/// change. Hashing the same field the equality derive compares keeps the
/// `Hash`/`Eq` contract intact.
impl Hash for SessionKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.namespace.hash(state);
        self.kind.kind_str().hash(state);
        self.repo_namespace.hash(state);
        self.repo_name.hash(state);
    }
}

/// The start and exec bounds shared by every browse request.
///
/// Three independent limits, each guarding a different scarce thing:
///
/// * `starts` — how many session **pods** may be being created at once. A burst
///   of clicks across many repositories otherwise turns into a burst of Jobs.
/// * the per-key single-flight lock — how many creations may race for the *same*
///   repository: one. Without it, ten clicks on one snapshot all miss the
///   find-or-create lookup together and all try to create the same Job, and nine
///   of them get a 409 they have to unwind.
/// * `exec_global` / per-identity — how many `pods/exec` calls may be open at
///   once, in total and for one person. Each one is a websocket to the
///   apiserver and a kopia process in the session pod; a download holds its
///   permit for the whole transfer.
#[derive(Debug)]
pub struct SessionPool {
    starts: Arc<Semaphore>,
    inflight: std::sync::Mutex<HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>>,
    exec_global: Arc<Semaphore>,
    exec_per_identity: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
    /// In-flight execs, mirrored onto `kopiur_ui_exec_inflight`. Separate from
    /// the semaphore's own count because a gauge wants "how many are running",
    /// which `available_permits` only answers by subtraction from a limit the
    /// metric would then have to know.
    exec_inflight: Arc<AtomicUsize>,
    limits: SessionLimits,
}

impl SessionPool {
    /// Build the pool from the resolved `KOPIUR_UI_MAX_*` limits.
    pub fn new(limits: SessionLimits) -> Self {
        Self {
            starts: Arc::new(Semaphore::new(limits.max_starts)),
            inflight: std::sync::Mutex::new(HashMap::new()),
            exec_global: Arc::new(Semaphore::new(limits.max_exec_global)),
            exec_per_identity: std::sync::Mutex::new(HashMap::new()),
            exec_inflight: Arc::new(AtomicUsize::new(0)),
            limits,
        }
    }

    /// Find-or-create a session for one key, with at most one creation in flight
    /// per key and at most `max_starts` creations in flight overall.
    ///
    /// `existing` runs first, **under the key's lock**: that ordering is the
    /// whole point. A caller that arrives while another is starting the same
    /// session waits, and then finds the session that caller just made — so the
    /// expensive `work` future runs once per key, not once per click. `work` is
    /// only reached when `existing` answered `None`, and only while holding a
    /// `starts` permit.
    ///
    /// Returns the session and whether it was reused (`true` when `existing`
    /// produced it).
    ///
    /// # Errors
    ///
    /// Whatever `existing` or `work` returned; plus [`ApiError`] if the pool was
    /// shut down, which cannot happen — nothing closes these semaphores.
    pub async fn ensure<T, E, Look, LookFut, Work, WorkFut>(
        &self,
        key: &SessionKey,
        existing: Look,
        work: Work,
    ) -> Result<(T, bool), E>
    where
        E: From<ApiError>,
        Look: FnOnce() -> LookFut,
        LookFut: Future<Output = Result<Option<T>, E>>,
        Work: FnOnce() -> WorkFut,
        WorkFut: Future<Output = Result<T, E>>,
    {
        let lock = self.key_lock(key);
        let _guard = lock.lock().await;

        if let Some(found) = existing().await? {
            return Ok((found, true));
        }

        let _permit = self
            .starts
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| E::from(pool_closed("session starts")))?;
        let started = work().await?;
        Ok((started, false))
    }

    /// The per-key lock, created on first use.
    ///
    /// Entries are never removed: a key is a `(namespace, repository)` pair, so
    /// the map is bounded by the repositories in the cluster rather than by
    /// traffic, and each entry is a `Mutex<()>` behind an `Arc`. Dropping them
    /// would need refcount bookkeeping to avoid handing two callers two
    /// different locks for one key — the exact race the map exists to prevent.
    fn key_lock(&self, key: &SessionKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.lock_inflight();
        map.entry(key.clone()).or_default().clone()
    }

    /// Take one in-flight `pods/exec` slot for `user`, or refuse.
    ///
    /// Both caps are `try_acquire`, never a wait: a browse request that queued
    /// behind four downloads would hold an HTTP connection and a `kube::Client`
    /// for as long as they run, and the honest answer to "the exec budget is
    /// full" is 429 with a retry, not a socket held open for ten minutes.
    ///
    /// The global permit is taken first so that a request refused by the global
    /// cap does not consume — and immediately release — a per-identity slot.
    ///
    /// # Errors
    ///
    /// A 429 `urn:kopiur:problem:too-many-requests` naming which cap was hit.
    pub fn exec_permit(
        &self,
        user: &str,
        metrics: &Arc<UiMetrics>,
    ) -> Result<ExecPermit, ApiError> {
        let global = self
            .exec_global
            .clone()
            .try_acquire_owned()
            .map_err(|e| exec_busy(e, "kopiur-ui as a whole", self.limits.max_exec_global))?;

        let per_user = self.identity_semaphore(user);
        let user_permit = per_user
            .try_acquire_owned()
            .map_err(|e| exec_busy(e, user, self.limits.max_exec_per_identity))?;

        let inflight = self.exec_inflight.fetch_add(1, Ordering::SeqCst) + 1;
        metrics.set_exec_inflight(inflight);

        Ok(ExecPermit {
            _global: global,
            _user: user_permit,
            inflight: self.exec_inflight.clone(),
            metrics: metrics.clone(),
        })
    }

    /// This identity's exec semaphore, created on first use and swept when the
    /// map grows past [`IDENTITY_SEMAPHORE_PRUNE_AT`].
    fn identity_semaphore(&self, user: &str) -> Arc<Semaphore> {
        let mut map = self.lock_identities();
        if map.len() > IDENTITY_SEMAPHORE_PRUNE_AT {
            let max = self.limits.max_exec_per_identity;
            // `strong_count == 1` means the map holds the only handle: an
            // `OwnedSemaphorePermit` keeps its own `Arc`, so nobody can be
            // mid-exec against a semaphore this drops.
            map.retain(|_, sem| Arc::strong_count(sem) > 1 || sem.available_permits() < max);
        }
        map.entry(user.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.limits.max_exec_per_identity)))
            .clone()
    }

    /// Recover the single-flight map from a poisoned lock.
    ///
    /// The guarded value is a plain `HashMap` of `Arc`s, so a panic elsewhere
    /// cannot have left it half-updated in a way that matters; refusing to serve
    /// browse for the rest of the process's life would be the worse failure.
    fn lock_inflight(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>> {
        self.inflight.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Recover the per-identity semaphore map from a poisoned lock. See
    /// [`SessionPool::lock_inflight`].
    fn lock_identities(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Semaphore>>> {
        self.exec_per_identity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for SessionPool {
    /// A pool with the shipped default limits.
    ///
    /// Exists so `AppState` can be built in tests and before `main` has resolved
    /// the configuration. `main` constructs the real one with
    /// [`SessionPool::new`] from [`crate::config::UiConfig`].
    fn default() -> Self {
        Self::new(SessionLimits {
            ttl: Duration::from_secs(900),
            ready_timeout: Duration::from_secs(300),
            max_starts: crate::config::DEFAULT_MAX_SESSION_STARTS,
            max_exec_per_identity: crate::config::DEFAULT_MAX_EXEC_PER_IDENTITY,
            max_exec_global: crate::config::DEFAULT_MAX_EXEC_GLOBAL,
        })
    }
}

/// One in-flight `pods/exec` slot, released on drop.
///
/// A download hands this to the task writing the response body, so the slot is
/// held for the whole transfer rather than for the call that started it — which
/// is the point: what the cap bounds is concurrent kopia processes and
/// apiserver websockets, and a streaming download owns both until its last byte.
pub struct ExecPermit {
    _global: OwnedSemaphorePermit,
    _user: OwnedSemaphorePermit,
    inflight: Arc<AtomicUsize>,
    metrics: Arc<UiMetrics>,
}

/// Hand-written because [`UiMetrics`] holds OpenTelemetry instruments and is not
/// `Debug`. The counter is the only interesting part anyway; the two permits are
/// opaque tokens.
impl std::fmt::Debug for ExecPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecPermit")
            .field("inflight", &self.inflight.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Drop for ExecPermit {
    fn drop(&mut self) {
        let remaining = self
            .inflight
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1);
        self.metrics.set_exec_inflight(remaining);
    }
}

/// 429 for an exhausted exec cap, naming the cap that was hit.
fn exec_busy(error: TryAcquireError, who: &str, limit: usize) -> ApiError {
    match error {
        TryAcquireError::NoPermits => problem(
            429,
            "too-many-requests",
            format!("Too many browse reads are already running for {who}."),
            format!(
                "kopiur-ui allows {limit} concurrent pods/exec calls there. Each one is a \
                 websocket to the apiserver and a kopia process in the session pod, and a \
                 file download holds its slot until the last byte."
            ),
            "wait for a listing or download in another tab to finish, then retry; raise \
             KOPIUR_UI_MAX_EXEC_PER_IDENTITY or KOPIUR_UI_MAX_EXEC_GLOBAL if this is normal \
             for your cluster",
        ),
        TryAcquireError::Closed => pool_closed("browse execs"),
    }
}

/// A semaphore was closed, which nothing in kopiur-ui does — so this is a bug,
/// reported as one rather than silently letting the bound lapse.
fn pool_closed(what: &str) -> ApiError {
    problem(
        500,
        "internal",
        format!("kopiur-ui's limit on {what} is no longer usable."),
        "The semaphore guarding it was closed, which nothing in kopiur-ui does. This is a \
         bug, not a problem with the request.",
        "restart kopiur-ui and report this at \
         https://github.com/home-operations/kopiur/issues with the UI's logs",
    )
}

/// Describe a session Job on the wire.
///
/// `expires_at` is read off **the Job itself** — `creationTimestamp +
/// spec.activeDeadlineSeconds` — not off this process's configuration. The
/// difference is not academic: a session started with `{"ttlSeconds": 120}`, or
/// one started by `kubectl kopiur browse` with its own TTL, or one left over
/// from a deployment whose `KOPIUR_UI_SESSION_TTL` has since changed, all have
/// lifetimes the running config knows nothing about. The SPA renders this as a
/// countdown, so a number taken from the wrong place counts down past the actual
/// death of the pod.
///
/// `activeDeadlineSeconds` is the kubelet-enforced hard stop rather than the
/// mover's own idle TTL (which is ~2 minutes shorter — see `JobLimits` in
/// `kopiur_ops::browse::session`). That direction is the safe one for a
/// countdown: it is the last moment the session could still be alive.
///
/// `None` when the Job carries no creation timestamp or no deadline — a guessed
/// expiry is worse than an absent one.
/// `limits` are the deployment's own caps, published so the SPA can refuse an
/// oversized download before navigating into a problem document it cannot read.
/// See [`WireLimits`].
pub fn session_info(job: &Job, reused: bool, limits: WireLimits) -> SessionInfo {
    SessionInfo {
        namespace: job.namespace().unwrap_or_default(),
        job: job.name_any(),
        pod: None,
        reused,
        expires_at: session_expiry(job),
        download_max_bytes: limits.download_max_bytes,
        manifest_max_bytes: limits.manifest_max_bytes,
    }
}

/// **Pure.** `creationTimestamp + spec.activeDeadlineSeconds`, as RFC 3339.
fn session_expiry(job: &Job) -> Option<String> {
    let created = kopiur_ops::snapshots::meta_time(job.metadata.creation_timestamp.as_ref()?)?;
    let deadline = job.spec.as_ref()?.active_deadline_seconds?;
    created
        .checked_add_signed(chrono::Duration::try_seconds(deadline)?)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// The read path's rule for an attach that may have created a session.
///
/// `GET …/tree` and `GET …/file` check for a live session Job and then attach to
/// it, and those are two apiserver calls. In between, the Job can go away — a
/// `kubectl kopiur session end`, or, far more ordinarily, the session simply
/// reaching the end of its life, which is a normal event that needs no human at
/// all. The attach is `ExecSession::ensure`, which is find-or-**create**, so
/// losing that race silently starts a mover pod from a `GET`.
///
/// This is the undo. `reused` comes from comparing the attached Job's UID with
/// the one the pre-check saw: when they differ, the attach created a Job, so
/// `undo` deletes it and the caller gets exactly the 409 it would have got had
/// the pre-check lost the race by a millisecond. The result is that a `GET`
/// never leaves a pod running, under any interleaving.
///
/// Generic over the session payload so the branch is testable: `ExecSession` has
/// no public constructor, so a test that had to build one could not reach here.
pub async fn reuse_or_undo<S, U, Fut>(
    attached: S,
    reused: bool,
    job: &str,
    refusal: ApiError,
    undo: U,
) -> Result<S, ApiError>
where
    U: FnOnce(String) -> Fut,
    Fut: Future<Output = ()>,
{
    if reused {
        return Ok(attached);
    }
    tracing::info!(
        job,
        "a read attached to a browse session that had just been replaced; deleting the \
         session it started, because a GET must never leave a mover pod running"
    );
    undo(job.to_string()).await;
    Err(refusal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    use kopiur_telemetry::MetricsProvider;

    fn metrics() -> Arc<UiMetrics> {
        Arc::new(UiMetrics::new(Arc::new(MetricsProvider::new(
            "kopiur-ui-test",
        ))))
    }

    fn limits(max_starts: usize, per_identity: usize, global: usize) -> SessionLimits {
        SessionLimits {
            ttl: Duration::from_secs(900),
            ready_timeout: Duration::from_secs(300),
            max_starts,
            max_exec_per_identity: per_identity,
            max_exec_global: global,
        }
    }

    fn key(namespace: &str, repo: &str) -> SessionKey {
        SessionKey {
            namespace: namespace.to_string(),
            kind: RepositoryKind::Repository,
            repo_namespace: Some(namespace.to_string()),
            repo_name: repo.to_string(),
        }
    }

    fn job(
        name: &str,
        namespace: &str,
        created: Option<&str>,
        active_deadline_seconds: Option<i64>,
    ) -> Job {
        let mut value = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": { "name": name, "namespace": namespace },
            "spec": { "template": { "spec": { "containers": [], "restartPolicy": "Never" } } },
        });
        if let Some(created) = created {
            value["metadata"]["creationTimestamp"] = serde_json::json!(created);
        }
        if let Some(deadline) = active_deadline_seconds {
            value["spec"]["activeDeadlineSeconds"] = serde_json::json!(deadline);
        }
        serde_json::from_value(value).expect("job fixture")
    }

    #[test]
    fn a_session_key_hashes_and_compares_on_every_field() {
        use std::collections::HashSet;

        let base = key("media", "nas");
        let mut other_kind = base.clone();
        other_kind.kind = RepositoryKind::ClusterRepository;
        let mut other_ns = base.clone();
        other_ns.namespace = "other".to_string();
        let mut other_repo_ns = base.clone();
        other_repo_ns.repo_namespace = None;
        let mut other_name = base.clone();
        other_name.repo_name = "tank".to_string();

        let set: HashSet<SessionKey> = [
            base.clone(),
            base.clone(),
            other_kind,
            other_ns,
            other_repo_ns,
            other_name,
        ]
        .into_iter()
        .collect();
        assert_eq!(
            set.len(),
            5,
            "the duplicate collapses; every distinguishing field must not"
        );
    }

    #[tokio::test]
    async fn concurrent_ensures_for_one_key_run_the_start_once() {
        // The behaviour the whole type exists for: ten clicks on one snapshot
        // must create one pod. The second caller is serialized behind the first
        // and then FINDS what the first made, so `work` runs once.
        let pool = Arc::new(SessionPool::new(limits(4, 4, 64)));
        let started = Arc::new(AtomicU32::new(0));
        let live = Arc::new(std::sync::Mutex::new(None::<u32>));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            let started = started.clone();
            let live = live.clone();
            tasks.push(tokio::spawn(async move {
                pool.ensure::<u32, ApiError, _, _, _, _>(
                    &key("media", "nas"),
                    || async {
                        let found = *live.lock().expect("test lock");
                        Ok(found)
                    },
                    || async {
                        // A real start yields; without the key lock every task
                        // would be inside here at once.
                        tokio::task::yield_now().await;
                        let id = started.fetch_add(1, Ordering::SeqCst) + 1;
                        *live.lock().expect("test lock") = Some(id);
                        Ok(id)
                    },
                )
                .await
                .expect("ensure")
            }));
        }

        let mut reused = 0;
        for task in tasks {
            let (id, was_reused) = task.await.expect("task");
            assert_eq!(
                id, 1,
                "every caller ends up on the one session that started"
            );
            reused += usize::from(was_reused);
        }
        assert_eq!(started.load(Ordering::SeqCst), 1, "one start, not eight");
        assert_eq!(reused, 7, "the other seven reused it");
    }

    #[tokio::test]
    async fn different_keys_do_not_serialize_behind_each_other() {
        let pool = SessionPool::new(limits(4, 4, 64));
        let started = Arc::new(AtomicU32::new(0));

        for repo in ["nas", "tank"] {
            let started = started.clone();
            let (_id, reused) = pool
                .ensure::<u32, ApiError, _, _, _, _>(
                    &key("media", repo),
                    || async { Ok(None) },
                    || async { Ok(started.fetch_add(1, Ordering::SeqCst) + 1) },
                )
                .await
                .expect("ensure");
            assert!(!reused, "{repo} had no session to reuse");
        }
        assert_eq!(
            started.load(Ordering::SeqCst),
            2,
            "two repositories are two sessions"
        );
    }

    #[tokio::test]
    async fn an_ensure_error_does_not_wedge_the_key() {
        let pool = SessionPool::new(limits(1, 4, 64));
        let failed: Result<(u32, bool), ApiError> = pool
            .ensure(
                &key("media", "nas"),
                || async { Ok(None) },
                || async { Err(pool_closed("a deliberate test failure")) },
            )
            .await;
        assert!(failed.is_err());

        // The key lock and the single start permit must both have been released.
        let (id, reused) = pool
            .ensure::<u32, ApiError, _, _, _, _>(
                &key("media", "nas"),
                || async { Ok(None) },
                || async { Ok(7) },
            )
            .await
            .expect("the second attempt still runs");
        assert_eq!((id, reused), (7, false));
    }

    #[test]
    fn the_exec_caps_refuse_with_429_and_recover_when_a_permit_drops() {
        let metrics = metrics();
        let pool = SessionPool::new(limits(4, 2, 3));

        let a1 = pool.exec_permit("alice", &metrics).expect("alice's first");
        let a2 = pool.exec_permit("alice", &metrics).expect("alice's second");

        let refused = pool
            .exec_permit("alice", &metrics)
            .expect_err("alice is at her cap");
        assert_eq!(refused.status(), 429);
        assert_eq!(refused.0.r#type, "urn:kopiur:problem:too-many-requests");
        assert!(refused.0.what.contains("alice"), "{:?}", refused.0.what);
        assert!(
            refused.0.fix.contains("KOPIUR_UI_MAX_EXEC_PER_IDENTITY"),
            "{:?}",
            refused.0.fix
        );

        // Another identity still has room until the global cap is reached.
        let b1 = pool.exec_permit("bob", &metrics).expect("bob's first");
        let global = pool
            .exec_permit("carol", &metrics)
            .expect_err("the global cap of 3 is full");
        assert_eq!(global.status(), 429);
        assert!(
            global.0.what.contains("kopiur-ui as a whole"),
            "{:?}",
            global.0.what
        );

        drop(a1);
        pool.exec_permit("alice", &metrics)
            .expect("a released permit restores capacity");
        drop((a2, b1));
    }

    #[test]
    fn the_inflight_gauge_tracks_permits_taken_and_released() {
        let metrics = metrics();
        let pool = SessionPool::new(limits(4, 4, 8));

        let one = pool.exec_permit("alice", &metrics).expect("permit");
        assert_eq!(pool.exec_inflight.load(Ordering::SeqCst), 1);
        let two = pool.exec_permit("bob", &metrics).expect("permit");
        assert_eq!(pool.exec_inflight.load(Ordering::SeqCst), 2);
        drop(two);
        assert_eq!(pool.exec_inflight.load(Ordering::SeqCst), 1);
        drop(one);
        assert_eq!(pool.exec_inflight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn idle_identity_semaphores_are_pruned_but_busy_ones_survive() {
        let metrics = metrics();
        let pool = SessionPool::new(limits(4, 4, 4096));

        let held = pool
            .exec_permit("alice", &metrics)
            .expect("alice keeps hers");
        for i in 0..(IDENTITY_SEMAPHORE_PRUNE_AT + 2) {
            drop(
                pool.exec_permit(&format!("user-{i}"), &metrics)
                    .expect("permit"),
            );
        }

        let map = pool.lock_identities();
        assert!(
            map.len() <= IDENTITY_SEMAPHORE_PRUNE_AT + 1,
            "the sweep ran: {} entries",
            map.len()
        );
        assert!(
            map.contains_key("alice"),
            "an identity holding a permit is never swept"
        );
        drop(map);
        drop(held);
    }

    /// Deliberately not the defaults: a session publishes the caps this
    /// deployment runs with.
    fn test_limits() -> WireLimits {
        WireLimits {
            download_max_bytes: 7_000,
            manifest_max_bytes: 900,
        }
    }

    #[test]
    fn session_info_expiry_comes_from_the_jobs_own_deadline() {
        // 1020s = a 900s TTL plus the 120s connect backstop `JobLimits` adds.
        let info = session_info(
            &job(
                "kopiur-browse-nas-0badc0de",
                "media",
                Some("2026-06-11T01:02:03Z"),
                Some(1020),
            ),
            true,
            test_limits(),
        );
        assert_eq!(info.namespace, "media");
        assert_eq!(info.job, "kopiur-browse-nas-0badc0de");
        assert!(info.reused);
        assert_eq!(info.pod, None);
        assert_eq!(info.expires_at.as_deref(), Some("2026-06-11T01:19:03Z"));

        // A session started with a SHORT ttl reports its own short expiry, even
        // though this process's configured TTL is 900s. Reading the expiry off
        // the running config instead is exactly the bug this pins: the SPA would
        // count down 15 minutes for a session that dies in two.
        let short = session_info(
            &job("j", "media", Some("2026-06-11T01:02:03Z"), Some(240)),
            true,
            test_limits(),
        );
        assert_eq!(short.expires_at.as_deref(), Some("2026-06-11T01:06:03Z"));

        // Missing either half is no expiry rather than a guess the SPA would
        // count down from.
        for (created, deadline) in [
            (None, Some(1020)),
            (Some("2026-06-11T01:02:03Z"), None),
            (None, None),
        ] {
            let info = session_info(&job("j", "media", created, deadline), false, test_limits());
            assert_eq!(info.expires_at, None, "{created:?}/{deadline:?}");
            assert!(!info.reused);
        }
    }

    /// The caps the SPA pre-checks against are the ones the server will enforce
    /// — carried through verbatim, never re-derived from a default. Publishing a
    /// default here while the server refused at a configured value would have
    /// the file table enable a file the download then rejects, into a problem
    /// document a top-level navigation renders as raw JSON.
    #[test]
    fn a_session_publishes_the_deployments_own_download_caps() {
        let info = session_info(
            &job("j", "media", Some("2026-06-11T01:02:03Z"), Some(1020)),
            true,
            test_limits(),
        );
        assert_eq!(info.download_max_bytes, 7_000);
        assert_eq!(info.manifest_max_bytes, 900);
        assert_ne!(
            info.download_max_bytes,
            crate::config::DEFAULT_MAX_DOWNLOAD_BYTES as i64,
            "the fixture must differ from the default, or this proves nothing"
        );
    }

    #[tokio::test]
    async fn a_read_that_accidentally_started_a_session_undoes_it_and_refuses() {
        // The GET-never-creates guarantee, at the exact interleaving that used
        // to break it: the pre-check saw a session, the attach landed on a
        // DIFFERENT Job (the old one expired between the two calls), so the
        // attach created one. Driven through the pool so the single-flight lock
        // and the starts permit are on the path too.
        let pool = SessionPool::new(limits(4, 4, 64));
        let deleted: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();

        let recorder = deleted.clone();
        let result: Result<(&str, bool), ApiError> = pool
            .ensure(
                &key("media", "nas"),
                || async {
                    reuse_or_undo(
                        "session",
                        false, // the UIDs differed: this attach created the Job
                        "kopiur-browse-nas-0badc0de",
                        problem(409, "session-required", "no session", "", "start one"),
                        |name| async move {
                            recorder.lock().expect("test lock").push(name);
                        },
                    )
                    .await
                    .map(Some)
                },
                || async { unreachable!("the existing branch already refused") },
            )
            .await;

        let error = result.expect_err("a read must not create a session");
        assert_eq!(error.status(), 409);
        assert_eq!(error.0.r#type, "urn:kopiur:problem:session-required");
        assert_eq!(
            deleted.lock().expect("test lock").as_slice(),
            ["kopiur-browse-nas-0badc0de"],
            "the Job the attach created must be deleted, not left running"
        );
    }

    #[tokio::test]
    async fn a_read_that_attached_to_the_session_it_found_is_served() {
        let undone: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let recorder = undone.clone();
        let served = reuse_or_undo(
            "session",
            true, // the UIDs matched: this is the session the pre-check saw
            "kopiur-browse-nas-0badc0de",
            problem(409, "session-required", "no session", "", "start one"),
            |name| async move {
                recorder.lock().expect("test lock").push(name);
            },
        )
        .await
        .expect("a reused session is served");

        assert_eq!(served, "session");
        assert!(
            undone.lock().expect("test lock").is_empty(),
            "nothing was created, so nothing may be deleted"
        );
    }
}
