//! `GET /api/v1/namespaces` — the namespaces that hold kopiur objects the
//! caller may see, with how many each holds.
//!
//! The namespace switcher's source. It lists every namespaced kopiur kind
//! through [`Source::list`](crate::cache::Source::list) cluster-wide, which is
//! SAR-filtered per identity exactly like each list endpoint: a caller sees a
//! namespace only if they may list something in it, and a caller permitted
//! nowhere gets `[]`. A namespace with no kopiur objects is not listed — the
//! console has nothing to show there.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::{Json, Router, routing::get};
use serde::Deserialize;

use kopiur_api::{
    Maintenance, Repository, RepositoryReplication, Restore, Snapshot, SnapshotPolicy,
    SnapshotReplication, SnapshotSchedule,
};
use kopiur_ui_model::views::NamespaceSummary;

use crate::AppState;
use crate::api::problem::ApiError;
use crate::api::{UiQuery, client_for};
use crate::auth::CurrentIdentity;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/namespaces", get(list))
}

/// An endpoint that takes no parameters still refuses one, for the same reason
/// every read query does: a parameter silently ignored reads as one obeyed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoQuery {}

/// **Pure.** Count objects per namespace, sorted by name. An empty namespace
/// string (a cluster-scoped object) is not a namespace.
pub fn tally(namespaces: impl IntoIterator<Item = String>) -> Vec<NamespaceSummary> {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for ns in namespaces.into_iter().filter(|ns| !ns.is_empty()) {
        *counts.entry(ns).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(name, objects)| NamespaceSummary { name, objects })
        .collect()
}

/// `GET /api/v1/namespaces`
async fn list(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(_): UiQuery<NoQuery>,
) -> Result<Json<Vec<NamespaceSummary>>, ApiError> {
    let client = client_for(&app, &id)?;
    let src = &app.source;
    let mut seen: Vec<String> = Vec::new();
    macro_rules! namespaces_of {
        ($kind:ty) => {
            seen.extend(
                src.list::<$kind>(&id, &client, None)
                    .await?
                    .iter()
                    .filter_map(|o| o.metadata.namespace.clone()),
            )
        };
    }
    namespaces_of!(Repository);
    namespaces_of!(SnapshotPolicy);
    namespaces_of!(SnapshotSchedule);
    namespaces_of!(Snapshot);
    namespaces_of!(Restore);
    namespaces_of!(Maintenance);
    namespaces_of!(RepositoryReplication);
    namespaces_of!(SnapshotReplication);
    Ok(Json(tally(seen)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn tally_counts_per_namespace_sorted_by_name() {
        let got = tally(ns(&["media", "kopiur-dev", "media", "billing", "media"]));
        assert_eq!(
            got,
            vec![
                NamespaceSummary {
                    name: "billing".into(),
                    objects: 1
                },
                NamespaceSummary {
                    name: "kopiur-dev".into(),
                    objects: 1
                },
                NamespaceSummary {
                    name: "media".into(),
                    objects: 3
                },
            ]
        );
    }

    /// A caller permitted nowhere sees nothing — an empty list, not an error,
    /// so the switcher still offers "all namespaces".
    #[test]
    fn nothing_visible_is_an_empty_list() {
        assert!(tally(Vec::<String>::new()).is_empty());
    }

    /// An object with no namespace (a cluster-scoped kind leaking in) is not a
    /// namespace called "".
    #[test]
    fn an_empty_namespace_is_not_counted() {
        assert_eq!(
            tally(ns(&["", "media"])),
            vec![NamespaceSummary {
                name: "media".into(),
                objects: 1
            }]
        );
    }
}
