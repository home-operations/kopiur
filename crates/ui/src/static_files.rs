//! The SPA, compiled into the binary.
//!
//! `build.rs` stages the bundle into `$OUT_DIR/web` — the real `web/dist` when it
//! exists, otherwise a placeholder page — and [`Web`] embeds whatever is there.
//! Serving from memory rather than from disk is what lets the image be
//! distroless with no writable filesystem and no `tower-http` `fs` layer.
//!
//! Routing is the usual single-page-app arrangement: an exact asset match is
//! served as itself, and anything else falls back to `index.html` so a deep link
//! like `/snapshots/prod/nightly-1` is handled by the client-side router instead
//! of 404ing.

use axum::body::Body;
use axum::http::Uri;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;

/// The staged SPA bundle. `build.rs` guarantees the folder exists and holds an
/// `index.html`, so the embed never fails to compile on a fresh checkout.
#[derive(rust_embed::Embed)]
#[folder = "$OUT_DIR/web"]
pub struct Web;

/// Marker `build.rs` writes into the stand-in `index.html`. Its presence is how
/// a running process knows it has no real UI to serve.
const PLACEHOLDER_MARKER: &str = "<!-- kopiur-ui-placeholder -->";

/// The SPA entry point, and the fallback for every unmatched path.
const INDEX: &str = "index.html";

/// Path prefix for the bundler's content-hashed output. Files under it carry a
/// hash in their name, so they are immutable by construction.
const HASHED_ASSET_PREFIX: &str = "assets/";

/// Whether this binary embeds the `build.rs` placeholder instead of a real SPA
/// bundle.
///
/// Reported through `/readyz` as `placeholder-web`. A released image can never
/// return true — `docker/Dockerfile.ui` builds with `KOPIUR_UI_REQUIRE_WEB=1`,
/// which turns a missing bundle into a build failure — so a true here always
/// means the image was built wrong.
pub fn is_placeholder() -> bool {
    Web::get(INDEX).is_some_and(|f| {
        std::str::from_utf8(f.data.as_ref()).is_ok_and(|html| html.contains(PLACEHOLDER_MARKER))
    })
}

/// Serve an embedded asset, falling back to `index.html`.
///
/// Cache-Control is chosen per asset class, and getting it wrong is a real
/// outage shape: `index.html` names the current hashed bundle, so caching it
/// would pin browsers to a deleted bundle after an upgrade, while the hashed
/// assets themselves can never change under a given name and are cached for a
/// year.
pub async fn spa_fallback(uri: Uri) -> Response<Body> {
    let path = uri.path().trim_start_matches('/');

    if !path.is_empty()
        && let Some(file) = Web::get(path)
    {
        let cache = if path.starts_with(HASHED_ASSET_PREFIX) {
            "public, max-age=31536000, immutable"
        } else {
            // A non-hashed asset (favicon.ico, robots.txt) may be replaced in
            // place by the next release, so it must be revalidated.
            "no-cache"
        };
        return asset(path, file.data.into_owned(), cache);
    }

    match Web::get(INDEX) {
        Some(file) => asset(INDEX, file.data.into_owned(), "no-cache"),
        // Unreachable in practice — `build.rs` always writes an index.html — but
        // a panic here would take down a server that is otherwise fine, and the
        // API half is still perfectly usable.
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "kopiur-ui was built without a web bundle and without a placeholder; rebuild it \
             with `mise run ui-build`\n",
        )
            .into_response(),
    }
}

/// One embedded file as a response, with its guessed content type.
fn asset(path: &str, body: Vec<u8>, cache_control: &'static str) -> Response<Body> {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [
            (CONTENT_TYPE, mime.as_ref().to_string()),
            (CACHE_CONTROL, cache_control.to_string()),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    /// Whatever `build.rs` staged, there is always an entry point to serve.
    #[test]
    fn an_index_html_is_always_embedded() {
        assert!(
            Web::get(INDEX).is_some(),
            "build.rs must stage an index.html — the real bundle or the placeholder"
        );
    }

    /// A backend-only checkout (no `web/dist`) must be recognisable at runtime,
    /// which is what `/readyz`'s `placeholder-web` reason depends on.
    ///
    /// The assertion is conditional on the marker actually being present so the
    /// test still passes in a full build that embeds the real SPA — and it
    /// asserts the two functions agree either way, which is the invariant that
    /// matters.
    #[test]
    fn is_placeholder_agrees_with_the_embedded_marker() {
        let index = Web::get(INDEX).expect("index.html must be embedded");
        let html = String::from_utf8(index.data.into_owned()).expect("index.html must be UTF-8");
        assert_eq!(
            is_placeholder(),
            html.contains(PLACEHOLDER_MARKER),
            "is_placeholder() must reflect the marker build.rs writes"
        );
        // This checkout has no `web/dist` (PR3 adds it), so the placeholder is
        // what a `cargo build -p kopiur-ui` here embeds.
        if !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("web")
            .join("dist")
            .join(INDEX)
            .is_file()
        {
            assert!(
                is_placeholder(),
                "with no web/dist, the placeholder must be what is embedded"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_path_falls_back_to_the_index_uncached() {
        let response = spa_fallback("/snapshots/prod/nightly-1".parse().unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .map(|v| v.to_str().unwrap()),
            Some("no-cache"),
            "index.html names the current hashed bundle, so it must never be cached"
        );
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .map(|v| v.to_str().unwrap()),
            Some("text/html")
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let expected = Web::get(INDEX).unwrap().data.into_owned();
        assert_eq!(body.as_ref(), expected.as_slice());
    }

    #[tokio::test]
    async fn the_root_path_serves_the_index() {
        let response = spa_fallback("/".parse().unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            body.as_ref(),
            Web::get(INDEX).unwrap().data.into_owned().as_slice()
        );
    }
}
