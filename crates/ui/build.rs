//! Stage the SPA bundle that `src/static_files.rs` embeds.
//!
//! `rust-embed`'s `#[folder]` attribute needs a directory that always exists at
//! compile time, and it must not be `web/dist` directly: a backend-only checkout
//! has no `web/dist`, and a missing embed folder is a hard compile error. So this
//! script owns `$OUT_DIR/web` and guarantees it holds an `index.html`, either the
//! real bundle or a placeholder page.
//!
//! It NEVER invokes pnpm or node. Building the SPA is `mise run ui-build`'s job —
//! a Rust build that shells out to a JS toolchain would make `cargo build` depend
//! on a node installation and a network fetch, which is exactly what the release
//! image's separate `node` build stage exists to avoid.
//!
//! `KOPIUR_UI_REQUIRE_WEB=1` turns a missing bundle into a build failure. Only
//! `docker/Dockerfile.ui` sets it: the shipped image must never embed the
//! placeholder, and finding that out at runtime is far too late.

use std::env;
use std::fs;
use std::path::Path;

/// Marker comment the placeholder page carries so
/// `kopiur_ui::static_files::is_placeholder` can recognise it at runtime and
/// `/readyz` can report `placeholder-web` instead of silently serving it.
///
/// Kept in sync by the `is_placeholder_agrees_with_the_embedded_marker` test in
/// `src/static_files.rs`, which asserts the embedded page carries the marker
/// whenever there is no `web/dist` to embed instead.
const PLACEHOLDER_MARKER: &str = "<!-- kopiur-ui-placeholder -->";

/// The stand-in page embedded when there is no `web/dist`. Deliberately tiny and
/// dependency-free: it exists to make a backend-only build runnable, not to look
/// like the product.
const PLACEHOLDER_HTML: &str = concat!(
    "<!doctype html>",
    "<!-- kopiur-ui-placeholder -->",
    "<title>kopiur-ui</title>",
    "<p>kopiur-ui was built without the web bundle. ",
    "Run <code>mise run ui-build</code>.</p>",
);

/// Env var that promotes "no SPA bundle" from a placeholder to a build failure.
const REQUIRE_WEB_ENV: &str = "KOPIUR_UI_REQUIRE_WEB";

fn main() {
    // Rebuild when the bundle appears, changes or vanishes, and when the
    // require-web gate flips — otherwise a `mise run ui-build` after a backend
    // build would leave the placeholder embedded in a stale artifact.
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-env-changed={REQUIRE_WEB_ENV}");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("cargo sets OUT_DIR");

    let dist = Path::new(&manifest_dir).join("web").join("dist");
    let out = Path::new(&out_dir).join("web");

    // Recreate from scratch so a file deleted from `web/dist` is also gone from
    // the embed folder; `rust-embed` walks whatever is there, and a stale asset
    // would otherwise be served forever.
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).unwrap_or_else(|e| panic!("creating {}: {e}", out.display()));

    if dist.join("index.html").is_file() {
        copy_tree(&dist, &out);
        return;
    }

    if env::var(REQUIRE_WEB_ENV).as_deref() == Ok("1") {
        panic!(
            "{REQUIRE_WEB_ENV}=1 but there is no {}, so this build has no SPA bundle to embed. \
             The released kopiur-ui image must ship the real web UI, never the placeholder \
             page, and a placeholder is only discoverable once the pod is already serving it \
             — so the build is refused here instead. Run `mise run ui-build` to produce \
             crates/ui/web/dist before building, or unset {REQUIRE_WEB_ENV} for a \
             backend-only development build.",
            dist.join("index.html").display(),
        );
    }

    fs::write(out.join("index.html"), PLACEHOLDER_HTML)
        .unwrap_or_else(|e| panic!("writing the placeholder index.html: {e}"));
    debug_assert!(PLACEHOLDER_HTML.contains(PLACEHOLDER_MARKER));
}

/// Copy `src` into `dst` recursively. Panics on any IO error: a partially staged
/// bundle would embed silently and serve a half-broken UI.
fn copy_tree(src: &Path, dst: &Path) {
    let entries = fs::read_dir(src).unwrap_or_else(|e| panic!("reading {}: {e}", src.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("reading an entry of {}: {e}", src.display()));
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = entry
            .file_type()
            .unwrap_or_else(|e| panic!("stat {}: {e}", from.display()));
        if kind.is_dir() {
            fs::create_dir_all(&to).unwrap_or_else(|e| panic!("creating {}: {e}", to.display()));
            copy_tree(&from, &to);
        } else {
            // Symlinks are followed (`fs::copy` copies the target's bytes): a
            // bundler may emit one, and embedding the link text would be useless.
            fs::copy(&from, &to)
                .unwrap_or_else(|e| panic!("copying {} to {}: {e}", from.display(), to.display()));
        }
    }
}
