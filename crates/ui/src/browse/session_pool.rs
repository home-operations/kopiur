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
use crate::config::SessionLimits;
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
/// `expires_at` is the Job's creation time plus the TTL the session was started
/// with, because that is what actually reaps it: the mover idles for its TTL and
/// exits, and the Job's `activeDeadlineSeconds` backstops it. It is `None` when
/// the Job carries no creation timestamp, which only a hand-made object does —
/// a guessed expiry would be worse than an absent one, since the SPA renders it
/// as a countdown.
pub fn session_info(job: &Job, ttl: Duration, reused: bool) -> SessionInfo {
    let expires_at = job
        .metadata
        .creation_timestamp
        .as_ref()
        .and_then(kopiur_ops::snapshots::meta_time)
        .and_then(|created| {
            chrono::Duration::from_std(ttl)
                .ok()
                .and_then(|d| created.checked_add_signed(d))
        })
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    SessionInfo {
        namespace: job.namespace().unwrap_or_default(),
        job: job.name_any(),
        pod: None,
        reused,
        expires_at,
    }
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

    fn job(name: &str, namespace: &str, created: Option<&str>) -> Job {
        let mut value = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": { "name": name, "namespace": namespace },
        });
        if let Some(created) = created {
            value["metadata"]["creationTimestamp"] = serde_json::json!(created);
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

    #[test]
    fn session_info_expiry_is_creation_plus_ttl() {
        let info = session_info(
            &job(
                "kopiur-browse-nas-0badc0de",
                "media",
                Some("2026-06-11T01:02:03Z"),
            ),
            Duration::from_secs(900),
            true,
        );
        assert_eq!(info.namespace, "media");
        assert_eq!(info.job, "kopiur-browse-nas-0badc0de");
        assert!(info.reused);
        assert_eq!(info.pod, None);
        assert_eq!(info.expires_at.as_deref(), Some("2026-06-11T01:17:03Z"));

        // A Job with no creation timestamp reports no expiry rather than a
        // guess the SPA would count down from.
        let none = session_info(&job("j", "media", None), Duration::from_secs(900), false);
        assert_eq!(none.expires_at, None);
        assert!(!none.reused);
    }
}
