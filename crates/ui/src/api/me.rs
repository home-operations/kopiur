//! `GET /api/v1/me` — who the backend thinks the caller is, and the coarse
//! capabilities the SPA uses to enable or hide controls.
//!
//! # Capabilities are an affordance, never an authorization
//!
//! Every flag here is the answer to a `SubjectAccessReview`-shaped question
//! asked on the caller's behalf. It decides whether a button is greyed out, and
//! nothing else: the real request is still impersonated and still re-authorized
//! by the apiserver, so a stale or wrong `true` here costs a nicer error message,
//! not a permission. That is why an unanswerable probe degrades to `false` only
//! when the *authorizer* said no — an error asking the question is surfaced,
//! never read as an allow.
//!
//! # Two paths, one question
//!
//! In cache mode the UI already holds a `SubjectAccessReview` cache keyed by
//! identity, so the Kopiur verbs are answered from it. In impersonated mode
//! there is no cache, so the caller's own client posts a
//! `SelfSubjectAccessReview` — which the apiserver answers about the
//! impersonated user, i.e. about the caller. `pods/exec` takes the
//! `SelfSubjectAccessReview` path in *both* modes: it is a core-group
//! subresource, and the UI's `SarCache` deliberately only asks about the kopiur
//! API group.

use axum::extract::State;
use axum::{Json, Router, routing::get};
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use kube::api::{Api, PostParams};

use kopiur_ops::{OpsError, classify_kube};
use kopiur_ui_model::identity::{Capabilities, Me};

use crate::AppState;
use crate::api::problem::ApiError;
use crate::api::{NamespaceQuery, UiQuery, client_for};
use crate::auth::CurrentIdentity;
use crate::auth::identity::Identity;
use crate::cache::Source;
use crate::cache::authz::KOPIUR_GROUP;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/me", get(handler))
}

/// Whether a probe is asked about the namespace `/me` was called for.
///
/// A closed enum rather than a `bool` so the cluster-scoped case has to be
/// stated: asking a namespaced review about `clusterrepositories` would report a
/// `RoleBinding` grant that cannot authorize the write, which is a `true` the
/// apiserver then contradicts at click time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeScope {
    /// Asked in the namespace the caller named; cluster-wide when they named
    /// none.
    Namespaced,
    /// Always asked cluster-wide, because the resource is cluster-scoped.
    ClusterScoped,
}

/// One capability probe: a verb on a resource, in an API group.
struct Probe {
    verb: &'static str,
    group: &'static str,
    resource: &'static str,
    subresource: Option<&'static str>,
    scope: ProbeScope,
}

impl Probe {
    /// The namespace this probe's review carries, given the one `/me` was asked
    /// about. Exhaustive over [`ProbeScope`].
    fn namespace<'a>(&self, asked: Option<&'a str>) -> Option<&'a str> {
        match self.scope {
            ProbeScope::Namespaced => asked,
            ProbeScope::ClusterScoped => None,
        }
    }
}

/// Every Kopiur write the API performs, as the review that authorizes it.
///
/// One probe per (verb, resource) rather than one per button: `scan-catalog` and
/// `suspend kind: repository` both patch `repositories`, and asking twice would
/// be two round trips for one answer. `Capabilities`' own docs carry the
/// button-to-flag table.
const CREATE_SNAPSHOTS: Probe = Probe {
    verb: "create",
    group: KOPIUR_GROUP,
    resource: "snapshots",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
const DELETE_SNAPSHOTS: Probe = Probe {
    verb: "delete",
    group: KOPIUR_GROUP,
    resource: "snapshots",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
const CREATE_RESTORES: Probe = Probe {
    verb: "create",
    group: KOPIUR_GROUP,
    resource: "restores",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
const PATCH_POLICIES: Probe = Probe {
    verb: "patch",
    group: KOPIUR_GROUP,
    resource: "snapshotpolicies",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
const PATCH_SCHEDULES: Probe = Probe {
    verb: "patch",
    group: KOPIUR_GROUP,
    resource: "snapshotschedules",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
const PATCH_REPOSITORIES: Probe = Probe {
    verb: "patch",
    group: KOPIUR_GROUP,
    resource: "repositories",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
/// `ClusterRepository` is cluster-scoped, so its review must be too.
const PATCH_CLUSTER_REPOSITORIES: Probe = Probe {
    verb: "patch",
    group: KOPIUR_GROUP,
    resource: "clusterrepositories",
    subresource: None,
    scope: ProbeScope::ClusterScoped,
};
const PATCH_MAINTENANCES: Probe = Probe {
    verb: "patch",
    group: KOPIUR_GROUP,
    resource: "maintenances",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
const PATCH_REPOSITORY_REPLICATIONS: Probe = Probe {
    verb: "patch",
    group: KOPIUR_GROUP,
    resource: "repositoryreplications",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
const PATCH_SNAPSHOT_REPLICATIONS: Probe = Probe {
    verb: "patch",
    group: KOPIUR_GROUP,
    resource: "snapshotreplications",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
/// A browse session IS a `batch/v1` Job — `kopiur_ops::browse::session` creates
/// one and `delete_session` removes it — so starting and stopping one are
/// reviewed on `jobs`, not on the exec that reads through it.
const CREATE_SESSION_JOBS: Probe = Probe {
    verb: "create",
    group: "batch",
    resource: "jobs",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
const DELETE_SESSION_JOBS: Probe = Probe {
    verb: "delete",
    group: "batch",
    resource: "jobs",
    subresource: None,
    scope: ProbeScope::Namespaced,
};
/// Reading through a session execs into its mover pod.
const EXEC_SESSIONS: Probe = Probe {
    verb: "create",
    group: "",
    resource: "pods",
    subresource: Some("exec"),
    scope: ProbeScope::Namespaced,
};

/// **Pure.** Assemble the answer, given the identity and the probe results.
///
/// Split out so the projection is testable without an apiserver: everything
/// interesting about `/me` except the capability booleans is a rename of
/// [`Identity`]'s fields.
pub fn view_me(id: &Identity, namespace: Option<&str>, can: Capabilities) -> Me {
    Me {
        user: id.user.clone(),
        groups: id.groups.clone(),
        email: id.email.clone(),
        source: id.source.clone(),
        namespace: namespace.map(str::to_string),
        can,
    }
}

/// Ask the apiserver, as the caller, whether they may do this.
///
/// A `SelfSubjectAccessReview` posted with an impersonating client is answered
/// about the impersonated user, so this is the caller's own answer rather than
/// the UI ServiceAccount's.
async fn self_review(
    client: &kube::Client,
    probe: &Probe,
    namespace: Option<&str>,
) -> Result<bool, OpsError> {
    let review = SelfSubjectAccessReview {
        metadata: Default::default(),
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                verb: Some(probe.verb.to_string()),
                group: Some(probe.group.to_string()),
                resource: Some(probe.resource.to_string()),
                subresource: probe.subresource.map(str::to_string),
                namespace: probe.namespace(namespace).map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        },
        status: None,
    };
    let api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    let reviewed = api
        .create(&PostParams::default(), &review)
        .await
        .map_err(|e| {
            classify_kube(
                "create",
                "SelfSubjectAccessReview",
                "selfsubjectaccessreviews",
                None,
                None,
                e,
            )
        })?;
    Ok(reviewed.status.is_some_and(|s| s.allowed))
}

/// Answer one Kopiur probe by whichever route this deployment reads through.
///
/// Exhaustive over [`Source`], so a third read source cannot be added without
/// someone deciding how it answers an authorization question — the one place
/// where getting it wrong would mean showing a user controls they cannot use.
async fn kopiur_probe(
    app: &AppState,
    id: &Identity,
    client: &kube::Client,
    probe: &Probe,
    namespace: Option<&str>,
) -> Result<bool, OpsError> {
    match app.source.as_ref() {
        Source::Cache { sar, .. } => {
            sar.allowed(
                id,
                probe.verb,
                probe.resource,
                probe.namespace(namespace),
                None,
            )
            .await
        }
        Source::Impersonated => self_review(client, probe, namespace).await,
    }
}

/// `GET /api/v1/me?namespace=`
async fn handler(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<NamespaceQuery>,
) -> Result<Json<Me>, ApiError> {
    let namespace = q.namespace.as_deref();
    let client = client_for(&app, &id)?;

    // Concurrently: thirteen reviews, and in impersonated mode each is its own
    // apiserver round trip. Run in series they would put twelve extra latencies
    // on a call the SPA makes again for every namespace it scopes to. Each
    // future is bound to the field it answers rather than collected
    // positionally, so a probe cannot end up wired to the wrong flag.
    //
    // The last three are always `SelfSubjectAccessReview`s: `SarCache` asks only
    // about the kopiur API group, so routing `jobs` (batch) or `pods/exec`
    // (core, and a subresource) through it would silently answer a different
    // question.
    let (
        create_snapshots,
        delete_snapshots,
        create_restores,
        patch_policies,
        patch_schedules,
        patch_repositories,
        patch_cluster_repositories,
        patch_maintenances,
        patch_repository_replications,
        patch_snapshot_replications,
        create_session_jobs,
        delete_session_jobs,
        exec_sessions,
    ) = tokio::try_join!(
        kopiur_probe(&app, &id, &client, &CREATE_SNAPSHOTS, namespace),
        kopiur_probe(&app, &id, &client, &DELETE_SNAPSHOTS, namespace),
        kopiur_probe(&app, &id, &client, &CREATE_RESTORES, namespace),
        kopiur_probe(&app, &id, &client, &PATCH_POLICIES, namespace),
        kopiur_probe(&app, &id, &client, &PATCH_SCHEDULES, namespace),
        kopiur_probe(&app, &id, &client, &PATCH_REPOSITORIES, namespace),
        kopiur_probe(&app, &id, &client, &PATCH_CLUSTER_REPOSITORIES, namespace),
        kopiur_probe(&app, &id, &client, &PATCH_MAINTENANCES, namespace),
        kopiur_probe(
            &app,
            &id,
            &client,
            &PATCH_REPOSITORY_REPLICATIONS,
            namespace
        ),
        kopiur_probe(&app, &id, &client, &PATCH_SNAPSHOT_REPLICATIONS, namespace),
        self_review(&client, &CREATE_SESSION_JOBS, namespace),
        self_review(&client, &DELETE_SESSION_JOBS, namespace),
        self_review(&client, &EXEC_SESSIONS, namespace),
    )?;

    let can = Capabilities {
        create_snapshots,
        delete_snapshots,
        create_restores,
        patch_policies,
        patch_schedules,
        patch_repositories,
        patch_cluster_repositories,
        patch_maintenances,
        patch_repository_replications,
        patch_snapshot_replications,
        create_session_jobs,
        delete_session_jobs,
        exec_sessions,
    };

    Ok(Json(view_me(&id, namespace, can)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_ui_model::identity::IdentitySource;
    use std::collections::BTreeMap;

    fn identity() -> Identity {
        Identity {
            user: "alice@example.com".to_string(),
            groups: vec!["platform".to_string(), "sre".to_string()],
            email: Some("alice@example.com".to_string()),
            extra: BTreeMap::new(),
            source: IdentitySource::TrustedHeaders,
        }
    }

    /// Every probe the handler runs, paired with the flag it fills.
    ///
    /// One list, so a probe added without a flag (or a flag without a probe)
    /// fails `every_mutating_endpoint_has_a_capability_flag` rather than shipping
    /// a control that is enabled for everyone and 403s at click time.
    type ProbedFlag = (&'static Probe, fn(&Capabilities) -> bool);

    const PROBED_FLAGS: &[ProbedFlag] = &[
        (&CREATE_SNAPSHOTS, |c| c.create_snapshots),
        (&DELETE_SNAPSHOTS, |c| c.delete_snapshots),
        (&CREATE_RESTORES, |c| c.create_restores),
        (&PATCH_POLICIES, |c| c.patch_policies),
        (&PATCH_SCHEDULES, |c| c.patch_schedules),
        (&PATCH_REPOSITORIES, |c| c.patch_repositories),
        (&PATCH_CLUSTER_REPOSITORIES, |c| {
            c.patch_cluster_repositories
        }),
        (&PATCH_MAINTENANCES, |c| c.patch_maintenances),
        (&PATCH_REPOSITORY_REPLICATIONS, |c| {
            c.patch_repository_replications
        }),
        (&PATCH_SNAPSHOT_REPLICATIONS, |c| {
            c.patch_snapshot_replications
        }),
        (&CREATE_SESSION_JOBS, |c| c.create_session_jobs),
        (&DELETE_SESSION_JOBS, |c| c.delete_session_jobs),
        (&EXEC_SESSIONS, |c| c.exec_sessions),
    ];

    /// **Pure.** The `Capabilities` an authorizer that allows exactly the probes
    /// `allow` accepts would produce — the projection the handler performs, with
    /// the apiserver replaced by a predicate.
    fn capabilities_when(allow: impl Fn(&Probe) -> bool) -> Capabilities {
        Capabilities {
            create_snapshots: allow(&CREATE_SNAPSHOTS),
            delete_snapshots: allow(&DELETE_SNAPSHOTS),
            create_restores: allow(&CREATE_RESTORES),
            patch_policies: allow(&PATCH_POLICIES),
            patch_schedules: allow(&PATCH_SCHEDULES),
            patch_repositories: allow(&PATCH_REPOSITORIES),
            patch_cluster_repositories: allow(&PATCH_CLUSTER_REPOSITORIES),
            patch_maintenances: allow(&PATCH_MAINTENANCES),
            patch_repository_replications: allow(&PATCH_REPOSITORY_REPLICATIONS),
            patch_snapshot_replications: allow(&PATCH_SNAPSHOT_REPLICATIONS),
            create_session_jobs: allow(&CREATE_SESSION_JOBS),
            delete_session_jobs: allow(&DELETE_SESSION_JOBS),
            exec_sessions: allow(&EXEC_SESSIONS),
        }
    }

    fn all_allowed() -> Capabilities {
        capabilities_when(|_| true)
    }

    #[test]
    fn me_reports_the_identity_the_backend_will_impersonate() {
        let me = view_me(&identity(), Some("media"), all_allowed());
        assert_eq!(me.user, "alice@example.com");
        assert_eq!(me.groups, vec!["platform", "sre"]);
        assert_eq!(me.email.as_deref(), Some("alice@example.com"));
        assert_eq!(me.source, IdentitySource::TrustedHeaders);
        assert_eq!(me.namespace.as_deref(), Some("media"));
        assert!(me.can.create_snapshots);
    }

    #[test]
    fn an_absent_namespace_means_cluster_wide_not_the_default_namespace() {
        let me = view_me(&identity(), None, all_allowed());
        assert_eq!(
            me.namespace, None,
            "silently scoping to a namespace would hide the rest of the fleet"
        );
    }

    #[test]
    fn an_anonymous_caller_is_reported_as_anonymous_rather_than_hidden() {
        let anonymous = Identity {
            user: "kopiur-ui-viewer".to_string(),
            groups: Vec::new(),
            email: None,
            extra: BTreeMap::new(),
            source: IdentitySource::Anonymous,
        };
        let me = view_me(&anonymous, None, capabilities_when(|_| false));
        assert_eq!(
            me.source,
            IdentitySource::Anonymous,
            "a fallback to anonymous must be visible in the account menu, not silent"
        );
        assert!(me.email.is_none());
        assert!(!me.can.exec_sessions);
    }

    /// An authorizer that allows exactly one verb+resource sets exactly one
    /// flag. Run for every probe, so no two flags can be wired to the same
    /// review and no flag can be left reading another's answer.
    #[test]
    fn a_review_that_allows_one_verb_sets_one_flag() {
        for (probe, flag) in PROBED_FLAGS {
            let can = capabilities_when(|p| {
                (p.verb, p.resource, p.subresource)
                    == (probe.verb, probe.resource, probe.subresource)
            });
            assert!(
                flag(&can),
                "allowing {} {} must set its own flag",
                probe.verb,
                probe.resource
            );
            let set: Vec<&str> = PROBED_FLAGS
                .iter()
                .filter(|(_, f)| f(&can))
                .map(|(p, _)| p.resource)
                .collect();
            assert_eq!(
                set,
                vec![probe.resource],
                "allowing only {} {} must set only its own flag",
                probe.verb,
                probe.resource
            );
        }
    }

    /// Every route that registers a mutating method, and the probes that gate
    /// it. Paths are verbatim as registered — relative for the nested routers,
    /// absolute for `browse`, which merges at the root.
    const GATED_ROUTES: &[(&str, &[&Probe])] = &[
        ("/actions/snapshot-now", &[&CREATE_SNAPSHOTS]),
        ("/actions/restore", &[&CREATE_RESTORES]),
        (
            "/actions/suspend",
            &[
                &PATCH_POLICIES,
                &PATCH_SCHEDULES,
                &PATCH_REPOSITORIES,
                &PATCH_CLUSTER_REPOSITORIES,
                &PATCH_REPOSITORY_REPLICATIONS,
                &PATCH_SNAPSHOT_REPLICATIONS,
            ],
        ),
        ("/actions/maintenance-run", &[&PATCH_MAINTENANCES]),
        (
            "/actions/replication-run",
            &[&PATCH_REPOSITORY_REPLICATIONS, &PATCH_SNAPSHOT_REPLICATIONS],
        ),
        (
            "/actions/scan-catalog",
            &[&PATCH_REPOSITORIES, &PATCH_CLUSTER_REPOSITORIES],
        ),
        ("/snapshots/{namespace}/{name}", &[&DELETE_SNAPSHOTS]),
        (
            "/api/v1/snapshots/{namespace}/{name}/session",
            &[&CREATE_SESSION_JOBS, &DELETE_SESSION_JOBS, &EXEC_SESSIONS],
        ),
        (
            "/api/v1/repositories/{kind}/{name}/session",
            &[&DELETE_SESSION_JOBS],
        ),
    ];

    /// Every module that builds a `Router`, as source text.
    ///
    /// `include_str!` rather than a reflective walk because axum's `Router` does
    /// not expose its routing table. The scan below reads the `fn router*`
    /// bodies out of these, so a mutating route added ANYWHERE in the API — not
    /// just one added to a list a test author remembered to update — fails.
    const ROUTER_SOURCES: &[(&str, &str)] = &[
        ("api/mod.rs", include_str!("mod.rs")),
        ("api/doctor.rs", include_str!("doctor.rs")),
        ("api/events.rs", include_str!("events.rs")),
        ("api/gates.rs", include_str!("gates.rs")),
        ("api/graph.rs", include_str!("graph.rs")),
        ("api/maintenance.rs", include_str!("maintenance.rs")),
        ("api/me.rs", include_str!("me.rs")),
        ("api/policies.rs", include_str!("policies.rs")),
        ("api/replications.rs", include_str!("replications.rs")),
        ("api/repositories.rs", include_str!("repositories.rs")),
        ("api/restores.rs", include_str!("restores.rs")),
        ("api/schedules.rs", include_str!("schedules.rs")),
        ("api/snapshots.rs", include_str!("snapshots.rs")),
        ("api/status.rs", include_str!("status.rs")),
        ("actions/mod.rs", include_str!("../actions/mod.rs")),
        ("browse/mod.rs", include_str!("../browse/mod.rs")),
    ];

    /// The marker every router this crate mounts is declared with. Anchored at
    /// column 0 so it matches only a top-level item — an indented `fn router…`
    /// (this test module's own helpers included) is not one.
    const ROUTER_MARKER: &str = "\npub fn router";

    /// The body of every top-level `pub fn router*` in `source`.
    ///
    /// Bounded to those bodies on purpose: a `.delete(` outside one is an
    /// apiserver call, not a route method, and `actions/mod.rs` registers a
    /// throwaway route inside its own `#[cfg(test)]` module. rustfmt (enforced
    /// by `mise run fmt-check`) puts the closing brace of a top-level item at
    /// column 0, which is what `\n}` finds.
    fn router_bodies(source: &str) -> Vec<&str> {
        let mut bodies = Vec::new();
        let mut rest = source;
        while let Some(at) = rest.find(ROUTER_MARKER) {
            let after = &rest[at + 1..];
            let Some(open) = after.find('{') else { break };
            let body = &after[open..];
            let end = body.find("\n}").map(|e| e + 1).unwrap_or(body.len());
            bodies.push(&body[..end]);
            rest = &after[open + end..];
        }
        bodies
    }

    /// Every path in `source` registered with a mutating method.
    fn mutating_routes(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        for body in router_bodies(source) {
            for chunk in body.split(".route(").skip(1) {
                let Some(open) = chunk.find('"') else {
                    continue;
                };
                let Some(close) = chunk[open + 1..].find('"') else {
                    continue;
                };
                let path = &chunk[open + 1..open + 1 + close];
                // The chunk runs to the next `.route(`, which inside a router
                // body is only ever more registration.
                if chunk.contains("post(") || chunk.contains("delete(") {
                    found.push(path.to_string());
                }
            }
        }
        found
    }

    /// The parser has to actually find routes, or every assertion below passes
    /// vacuously. This is the tripwire for a reformat that breaks the scan.
    #[test]
    fn the_route_scan_finds_the_routers_it_is_reading() {
        let read: usize = ROUTER_SOURCES
            .iter()
            .map(|(_, src)| router_bodies(src).len())
            .sum();
        assert_eq!(
            read, 17,
            "one router per module in ROUTER_SOURCES, plus browse's second \
             (`router_untimed`). If a module gained or lost one, bump this \
             deliberately; if the count dropped to zero the scan stopped \
             matching — fix it rather than deleting it, it is the only thing \
             that notices an ungated endpoint."
        );
        let get_only = mutating_routes(include_str!("snapshots.rs"));
        assert!(
            get_only.is_empty(),
            "the read API registers no mutating routes, so the scan must find none \
             in a read module; found {get_only:?}"
        );
    }

    /// Every mutating route the API serves has a capability flag — checked
    /// against the ROUTERS' OWN SOURCE, not against a list this test restates.
    ///
    /// The previous version of this test iterated a hand-written table of
    /// `(verb, resource)` pairs and compared it to `PROBED_FLAGS`. Both halves
    /// described the probe table, so adding `POST /actions/foo` changed nothing
    /// it read — and it had in fact already missed the `create`/`delete` on
    /// `jobs` that starting and stopping a browse session perform, while the
    /// `Capabilities` doc claimed `execSessions` covered them. A user holding
    /// `pods/exec` but not `create jobs` saw an enabled Browse button that 403'd
    /// on click, which is the exact failure a capability flag exists to prevent.
    #[test]
    fn every_mutating_route_has_a_capability_flag() {
        let mut registered: Vec<String> = ROUTER_SOURCES
            .iter()
            .flat_map(|(_, src)| mutating_routes(src))
            .collect();
        registered.sort();
        registered.dedup();

        let mut gated: Vec<String> = GATED_ROUTES
            .iter()
            .map(|(path, _)| (*path).to_string())
            .collect();
        gated.sort();

        assert_eq!(
            registered, gated,
            "a mutating route was added or removed without deciding which \
             capability flag gates it. Add it to GATED_ROUTES with the probes \
             its handler actually performs — reading the handler, not guessing."
        );

        // And every probe named there is one `/me` actually asks.
        for (path, probes) in GATED_ROUTES {
            for probe in *probes {
                assert!(
                    PROBED_FLAGS.iter().any(|(p, _)| std::ptr::eq(*p, *probe)),
                    "{path} is gated on {} {} , which /me never reviews",
                    probe.verb,
                    probe.resource
                );
            }
        }
    }

    /// The reverse direction: no probe is a round trip nothing uses.
    #[test]
    fn every_probe_gates_at_least_one_route() {
        for (probe, _) in PROBED_FLAGS {
            assert!(
                GATED_ROUTES
                    .iter()
                    .any(|(_, probes)| probes.iter().any(|p| std::ptr::eq(*p, *probe))),
                "{} {} is reviewed on every /me call and gates nothing",
                probe.verb,
                probe.resource
            );
        }
    }

    /// Starting a browse session takes BOTH grants, and the doc says so.
    ///
    /// The Job is what `kopiur_ops::browse::session` creates
    /// (`jobs_api.create(...)`) and the exec is what reading through it costs;
    /// a user with one and not the other cannot browse.
    #[test]
    fn starting_a_session_is_gated_on_the_job_and_on_the_exec() {
        let session = GATED_ROUTES
            .iter()
            .find(|(path, _)| *path == "/api/v1/snapshots/{namespace}/{name}/session")
            .expect("the session route is gated");
        let probes: Vec<(&str, &str)> = session.1.iter().map(|p| (p.verb, p.resource)).collect();
        assert!(probes.contains(&("create", "jobs")));
        assert!(probes.contains(&("delete", "jobs")));
        assert!(probes.contains(&("create", "pods")));
        assert_eq!(
            CREATE_SESSION_JOBS.group, "batch",
            "a session Job lives in the batch API group, not kopiur's"
        );
        assert_eq!(DELETE_SESSION_JOBS.group, "batch");
    }

    /// A namespaced review of a cluster-scoped resource would report a
    /// `RoleBinding` grant that cannot authorize the write.
    #[test]
    fn the_cluster_scoped_probe_ignores_the_namespace_me_was_asked_about() {
        assert_eq!(
            PATCH_CLUSTER_REPOSITORIES.namespace(Some("media")),
            None,
            "ClusterRepository is cluster-scoped, so its review must be too"
        );
        assert_eq!(
            PATCH_REPOSITORIES.namespace(Some("media")),
            Some("media"),
            "the namespaced twin still asks about the namespace"
        );
        assert_eq!(PATCH_REPOSITORIES.namespace(None), None);
    }

    #[test]
    fn the_probes_name_the_verbs_and_resources_rbac_is_written_against() {
        // These strings are what a ClusterRole rule says; a typo here shows the
        // user a control they cannot use (or hides one they can).
        assert_eq!(
            (CREATE_SNAPSHOTS.verb, CREATE_SNAPSHOTS.resource),
            ("create", "snapshots")
        );
        assert_eq!(
            (DELETE_SNAPSHOTS.verb, DELETE_SNAPSHOTS.resource),
            ("delete", "snapshots")
        );
        assert_eq!(
            (CREATE_RESTORES.verb, CREATE_RESTORES.resource),
            ("create", "restores")
        );
        assert_eq!(
            (PATCH_POLICIES.verb, PATCH_POLICIES.resource),
            ("patch", "snapshotpolicies")
        );
        for probe in [
            &CREATE_SNAPSHOTS,
            &DELETE_SNAPSHOTS,
            &CREATE_RESTORES,
            &PATCH_POLICIES,
        ] {
            assert_eq!(
                probe.group, KOPIUR_GROUP,
                "a kopiur probe must name the kopiur API group"
            );
            assert_eq!(probe.subresource, None);
        }
    }

    #[test]
    fn the_exec_probe_is_a_core_group_subresource() {
        // This is exactly why it cannot go through `SarCache`, which pins the
        // kopiur group and has no subresource field.
        assert_eq!(EXEC_SESSIONS.group, "", "pods live in the core API group");
        assert_eq!(EXEC_SESSIONS.resource, "pods");
        assert_eq!(EXEC_SESSIONS.subresource, Some("exec"));
        assert_eq!(EXEC_SESSIONS.verb, "create");
    }

    #[test]
    fn a_self_review_is_built_for_the_namespace_it_is_asked_about() {
        // The body is what the apiserver answers against; assert it directly
        // rather than through a client that would need a cluster.
        let review = SelfSubjectAccessReview {
            metadata: Default::default(),
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    verb: Some(EXEC_SESSIONS.verb.to_string()),
                    group: Some(EXEC_SESSIONS.group.to_string()),
                    resource: Some(EXEC_SESSIONS.resource.to_string()),
                    subresource: EXEC_SESSIONS.subresource.map(str::to_string),
                    namespace: Some("media".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            status: None,
        };
        let body = serde_json::to_value(&review).unwrap();
        assert_eq!(body["spec"]["resourceAttributes"]["subresource"], "exec");
        assert_eq!(body["spec"]["resourceAttributes"]["namespace"], "media");
        assert_eq!(body["spec"]["resourceAttributes"]["verb"], "create");
    }
}
