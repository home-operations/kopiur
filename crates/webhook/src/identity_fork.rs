//! Fork-on-edit guard at admission.
//!
//! Kopia records every snapshot under `username@hostname:sourcePath`. If a
//! `SnapshotPolicy` is *edited* so its resolved identity changes — renamed, a
//! namespace/label change feeding an `identityDefaults` CEL expression, a hand-edited
//! `spec.identity`, or a per-source `sourcePathOverride` change — new snapshots land
//! under the **new** kopia source while the old history stays under the old one:
//! restore/verify/`fromPolicy` resolve the new identity (old history is reachable only
//! via `Restore.spec.source.identity`), and old- and new-lineage `Snapshot` CRs keep
//! competing in the policy's one GFS retention timeline. That is a silent
//! re-identification, so the webhook **rejects** such an edit unless the operator
//! acknowledges it with the [`ALLOW_IDENTITY_CHANGE_ANNOTATION`].
//!
//! ## Pure core + thin IO (mirrors [`crate::identity_collision`])
//!
//! - The decisions are the pure, unit-tested [`api::validate::detect_identity_fork`]
//!   (single-repo, on the `username@hostname`),
//!   [`api::validate::detect_identity_fork_multi`] (multi-repo, over
//!   `{repo_key -> username@hostname}` maps — the unit of identity is the
//!   `(repository, identity-under-that-repo's-defaults)` pair), and
//!   [`api::validate::detect_source_path_fork`] (on each PVC source's path — paths are
//!   never CEL-driven, so an old-vs-new spec diff is complete).
//! - [`check_identity_fork`] is the thin IO caller: it reads the old object's pinned
//!   identity/per-repo baselines + history from `oldObject.status`, resolves the *new*
//!   identity per repository (fetching each referenced repository's `identityDefaults`
//!   for CEL), and calls the pure decisions.
//! - It only fires on UPDATE, only when the policy has real history
//!   (`status.lastSuccessfulSnapshot`), and **degrades to allow** when it cannot make
//!   a confident decision (no client, or no pinned baseline yet) — the same
//!   fail-open posture as the collision guard.
//!
//! ## The multi-repo shape is a HARD BRANCH, not a fallthrough
//!
//! A multi-repo policy's `status.resolved.identity` is `None` (per-repo identities
//! live in `status.resolved.repositories`), so the old single-repo code's early `?`
//! on the top-level identity would have silently admitted **every** re-identifying
//! edit for multi-repo policies. [`check_identity_fork`] branches explicitly on the
//! NEW spec's shape instead: the single-repo path behaves exactly as it always has
//! (with the per-repo baselines as a fallback for a multi→single edit), and the
//! multi-repo path compares per-repo baselines against freshly-resolved per-repo
//! identities. An added repository has no history to orphan (no fork); a removed
//! repository on a policy with history is surfaced as an admission **warning**
//! (its lineage stays in the repository, but this recipe no longer covers it, and
//! children pinned to it become terminal).

use std::collections::{BTreeMap, BTreeSet};

use kopiur_api as api;

use api::consts::ALLOW_IDENTITY_CHANGE_ANNOTATION;
use api::error::ValidationError;
use api::snapshot_policy::{PolicyRepositories, SnapshotPolicySpec, SnapshotPolicyStatus};
use kube::Client;

use api::common::{IdentityDefaults, RepositoryRef};

use crate::identity_collision::{RepoDefaults, repo_key};

/// Whether the incoming object acknowledges an intentional re-identification via the
/// [`ALLOW_IDENTITY_CHANGE_ANNOTATION`] (any non-empty value; presence-only).
fn acknowledged(annotations: Option<&BTreeMap<String, String>>) -> bool {
    annotations
        .and_then(|a| a.get(ALLOW_IDENTITY_CHANGE_ANNOTATION))
        .is_some_and(|v| !v.trim().is_empty())
}

/// Outcome of the fork-on-edit guard: the deny error (if any) plus non-blocking
/// admission warnings (currently: a repository removed from a policy that
/// already has snapshot history). Warnings ride whether or not the edit is
/// denied for another reason — the caller attaches them only to an allowed
/// response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkOutcome {
    /// `Some` iff the edit is rejected.
    pub error: Option<ValidationError>,
    /// Non-blocking warnings to attach when the edit is allowed.
    pub warnings: Vec<String>,
}

impl ForkOutcome {
    fn deny(error: ValidationError, warnings: Vec<String>) -> Self {
        Self {
            error: Some(error),
            warnings,
        }
    }
}

/// `username@hostname` for a pinned [`api::common::ResolvedIdentity`].
fn uh(id: &api::common::ResolvedIdentity) -> String {
    format!("{}@{}", id.username, id.hostname)
}

/// The old object's per-repo identity baselines: `repo_key` →
/// `username@hostname`, from `status.resolved.repositories`, plus the top-level
/// `status.resolved.identity` keyed under the OLD spec's single repository when
/// that shape applies (the single→multi edit case: the old policy pinned only
/// the top-level identity, and it belongs to the old single repo's lineage).
fn old_baselines(
    old_spec: &SnapshotPolicySpec,
    old_status: &SnapshotPolicyStatus,
    namespace: &str,
) -> BTreeMap<String, String> {
    let resolved = old_status.resolved.as_ref();
    let mut baselines: BTreeMap<String, String> = resolved
        .map(|r| {
            r.repositories
                .iter()
                .filter_map(|e| {
                    e.identity
                        .as_ref()
                        .map(|id| (repo_key(&e.repository, namespace), uh(id)))
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(top) = resolved.and_then(|r| r.identity.as_ref())
        && let Ok(PolicyRepositories::Single(old_repo)) = api::policy_repositories(old_spec)
    {
        baselines
            .entry(repo_key(old_repo, namespace))
            .or_insert_with(|| uh(top));
    }
    baselines
}

/// The removed-repository warning (multi-repo shapes only): repositories the
/// OLD spec named that the NEW spec no longer does, on a policy with history.
/// Their kopia lineages stay in those repositories, but this recipe stops
/// covering them (no new snapshots, no retention/verification), and existing
/// children pinned to a removed member become terminal
/// (`SnapshotPinNotInPolicy`). Single→single repository swaps keep their
/// historical silence — this fires only when either side is multi-repo.
fn removed_repo_warning(
    old_spec: &SnapshotPolicySpec,
    new_spec: &SnapshotPolicySpec,
    namespace: &str,
) -> Option<String> {
    if !api::is_multi_repo(old_spec) && !api::is_multi_repo(new_spec) {
        return None;
    }
    let new_keys: BTreeSet<String> = api::repository_refs(new_spec)
        .map(|r| repo_key(r, namespace))
        .collect();
    let removed: BTreeSet<String> = api::repository_refs(old_spec)
        .map(|r| repo_key(r, namespace))
        .filter(|k| !new_keys.contains(k))
        .collect();
    if removed.is_empty() {
        return None;
    }
    Some(format!(
        "this edit removes repository(ies) {} from a policy with snapshot history: their \
         snapshots stay there but get no new backups, retention, or verification, and \
         Snapshot CRs pinned to them will fail. If unintended, restore the entry under \
         spec.repositories",
        removed.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

/// Check whether an UPDATE to a `SnapshotPolicy` would fork its kopia identity while it
/// already has snapshot history. Returns a [`ForkOutcome`]: the
/// [`ValidationError::IdentityWouldFork`] /
/// [`ValidationError::IdentityWouldForkInRepository`] to reject with (or `None` to
/// allow), plus any non-blocking warnings.
///
/// `old_spec`/`old_status` come from the admission request's `oldObject`. The guard:
/// - allows when the change is acknowledged, or the policy has no successful snapshot
///   yet (`status.lastSuccessfulSnapshot` unset → no history to orphan);
/// - checks per-source path forks purely (no client);
/// - checks the `username@hostname` fork by resolving the new identity **per
///   repository** (each needs that repository's `identityDefaults` for CEL), so
///   without a client, or without a baseline for a repository, it degrades to allow
///   for that repository.
#[allow(clippy::too_many_arguments)]
pub async fn check_identity_fork(
    client: Option<&Client>,
    name: &str,
    namespace: &str,
    new_spec: &SnapshotPolicySpec,
    new_labels: Option<&BTreeMap<String, String>>,
    new_annotations: Option<&BTreeMap<String, String>>,
    old_spec: &SnapshotPolicySpec,
    old_status: &SnapshotPolicyStatus,
    old_labels: Option<&BTreeMap<String, String>>,
    old_annotations: Option<&BTreeMap<String, String>>,
) -> ForkOutcome {
    let acked = acknowledged(new_annotations);
    let has_history = old_status.last_successful_snapshot.is_some();
    if !has_history {
        return ForkOutcome::default();
    }

    // Removed-repo warning: informational, so it rides even an acknowledged edit.
    let warnings: Vec<String> = removed_repo_warning(old_spec, new_spec, namespace)
        .into_iter()
        .collect();

    if acked {
        return ForkOutcome {
            error: None,
            warnings,
        };
    }

    // Per-source path fork: pure spec diff, no client required.
    if let Some(e) = api::validate::detect_source_path_fork(old_spec, new_spec, has_history, acked)
    {
        return ForkOutcome::deny(e, warnings);
    }

    // username@hostname fork: compare the previously-pinned baseline(s) against the
    // freshly-resolved identity(ies). Resolving needs each referenced repository's
    // identityDefaults (CEL), so without a client we degrade to allow.
    let Some(client) = client else {
        return ForkOutcome {
            error: None,
            warnings,
        };
    };
    let baselines = old_baselines(old_spec, old_status, namespace);

    // HARD BRANCH on the new spec's repository shape (see module doc): the
    // single-repo arm preserves the historical comparison exactly; the
    // multi-repo arm is the per-repo map comparison. A garbage stored shape
    // (both/neither set) is denied with the shared validator's error rather
    // than silently admitting a re-identifying edit.
    let inputs = ForkInputs {
        name,
        namespace,
        new_spec,
        new_labels,
        new_annotations,
        old_spec,
        old_labels,
        old_annotations,
    };
    let error = match api::policy_repositories(new_spec) {
        Err(e) => Some(e),
        Ok(PolicyRepositories::Single(repo)) => {
            // EXACTLY the historical path: baseline = the pinned top-level
            // identity; the per-repo baseline for this repository is the
            // multi→single fallback (the top-level pin is None on a policy
            // that was multi-repo). No baseline at all → degrade to allow.
            let old_uh = old_status
                .resolved
                .as_ref()
                .and_then(|r| r.identity.as_ref())
                .map(uh)
                .or_else(|| baselines.get(&repo_key(repo, namespace)).cloned());
            match old_uh {
                None => None,
                Some(old_uh) => match fork_comparison(client, repo, &inputs, old_uh).await {
                    ForkCheck::Compare { old, new } => {
                        api::validate::detect_identity_fork(&old, &new, has_history, acked)
                    }
                    ForkCheck::Degrade => None,
                    ForkCheck::Uncheckable { old } => {
                        Some(ValidationError::IdentityForkUncheckable {
                            repo: repo_key(repo, namespace),
                            old,
                        })
                    }
                },
            }
        }
        Ok(PolicyRepositories::Multi(_)) => {
            multi_repo_fork(client, &inputs, &baselines, has_history, acked).await
        }
    };
    ForkOutcome { error, warnings }
}

/// The multi-repo arm of [`check_identity_fork`]: resolve the NEW identity per
/// member repository, under that repository's identityDefaults, and fork iff any
/// member present in BOTH maps differs. An added repo has no baseline (no fork);
/// an unresolvable member is left out (degrade for that member only); a missing
/// member whose identity inputs changed is uncheckable and denied.
async fn multi_repo_fork(
    client: &Client,
    inputs: &ForkInputs<'_>,
    baselines: &BTreeMap<String, String>,
    has_history: bool,
    acked: bool,
) -> Option<ValidationError> {
    let mut effective_baselines = baselines.clone();
    let mut new_identities: BTreeMap<String, String> = BTreeMap::new();
    for repo in api::repository_refs(inputs.new_spec) {
        let key = repo_key(repo, inputs.namespace);
        let Some(old_uh) = baselines.get(&key).cloned() else {
            continue;
        };
        match fork_comparison(client, repo, inputs, old_uh).await {
            ForkCheck::Compare { old, new } => {
                effective_baselines.insert(key.clone(), old);
                new_identities.insert(key, new);
            }
            ForkCheck::Degrade => {}
            ForkCheck::Uncheckable { old } => {
                return Some(ValidationError::IdentityForkUncheckable { repo: key, old });
            }
        }
    }
    api::validate::detect_identity_fork_multi(
        &effective_baselines,
        &new_identities,
        has_history,
        acked,
    )
}

/// The old and new objects' identity inputs (bundled to keep
/// [`fork_comparison`]'s signature readable).
struct ForkInputs<'a> {
    name: &'a str,
    namespace: &'a str,
    new_spec: &'a SnapshotPolicySpec,
    new_labels: Option<&'a BTreeMap<String, String>>,
    new_annotations: Option<&'a BTreeMap<String, String>>,
    old_spec: &'a SnapshotPolicySpec,
    old_labels: Option<&'a BTreeMap<String, String>>,
    old_annotations: Option<&'a BTreeMap<String, String>>,
}

/// What [`fork_comparison`] decided for one repository. A closed set, matched
/// exhaustively by both arms of [`check_identity_fork`].
enum ForkCheck {
    /// Compare these two `username@hostname` values; a difference is a fork.
    Compare { old: String, new: String },
    /// The new identity cannot be resolved: degrade to allow for this repository.
    Degrade,
    /// The repository is missing and an identity input its (unreadable)
    /// `identityDefaults` could render from changed: the edit cannot be shown to
    /// preserve the pinned identity `old`, so it is denied unless acknowledged.
    Uncheckable { old: String },
}

/// Annotations that are never identity inputs, ignored when deciding whether a
/// missing repository's `identityDefaults` could see a change:
/// - kubectl's `last-applied-configuration` mirrors the object itself (its labels
///   are compared directly; its spec is compared by resolution);
/// - the acknowledgement annotation is the release valve, not an input.
const NON_IDENTITY_ANNOTATIONS: &[&str] = &[
    "kubectl.kubernetes.io/last-applied-configuration",
    ALLOW_IDENTITY_CHANGE_ANNOTATION,
];

/// **Pure.** Whether any input a repository's `identityDefaults` could render a
/// policy identity from changed between the old and new object. The CEL
/// environment is `namespace`, `policyName` (both immutable on UPDATE),
/// `labels`, `annotations` and the defaults' own `cluster`; and a policy's
/// literal `spec.identity` override wins over the expression, so adding,
/// removing or changing it changes which side of that precedence applies.
fn defaults_visible_inputs_changed(inputs: &ForkInputs<'_>) -> bool {
    let empty = BTreeMap::new();
    let labels = |l: Option<&BTreeMap<String, String>>| l.unwrap_or(&empty).clone();
    let annotations = |a: Option<&BTreeMap<String, String>>| {
        let mut a = a.unwrap_or(&empty).clone();
        a.retain(|k, _| !NON_IDENTITY_ANNOTATIONS.contains(&k.as_str()));
        a
    };
    labels(inputs.old_labels) != labels(inputs.new_labels)
        || annotations(inputs.old_annotations) != annotations(inputs.new_annotations)
        || inputs.old_spec.identity != inputs.new_spec.identity
}

/// The verdict for one repository.
///
/// Exhaustive over what the admission-time read of the repository returned:
///
/// - **`Known`** — the historical comparison: the pinned baseline vs the new
///   spec resolved under the repository's live `identityDefaults`.
/// - **`Unreadable`** — historical too: resolved with no defaults.
/// - **`Missing`** (issue #677) — the repository does not exist yet, so its
///   `identityDefaults` cannot be seen, and comparing the pin (which may have been
///   rendered under them) against a no-defaults resolution would deny an
///   UNCHANGED re-apply — exactly the GitOps steady state. Instead:
///   - the OLD object is resolved against the NEW one, both with no defaults,
///     each with its OWN labels/annotations. A difference is a spec-driven fork
///     (a hostname override changed), caught exactly as before;
///   - when those agree but an input the defaults could read changed
///     ([`defaults_visible_inputs_changed`]), the edit is [`ForkCheck::Uncheckable`]:
///     admitting it would let the controller re-identify the policy, unguarded,
///     once the repository appears (PR #705 review);
///   - when nothing the identity is rendered from changed, the identity is the
///     same under ANY fixed `identityDefaults`, so the edit is safe to admit.
async fn fork_comparison(
    client: &Client,
    repo: &RepositoryRef,
    inputs: &ForkInputs<'_>,
    old_uh: String,
) -> ForkCheck {
    let resolve = |spec: &SnapshotPolicySpec,
                   labels: Option<&BTreeMap<String, String>>,
                   annotations: Option<&BTreeMap<String, String>>,
                   defaults: Option<&IdentityDefaults>| {
        crate::identity_collision::resolve_policy_identity(
            inputs.name,
            inputs.namespace,
            spec,
            labels,
            annotations,
            defaults,
        )
        .map(|id| uh(&id))
    };
    let new_under = |defaults: Option<&IdentityDefaults>| {
        resolve(
            inputs.new_spec,
            inputs.new_labels,
            inputs.new_annotations,
            defaults,
        )
    };
    let compare = |new: Option<String>, old: String| match new {
        Some(new) => ForkCheck::Compare { old, new },
        None => ForkCheck::Degrade,
    };
    match crate::identity_collision::repo_defaults_lookup(client, repo, inputs.namespace).await {
        RepoDefaults::Known(defaults) => compare(new_under(defaults.as_ref()), old_uh),
        RepoDefaults::Unreadable => compare(new_under(None), old_uh),
        RepoDefaults::Missing => {
            let old = resolve(
                inputs.old_spec,
                inputs.old_labels,
                inputs.old_annotations,
                None,
            );
            let (Some(old), Some(new)) = (old, new_under(None)) else {
                return ForkCheck::Degrade;
            };
            if old != new {
                ForkCheck::Compare { old, new }
            } else if defaults_visible_inputs_changed(inputs) {
                ForkCheck::Uncheckable { old: old_uh }
            } else {
                ForkCheck::Compare { old, new }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn spec(v: Value) -> SnapshotPolicySpec {
        serde_json::from_value(v).expect("spec fixture decodes")
    }

    fn status(v: Value) -> SnapshotPolicyStatus {
        serde_json::from_value(v).expect("status fixture decodes")
    }

    /// Repositories a/b in the policy's namespace.
    fn multi_spec_with_identity(hostname: Option<&str>) -> SnapshotPolicySpec {
        let mut s = json!({
            "repositories": [
                { "kind": "Repository", "name": "a" },
                { "kind": "Repository", "name": "b" },
            ],
            "sources": [ { "pvc": { "name": "data" } } ],
        });
        if let Some(h) = hostname {
            s["identity"] = json!({ "hostname": h });
        }
        spec(s)
    }

    /// A multi-repo status: per-repo pins for a/b (username `pg`, hostname =
    /// the policy namespace `billing`), top-level identity None, with history.
    fn multi_status_with_history() -> SnapshotPolicyStatus {
        status(json!({
            "resolved": { "repositories": [
                {
                    "repository": { "kind": "Repository", "name": "a" },
                    "identity": { "username": "pg", "hostname": "billing" },
                },
                {
                    "repository": { "kind": "Repository", "name": "b" },
                    "identity": { "username": "pg", "hostname": "billing" },
                },
            ] },
            "lastSuccessfulSnapshot": "2026-06-19T00:00:00Z",
        }))
    }

    /// A `Client` whose every request returns a minimal `Repository` with no
    /// `identityDefaults` — enough for the per-repo defaults GETs the guard
    /// performs while resolving new identities.
    fn mock_repo_client() -> Client {
        let body = json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Repository",
            "metadata": { "name": "r", "namespace": "billing" },
            "spec": {
                "backend": { "filesystem": { "path": "/r" } },
                "encryption": { "passwordSecretRef": { "name": "s" } },
            }
        });
        let svc = tower::service_fn(move |_req: http::Request<kube::client::Body>| {
            let body = body.clone();
            async move {
                let resp = http::Response::builder()
                    .status(http::StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        Client::new(svc, "billing")
    }

    /// The audit's must-have: a multi-repo policy with history and an unacked
    /// re-identifying edit is DENIED — the old code's early `?` on the (None)
    /// top-level identity would have silently admitted this.
    #[tokio::test]
    async fn multi_repo_unacked_identity_change_with_history_is_denied() {
        let client = mock_repo_client();
        let outcome = check_identity_fork(
            Some(&client),
            "pg",
            "billing",
            &multi_spec_with_identity(Some("payments")), // pg@billing → pg@payments
            None,
            None,
            &multi_spec_with_identity(None),
            &multi_status_with_history(),
            None,
            None,
        )
        .await;
        match outcome.error {
            Some(ValidationError::IdentityWouldForkInRepository { repo, old, new }) => {
                assert_eq!(repo, "Repository/billing/a", "first differing member named");
                assert_eq!(old, "pg@billing");
                assert_eq!(new, "pg@payments");
            }
            other => panic!("expected IdentityWouldForkInRepository, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multi_repo_ack_annotation_releases_the_fork() {
        let client = mock_repo_client();
        let annotations: BTreeMap<String, String> = [(
            ALLOW_IDENTITY_CHANGE_ANNOTATION.to_string(),
            "intentional".to_string(),
        )]
        .into();
        let outcome = check_identity_fork(
            Some(&client),
            "pg",
            "billing",
            &multi_spec_with_identity(Some("payments")),
            None,
            Some(&annotations),
            &multi_spec_with_identity(None),
            &multi_status_with_history(),
            None,
            None,
        )
        .await;
        assert_eq!(outcome.error, None, "acked edit must be allowed");
    }

    #[tokio::test]
    async fn multi_repo_no_history_allows_the_change() {
        let client = mock_repo_client();
        let no_history = status(json!({
            "resolved": { "repositories": [
                {
                    "repository": { "kind": "Repository", "name": "a" },
                    "identity": { "username": "pg", "hostname": "billing" },
                },
            ] }
        }));
        let outcome = check_identity_fork(
            Some(&client),
            "pg",
            "billing",
            &multi_spec_with_identity(Some("payments")),
            None,
            None,
            &multi_spec_with_identity(None),
            &no_history,
            None,
            None,
        )
        .await;
        assert_eq!(outcome, ForkOutcome::default());
    }

    /// An ADDED repository has no baseline → no fork. Single→multi: the old
    /// top-level identity is keyed under the OLD single repo, matches the new
    /// resolution for that member, and the new member is ignored.
    #[tokio::test]
    async fn added_repository_is_not_a_fork() {
        let client = mock_repo_client();
        let old_spec = spec(json!({
            "repository": { "kind": "Repository", "name": "a" },
            "sources": [ { "pvc": { "name": "data" } } ],
        }));
        let old_status = status(json!({
            "resolved": { "identity": { "username": "pg", "hostname": "billing" } },
            "lastSuccessfulSnapshot": "2026-06-19T00:00:00Z",
        }));
        let outcome = check_identity_fork(
            Some(&client),
            "pg",
            "billing",
            &multi_spec_with_identity(None), // a + b, identity unchanged
            None,
            None,
            &old_spec,
            &old_status,
            None,
            None,
        )
        .await;
        assert_eq!(outcome.error, None, "added repo must not fork");
        assert!(
            outcome.warnings.is_empty(),
            "nothing was removed: {:?}",
            outcome.warnings
        );
    }

    /// …but the same single→multi edit that ALSO re-identifies the retained
    /// member is still caught via the top-level-identity fallback baseline.
    #[tokio::test]
    async fn single_to_multi_edit_with_identity_change_is_denied() {
        let client = mock_repo_client();
        let old_spec = spec(json!({
            "repository": { "kind": "Repository", "name": "a" },
            "sources": [ { "pvc": { "name": "data" } } ],
        }));
        let old_status = status(json!({
            "resolved": { "identity": { "username": "pg", "hostname": "billing" } },
            "lastSuccessfulSnapshot": "2026-06-19T00:00:00Z",
        }));
        let outcome = check_identity_fork(
            Some(&client),
            "pg",
            "billing",
            &multi_spec_with_identity(Some("payments")),
            None,
            None,
            &old_spec,
            &old_status,
            None,
            None,
        )
        .await;
        match outcome.error {
            Some(ValidationError::IdentityWouldForkInRepository { repo, .. }) => {
                assert_eq!(repo, "Repository/billing/a");
            }
            other => panic!("expected IdentityWouldForkInRepository, got {other:?}"),
        }
    }

    /// Removing a member from a multi-repo policy with history warns (never
    /// denies), naming the removed repository. No client needed — the warning
    /// is a pure spec diff.
    #[tokio::test]
    async fn removed_repository_with_history_warns() {
        let new_spec = spec(json!({
            "repositories": [ { "kind": "Repository", "name": "a" } ],
            "sources": [ { "pvc": { "name": "data" } } ],
        }));
        let outcome = check_identity_fork(
            None,
            "pg",
            "billing",
            &new_spec,
            None,
            None,
            &multi_spec_with_identity(None),
            &multi_status_with_history(),
            None,
            None,
        )
        .await;
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(
            outcome.warnings[0].contains("Repository/billing/b"),
            "must name the removed repository: {:?}",
            outcome.warnings
        );
        assert!(
            !outcome.warnings[0].contains("Repository/billing/a,"),
            "must not name the retained repository: {:?}",
            outcome.warnings
        );
    }

    /// A single→single repository swap keeps its historical silence: no
    /// removed-repo warning, and (identity unchanged) no fork.
    #[tokio::test]
    async fn single_repo_swap_stays_silent() {
        let client = mock_repo_client();
        let old_spec = spec(json!({
            "repository": { "kind": "Repository", "name": "a" },
            "sources": [ { "pvc": { "name": "data" } } ],
        }));
        let new_spec = spec(json!({
            "repository": { "kind": "Repository", "name": "b" },
            "sources": [ { "pvc": { "name": "data" } } ],
        }));
        let old_status = status(json!({
            "resolved": { "identity": { "username": "pg", "hostname": "billing" } },
            "lastSuccessfulSnapshot": "2026-06-19T00:00:00Z",
        }));
        let outcome = check_identity_fork(
            Some(&client),
            "pg",
            "billing",
            &new_spec,
            None,
            None,
            &old_spec,
            &old_status,
            None,
            None,
        )
        .await;
        assert_eq!(outcome, ForkOutcome::default());
    }

    /// Multi→single edit: the retained member's baseline comes from the
    /// per-repo entries (the top-level identity is None on a multi-repo
    /// policy), so a re-identifying multi→single edit is still denied — with
    /// the single-repo error, since the new shape is single.
    #[tokio::test]
    async fn multi_to_single_edit_with_identity_change_is_denied() {
        let client = mock_repo_client();
        let new_spec = spec(json!({
            "repository": { "kind": "Repository", "name": "a" },
            "identity": { "hostname": "payments" },
            "sources": [ { "pvc": { "name": "data" } } ],
        }));
        let outcome = check_identity_fork(
            Some(&client),
            "pg",
            "billing",
            &new_spec,
            None,
            None,
            &multi_spec_with_identity(None),
            &multi_status_with_history(),
            None,
            None,
        )
        .await;
        match outcome.error {
            Some(ValidationError::IdentityWouldFork { old, new }) => {
                assert_eq!(old, "pg@billing");
                assert_eq!(new, "pg@payments");
            }
            other => panic!("expected IdentityWouldFork, got {other:?}"),
        }
        // The dropped member b is surfaced as the removed-repo warning.
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("Repository/billing/b")),
            "{:?}",
            outcome.warnings
        );
    }

    /// No client on a multi-repo edit → degrade to allow (no new identities
    /// resolvable), same fail-open posture as the single-repo arm.
    #[tokio::test]
    async fn multi_repo_without_client_degrades_to_allow() {
        let outcome = check_identity_fork(
            None,
            "pg",
            "billing",
            &multi_spec_with_identity(Some("payments")),
            None,
            None,
            &multi_spec_with_identity(None),
            &multi_status_with_history(),
            None,
            None,
        )
        .await;
        assert_eq!(outcome.error, None, "no client must degrade to allow");
    }
}
