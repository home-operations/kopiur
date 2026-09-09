//! `GET /api/v1/events` — Kubernetes Events for a resource, read under the
//! caller's own identity.
//!
//! Events are not a Kopiur CRD, so they have no reflector store and no
//! [`crate::cache::KopiurKind`]: this is one of the three reads that goes
//! straight through the impersonated client. That is also why the query is
//! required to name an object — an unfiltered cluster-wide event list is a
//! firehose, and a UI that offered one would be asking the apiserver to page
//! through every namespace on every screen refresh.

use axum::extract::State;
use axum::{Json, Router, routing::get};
use k8s_openapi::api::events::v1::Event;
use kube::api::{Api, ListParams};
use serde::Deserialize;

use kopiur_ui_model::views::EventRow;

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::api::{UiQuery, client_for, rfc3339};
use crate::auth::CurrentIdentity;
use crate::auth::redact::redact_text;

/// The most events one screen renders. Kubernetes expires them after an hour by
/// default, so this is a display bound rather than a retention one.
const MAX_EVENTS: usize = 100;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/events", get(handler))
}

/// Which object's events to fetch.
///
/// `deny_unknown_fields`: all three parameters are required, so an extra one is
/// always a mistake — most likely `?name=` misspelled next to a `?namespace=`
/// that happens to parse, which would otherwise answer with a *different*
/// object's events rather than a 400.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventQuery {
    /// Namespace the object lives in.
    pub namespace: String,
    /// The object's kind, e.g. `Snapshot`.
    pub kind: String,
    /// The object's name.
    pub name: String,
}

/// **Pure.** The `regarding.*` field selector for one object.
///
/// Server-side, so the apiserver does the filtering: pulling every event in a
/// busy namespace back to filter client-side would move megabytes to show a
/// handful of rows.
pub fn field_selector(kind: &str, name: &str) -> String {
    format!("regarding.kind={kind},regarding.name={name}")
}

/// **Pure.** One event as a table row.
///
/// The timestamp is whichever of the three the writer set: `eventTime` is what
/// `events.k8s.io/v1` writes, and the deprecated fields are what a controller
/// still using the core API produces — a row with no time at all would sort to
/// the bottom forever.
///
/// `note` is redacted: an event message is free text a controller composed, and
/// a failing backup's event can quote the same kopia output the snapshot's log
/// tail does.
pub fn event_row(e: &Event) -> EventRow {
    let regarding = e
        .regarding
        .as_ref()
        .map(|r| {
            format!(
                "{}/{}/{}",
                r.kind.clone().unwrap_or_default(),
                r.namespace.clone().unwrap_or_default(),
                r.name.clone().unwrap_or_default()
            )
        })
        .unwrap_or_default();
    EventRow {
        time: e
            .event_time
            .as_ref()
            .and_then(micro_rfc3339)
            .or_else(|| e.deprecated_last_timestamp.as_ref().and_then(rfc3339))
            .or_else(|| e.deprecated_first_timestamp.as_ref().and_then(rfc3339))
            .or_else(|| e.metadata.creation_timestamp.as_ref().and_then(rfc3339)),
        r#type: e.type_.clone(),
        reason: e.reason.clone(),
        message: e.note.as_deref().map(redact_text),
        regarding,
    }
}

/// **Pure.** A `MicroTime` as RFC 3339.
fn micro_rfc3339(t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime) -> Option<String> {
    chrono::DateTime::from_timestamp(t.0.as_second(), t.0.subsec_nanosecond().max(0) as u32)
        .map(|t| t.to_rfc3339())
}

/// **Pure.** Newest first, capped for display.
fn ordered(mut rows: Vec<EventRow>) -> Vec<EventRow> {
    rows.sort_by(|a, b| b.time.cmp(&a.time));
    rows.truncate(MAX_EVENTS);
    rows
}

/// `GET /api/v1/events?namespace=&kind=&name=`
async fn handler(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<EventQuery>,
) -> Result<Json<Vec<EventRow>>, ApiError> {
    if q.kind.is_empty() || q.name.is_empty() || q.namespace.is_empty() {
        return Err(incomplete_query());
    }
    let client = client_for(&app, &id)?;
    let api: Api<Event> = Api::namespaced(client, &q.namespace);
    let list = api
        .list(&ListParams::default().fields(&field_selector(&q.kind, &q.name)))
        .await
        .map_err(|e| {
            kopiur_ops::classify_kube("list", "Event", "events", Some(&q.namespace), None, e)
        })?;
    Ok(Json(ordered(list.items.iter().map(event_row).collect())))
}

/// The 400 for a query that does not name one object.
fn incomplete_query() -> ApiError {
    problem(
        400,
        "invalid-filter",
        "An events request has to name the object whose events you want.",
        "Events are read straight from the apiserver with a field selector, and an unfiltered \
         cluster-wide list would page through every event in the cluster on every screen \
         refresh.",
        "pass all three of ?namespace=, ?kind= and ?name=",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(json: serde_json::Value) -> Event {
        serde_json::from_value(json).expect("a valid Event")
    }

    #[test]
    fn the_field_selector_names_the_regarding_object() {
        assert_eq!(
            field_selector("Snapshot", "nightly-1"),
            "regarding.kind=Snapshot,regarding.name=nightly-1"
        );
    }

    #[test]
    fn an_event_row_flattens_the_object_it_concerns() {
        let e = event(serde_json::json!({
            "apiVersion": "events.k8s.io/v1",
            "kind": "Event",
            "metadata": { "name": "nightly-1.17a", "namespace": "media" },
            "eventTime": "2026-09-08T02:00:00.000000Z",
            "type": "Warning",
            "reason": "MoverJobFailed",
            "note": "the mover exited 1",
            "regarding": { "kind": "Snapshot", "namespace": "media", "name": "nightly-1" }
        }));
        let row = event_row(&e);
        assert_eq!(row.regarding, "Snapshot/media/nightly-1");
        assert_eq!(row.r#type.as_deref(), Some("Warning"));
        assert_eq!(row.reason.as_deref(), Some("MoverJobFailed"));
        assert_eq!(row.message.as_deref(), Some("the mover exited 1"));
        assert!(
            row.time
                .as_deref()
                .is_some_and(|t| t.starts_with("2026-09-08T02:00:00")),
            "got {:?}",
            row.time
        );
    }

    #[test]
    fn an_event_from_a_core_api_writer_still_has_a_time() {
        let e = event(serde_json::json!({
            "apiVersion": "events.k8s.io/v1",
            "kind": "Event",
            "metadata": {
                "name": "nas.17b",
                "namespace": "media",
                "creationTimestamp": "2026-09-08T01:00:00Z"
            },
            "deprecatedLastTimestamp": "2026-09-08T03:00:00Z",
            "regarding": { "kind": "Repository", "namespace": "media", "name": "nas" }
        }));
        let row = event_row(&e);
        assert!(
            row.time
                .as_deref()
                .is_some_and(|t| t.starts_with("2026-09-08T03:00:00")),
            "the last-seen time wins over the creation time: {:?}",
            row.time
        );
    }

    #[test]
    fn an_event_note_is_redacted_like_every_other_passthrough_text() {
        let e = event(serde_json::json!({
            "apiVersion": "events.k8s.io/v1",
            "kind": "Event",
            "metadata": { "name": "nas.17c", "namespace": "media" },
            "note": "connect refused: AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI",
            "regarding": { "kind": "Repository", "namespace": "media", "name": "nas" }
        }));
        let message = event_row(&e).message.unwrap_or_default();
        assert!(
            !message.contains("wJalrXUtnFEMI"),
            "an event can quote the same kopia output a log tail does: {message}"
        );
    }

    #[test]
    fn rows_are_newest_first_and_capped() {
        let rows: Vec<EventRow> = (0..MAX_EVENTS + 20)
            .map(|i| EventRow {
                time: Some(format!("2026-09-08T{:02}:00:00Z", i % 24)),
                r#type: None,
                reason: None,
                message: None,
                regarding: "Snapshot/media/nightly-1".into(),
            })
            .collect();
        let ordered = ordered(rows);
        assert_eq!(ordered.len(), MAX_EVENTS);
        assert!(
            ordered[0].time >= ordered[1].time,
            "newest first: {:?} then {:?}",
            ordered[0].time,
            ordered[1].time
        );
    }

    #[test]
    fn an_event_with_no_time_sorts_last_rather_than_breaking_the_list() {
        let rows = vec![
            EventRow {
                time: None,
                r#type: None,
                reason: None,
                message: None,
                regarding: "Snapshot/media/a".into(),
            },
            EventRow {
                time: Some("2026-09-08T02:00:00Z".into()),
                r#type: None,
                reason: None,
                message: None,
                regarding: "Snapshot/media/b".into(),
            },
        ];
        let ordered = ordered(rows);
        assert_eq!(ordered[0].regarding, "Snapshot/media/b");
        assert_eq!(ordered[1].time, None);
    }

    #[test]
    fn an_incomplete_query_is_refused_with_the_three_parameters_it_needs() {
        let err = incomplete_query();
        assert_eq!(err.0.status, 400);
        assert!(
            err.0.fix.contains("namespace")
                && err.0.fix.contains("kind")
                && err.0.fix.contains("name"),
            "got {}",
            err.0.fix
        );
    }
}
