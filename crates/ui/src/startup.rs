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

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use tokio::sync::watch;

use crate::auth::identity::SYSTEM_AUTHENTICATED;
use crate::cache::stores::CacheHealth;
use crate::config::{AuthConfig, AuthMode, CACHE_SYNC_TIMEOUT, UiConfig};
use crate::ops_listener::{CacheState, Readiness};

/// API group of `users` and `groups`. The empty string is the core group.
pub const CORE_GROUP: &str = "";

/// API group of `userextras/<key>` — which is NOT the core group.
///
/// This line used to be a comment on [`CORE_GROUP`] claiming all three
/// impersonation resources lived there, and the startup check believed it, so a
/// deployment with `impersonateExtraKeys` asked about `userextras/<key>` in the
/// core group while the role granted it in this one. The review came back
/// denied and the pod never became ready — the same symptom as the
/// `resourceNames` mismatch, from an unrelated cause, and latent until someone
/// configured an extra key.
pub const AUTHENTICATION_GROUP: &str = "authentication.k8s.io";

/// The two resources the UI must be able to `impersonate` for anything at all to
/// work. Configured `userextras/<key>` subjects are checked alongside them.
pub const IMPERSONATION_RESOURCES: [&str; 2] = ["users", "groups"];

/// One `impersonate` permission this deployment needs, phrased the way the
/// apiserver will be asked about it.
///
/// The `name` is the load-bearing part. An RBAC rule carrying
/// `resourceNames: ["kopiur-console"]` authorizes `impersonate users/kopiur-console`
/// and NOTHING ELSE — in particular it does not authorize the *unnamed* question
/// "may I impersonate users?", which is what a `SelfSubjectAccessReview` asks
/// when its `ResourceAttributes` carry no name. So a startup check that always
/// asked the unnamed question was denied by exactly the narrow grant the chart
/// ships for anonymous mode, and the pod never became ready: the grant and the
/// check were each right on their own and never met.
///
/// Carrying the name here makes the check ask for the permission the deployment
/// will actually exercise, so the two cannot drift apart again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpersonationTarget {
    /// API group the resource lives in: [`CORE_GROUP`] for `users` and `groups`,
    /// [`AUTHENTICATION_GROUP`] for `userextras/<key>`.
    pub api_group: String,
    /// `users`, `groups`, or `userextras/<key>`.
    pub resource: String,
    /// The single principal this deployment can impersonate for `resource`, when
    /// that set is closed and known at startup. `None` means the deployment may
    /// impersonate arbitrary names, which is also how the RBAC is written, so
    /// the unnamed question is the right one to ask.
    pub name: Option<String>,
}

impl ImpersonationTarget {
    /// A core-group target whose RBAC grant is not name-scoped.
    fn any(resource: impl Into<String>) -> Self {
        Self {
            api_group: CORE_GROUP.to_string(),
            resource: resource.into(),
            name: None,
        }
    }

    /// A core-group target pinned to one principal, mirroring `resourceNames`.
    fn named(resource: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            api_group: CORE_GROUP.to_string(),
            resource: resource.into(),
            name: Some(name.into()),
        }
    }

    /// A `userextras/<key>` target, which lives in a different API group and is
    /// never name-scoped — Kubernetes RBAC has no `userextras/*`, so the chart
    /// emits one unscoped rule per key.
    fn extra(key: &str) -> Self {
        Self {
            api_group: AUTHENTICATION_GROUP.to_string(),
            resource: format!("userextras/{key}"),
            name: None,
        }
    }
}

impl fmt::Display for ImpersonationTarget {
    /// `users/kopiur-console`, bare `users`, or
    /// `authentication.k8s.io/userextras/scopes` — the group is shown only when
    /// it is not the core one, because that is precisely the detail that was
    /// wrong and an operator reading the error needs it to write the rule.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.api_group.is_empty() {
            write!(f, "{}/", self.api_group)?;
        }
        match &self.name {
            Some(name) => write!(f, "{}/{name}", self.resource),
            None => f.write_str(&self.resource),
        }
    }
}

/// Every impersonation permission the UI must hold for this configuration.
///
/// `users` and `groups` always; plus `userextras/<key>` for each configured
/// extra key, because a proxy that asserts an extra the UI may not impersonate
/// fails every request — and fails with a message pointing at the wrong thing.
///
/// Each target is name-scoped exactly when the chart's rule for it is, so the
/// question asked matches the grant given:
///
/// | mode | `users` | `groups` |
/// |---|---|---|
/// | anonymous-only | the anonymous user | its groups + `system:authenticated` |
/// | header, `allowedGroups` set | any | the allowlist + `system:authenticated` |
/// | header, no allowlist | any | any |
///
/// `userextras/<key>` is never name-scoped: the chart emits one unscoped rule
/// per key, because Kubernetes RBAC has no `userextras/*`.
#[must_use]
pub fn impersonation_targets(cfg: &UiConfig) -> Vec<ImpersonationTarget> {
    let mut targets = vec![user_target(&cfg.auth.mode)];
    targets.extend(group_targets(&cfg.auth));
    targets.extend(
        cfg.auth
            .extra_keys
            .iter()
            .map(|key| ImpersonationTarget::extra(key)),
    );
    targets
}

/// The `users` permission: one fixed name in anonymous-only mode, any name when
/// a proxy asserts whoever authenticated.
fn user_target(mode: &AuthMode) -> ImpersonationTarget {
    match mode {
        AuthMode::AnonymousOnly(anonymous) => {
            ImpersonationTarget::named(USERS_RESOURCE, &anonymous.user)
        }
        // Header mode covers the `anonymous_fallback` case too: a fallback does
        // not close the set, because the header can still assert anyone.
        AuthMode::Headers { .. } => ImpersonationTarget::any(USERS_RESOURCE),
    }
}

/// The `groups` permissions, one per name whenever the impersonatable set is
/// closed.
///
/// `system:authenticated` is appended to every identity by
/// `auth::identity::finish`, and it is never filtered by the allowlist, so it
/// belongs in every closed set here — a check that omitted it would pass while
/// the first real request was refused for the one group the console always
/// sends.
fn group_targets(auth: &AuthConfig) -> Vec<ImpersonationTarget> {
    let closed: BTreeSet<&str> = match (&auth.mode, &auth.allowed_groups) {
        (AuthMode::AnonymousOnly(anonymous), _) => {
            anonymous.groups.iter().map(String::as_str).collect()
        }
        (AuthMode::Headers { .. }, Some(allowed)) => allowed.iter().map(String::as_str).collect(),
        // Nothing closes the set, and the RBAC rule is unscoped to match.
        (AuthMode::Headers { .. }, None) => {
            return vec![ImpersonationTarget::any(GROUPS_RESOURCE)];
        }
    };

    closed
        .into_iter()
        .chain(std::iter::once(SYSTEM_AUTHENTICATED))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|group| ImpersonationTarget::named(GROUPS_RESOURCE, group))
        .collect()
}

/// `IMPERSONATION_RESOURCES[0]`, named so the two constructors below read.
const USERS_RESOURCE: &str = IMPERSONATION_RESOURCES[0];

/// `IMPERSONATION_RESOURCES[1]`.
const GROUPS_RESOURCE: &str = IMPERSONATION_RESOURCES[1];

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
        "the kopiur-ui cache supervisor ended; nothing is watching the reflector stores any \
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
            "a kopiur-ui reflector ended, so its store is frozen and will go stale; the UI \
             has stopped reporting ready. This does not recover on its own — restart the pod."
        ),
    }
}

/// The one message an operator gets when the stores never finished listing.
fn report_sync_timeout() {
    tracing::error!(
        budget_secs = CACHE_SYNC_TIMEOUT.as_secs(),
        "the kopiur-ui reflector stores have not completed their initial list; /readyz \
         reports cache-sync-timed-out. The usual cause is the UI's own ServiceAccount \
         lacking list/watch on the kopiur CRDs, or the CRDs not being installed — check the \
         chart's ui.rbac. Set KOPIUR_UI_CACHE=false to read through impersonated calls \
         instead. Still watching."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use http::HeaderName;

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

    // --- the log strings an operator reads ---------------------------------

    /// A multi-line log string must end its lines with a single `\` — the Rust
    /// line continuation, which eats the newline and the next line's indentation.
    /// `\\` is an *escaped backslash*, so the message an operator greps ends up
    /// carrying a literal backslash, a newline and nine spaces in the middle of a
    /// sentence.
    ///
    /// The two forms are one character apart, look identical in a diff, and
    /// differ only in output nobody sees until something has already gone wrong —
    /// which is exactly when these particular messages are read. Checked across
    /// the whole crate rather than this file, because the mistake is not specific
    /// to it.
    #[test]
    fn no_log_string_ends_a_line_with_an_escaped_backslash() {
        let sources = [
            ("startup.rs", include_str!("startup.rs")),
            ("lib.rs", include_str!("lib.rs")),
            ("main.rs", include_str!("main.rs")),
            ("ops_listener.rs", include_str!("ops_listener.rs")),
            ("config.rs", include_str!("config.rs")),
            ("metrics.rs", include_str!("metrics.rs")),
            ("static_files.rs", include_str!("static_files.rs")),
            ("cache/stores.rs", include_str!("cache/stores.rs")),
            ("cache/mod.rs", include_str!("cache/mod.rs")),
            ("cache/authz.rs", include_str!("cache/authz.rs")),
            ("auth/mod.rs", include_str!("auth/mod.rs")),
            ("api/problem.rs", include_str!("api/problem.rs")),
        ];

        let offenders: Vec<String> = sources
            .iter()
            .flat_map(|(name, source)| {
                source
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| line.ends_with("\\\\"))
                    .map(move |(n, line)| format!("{name}:{}: {}", n + 1, line.trim()))
            })
            .collect();

        assert!(
            offenders.is_empty(),
            "these lines end with an escaped backslash where they meant a line \
             continuation, so the rendered message carries a stray `\\` and a newline:\n{}",
            offenders.join("\n"),
        );
    }

    // --- impersonation targets ---------------------------------------------

    /// Render targets the way the error message and the review do, so a test
    /// failure reads like the thing an operator would paste into `can-i`.
    fn shown(cfg: &UiConfig) -> Vec<String> {
        impersonation_targets(cfg)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn the_two_mandatory_impersonation_resources_are_always_checked() {
        assert_eq!(
            shown(&config(&[])),
            ["users/viewer", "groups/system:authenticated"],
        );
    }

    /// The regression this whole type exists for.
    ///
    /// Anonymous mode's ClusterRole pins `resourceNames`, and a name-scoped RBAC
    /// rule does not answer the unnamed question. The startup check asked the
    /// unnamed question anyway, so the apiserver said no, `/readyz` stayed 503
    /// for the pod's whole life, and Helm rolled the release back after four
    /// attempts — with both halves individually correct. Found by deploying it,
    /// not by reading it: the previous version of the test above asserted bare
    /// `["users", "groups"]` against an anonymous-mode config and so agreed with
    /// the bug.
    #[test]
    fn anonymous_mode_asks_about_the_one_identity_its_rbac_actually_grants() {
        let mut cfg = config(&[]);
        cfg.auth.mode = AuthMode::AnonymousOnly(AnonymousIdentity {
            user: "kopiur-console".to_string(),
            groups: vec!["platform".to_string()],
        });

        assert_eq!(
            shown(&cfg),
            [
                "users/kopiur-console",
                "groups/platform",
                "groups/system:authenticated",
            ],
        );
        assert!(
            impersonation_targets(&cfg).iter().all(|t| t.name.is_some()),
            "every anonymous-mode target must be name-scoped: the chart's rules are, \
             and an unnamed review against a resourceNames rule is denied",
        );
    }

    /// `system:authenticated` is appended to every identity and is never
    /// filtered by the allowlist, so a check that omitted it would pass while
    /// the first real request was refused for the one group always sent.
    #[test]
    fn the_always_appended_group_is_checked_even_when_no_groups_are_configured() {
        let mut cfg = config(&[]);
        cfg.auth.mode = AuthMode::AnonymousOnly(AnonymousIdentity {
            user: "solo".to_string(),
            groups: Vec::new(),
        });

        assert_eq!(shown(&cfg), ["users/solo", "groups/system:authenticated"]);
    }

    /// Header mode cannot close the user set — the proxy asserts whoever
    /// authenticated — so the RBAC rule is unscoped and the unnamed question is
    /// the right one. A name here would be denied by an unscoped rule's absence
    /// of that name... no: it would be *allowed*, which is worse, because it
    /// would prove nothing about the arbitrary names actually impersonated.
    #[test]
    fn header_mode_leaves_the_user_question_unnamed_because_its_grant_is() {
        let mut cfg = config(&[]);
        cfg.auth.mode = AuthMode::Headers {
            user: HeaderName::from_static("x-forwarded-user"),
            groups: Some(HeaderName::from_static("x-forwarded-groups")),
            anonymous_fallback: None,
        };

        assert_eq!(shown(&cfg), ["users", "groups"]);
    }

    /// An anonymous *fallback* does not close the set: the header can still
    /// assert anyone, and the chart's `$anonOnly` guard agrees (it requires an
    /// empty `userHeader`).
    #[test]
    fn an_anonymous_fallback_does_not_make_header_mode_name_scoped() {
        let mut cfg = config(&[]);
        cfg.auth.mode = AuthMode::Headers {
            user: HeaderName::from_static("x-forwarded-user"),
            groups: None,
            anonymous_fallback: Some(AnonymousIdentity {
                user: "guest".to_string(),
                groups: Vec::new(),
            }),
        };

        assert_eq!(shown(&cfg), ["users", "groups"]);
    }

    // --- the check against the grant ---------------------------------------

    /// Does `rule` authorize `target`, by Kubernetes' RBAC semantics?
    ///
    /// The `resourceNames` arm is the whole point. A rule that carries names
    /// authorizes ONLY requests naming one of them — an unnamed request is not
    /// "less specific and therefore covered", it is simply not matched. Getting
    /// this backwards is what shipped.
    fn authorizes(
        rule: &k8s_openapi::api::rbac::v1::PolicyRule,
        target: &ImpersonationTarget,
    ) -> bool {
        let has = |list: &Option<Vec<String>>, want: &str| {
            list.as_ref().is_some_and(|v| v.iter().any(|x| x == want))
        };
        if !rule.verbs.iter().any(|v| v == "impersonate")
            || !has(&rule.api_groups, &target.api_group)
            || !has(&rule.resources, &target.resource)
        {
            return false;
        }
        match (rule.resource_names.as_ref(), target.name.as_ref()) {
            (None, _) => true,
            (Some(names), _) if names.is_empty() => true,
            (Some(names), Some(name)) => names.iter().any(|n| n == name),
            // A named rule cannot answer an unnamed question.
            (Some(_), None) => false,
        }
    }

    /// Every permission the console checks for must be one the generated role
    /// grants — checked under real RBAC semantics, for every mode.
    ///
    /// This is the test that was missing. The console's startup check and
    /// xtask's role generator are two halves of one decision, written months
    /// apart in different crates, and nothing compared them: the check asked
    /// unnamed questions, the generator emitted `resourceNames` for anonymous
    /// mode, both had passing unit tests, and the pod never became ready. Any
    /// future edit to either half that breaks the pairing fails here instead of
    /// in someone's cluster.
    #[test]
    fn every_checked_impersonation_is_one_the_generated_role_grants() {
        let anon = xtask::rbac::UiAnonymous {
            user: "kopiur-console".to_string(),
            groups: vec!["platform".to_string()],
        };

        let mut anonymous_cfg = config(&["scopes"]);
        anonymous_cfg.auth.mode = AuthMode::AnonymousOnly(AnonymousIdentity {
            user: anon.user.clone(),
            groups: anon.groups.clone(),
        });

        let mut header_cfg = config(&["scopes"]);
        header_cfg.auth.mode = AuthMode::Headers {
            user: HeaderName::from_static("x-forwarded-user"),
            groups: Some(HeaderName::from_static("x-forwarded-groups")),
            anonymous_fallback: None,
        };

        let mut allowlist_cfg = header_cfg.clone();
        allowlist_cfg.auth.allowed_groups =
            Some(["ops", "dev"].iter().map(|g| (*g).to_string()).collect());

        let extras = vec!["scopes".to_string()];
        let allowed = vec!["ops".to_string(), "dev".to_string()];
        let cases: [(&str, &UiConfig, Vec<k8s_openapi::api::rbac::v1::PolicyRule>); 3] = [
            (
                "anonymous-only",
                &anonymous_cfg,
                xtask::rbac::ui_rules(&extras, &[], Some(&anon), true),
            ),
            (
                "header, no allowlist",
                &header_cfg,
                xtask::rbac::ui_rules(&extras, &[], None, true),
            ),
            (
                "header + allowedGroups",
                &allowlist_cfg,
                xtask::rbac::ui_rules(&extras, &allowed, None, true),
            ),
        ];

        for (mode, cfg, rules) in cases {
            for target in impersonation_targets(cfg) {
                assert!(
                    rules.iter().any(|rule| authorizes(rule, &target)),
                    "in {mode} mode the console checks `impersonate {target}` at startup, but \
                     no generated rule authorizes it — so /readyz would report \
                     impersonation-unavailable forever and Helm would roll the release back. \
                     Rules: {rules:#?}",
                );
            }
        }
    }

    /// With `allowedGroups` the chart pins the groups rule to the allowlist plus
    /// `system:authenticated`, so the check is name-scoped for groups only.
    #[test]
    fn an_allowlist_closes_the_group_set_but_not_the_user_set() {
        let mut cfg = config(&[]);
        cfg.auth.mode = AuthMode::Headers {
            user: HeaderName::from_static("x-forwarded-user"),
            groups: Some(HeaderName::from_static("x-forwarded-groups")),
            anonymous_fallback: None,
        };
        cfg.auth.allowed_groups = Some(["ops", "dev"].iter().map(|g| (*g).to_string()).collect());

        assert_eq!(
            shown(&cfg),
            [
                "users",
                "groups/dev",
                "groups/ops",
                "groups/system:authenticated",
            ],
        );
    }

    /// The fan-out addendum 11 asked for: a deployment that forwards extras must
    /// be checked for `userextras/<key>` on each one, or a proxy asserting an
    /// extra the UI may not impersonate fails every request with a message that
    /// points at the wrong permission.
    #[test]
    fn every_configured_extra_key_becomes_its_own_userextras_subresource() {
        assert_eq!(
            shown(&config(&["scopes", "auth-time"])),
            [
                "users/viewer",
                "groups/system:authenticated",
                // Never name-scoped: the chart emits one unscoped rule per key,
                // because Kubernetes RBAC has no `userextras/*`. And NOT in the
                // core group — that mismatch was the second bug the coupling
                // test below found, so the group is spelled out here.
                "authentication.k8s.io/userextras/scopes",
                "authentication.k8s.io/userextras/auth-time",
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
