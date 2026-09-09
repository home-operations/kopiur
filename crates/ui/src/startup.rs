//! The parts of startup that are decisions rather than IO.
//!
//! `main` is a binary crate, so anything that lives there is untestable by
//! construction — and two of the things that lived there are the readiness state
//! machine and the list of resources the UI must be allowed to impersonate,
//! which are exactly the parts where being wrong is silent. A wrong
//! `userextras/<key>` fan-out fails every request from a proxy that asserts
//! extras; a readiness machine that stops following its channel leaves `/readyz`
//! answering `ok` over a frozen cache.
//!
//! So they live here instead. What stays in `main` is the IO around them: build
//! the client, spawn the tasks, hand the results to these functions.

use std::sync::Arc;

use tokio::sync::watch;

use crate::cache::stores::CacheHealth;
use crate::config::{CACHE_SYNC_TIMEOUT, UiConfig};
use crate::ops_listener::{CacheState, Readiness};

/// API group of the `impersonate` verb's resources (`users`, `groups`,
/// `userextras/<key>`). The empty string is the core group.
pub const CORE_GROUP: &str = "";

/// The two resources the UI must be able to `impersonate` for anything at all to
/// work. Configured `userextras/<key>` subjects are checked alongside them.
pub const IMPERSONATION_RESOURCES: [&str; 2] = ["users", "groups"];

/// Every resource the UI must be able to impersonate for this configuration.
///
/// `users` and `groups` always; plus `userextras/<key>` for each configured
/// extra key, because a proxy that asserts an extra the UI may not impersonate
/// fails every request — and fails with a message pointing at the wrong thing.
#[must_use]
pub fn impersonation_targets(cfg: &UiConfig) -> Vec<String> {
    IMPERSONATION_RESOURCES
        .iter()
        .map(|resource| (*resource).to_string())
        .chain(
            cfg.auth
                .extra_keys
                .iter()
                .map(|key| format!("userextras/{key}")),
        )
        .collect()
}

/// What one [`CacheHealth`] means for `/readyz`.
///
/// The single translation point between what the stores know and what a probe
/// reports, exhaustive so a new health cannot reach a kubelet without someone
/// deciding what it is called. [`CacheState::Disabled`] and
/// [`CacheState::SyncTimedOut`] are deliberately not producible here: the stores
/// have no idea whether they were configured at all, and the sync budget is
/// startup's rule, not theirs.
#[must_use]
pub fn cache_state(health: CacheHealth) -> CacheState {
    match health {
        CacheHealth::Syncing => CacheState::Syncing,
        CacheHealth::Synced => CacheState::Synced,
        CacheHealth::WatchEnded { .. } => CacheState::WatchEnded,
    }
}

/// Follow the cache's health channel for the process's life, keeping `/readyz`
/// telling the truth about it.
///
/// Never returns while the supervisor lives. That is the whole point: readiness
/// has to be able to *degrade*, so a task that stopped watching once the cache
/// went green would be the bug rather than the optimisation. It also has to be
/// able to recover — a sync that lands after the budget makes the pod ready
/// without a restart.
///
/// Awaiting the channel rather than `Store::wait_until_ready` is load-bearing:
/// kube delivers per-store readiness through a `oneshot` that holds exactly one
/// waker, so a second waiter on a still-cold store can be lost forever. The
/// single join lives in [`crate::cache::stores`]; everyone else reads this
/// channel.
pub async fn track_cache_readiness(
    mut health_rx: watch::Receiver<CacheHealth>,
    readiness: Arc<Readiness>,
) {
    let initial = initial_cache_state(&mut health_rx).await;
    readiness.set_cache(initial);
    if let CacheState::SyncTimedOut = initial {
        report_sync_timeout();
    }

    while health_rx.changed().await.is_ok() {
        let state = cache_state(*health_rx.borrow_and_update());
        readiness.set_cache(state);
        report_cache_state(state);
    }

    // The supervisor task itself ended, so nothing will ever update this again.
    // Whatever the cache was, it is now unattended.
    readiness.set_cache(CacheState::WatchEnded);
    tracing::error!(
        "the kopiur-ui cache supervisor ended; nothing is watching the reflector stores any \\
         more, so /readyz reports cache-watch-ended. Restart the pod."
    );
}

/// What the first [`CACHE_SYNC_TIMEOUT`] of watching produced.
///
/// Never [`CacheState::Syncing`]: the point of the budget is that "still
/// waiting" stops being an acceptable answer once it has been exceeded.
pub async fn initial_cache_state(health_rx: &mut watch::Receiver<CacheHealth>) -> CacheState {
    tokio::time::timeout(CACHE_SYNC_TIMEOUT, settled(health_rx))
        .await
        .unwrap_or(CacheState::SyncTimedOut)
}

/// Resolve as soon as the cache's health is something other than
/// [`CacheHealth::Syncing`], or the channel closes.
///
/// `borrow_and_update` before each wait, so a value published before this was
/// called (a store set that synced while the task was still being spawned) is
/// seen rather than waited past.
pub async fn settled(health_rx: &mut watch::Receiver<CacheHealth>) -> CacheState {
    loop {
        match *health_rx.borrow_and_update() {
            CacheHealth::Syncing => {}
            CacheHealth::Synced => return CacheState::Synced,
            CacheHealth::WatchEnded { .. } => return CacheState::WatchEnded,
        }
        if health_rx.changed().await.is_err() {
            // Every sender is gone, so `Syncing` can never become anything else.
            return CacheState::WatchEnded;
        }
    }
}

/// Say what a change of cache state means, at the level it deserves.
///
/// Exhaustive: a state nobody chose a log line for is a state that changes
/// `/readyz` silently. The two that stop the pod serving get their own
/// remediation; the rest share one line, because the state token is what an
/// operator greps for and `/readyz` reports it either way.
fn report_cache_state(state: CacheState) {
    match state {
        // Ready again, still filling, or never configured. `Disabled` is set by
        // `main` before anything can publish, so it is not a transition this
        // task can actually observe.
        CacheState::Synced | CacheState::Syncing | CacheState::Disabled => {
            tracing::info!(?state, "the kopiur-ui cache state changed");
        }
        CacheState::SyncTimedOut => report_sync_timeout(),
        CacheState::WatchEnded => tracing::error!(
            "a kopiur-ui reflector ended, so its store is frozen and will go stale; the UI \\
             has stopped reporting ready. This does not recover on its own — restart the pod."
        ),
    }
}

/// The one message an operator gets when the stores never finished listing.
fn report_sync_timeout() {
    tracing::error!(
        budget_secs = CACHE_SYNC_TIMEOUT.as_secs(),
        "the kopiur-ui reflector stores have not completed their initial list; /readyz \\
         reports cache-sync-timed-out. The usual cause is the UI's own ServiceAccount \\
         lacking list/watch on the kopiur CRDs, or the CRDs not being installed — check the \\
         chart's ui.rbac. Set KOPIUR_UI_CACHE=false to read through impersonated calls \\
         instead. Still watching."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::config::{
        AnonymousIdentity, AuthConfig, AuthMode, CacheLimits, DEFAULT_ADDR,
        DEFAULT_CLIENT_CACHE_SIZE, DEFAULT_GROUPS_SEPARATOR, DEFAULT_MAX_DOWNLOAD_BYTES,
        DEFAULT_MAX_EXEC_GLOBAL, DEFAULT_MAX_EXEC_PER_IDENTITY, DEFAULT_MAX_MANIFEST_BYTES,
        DEFAULT_MAX_SESSION_STARTS, DEFAULT_OPS_ADDR, DEFAULT_SAR_CACHE_SIZE,
        DEFAULT_SNAPSHOT_LIST_CAP, SessionLimits,
    };

    fn config(extra_keys: &[&str]) -> UiConfig {
        UiConfig {
            addr: DEFAULT_ADDR.parse().expect("default addr"),
            ops_addr: DEFAULT_OPS_ADDR.parse().expect("default ops addr"),
            auth: AuthConfig {
                mode: AuthMode::AnonymousOnly(AnonymousIdentity {
                    user: "viewer".to_string(),
                    groups: Vec::new(),
                }),
                groups_separator: DEFAULT_GROUPS_SEPARATOR.to_string(),
                email_header: None,
                extra_keys: extra_keys.iter().map(|k| (*k).to_string()).collect(),
                allowed_groups: None,
                proxy_secret: None,
            },
            operator_namespace: None,
            mover_image: None,
            cache_enabled: true,
            session: SessionLimits {
                ttl: Duration::from_secs(900),
                ready_timeout: Duration::from_secs(300),
                max_starts: DEFAULT_MAX_SESSION_STARTS,
                max_exec_per_identity: DEFAULT_MAX_EXEC_PER_IDENTITY,
                max_exec_global: DEFAULT_MAX_EXEC_GLOBAL,
            },
            download_max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            download_chunk_timeout: Duration::from_secs(60),
            manifest_max_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            snapshot_list_cap: DEFAULT_SNAPSHOT_LIST_CAP,
            client_cache: CacheLimits {
                size: DEFAULT_CLIENT_CACHE_SIZE,
                ttl: Duration::from_secs(600),
            },
            sar_ttl: Duration::from_secs(60),
            sar_cache_size: DEFAULT_SAR_CACHE_SIZE,
            tls: None,
            cors_origins: Vec::new(),
        }
    }

    // --- impersonation targets ---------------------------------------------

    #[test]
    fn the_two_mandatory_impersonation_resources_are_always_checked() {
        assert_eq!(impersonation_targets(&config(&[])), ["users", "groups"]);
    }

    /// The fan-out addendum 11 asked for: a deployment that forwards extras must
    /// be checked for `userextras/<key>` on each one, or a proxy asserting an
    /// extra the UI may not impersonate fails every request with a message that
    /// points at the wrong permission.
    #[test]
    fn every_configured_extra_key_becomes_its_own_userextras_subresource() {
        assert_eq!(
            impersonation_targets(&config(&["scopes", "auth-time"])),
            [
                "users",
                "groups",
                "userextras/scopes",
                "userextras/auth-time",
            ],
        );
    }

    // --- the readiness state machine ---------------------------------------

    #[test]
    fn every_cache_health_maps_to_exactly_one_readiness_state() {
        assert_eq!(cache_state(CacheHealth::Syncing), CacheState::Syncing);
        assert_eq!(cache_state(CacheHealth::Synced), CacheState::Synced);
        assert_eq!(
            cache_state(CacheHealth::WatchEnded { kind: "Snapshot" }),
            CacheState::WatchEnded,
        );
    }

    /// A store set that synced before this task was spawned must be *seen*, not
    /// waited past — `watch` delivers the current value, and a `changed()` that
    /// skipped it would hang for the whole budget over a healthy cache.
    #[tokio::test]
    async fn an_already_synced_cache_is_recognised_without_waiting() {
        let (tx, mut rx) = watch::channel(CacheHealth::Synced);
        assert_eq!(initial_cache_state(&mut rx).await, CacheState::Synced);
        drop(tx);
    }

    #[tokio::test]
    async fn a_sync_inside_the_budget_is_recognised() {
        let (tx, mut rx) = watch::channel(CacheHealth::Syncing);
        let publisher = tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = tx.send(CacheHealth::Synced);
            tx
        });

        assert_eq!(initial_cache_state(&mut rx).await, CacheState::Synced);
        drop(publisher.await.expect("publisher"));
    }

    /// The budget branch. The clock is paused, so this costs no wall time and
    /// still exercises the real `CACHE_SYNC_TIMEOUT`.
    #[tokio::test(start_paused = true)]
    async fn a_cache_that_never_syncs_times_out_rather_than_waiting_forever() {
        // The sender is held, so the channel stays open and only the budget can
        // end the wait.
        let (_tx, mut rx) = watch::channel(CacheHealth::Syncing);
        assert_eq!(initial_cache_state(&mut rx).await, CacheState::SyncTimedOut);
    }

    #[tokio::test]
    async fn a_watch_that_ended_before_the_sync_is_terminal_not_a_timeout() {
        let (tx, mut rx) = watch::channel(CacheHealth::WatchEnded { kind: "Snapshot" });
        assert_eq!(initial_cache_state(&mut rx).await, CacheState::WatchEnded);
        drop(tx);
    }

    /// Every sender gone means `Syncing` can never become anything else, so the
    /// honest answer is the terminal one rather than a wait that never ends.
    #[tokio::test]
    async fn a_closed_channel_ends_the_wait_instead_of_hanging() {
        let (tx, mut rx) = watch::channel(CacheHealth::Syncing);
        drop(tx);
        assert_eq!(settled(&mut rx).await, CacheState::WatchEnded);
    }

    /// The recovery path: a sync that lands after the budget must make the pod
    /// ready without a restart. Nothing exercised this before.
    #[tokio::test(start_paused = true)]
    async fn readiness_recovers_when_a_slow_cache_syncs_after_the_budget() {
        let (tx, rx) = watch::channel(CacheHealth::Syncing);
        let readiness = Arc::new(Readiness::new(false));
        let tracker = tokio::spawn(track_cache_readiness(rx, Arc::clone(&readiness)));

        // Let the budget expire (paused clock: no wall time).
        tokio::time::sleep(CACHE_SYNC_TIMEOUT * 2).await;
        assert_eq!(readiness.cache(), CacheState::SyncTimedOut);
        assert_eq!(
            readiness.reasons(),
            ["impersonation-unavailable", "cache-sync-timed-out"]
        );

        tx.send(CacheHealth::Synced)
            .expect("the tracker is listening");
        tokio::task::yield_now().await;
        assert_eq!(readiness.cache(), CacheState::Synced);

        drop(tx);
        tracker.await.expect("the tracker must not panic");
    }

    /// The degrade path, end to end through the thing `/readyz` actually reads:
    /// a reflector dying AFTER the cache went green must take readiness with it.
    #[tokio::test]
    async fn readiness_degrades_when_a_reflector_dies_after_the_sync() {
        let (tx, rx) = watch::channel(CacheHealth::Synced);
        let readiness = Arc::new(Readiness::new(false));
        readiness.impersonation_ok.store(true, Ordering::Relaxed);
        let tracker = tokio::spawn(track_cache_readiness(rx, Arc::clone(&readiness)));

        tokio::task::yield_now().await;
        assert!(
            readiness.reasons().is_empty(),
            "a synced cache with impersonation up is ready: {:?}",
            readiness.reasons(),
        );

        tx.send(CacheHealth::WatchEnded { kind: "Snapshot" })
            .expect("the tracker is listening");
        tokio::task::yield_now().await;

        assert_eq!(readiness.cache(), CacheState::WatchEnded);
        assert_eq!(
            readiness.reasons(),
            ["cache-watch-ended"],
            "/readyz must 503 rather than answer ok over a frozen store",
        );

        drop(tx);
        tracker.await.expect("the tracker must not panic");
    }

    /// The tracker outliving its channel is itself a reason to stop serving:
    /// nothing is watching the cache any more.
    #[tokio::test]
    async fn a_lost_supervisor_leaves_the_pod_unready() {
        let (tx, rx) = watch::channel(CacheHealth::Synced);
        let readiness = Arc::new(Readiness::new(false));
        readiness.impersonation_ok.store(true, Ordering::Relaxed);
        let tracker = tokio::spawn(track_cache_readiness(rx, Arc::clone(&readiness)));

        tokio::task::yield_now().await;
        drop(tx);
        tracker.await.expect("the tracker must not panic");

        assert_eq!(readiness.cache(), CacheState::WatchEnded);
        assert_eq!(readiness.reasons(), ["cache-watch-ended"]);
    }
}
