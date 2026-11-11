//! Cross-namespace `ClusterRepository` tenancy enforcement (ADR §3.2).
//!
//! The validating webhook is the FIRST enforcement point for `allowedNamespaces`
//! on a `ClusterRepository`: consumer CRs (`SnapshotPolicy`, `Restore`,
//! `Maintenance`, both replication kinds, a seeding `Repository`) that reference
//! a `ClusterRepository` have their namespace gated here. The controller is the
//! second: it re-evaluates the same gate before it does work against the
//! repository, which is what lets admission defer the check when the repository
//! does not exist yet (issue #677).
//!
//! ## Pure core + thin IO (mirrors the api-crate pattern)
//!
//! - [`evaluate_tenancy`] — a **pure function** taking the resolved
//!   `ClusterRepository` spec + the consumer namespace's labels as inputs, a typed
//!   adapter over the shared fail-closed
//!   [`api::validate::validate_consumer_against_cluster_repo`]:
//!   * `matchExpressions` are evaluated; an unknown operator denies.
//!   * labels unresolvable → deny (fail closed).
//! - [`resolve_tenancy_inputs`] — the **thin IO caller** that fetches the
//!   `ClusterRepository` and the consumer `Namespace`'s labels via a
//!   [`kube::Client`], then calls [`evaluate_tenancy`]. Any fetch failure (client
//!   absent, namespace not found, API error) becomes a deny — EXCEPT a
//!   `ClusterRepository` that does not exist (yet): that is
//!   [`TenancyDecision::RepoMissing`], which the handlers ADMIT with a warning
//!   (issue #677). There is no gate to evaluate until the repository exists, and
//!   GitOps apply order must not be enforced at admission; the controller
//!   re-evaluates the same gate (via the shared
//!   [`api::validate::validate_consumer_against_cluster_repo`]) before it does any
//!   work against the repository, so admitting here never opens the boundary.

use kopiur_api as api;

use crate::referent::{self, Referent};
use api::cluster_repository::{AllowedNamespaces, ClusterRepository};
use k8s_openapi::api::core::v1::Namespace;
use kube::{Api, Client};
use std::collections::BTreeMap;

/// Why a `ClusterRepository` tenancy check rejected a consumer reference. One
/// typed variant per denial mode (ADR §5.5) — each `Display` is the exact
/// user-facing reason surfaced verbatim in the `kubectl apply` rejection, and
/// the fail-closed resolver errors keep their `kube::Error` source.
#[derive(Debug, thiserror::Error)]
pub enum TenancyDenial {
    /// The consumer namespace is simply not in the gate (list miss, `All(false)`,
    /// or an unsatisfied selector).
    #[error(
        "namespace {consumer_namespace:?} is not in the allowedNamespaces of ClusterRepository \
         {repo_name:?}"
    )]
    NotAllowed {
        /// The consumer CR's namespace.
        consumer_namespace: String,
        /// The referenced `ClusterRepository`.
        repo_name: String,
    },

    /// A `Selector` gate could not be evaluated because the namespace's labels
    /// were unresolvable (fail-closed).
    #[error(
        "ClusterRepository {repo_name:?} selects namespaces by label, but the labels of \
         namespace {consumer_namespace:?} could not be read; denying"
    )]
    SelectorLabelsUnresolved {
        /// The consumer CR's namespace.
        consumer_namespace: String,
        /// The referenced `ClusterRepository`.
        repo_name: String,
    },

    /// The webhook has no Kubernetes client to resolve the gate (fail-closed).
    #[error(
        "cannot check ClusterRepository {repo_name:?} access for namespace \
         {consumer_namespace:?}: the webhook has no Kubernetes client; denying"
    )]
    NoClient {
        /// The consumer CR's namespace.
        consumer_namespace: String,
        /// The referenced `ClusterRepository`.
        repo_name: String,
    },

    /// The referenced `ClusterRepository` could not be fetched (fail-closed).
    #[error(
        "cannot read ClusterRepository {repo_name:?} (referenced from namespace \
         {consumer_namespace:?}): {source}; denying"
    )]
    RepoUnresolvable {
        /// The consumer CR's namespace.
        consumer_namespace: String,
        /// The referenced `ClusterRepository`.
        repo_name: String,
        /// The kube fetch failure.
        #[source]
        source: Box<kube::Error>,
    },

    /// The consumer namespace's labels could not be fetched for a `Selector`
    /// gate (fail-closed).
    #[error(
        "cannot read the labels of namespace {consumer_namespace:?} for ClusterRepository \
         {repo_name:?}: {source}; denying"
    )]
    NamespaceLabelsUnresolvable {
        /// The consumer CR's namespace.
        consumer_namespace: String,
        /// The referenced `ClusterRepository`.
        repo_name: String,
        /// The kube fetch failure.
        #[source]
        source: Box<kube::Error>,
    },

    /// The admission request carried no consumer namespace at all (fail-closed).
    #[error(
        "the admission request has no namespace, so ClusterRepository access cannot be \
         checked; denying"
    )]
    NoConsumerNamespace,
}

/// The outcome of a tenancy evaluation. `Deny` carries the typed reason whose
/// `Display` is surfaced verbatim in the `kubectl apply` rejection.
#[derive(Debug)]
pub enum TenancyDecision {
    /// The consumer namespace is permitted to reference the `ClusterRepository`.
    Allow,
    /// The reference is rejected for the given typed reason.
    Deny(TenancyDenial),
    /// The referenced `ClusterRepository` does not exist (the API server answered
    /// 404). Admitted with a warning (issue #677): the gate is evaluated by the
    /// controller once the repository exists.
    RepoMissing {
        /// The referenced (absent) `ClusterRepository`.
        repo_name: String,
    },
}

impl TenancyDecision {
    /// `true` iff this is [`TenancyDecision::Allow`].
    ///
    /// ```
    /// use kopiur_webhook::tenancy::{TenancyDecision, TenancyDenial};
    /// assert!(TenancyDecision::Allow.is_allow());
    /// assert!(!TenancyDecision::Deny(TenancyDenial::NoConsumerNamespace).is_allow());
    /// ```
    pub fn is_allow(&self) -> bool {
        matches!(self, TenancyDecision::Allow)
    }
}

/// Pure tenancy decision. **Fails closed.**
///
/// `consumer_namespace` is the namespace of the consumer CR being admitted;
/// `repo_name` is the referenced `ClusterRepository`; `allowed` is its
/// `allowedNamespaces` gate; `labels` are the consumer namespace's labels (the
/// caller resolved them from a `Namespace` get — `None` means "could not resolve").
///
/// A thin typed adapter over the shared, fail-closed
/// [`api::validate::validate_consumer_against_cluster_repo`] — the SAME function
/// the controller evaluates before doing work for a consumer it could not gate at
/// admission (issue #677), so the two enforcement points can never disagree.
/// `Selector` with no resolvable labels denies, every `matchExpressions` term is
/// evaluated, and an unknown operator denies.
pub fn evaluate_tenancy(
    consumer_namespace: &str,
    repo_name: &str,
    allowed: &AllowedNamespaces,
    labels: Option<&BTreeMap<String, String>>,
) -> TenancyDecision {
    use api::error::ValidationError;
    match api::validate::validate_consumer_against_cluster_repo(
        consumer_namespace,
        repo_name,
        allowed,
        labels,
    ) {
        Ok(()) => TenancyDecision::Allow,
        Err(ValidationError::SelectorLabelsUnavailable { .. }) => {
            TenancyDecision::Deny(TenancyDenial::SelectorLabelsUnresolved {
                consumer_namespace: consumer_namespace.to_string(),
                repo_name: repo_name.to_string(),
            })
        }
        // `ConsumerNamespaceNotAllowed` is the only other error the validator
        // returns; anything else would still be a refusal, so it denies too.
        Err(_) => TenancyDecision::Deny(TenancyDenial::NotAllowed {
            consumer_namespace: consumer_namespace.to_string(),
            repo_name: repo_name.to_string(),
        }),
    }
}

/// Thin IO caller: fetch the referenced `ClusterRepository` and the consumer
/// `Namespace`'s labels, then delegate to [`evaluate_tenancy`]. **Fails closed** on
/// any error (no client, namespace missing, API error); a repository that does not
/// exist yet is [`TenancyDecision::RepoMissing`] (admit + warn, issue #677).
pub async fn resolve_tenancy_inputs(
    client: Option<&Client>,
    consumer_namespace: &str,
    repo_name: &str,
) -> TenancyDecision {
    let Some(client) = client else {
        return TenancyDecision::Deny(TenancyDenial::NoClient {
            consumer_namespace: consumer_namespace.to_string(),
            repo_name: repo_name.to_string(),
        });
    };

    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let crepo = match referent::fetch(&crepos, repo_name).await {
        Referent::Found(c) => c,
        Referent::Missing => {
            return TenancyDecision::RepoMissing {
                repo_name: repo_name.to_string(),
            };
        }
        Referent::Unreadable(e) => {
            return TenancyDecision::Deny(TenancyDenial::RepoUnresolvable {
                consumer_namespace: consumer_namespace.to_string(),
                repo_name: repo_name.to_string(),
                source: Box::new(e),
            });
        }
    };

    // Only fetch namespace labels when the gate actually needs them. For List/All the
    // labels are irrelevant, so a Namespace-get failure must not block an otherwise
    // valid reference.
    let labels = match &crepo.spec.allowed_namespaces {
        AllowedNamespaces::Selector(_) => {
            let nss: Api<Namespace> = Api::all(client.clone());
            match referent::fetch(&nss, consumer_namespace).await {
                Referent::Found(ns) => Some(ns.metadata.labels.clone().unwrap_or_default()),
                // The consumer's own namespace cannot really be absent while a CR
                // is being admitted into it; if it is, the labels are simply
                // unresolvable and the Selector gate fails closed.
                Referent::Missing => None,
                Referent::Unreadable(e) => {
                    return TenancyDecision::Deny(TenancyDenial::NamespaceLabelsUnresolvable {
                        consumer_namespace: consumer_namespace.to_string(),
                        repo_name: repo_name.to_string(),
                        source: Box::new(e),
                    });
                }
            }
        }
        _ => None,
    };

    evaluate_tenancy(
        consumer_namespace,
        repo_name,
        &crepo.spec.allowed_namespaces,
        labels.as_ref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, LabelSelectorRequirement};

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn list_membership_allows() {
        let allowed = AllowedNamespaces::List(vec!["billing".into(), "staging".into()]);
        assert!(evaluate_tenancy("billing", "repo", &allowed, None).is_allow());
    }

    #[test]
    fn list_non_membership_denies() {
        let allowed = AllowedNamespaces::List(vec!["billing".into()]);
        assert!(!evaluate_tenancy("evil", "repo", &allowed, None).is_allow());
    }

    #[test]
    fn all_true_allows_all_false_denies() {
        assert!(evaluate_tenancy("any", "repo", &AllowedNamespaces::All(true), None).is_allow());
        assert!(!evaluate_tenancy("any", "repo", &AllowedNamespaces::All(false), None).is_allow());
    }

    #[test]
    fn selector_match_labels_match_allows() {
        let sel = LabelSelector {
            match_labels: Some(labels(&[("kopiur.home-operations.com/tier", "enterprise")])),
            ..Default::default()
        };
        let allowed = AllowedNamespaces::Selector(sel);
        let ns_labels = labels(&[("kopiur.home-operations.com/tier", "enterprise")]);
        assert!(evaluate_tenancy("ns", "repo", &allowed, Some(&ns_labels)).is_allow());
    }

    #[test]
    fn selector_match_labels_mismatch_denies() {
        let sel = LabelSelector {
            match_labels: Some(labels(&[("kopiur.home-operations.com/tier", "enterprise")])),
            ..Default::default()
        };
        let allowed = AllowedNamespaces::Selector(sel);
        let ns_labels = labels(&[("kopiur.home-operations.com/tier", "free")]);
        assert!(!evaluate_tenancy("ns", "repo", &allowed, Some(&ns_labels)).is_allow());
    }

    #[test]
    fn selector_without_labels_fails_closed() {
        let allowed = AllowedNamespaces::Selector(LabelSelector::default());
        let d = evaluate_tenancy("ns", "repo", &allowed, None);
        assert!(!d.is_allow());
        match d {
            TenancyDecision::Deny(denial) => {
                assert!(matches!(
                    denial,
                    TenancyDenial::SelectorLabelsUnresolved { .. }
                ));
                assert!(denial.to_string().contains("denying"));
            }
            TenancyDecision::Allow | TenancyDecision::RepoMissing { .. } => unreachable!(),
        }
    }

    // --- TenancyDenial Display: the deny text reaches the user verbatim in the
    // `kubectl apply` rejection, so each variant's message is pinned here — the
    // typed refactor must not drift the historical strings. ---

    #[test]
    fn not_allowed_message_is_byte_identical_to_the_historical_string() {
        let d = TenancyDenial::NotAllowed {
            consumer_namespace: "evil".into(),
            repo_name: "shared".into(),
        };
        assert_eq!(
            d.to_string(),
            "namespace \"evil\" is not in the allowedNamespaces of ClusterRepository \"shared\""
        );
    }

    #[test]
    fn every_fail_closed_variant_says_so() {
        let denials = [
            TenancyDenial::SelectorLabelsUnresolved {
                consumer_namespace: "ns".into(),
                repo_name: "repo".into(),
            },
            TenancyDenial::NoClient {
                consumer_namespace: "ns".into(),
                repo_name: "repo".into(),
            },
            TenancyDenial::NoConsumerNamespace,
        ];
        for d in denials {
            let msg = d.to_string();
            assert!(msg.contains("denying"), "{msg}");
        }
    }

    #[test]
    fn selector_match_expressions_in_allows_and_denies() {
        let sel = LabelSelector {
            match_expressions: Some(vec![LabelSelectorRequirement {
                key: "kopiur.home-operations.com/tier".into(),
                operator: "In".into(),
                values: Some(vec!["gold".into(), "platinum".into()]),
            }]),
            ..Default::default()
        };
        let allowed = AllowedNamespaces::Selector(sel);
        assert!(
            evaluate_tenancy(
                "ns",
                "repo",
                &allowed,
                Some(&labels(&[("kopiur.home-operations.com/tier", "gold")]))
            )
            .is_allow()
        );
        assert!(
            !evaluate_tenancy(
                "ns",
                "repo",
                &allowed,
                Some(&labels(&[("kopiur.home-operations.com/tier", "bronze")]))
            )
            .is_allow()
        );
    }

    #[test]
    fn selector_match_expressions_exists_evaluated_not_fail_open() {
        // The api crate would treat matchExpressions as "no constraint" (fail open).
        // We evaluate it: Exists on a missing key must DENY (fail closed).
        let sel = LabelSelector {
            match_expressions: Some(vec![LabelSelectorRequirement {
                key: "kopiur.home-operations.com/team".into(),
                operator: "Exists".into(),
                values: None,
            }]),
            ..Default::default()
        };
        let allowed = AllowedNamespaces::Selector(sel);
        assert!(
            evaluate_tenancy(
                "ns",
                "repo",
                &allowed,
                Some(&labels(&[("kopiur.home-operations.com/team", "x")]))
            )
            .is_allow()
        );
        assert!(
            !evaluate_tenancy("ns", "repo", &allowed, Some(&labels(&[("other", "y")]))).is_allow()
        );
    }

    #[test]
    fn selector_unknown_operator_fails_closed() {
        let sel = LabelSelector {
            match_expressions: Some(vec![LabelSelectorRequirement {
                key: "k".into(),
                operator: "Bogus".into(),
                values: None,
            }]),
            ..Default::default()
        };
        let allowed = AllowedNamespaces::Selector(sel);
        assert!(!evaluate_tenancy("ns", "repo", &allowed, Some(&labels(&[("k", "v")]))).is_allow());
    }

    #[tokio::test]
    async fn resolve_without_client_fails_closed() {
        let d = resolve_tenancy_inputs(None, "ns", "repo").await;
        assert!(!d.is_allow());
    }

    /// A `Client` answering every request with `code`/`reason` as a Kubernetes
    /// `Status` body.
    fn status_client(code: u16, reason: &'static str) -> Client {
        let svc = tower::service_fn(move |_req: http::Request<kube::client::Body>| async move {
            let body = serde_json::json!({
                "apiVersion": "v1", "kind": "Status", "status": "Failure",
                "message": reason, "reason": reason, "code": code,
            });
            Ok::<_, std::convert::Infallible>(
                http::Response::builder()
                    .status(code)
                    .header("content-type", "application/json")
                    .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
        });
        Client::new(svc, "test-ns")
    }

    /// Issue #677: a ClusterRepository that does not exist yet is NOT a denial —
    /// there is no gate to evaluate until it exists, and the controller enforces
    /// it then.
    #[tokio::test]
    async fn a_missing_cluster_repository_is_repo_missing_not_a_denial() {
        let client = status_client(404, "NotFound");
        match resolve_tenancy_inputs(Some(&client), "billing", "onsite").await {
            TenancyDecision::RepoMissing { repo_name } => assert_eq!(repo_name, "onsite"),
            other => panic!("expected RepoMissing, got {other:?}"),
        }
    }

    /// Every OTHER read failure keeps failing closed.
    #[tokio::test]
    async fn an_unreadable_cluster_repository_still_denies() {
        for (code, reason) in [(403, "Forbidden"), (500, "InternalError")] {
            let client = status_client(code, reason);
            match resolve_tenancy_inputs(Some(&client), "billing", "onsite").await {
                TenancyDecision::Deny(TenancyDenial::RepoUnresolvable { .. }) => {}
                other => panic!("{code}: expected a RepoUnresolvable deny, got {other:?}"),
            }
        }
    }
}
