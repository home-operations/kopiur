//! `kopiur-ui`'s metrics.
//!
//! Instrumented **once** against the OpenTelemetry metrics API and fanned out to
//! two readers by [`kopiur_telemetry::MetricsProvider`]: an always-on Prometheus
//! exporter (the `/metrics` pull endpoint on the ops listener) and — when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set — an OTLP push reader. Recording a value
//! updates both; there is no double instrumentation.
//!
//! Every metric is under the `kopiur_ui_` namespace. The Prometheus exporter
//! applies the usual OTel→Prometheus conventions, so a `u64_counter` named
//! `kopiur_ui_requests` is exported as `kopiur_ui_requests_total`.
//!
//! The provider is shared with the caller behind an `Arc` so `main` can build it
//! once, before anything else, and hand the same handle to the ops listener.

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};

use kopiur_telemetry::MetricsProvider;
use kopiur_ui_model::identity::IdentitySource;

/// Why a download ended without delivering exactly the bytes the snapshot entry
/// promised.
///
/// A closed enum rather than a free-form label: both cases mean the browser was
/// handed a file that does not match the snapshot, and a new case must force
/// every reporting site to decide what it is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadIncomplete {
    /// The stream ended before `entry.size` bytes — the user has a truncated file.
    Short,
    /// The stream produced more than `entry.size` bytes, so the copy was cut off.
    Overrun,
}

impl DownloadIncomplete {
    /// The `cause` label value. Exhaustive by construction.
    fn label(self) -> &'static str {
        match self {
            Self::Short => "download-short",
            Self::Overrun => "download-overrun",
        }
    }
}

/// The `identity_source` label value for a request. Exhaustive over
/// [`IdentitySource`], so a new source cannot be silently folded into an
/// existing bucket — the whole point of the label is that a downgrade from
/// proxy-asserted to anonymous is visible on a dashboard.
fn identity_source_label(source: &IdentitySource) -> &'static str {
    match source {
        IdentitySource::TrustedHeaders => "trusted-headers",
        IdentitySource::Anonymous => "anonymous",
    }
}

/// Every `kopiur-ui` metric, sharing one meter provider and Prometheus registry.
///
/// Instruments are internally reference-counted, so this is cheap to hold behind
/// an `Arc` and share across handlers.
pub struct UiMetrics {
    provider: Arc<MetricsProvider>,

    requests: Counter<u64>,
    identity_cache_size: Gauge<u64>,
    sessions_started: Counter<u64>,
    download_incomplete: Counter<u64>,
    exec_inflight: Gauge<u64>,
    cache_objects: Gauge<u64>,
    sar: Counter<u64>,
    sar_cache_size: Gauge<u64>,
}

impl UiMetrics {
    /// Build every instrument on `provider`.
    ///
    /// Infallible: telemetry is non-critical, so a provider that failed to build
    /// degrades to an empty `/metrics` rather than stopping the UI from serving.
    pub fn new(provider: Arc<MetricsProvider>) -> Self {
        let m = provider.meter();

        let requests = m
            .u64_counter("kopiur_ui_requests")
            .with_description(
                "Every HTTP request the UI answered, by route template (never the raw path, \
                 which carries namespaces and names), response status, and how the caller's \
                 identity was established (trusted-headers | anonymous). The identity_source \
                 label is what makes a silent downgrade to the anonymous identity — a \
                 misconfigured proxy, or KOPIUR_UI_ANONYMOUS_FALLBACK left on — visible \
                 instead of invisible.",
            )
            .build();

        let identity_cache_size = m
            .u64_gauge("kopiur_ui_identity_cache_size")
            .with_description(
                "Impersonating kube clients currently cached. Each entry owns a connection \
                 pool, so this tracking KOPIUR_UI_CLIENT_CACHE_SIZE means the cache is \
                 saturated and clients are being evicted and rebuilt.",
            )
            .build();

        let sessions_started = m
            .u64_counter("kopiur_ui_sessions_started")
            .with_description(
                "Browse session pods the UI asked the cluster to start. Sessions are shared \
                 with the CLI, so this counts starts the UI caused, not sessions in existence.",
            )
            .build();

        let download_incomplete = m
            .u64_counter("kopiur_ui_download_incomplete")
            .with_description(
                "File downloads that did not deliver exactly the size recorded in the \
                 snapshot entry, by cause (download-short | download-overrun). Any value \
                 above zero means a user may hold a file that does not match the backup.",
            )
            .build();

        let exec_inflight = m
            .u64_gauge("kopiur_ui_exec_inflight")
            .with_description(
                "pods/exec calls in flight right now, across all identities. Approaching \
                 KOPIUR_UI_MAX_EXEC_GLOBAL means browse requests are being answered with 429.",
            )
            .build();

        let cache_objects = m
            .u64_gauge("kopiur_ui_cache_objects")
            .with_description(
                "Objects held in the reflector store for each Kopiur kind. Zero for a kind \
                 that exists in the cluster means that store never synced.",
            )
            .build();

        let sar = m
            .u64_counter("kopiur_ui_sar")
            .with_description(
                "SubjectAccessReviews the UI performed to gate a cache-backed read, by \
                 outcome (allowed=true|false). Only the cache path issues these; the \
                 impersonated path is authorized by the apiserver on the real request.",
            )
            .build();

        let sar_cache_size = m
            .u64_gauge("kopiur_ui_sar_cache_size")
            .with_description(
                "SubjectAccessReview decisions currently cached, across all identities. \
                 This tracking KOPIUR_UI_SAR_CACHE_SIZE means the cache is saturated and \
                 decisions are being evicted before their TTL, so the SAR rate rises.",
            )
            .build();

        Self {
            provider,
            requests,
            identity_cache_size,
            sessions_started,
            download_incomplete,
            exec_inflight,
            cache_objects,
            sar,
            sar_cache_size,
        }
    }

    /// Render the Prometheus text exposition served at `/metrics`.
    pub fn gather(&self) -> Vec<u8> {
        self.provider.gather()
    }

    /// Count one answered request. `route` must be the route TEMPLATE
    /// (`/api/v1/snapshots/{namespace}/{name}`), never the concrete path — a
    /// per-object label would make the series cardinality the size of the fleet.
    pub fn inc_request(&self, route: &'static str, status: u16, source: &IdentitySource) {
        self.requests.add(
            1,
            &[
                KeyValue::new("route", route),
                KeyValue::new("status", i64::from(status)),
                KeyValue::new("identity_source", identity_source_label(source)),
            ],
        );
    }

    /// Publish the current size of the impersonating client cache.
    pub fn set_identity_cache_size(&self, entries: usize) {
        self.identity_cache_size.record(entries as u64, &[]);
    }

    /// Count one browse session pod start.
    pub fn inc_session_started(&self) {
        self.sessions_started.add(1, &[]);
    }

    /// Count one download that did not match the snapshot entry's size.
    pub fn inc_download_incomplete(&self, cause: DownloadIncomplete) {
        self.download_incomplete
            .add(1, &[KeyValue::new("cause", cause.label())]);
    }

    /// Publish the current number of in-flight `pods/exec` calls.
    pub fn set_exec_inflight(&self, inflight: usize) {
        self.exec_inflight.record(inflight as u64, &[]);
    }

    /// Publish how many objects a kind's reflector store holds.
    pub fn set_cache_objects(&self, kind: &'static str, objects: usize) {
        self.cache_objects
            .record(objects as u64, &[KeyValue::new("kind", kind)]);
    }

    /// Count one `SubjectAccessReview` and its outcome.
    pub fn inc_sar(&self, allowed: bool) {
        self.sar.add(1, &[KeyValue::new("allowed", allowed)]);
    }

    /// Publish how many authorization decisions the SAR cache holds.
    pub fn set_sar_cache_size(&self, entries: usize) {
        self.sar_cache_size.record(entries as u64, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_metrics_reach_the_prometheus_exposition() {
        let metrics = UiMetrics::new(Arc::new(MetricsProvider::new("kopiur-ui-test")));

        metrics.inc_request("/api/v1/me", 200, &IdentitySource::TrustedHeaders);
        metrics.inc_request("/api/v1/me", 401, &IdentitySource::Anonymous);
        metrics.inc_session_started();
        metrics.inc_download_incomplete(DownloadIncomplete::Short);
        metrics.inc_sar(false);
        metrics.set_identity_cache_size(3);
        metrics.set_exec_inflight(1);
        metrics.set_cache_objects("Snapshot", 12);
        metrics.set_sar_cache_size(7);

        let text = String::from_utf8(metrics.gather()).expect("exposition must be UTF-8");

        // The names the chart's ServiceMonitor and the docs promise. The
        // exporter appends `_total` to counters, so assert the exported name.
        for needle in [
            "kopiur_ui_requests_total",
            "kopiur_ui_identity_cache_size",
            "kopiur_ui_sessions_started_total",
            "kopiur_ui_download_incomplete_total",
            "kopiur_ui_exec_inflight",
            "kopiur_ui_cache_objects",
            "kopiur_ui_sar_total",
            "kopiur_ui_sar_cache_size",
        ] {
            assert!(text.contains(needle), "missing {needle} in:\n{text}");
        }

        // The labels a dashboard joins on.
        assert!(
            text.contains("identity_source=\"trusted-headers\""),
            "{text}"
        );
        assert!(text.contains("identity_source=\"anonymous\""), "{text}");
        assert!(text.contains("cause=\"download-short\""), "{text}");
        assert!(text.contains("allowed=\"false\""), "{text}");
    }

    #[test]
    fn every_identity_source_and_download_cause_has_a_distinct_label() {
        let labels = [
            identity_source_label(&IdentitySource::TrustedHeaders),
            identity_source_label(&IdentitySource::Anonymous),
        ];
        assert_ne!(labels[0], labels[1]);
        assert_ne!(
            DownloadIncomplete::Short.label(),
            DownloadIncomplete::Overrun.label()
        );
    }
}
