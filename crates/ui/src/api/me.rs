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
//! identity, so the four Kopiur verbs are answered from it. In impersonated mode
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

/// One capability probe: a verb on a resource, in an API group.
struct Probe {
    verb: &'static str,
    group: &'static str,
    resource: &'static str,
    subresource: Option<&'static str>,
}

/// The four Kopiur writes the SPA offers buttons for.
const CREATE_SNAPSHOTS: Probe = Probe {
    verb: "create",
    group: KOPIUR_GROUP,
    resource: "snapshots",
    subresource: None,
};
const DELETE_SNAPSHOTS: Probe = Probe {
    verb: "delete",
    group: KOPIUR_GROUP,
    resource: "snapshots",
    subresource: None,
};
const CREATE_RESTORES: Probe = Probe {
    verb: "create",
    group: KOPIUR_GROUP,
    resource: "restores",
    subresource: None,
};
const PATCH_POLICIES: Probe = Probe {
    verb: "patch",
    group: KOPIUR_GROUP,
    resource: "snapshotpolicies",
    subresource: None,
};
/// Opening a browse session execs into a mover pod.
const EXEC_SESSIONS: Probe = Probe {
    verb: "create",
    group: "",
    resource: "pods",
    subresource: Some("exec"),
};

/// **Pure.** Assemble the answer, given the identity and the probe results.
///
/// Split out so the projection is testable without an apiserver: everything
/// interesting about `/me` except the five booleans is a rename of
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
                namespace: namespace.map(str::to_string),
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
            sar.allowed(id, probe.verb, probe.resource, namespace, None)
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

    let can = Capabilities {
        create_snapshots: kopiur_probe(&app, &id, &client, &CREATE_SNAPSHOTS, namespace).await?,
        delete_snapshots: kopiur_probe(&app, &id, &client, &DELETE_SNAPSHOTS, namespace).await?,
        create_restores: kopiur_probe(&app, &id, &client, &CREATE_RESTORES, namespace).await?,
        patch_policies: kopiur_probe(&app, &id, &client, &PATCH_POLICIES, namespace).await?,
        // Always a `SelfSubjectAccessReview`: `pods/exec` is a core-group
        // subresource and `SarCache` asks only about the kopiur API group, so
        // routing it through the cache would silently answer a different
        // question.
        exec_sessions: self_review(&client, &EXEC_SESSIONS, namespace).await?,
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

    fn all_allowed() -> Capabilities {
        Capabilities {
            create_snapshots: true,
            delete_snapshots: true,
            create_restores: true,
            patch_policies: true,
            exec_sessions: true,
        }
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
        let me = view_me(
            &anonymous,
            None,
            Capabilities {
                create_snapshots: false,
                delete_snapshots: false,
                create_restores: false,
                patch_policies: false,
                exec_sessions: false,
            },
        );
        assert_eq!(
            me.source,
            IdentitySource::Anonymous,
            "a fallback to anonymous must be visible in the account menu, not silent"
        );
        assert!(me.email.is_none());
        assert!(!me.can.exec_sessions);
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
