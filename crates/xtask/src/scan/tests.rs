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
fn a_body_less_cfg_test_declaration_does_not_eat_the_next_item() {
    // REGRESSION. The repo's dominant convention is `#[cfg(test)]\nmod tests;`
    // — no body, contents in a separate `tests.rs` the walker already skips.
    // Scanning to "the next `{`" lands on the FOLLOWING item's body and deletes
    // everything in between. Measured on crates/controller/src/snapshot/mod.rs,
    // that silently removed lines 72-109 from the scan corpus, including the
    // const below's real counterpart and all of `pub async fn reconcile`.
    let src = "#[cfg(test)]\n\
               mod tests;\n\
               const UNKNOWN_PHASE_HOLD_REQUEUE: u64 = 600;\n\
               pub async fn reconcile(x: u8) -> u8 { x }\n";
    for out in [strip_cfg_test(src), strip_cfg_test_lines(src)] {
        assert!(!out.contains("mod tests"), "declaration survived: {out:?}");
        assert!(
            out.contains("UNKNOWN_PHASE_HOLD_REQUEUE"),
            "production const was eaten: {out:?}"
        );
        assert!(
            out.contains("pub async fn reconcile"),
            "production fn was eaten: {out:?}"
        );
    }
}

#[test]
fn a_body_less_cfg_test_use_ends_at_its_semicolon_too() {
    let src = "#[cfg(test)] use x::y;\nfn real() { keep(); }\n";
    let out = strip_cfg_test(src);
    assert!(!out.contains("x::y"), "got: {out:?}");
    assert!(out.contains("keep()"), "got: {out:?}");
}

#[test]
fn a_semicolon_inside_a_type_does_not_end_the_item_early() {
    // `[u8; 4]` puts a `;` before the body's brace. Depth tracking is what keeps
    // the `;`/`{` race honest.
    let src = "#[cfg(test)]\nfn f() -> [u8; 4] { inert(); }\nfn real() { keep(); }\n";
    let out = strip_cfg_test(src);
    assert!(!out.contains("inert()"), "test body survived: {out:?}");
    assert!(out.contains("keep()"), "got: {out:?}");
}

#[test]
fn a_comment_semicolon_does_not_make_a_braced_module_look_body_less() {
    // REGRESSION, and the dangerous direction: this one fails SILENTLY toward
    // over-inclusion. Ending the "item" at a `;` inside a comment leaves the
    // whole test module in the corpus, where its fixtures start counting as
    // production code — a field "wired" only by a test, a phase construct
    // charged to a file that does not contain it.
    let src = "#[cfg(test)]\n\
               // fixtures; see docs/dev/api-conventions.md\n\
               mod tests {\n\
               \x20   fn f() { inert_fixture(); }\n\
               }\n\
               fn real() { keep(); }\n";
    for out in [strip_cfg_test(src), strip_cfg_test_lines(src)] {
        assert!(
            !out.contains("inert_fixture"),
            "test module leaked in: {out:?}"
        );
        assert!(out.contains("keep()"), "production code was eaten: {out:?}");
    }
}

#[test]
fn a_block_comment_or_literal_cannot_decide_the_item_extent() {
    // The same hole in its other three spellings: a `;` in a block comment, and
    // a brace inside a string literal closing the body early.
    let src =
        "#[cfg(test)]\n/* one; two */\nmod t { fn f() { inert(); } }\nfn real() { keep(); }\n";
    let out = strip_cfg_test(src);
    assert!(!out.contains("inert()"), "got: {out:?}");
    assert!(out.contains("keep()"), "got: {out:?}");

    let src = "#[cfg(test)]\nmod t { fn f() { let s = \"}\"; inert(); } }\nfn real() { keep(); }\n";
    let out = strip_cfg_test(src);
    assert!(
        !out.contains("inert()"),
        "brace in a literal closed the body: {out:?}"
    );
    assert!(out.contains("keep()"), "got: {out:?}");
}

#[test]
fn a_lifetime_is_not_an_unterminated_char_literal() {
    // `'a` must not swallow the rest of the item.
    let src = "#[cfg(test)]\nfn f<'a>(x: &'a str) { inert(); }\nfn real() { keep(); }\n";
    let out = strip_cfg_test(src);
    assert!(!out.contains("inert()"), "got: {out:?}");
    assert!(out.contains("keep()"), "got: {out:?}");
}

#[test]
fn cfg_test_item_end_races_the_semicolon_against_the_brace() {
    // The unit behind the three cases above.
    assert_eq!(cfg_test_item_end("\nmod tests;\nfn real() {}"), Some(11));
    assert_eq!(cfg_test_item_end(" mod t { fn f() {} }"), Some(20));
    assert_eq!(cfg_test_item_end(" fn f() -> [u8; 4] { }"), Some(22));
    // Trivia never supplies the deciding `;`, `{` or `}`.
    assert_eq!(
        cfg_test_item_end(" // fixtures; see docs\nmod t { }"),
        Some(32)
    );
    assert_eq!(cfg_test_item_end(" /* a; b */ mod t { }"), Some(21));
    assert_eq!(cfg_test_item_end(" mod t { let s = \"}\"; }"), Some(23));
    // Neither a body nor a `;`: the conservative drop-the-tail fallback.
    assert_eq!(cfg_test_item_end(" mod t { unbalanced"), None);
    assert_eq!(cfg_test_item_end(" nothing at all"), None);
}

#[test]
fn an_escaped_quote_char_literal_ends_where_it_really_ends() {
    // `'\''` is the one char literal whose BODY is a quote. Ending it at that
    // body leaves its real terminator to be re-read as the opening of a fresh
    // literal, which then swallows the byte after it — here the `}` that closes
    // the module, so the item's extent came up short and the tail of a test
    // module leaked back into the production corpus.
    assert_eq!(
        cfg_test_item_end(" mod t { let c = ('\\'','}'); }"),
        Some(30)
    );
    // The neighbouring escape forms must keep working: a one-char escape, a
    // unicode escape whose own braces must not count, and the byte spelling.
    assert_eq!(cfg_test_item_end(" mod t { let c = '\\n'; }"), Some(24));
    assert_eq!(cfg_test_item_end(" mod t { let c = '\\u{7d}'; }"), Some(28));
    assert_eq!(cfg_test_item_end(" mod t { let c = b'\\''; }"), Some(25));
}

#[test]
fn collapsing_strip_cfg_test_drops_the_tail_only_when_truly_unterminated() {
    // The documented conservative fallback, now reached only by invalid source.
    assert_eq!(
        strip_cfg_test("fn a() {}\n#[cfg(test)] mod t { unbalanced"),
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
