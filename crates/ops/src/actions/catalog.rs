//! Requests an on-demand repository catalog scan.
//!
//! A repository re-materializes its discovered snapshots when its spec changes
//! or (opt-in) on a timer. [`request_scan`] is the "do it now" button: it
//! stamps [`kopiur_api::consts::CATALOG_SCAN_REQUESTED_ANNOTATION`] with an
//! opaque RFC3339 token the repository reconciler honors exactly once
//! (equality against `status.catalog.scanRequestHonored`). Stamping the same
//! token twice is therefore a no-op, and a fresh `now` is a fresh request.

use chrono::{DateTime, SecondsFormat, Utc};
use kopiur_api::common::RepositoryKind;
use kopiur_api::{ClusterRepository, Repository};
use kube::api::{Api, Patch, PatchParams};

use crate::ctx::OpsCtx;
use crate::error::{OpsError, classify_kube};

/// The field manager every kopiur client-side write identifies itself with, so
/// `kubectl get -o yaml --show-managed-fields` names the tool that made the
/// change. Mirrors `kubectl kopiur maintenance run`'s merge patch; module-local
/// only until the shared layer grows one home for it.
const FIELD_MANAGER: &str = "kubectl-kopiur";

/// **Pure.** The merge patch that requests a catalog scan as of `now`.
///
/// The annotation VALUE is the token: second-precision UTC RFC3339, so two
/// requests inside the same second collapse into one honored scan rather than
/// re-triggering a bootstrap Job.
pub fn scan_patch(now: DateTime<Utc>) -> serde_json::Value {
    serde_json::json!({
        "metadata": {
            "annotations": {
                kopiur_api::consts::CATALOG_SCAN_REQUESTED_ANNOTATION: token(now),
            }
        }
    })
}

/// Stamp the catalog-scan request on one repository and return the token that
/// was written (so a caller can report it, or watch for it to be honored).
///
/// `namespace` selects the `Repository`'s namespace and defaults to the
/// context's; it is meaningless for the cluster-scoped `ClusterRepository` and
/// ignored there. Exhaustive over [`RepositoryKind`]: a new repository kind
/// cannot compile until its scan route is decided.
///
/// A merge patch (not a server-side apply) — the annotation is a request the
/// operator consumes, not a field this tool owns; an apply would fight the
/// controller for ownership of the whole annotations map. It still carries the
/// [`FIELD_MANAGER`], exactly as `kubectl kopiur maintenance run` does, so the
/// write is attributable.
pub async fn request_scan(
    ctx: &OpsCtx,
    kind: RepositoryKind,
    namespace: Option<&str>,
    name: &str,
    now: DateTime<Utc>,
) -> Result<String, OpsError> {
    let patch = Patch::Merge(scan_patch(now));
    let pp = PatchParams {
        field_manager: Some(FIELD_MANAGER.to_string()),
        ..Default::default()
    };
    match kind {
        RepositoryKind::Repository => {
            let ns = namespace.unwrap_or(ctx.namespace.as_str());
            let api: Api<Repository> = Api::namespaced(ctx.client.clone(), ns);
            api.patch(name, &pp, &patch).await.map_err(|e| {
                classify_kube(
                    "patch",
                    "Repository",
                    "repositories",
                    Some(ns),
                    Some(name),
                    e,
                )
            })?;
        }
        RepositoryKind::ClusterRepository => {
            let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
            api.patch(name, &pp, &patch).await.map_err(|e| {
                classify_kube(
                    "patch",
                    "ClusterRepository",
                    "clusterrepositories",
                    None,
                    Some(name),
                    e,
                )
            })?;
        }
    }
    Ok(token(now))
}

/// The opaque scan token for `now` — the annotation value [`scan_patch`] writes.
fn token(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn scan_patch_sets_the_promoted_annotation() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap();
        let v = scan_patch(now);
        assert_eq!(
            v["metadata"]["annotations"][kopiur_api::consts::CATALOG_SCAN_REQUESTED_ANNOTATION],
            "2026-09-08T12:00:00Z"
        );
    }

    #[test]
    fn the_patch_touches_nothing_but_that_one_annotation() {
        // A merge patch that carried anything else would silently rewrite spec
        // or clobber annotations the controller owns.
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap();
        let v = scan_patch(now);
        assert_eq!(
            v,
            serde_json::json!({
                "metadata": {
                    "annotations": {
                        "kopiur.home-operations.com/catalog-scan-requested-at":
                            "2026-09-08T12:00:00Z"
                    }
                }
            })
        );
    }

    #[test]
    fn the_returned_token_is_the_value_that_was_stamped() {
        // `request_scan` reports what it wrote; the reconciler honors that
        // exact string, so a drift here would make the report a lie.
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap();
        assert_eq!(
            serde_json::Value::String(token(now)),
            scan_patch(now)["metadata"]["annotations"]
                [kopiur_api::consts::CATALOG_SCAN_REQUESTED_ANNOTATION]
        );
    }

    #[test]
    fn sub_second_requests_collapse_onto_one_token() {
        // Second precision is deliberate: the token is honored once, and a
        // burst of clicks must not recreate the bootstrap Job per click.
        let base = chrono::Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap();
        let later = base + chrono::Duration::milliseconds(400);
        assert_eq!(token(base), token(later));
        assert_ne!(token(base), token(base + chrono::Duration::seconds(1)));
    }
}
