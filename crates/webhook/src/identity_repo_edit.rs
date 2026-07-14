//! Repository-edit identity-change guard at admission.
//!
//! A `Repository`/`ClusterRepository`'s `identityDefaults` (`cluster`,
//! `hostnameExpr`, `usernameExpr`) is a CEL recipe every consumer `SnapshotPolicy`
//! that doesn't override identity resolves against — and it is resolved from the
//! LIVE repository on every reconcile/backup (identity is only pinned per-policy
//! at admission, never against the repository's defaults). So editing
//! `identityDefaults` on a repository with consumers that already have snapshot
//! history silently re-identifies all of them on their very next backup, with
//! **no per-policy edit** to acknowledge it: new snapshots land under a new kopia
//! source while the old history orphans (GFS retention treats it as a separate
//! source and never prunes the old set). This closes that hole, mirroring
//! [`crate::identity_fork`]'s per-policy guard but fleet-wide.
//!
//! ## Pure core + thin IO (mirrors [`crate::identity_fork`] / [`crate::identity_collision`])
//!
//! - The decision is the pure, unit-tested
//!   [`api::validate::detect_repository_identity_change`].
//! - [`check_repository_identity_change`] is the thin IO caller: it lists every
//!   `SnapshotPolicy` cluster-wide (exactly like
//!   [`crate::identity_collision::check_identity_collision`]), keeps the ones
//!   that reference the repository being edited, already have a successful
//!   snapshot, and don't pin both `username` and `hostname` explicitly (a fully
//!   pinned policy never consults the defaults), then calls the pure decision.
//! - It only fires when `identityDefaults` actually changed, and **degrades to
//!   allow** when it cannot make a confident decision (no client, or the LIST
//!   fails — e.g. a 403 under a namespaced Role install, or a transient apiserver
//!   blip) — the same fail-open posture as the collision/fork guards. A repository
//!   apply must never wedge on a best-effort admission check.
//!
//! ## Residual gap (by design, documented)
//!
//! A delete + re-apply of the repository is a CREATE, which carries no
//! `oldObject` to diff against, so this guard cannot fire on it — exactly like
//! the per-policy fork guard's CREATE gap. Nothing currently closes that gap; it
//! is a known, accepted limitation of admission-time guards in general.

use std::collections::BTreeMap;

use kopiur_api as api;

use api::common::Identity;
use api::consts::ALLOW_IDENTITY_CHANGE_ANNOTATION;
use api::error::ValidationError;
use api::snapshot_policy::SnapshotPolicy;
use kube::{Api, Client, ResourceExt};

/// Whether the incoming repository object acknowledges an intentional
/// `identityDefaults` change via the [`ALLOW_IDENTITY_CHANGE_ANNOTATION`] (any
/// non-empty value; presence-only — mirrors
/// [`crate::identity_fork::acknowledged`]).
fn acknowledged(annotations: Option<&BTreeMap<String, String>>) -> bool {
    annotations
        .and_then(|a| a.get(ALLOW_IDENTITY_CHANGE_ANNOTATION))
        .is_some_and(|v| !v.trim().is_empty())
}

/// Whether a consumer `SnapshotPolicy` is affected by a change to its
/// repository's `identityDefaults`: it must reference the repository being
/// edited (`policy_repo_key == self_key`, both computed by
/// [`crate::identity_collision::repo_key`]), have produced at least one
/// successful snapshot (else there is no history to orphan), and NOT pin both
/// `username` and `hostname` explicitly in `spec.identity` (a fully pinned
/// policy never consults the defaults, so a defaults edit can't re-identify it).
fn is_affected(
    policy_repo_key: &str,
    self_key: &str,
    has_history: bool,
    identity: Option<&Identity>,
) -> bool {
    if policy_repo_key != self_key || !has_history {
        return false;
    }
    let fully_pinned = identity.is_some_and(|i| i.username.is_some() && i.hostname.is_some());
    !fully_pinned
}

/// The `namespace/name` of every consumer `SnapshotPolicy` referencing the
/// repository keyed by `self_key` that already has snapshot history and isn't
/// fully pinned (see [`is_affected`]). Fails open (empty vec, with a
/// `tracing::warn!`) when the list call fails — a transient IO error must not
/// wedge a repository apply, and this is a best-effort guard.
async fn affected_consumers(client: &Client, self_key: &str) -> Vec<String> {
    let api: Api<SnapshotPolicy> = Api::all(client.clone());
    let policies = match api.list(&Default::default()).await {
        Ok(list) => list,
        Err(error) => {
            tracing::warn!(
                repo = self_key,
                %error,
                "listing SnapshotPolicy consumers for the identityDefaults edit guard failed; \
                 degrading to allow"
            );
            return Vec::new();
        }
    };
    policies
        .into_iter()
        .filter_map(|p| {
            let ns = p.namespace()?;
            let key = crate::identity_collision::repo_key(&p.spec.repository, &ns);
            let has_history = p
                .status
                .as_ref()
                .is_some_and(|s| s.last_successful_snapshot.is_some());
            is_affected(&key, self_key, has_history, p.spec.identity.as_ref())
                .then(|| format!("{ns}/{}", p.name_any()))
        })
        .collect()
}

/// Outcome of the repository `identityDefaults`-edit guard: the deny error (if
/// any) plus the affected-consumer list, independent of whether the edit was
/// ultimately denied or allowed. [`api::validate::detect_repository_identity_change`]
/// already renders `consumers` into the deny message; the caller still needs the
/// same list when `error` is `None` and the change was acknowledged, to attach an
/// admission WARNING naming what was just re-identified.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityChangeOutcome {
    /// `Some` iff the edit is rejected.
    pub error: Option<ValidationError>,
    /// Affected consumers (`namespace/name`), whether or not the edit was
    /// ultimately allowed. Empty when `identityDefaults` didn't change, or the
    /// guard degraded to allow (no client / LIST failure).
    pub consumers: Vec<String>,
}

/// Check whether an UPDATE to a `Repository`/`ClusterRepository`'s
/// `identityDefaults` would re-identify a consumer `SnapshotPolicy` with
/// existing snapshot history. `self_key` is the edited repository's own
/// normalized key (build it with [`crate::identity_collision::repo_key`], e.g.
/// `"ClusterRepository/shared"` or `"Repository/billing/nas"`) so it compares
/// against each candidate policy's own resolved key the same way the collision
/// guard does.
///
/// Only does the LIST when `old_defaults != new_defaults` (a no-op edit never
/// needs to ask). Degrades to allow (empty consumers, no error, `tracing::warn!`)
/// when there is no client — a repository apply must not wedge on a webhook that
/// can't reach the API server for a best-effort check.
pub async fn check_repository_identity_change(
    client: Option<&Client>,
    self_key: &str,
    old_defaults: Option<&api::cluster_repository::IdentityDefaults>,
    new_defaults: Option<&api::cluster_repository::IdentityDefaults>,
    new_annotations: Option<&BTreeMap<String, String>>,
) -> IdentityChangeOutcome {
    if old_defaults == new_defaults {
        return IdentityChangeOutcome::default();
    }
    let Some(client) = client else {
        tracing::warn!(
            repo = self_key,
            "no client available for the identityDefaults edit guard; degrading to allow"
        );
        return IdentityChangeOutcome::default();
    };
    let consumers = affected_consumers(client, self_key).await;
    let acked = acknowledged(new_annotations);
    let error = api::validate::detect_repository_identity_change(
        old_defaults,
        new_defaults,
        acked,
        &consumers,
    );
    IdentityChangeOutcome { error, consumers }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledged_requires_non_empty_value() {
        let mut annotations = BTreeMap::new();
        assert!(!acknowledged(Some(&annotations)));
        assert!(!acknowledged(None));
        annotations.insert(
            ALLOW_IDENTITY_CHANGE_ANNOTATION.to_string(),
            "  ".to_string(),
        );
        assert!(!acknowledged(Some(&annotations)));
        annotations.insert(
            ALLOW_IDENTITY_CHANGE_ANNOTATION.to_string(),
            "intentional".to_string(),
        );
        assert!(acknowledged(Some(&annotations)));
    }

    #[test]
    fn is_affected_requires_same_repo_and_history_and_not_fully_pinned() {
        // Different repository → unaffected regardless of history.
        assert!(!is_affected(
            "ClusterRepository/other",
            "ClusterRepository/shared",
            true,
            None
        ));
        // Same repository, no history yet → unaffected (nothing to orphan).
        assert!(!is_affected(
            "ClusterRepository/shared",
            "ClusterRepository/shared",
            false,
            None
        ));
        // Same repository, history, no identity override → affected.
        assert!(is_affected(
            "ClusterRepository/shared",
            "ClusterRepository/shared",
            true,
            None
        ));
        // Fully pinned (both username AND hostname) → unaffected, the defaults are
        // never consulted.
        let pinned = Identity {
            username: Some("u".into()),
            hostname: Some("h".into()),
        };
        assert!(!is_affected(
            "ClusterRepository/shared",
            "ClusterRepository/shared",
            true,
            Some(&pinned)
        ));
        // Only ONE of username/hostname pinned → still affected (the other still
        // resolves through the defaults).
        let half_pinned = Identity {
            username: Some("u".into()),
            hostname: None,
        };
        assert!(is_affected(
            "ClusterRepository/shared",
            "ClusterRepository/shared",
            true,
            Some(&half_pinned)
        ));
    }

    #[tokio::test]
    async fn no_change_short_circuits_without_a_client() {
        // old == new (both None) → must not even ask for a client.
        let outcome =
            check_repository_identity_change(None, "ClusterRepository/shared", None, None, None)
                .await;
        assert_eq!(outcome, IdentityChangeOutcome::default());
    }

    #[tokio::test]
    async fn no_client_degrades_to_allow_on_a_real_change() {
        use api::cluster_repository::IdentityDefaults;
        let old = IdentityDefaults {
            cluster: Some("east".into()),
            ..Default::default()
        };
        let new = IdentityDefaults {
            cluster: Some("west".into()),
            ..Default::default()
        };
        let outcome = check_repository_identity_change(
            None,
            "ClusterRepository/shared",
            Some(&old),
            Some(&new),
            None,
        )
        .await;
        assert_eq!(outcome, IdentityChangeOutcome::default());
    }
}
