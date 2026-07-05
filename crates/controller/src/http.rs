//! The controller's HTTP surface: probes + Prometheus exposition.

use crate::metrics::Metrics;

/// The controller's HTTP server: `/metrics` (Prometheus exposition) plus real
/// `/healthz` + `/readyz` endpoints matching the chart's liveness/readiness
/// probes (the previous raw listener returned the metrics body for any path).
///
/// `addr` is validated at parse time (`--http-addr`/`KOPIUR_HTTP_ADDR`, see
/// [`crate::config::ControllerArgs`]), so a bind failure here is a genuine
/// runtime issue (port in use, no permission), not a bad address.
pub(crate) async fn serve_http(metrics: Metrics, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    use axum::extract::State;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    use axum::routing::get;

    async fn metrics_handler(State(metrics): State<Metrics>) -> impl IntoResponse {
        (
            [(CONTENT_TYPE, "text/plain; version=0.0.4")],
            metrics.gather(),
        )
    }
    async fn health() -> &'static str {
        "ok"
    }

    let app = axum::Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(health))
        .route("/readyz", get(health))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        %addr,
        "http server listening (/metrics, /healthz, /readyz)"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
