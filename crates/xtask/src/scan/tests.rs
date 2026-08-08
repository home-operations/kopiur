//! Unit tests for the shared source-scanning primitives.
//!
//! Two properties matter here and nothing else does:
//!
//! 1. the **collapsing** forms are byte-identical to what the wiring ratchet
//!    used before these functions moved out of `wiring.rs` — a silent change
//!    there would move the wiring corpus and could flip a field from wired to
//!    inert (or back) with no visible cause; and
//! 2. the **line-preserving** forms keep every surviving character on its
//!    original 1-based line, which is the entire reason they exist (the phase
//!    ratchet reports `file:line`).

use super::*;

/// The one shape that made the two modes differ: text removed *across* lines.
const MULTILINE: &str = "let a = 1; /* one\ntwo */ let b = 2;\n\
                         let s = \"three\nfour\";\n\
                         let c = 3;\n";

// --- collapsing modes are unchanged ----------------------------------------

#[test]
fn collapsing_scrub_replaces_each_removed_span_with_exactly_one_space() {
    // The pre-extraction behavior, pinned literally.
    assert_eq!(scrub("let a = 1; // c"), "let a = 1; ");
    assert_eq!(scrub("a /* x */ b"), "a   b");
    assert_eq!(scrub(r#"f("lit")"#), "f( )");
    assert_eq!(scrub(r##"f(r#"lit"#)"##), "f( )");
    // Multi-line removals collapse, so line numbers do NOT survive here.
    assert!(scrub(MULTILINE).lines().count() < MULTILINE.lines().count());
}

#[test]
fn collapsing_strip_cfg_test_emits_no_filler_at_all() {
    // `strip_cfg_test` never emitted a space, unlike `scrub`. Keeping that
    // difference is what makes the wiring corpus byte-identical.
    let src = "fn a() {}\n#[cfg(test)]\nmod tests { fn b() {} }\nfn c() {}\n";
    assert_eq!(strip_cfg_test(src), "fn a() {}\n\nfn c() {}\n");
}

#[test]
fn collapsing_strip_cfg_test_drops_the_tail_when_there_is_no_brace() {
    // `#[cfg(test)] use ...;` — the documented conservative fallback.
    assert_eq!(
        strip_cfg_test("fn a() {}\n#[cfg(test)] use x;"),
        "fn a() {}\n"
    );
}

// --- line-preserving modes --------------------------------------------------

#[test]
fn scrub_lines_keeps_every_line_number() {
    let out = scrub_lines(MULTILINE);
    assert_eq!(
        out.lines().count(),
        MULTILINE.lines().count(),
        "got: {out:?}"
    );
    // `let c = 3;` is line 5 of the fixture and must still be line 5.
    assert!(
        out.lines().nth(4).unwrap().contains("let c = 3;"),
        "got: {out:?}"
    );
    // And the removed text really is gone.
    assert!(!out.contains("two"), "got: {out:?}");
    assert!(!out.contains("four"), "got: {out:?}");
}

#[test]
fn strip_cfg_test_lines_keeps_every_line_number() {
    let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\nfn c() {}\n";
    let out = strip_cfg_test_lines(src);
    assert!(!out.contains("mod tests"), "got: {out:?}");
    assert_eq!(out.lines().count(), src.lines().count(), "got: {out:?}");
    assert_eq!(out.lines().nth(5).unwrap().trim(), "fn c() {}");
}

#[test]
fn the_two_modes_agree_once_whitespace_is_ignored() {
    // The only difference between the modes is filler. Anything else would mean
    // the line-preserving form removes a different set of spans.
    for src in [
        MULTILINE,
        "fn a() {}\n#[cfg(test)]\nmod t { fn b() {} }\nfn c() {}\n",
    ] {
        let collapse: String = scrub(&strip_cfg_test(src))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let preserve: String = scrub_lines(&strip_cfg_test_lines(src))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(collapse, preserve, "modes disagree on {src:?}");
    }
}

// --- the walker -------------------------------------------------------------

#[test]
fn sources_returns_workspace_relative_paths_and_skips_test_files() {
    let files = sources(&["xtask"]).expect("xtask sources");
    assert!(!files.is_empty());
    for (p, _) in &files {
        let rel = rel_display(p);
        assert!(
            rel.starts_with("crates/xtask/src/"),
            "path is not workspace-relative: {rel}"
        );
        assert!(
            !rel.ends_with("/tests.rs"),
            "test file was not skipped: {rel}"
        );
    }
    // This very file's parent module is present; this file is not.
    let names: Vec<String> = files.iter().map(|(p, _)| rel_display(p)).collect();
    assert!(names.contains(&"crates/xtask/src/scan.rs".to_string()));
    assert!(!names.contains(&"crates/xtask/src/scan/tests.rs".to_string()));
}

#[test]
fn sources_is_sorted_and_ignores_a_crate_that_does_not_exist() {
    let files = sources(&["xtask", "definitely-not-a-crate"]).expect("sources");
    let names: Vec<String> = files.iter().map(|(p, _)| rel_display(p)).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "walker order must be deterministic");
}
