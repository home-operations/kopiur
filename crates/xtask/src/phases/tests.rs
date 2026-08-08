//! Unit tests for the phase-handling ratchet.
//!
//! The decision half (`evaluate`) is pure, so the ratchet is testable without
//! touching disk. The scanning half is where a WRONG answer would come from,
//! and both directions of wrong matter here:
//!
//! * a **miss** (a real `matches!` / `_ =>` / `==` the scanner does not see) is
//!   the ratchet silently not working — so every rule has a hit fixture *and* a
//!   near-miss fixture that must not fire;
//! * a **false hit** would make the check unusable and push people to blanket
//!   exemptions — so the scrub-immunity and boundary cases are pinned too.

use super::*;

fn enums() -> Vec<&'static str> {
    phase_enums()
}

fn allow_of(entries: &[(&str, &str)]) -> Allowlist {
    Allowlist {
        allow: entries
            .iter()
            .map(|(f, s)| Entry {
                file: (*f).to_string(),
                snippet: (*s).to_string(),
                reason: "reviewed in the fixture".to_string(),
            })
            .collect(),
    }
}

fn scan(src: &str) -> Vec<Finding> {
    scan_source("crates/fixture/src/lib.rs", src, &enums())
}

fn rules(findings: &[Finding]) -> Vec<Rule> {
    findings.iter().map(|f| f.rule).collect()
}

// --- normalization ----------------------------------------------------------

#[test]
fn a_rustfmt_reflow_does_not_change_a_snippet_key() {
    // The property the whole allowlist rests on: the same expression laid out
    // by rustfmt two different ways has to be the same exemption.
    let one_line =
        "repo.status.as_ref().and_then(|s| s.phase.as_ref()) != Some(&RepositoryPhase::Degraded)";
    let reflowed = "repo.status\n    .as_ref()\n    .and_then(|s| s.phase.as_ref())\n    != Some(&RepositoryPhase::Degraded)";
    assert_eq!(normalize_code(one_line), normalize_code(reflowed));
    // …but two genuinely different expressions must not collide.
    assert_ne!(
        normalize_code(one_line),
        normalize_code(
            "repo.status.as_ref().and_then(|s| s.phase.as_ref()) == Some(&RepositoryPhase::Degraded)"
        )
    );
}

#[test]
fn normalize_code_keeps_the_spaces_that_carry_meaning() {
    assert_eq!(normalize_code("x as u8 == P::A"), "x as u8==P::A");
    assert_eq!(normalize_code("_ if n > 3 =>"), "_ if n>3=>");
}

// --- mentions_enum ----------------------------------------------------------

#[test]
fn mentions_enum_needs_a_variant_path_not_a_type_mention() {
    let e = ["SnapshotPhase"];
    assert!(mentions_enum("SnapshotPhase::Failed", &e));
    assert!(mentions_enum("kopiur_api::SnapshotPhase::Failed", &e));
    // A type annotation is not dispatch.
    assert!(!mentions_enum("phase: Option<SnapshotPhase>", &e));
    // A longer identifier that merely ends with the name is a different type.
    assert!(!mentions_enum("MySnapshotPhase::Failed", &e));
    assert!(!mentions_enum("", &e));
}

#[test]
fn the_enum_list_does_not_confuse_repository_with_repository_replication() {
    // `RepositoryPhase` is not a prefix of `RepositoryReplicationPhase`, but a
    // sloppy substring rule would report the wrong enum for both.
    assert!(!mentions_enum(
        "RepositoryReplicationPhase::Failed",
        &["RepositoryPhase"]
    ));
    assert!(mentions_enum(
        "RepositoryReplicationPhase::Failed",
        &["RepositoryReplicationPhase"]
    ));
}

// --- Rule A: `matches!` -----------------------------------------------------

#[test]
fn rule_a_flags_a_matches_over_a_phase_enum() {
    let f = scan("let t = matches!(phase, Some(SnapshotPhase::Failed));");
    assert_eq!(rules(&f), vec![Rule::NonExhaustiveMatches]);
    assert_eq!(f[0].snippet, "matches!(phase, Some(SnapshotPhase::Failed))");
    assert_eq!(f[0].line, 1);
}

#[test]
fn rule_a_ignores_a_matches_over_an_unrelated_enum() {
    // The workspace is full of these (`matches!(scope, WatchScope::Cluster)`);
    // flagging them would make the check noise.
    assert!(scan("let c = matches!(scope, WatchScope::Cluster);").is_empty());
    // Including the deliberately-out-of-scope `HookPhase` name collision.
    assert!(scan("let p = matches!(h, HookPhase::Before);").is_empty());
}

#[test]
fn rule_a_extracts_arguments_by_balanced_parens_not_the_first_close() {
    let f = scan("if matches!(p, Some(SnapshotPhase::Deleting)) && ok(x) { }");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].snippet, "matches!(p, Some(SnapshotPhase::Deleting))");
}

#[test]
fn rule_a_survives_a_comment_containing_a_paren_inside_the_arguments() {
    // The `sweep.rs` shape: a `// …)` comment sitting between the arguments. A
    // naive scan-to-the-next-`)` extractor cuts the call in half here; scrubbing
    // first plus balanced parens does not.
    let raw = "let t = matches!(\n    phase, // terminal (per #351)\n    Some(SnapshotPhase::Unchanged)\n);";
    let f = scan(&crate::scan::scrub_lines(raw));
    assert_eq!(f.len(), 1, "got {f:?}");
    assert_eq!(
        f[0].snippet,
        "matches!( phase, Some(SnapshotPhase::Unchanged) )"
    );
    assert_eq!(f[0].line, 1);
}

#[test]
fn rule_a_ignores_a_char_literal_paren_in_the_arguments() {
    let f = scan("let t = matches!(c, Sep(')') ) || phase_is(SnapshotPhase::Failed);");
    // The `matches!` names no phase enum; the balanced scan must not have run
    // away past the char literal and swallowed the following call.
    assert!(f.is_empty(), "got {f:?}");
}

#[test]
fn rule_a_recognizes_all_three_macro_delimiters() {
    // `matches![…]` / `matches!{…}` are identical to the compiler, so a rule
    // that only knew `(` could be sidestepped by a formatting choice.
    for src in [
        "let t = matches!(p, SnapshotPhase::Failed);",
        "let t = matches![p, SnapshotPhase::Failed];",
        "let t = matches!{p, SnapshotPhase::Failed};",
    ] {
        assert_eq!(rules(&scan(src)), vec![Rule::NonExhaustiveMatches], "{src}");
    }
}

#[test]
fn rule_a_is_immune_to_a_scrubbed_comment_or_string() {
    // A doc comment showing the very construct the ratchet flags is not code.
    let raw = "/// Do not write `matches!(p, SnapshotPhase::Failed)`.\n\
               fn f() { let s = \"matches!(p, SnapshotPhase::Failed)\"; }\n";
    assert!(scan(&crate::scan::scrub_lines(raw)).is_empty());
}

// --- Rule B: wildcard arms --------------------------------------------------

#[test]
fn rule_b_flags_a_wildcard_arm_over_a_phase_enum() {
    let f = scan("match p { SnapshotPhase::Failed => 1, _ => 0 }");
    assert_eq!(rules(&f), vec![Rule::WildcardArm]);
    assert_eq!(f[0].snippet, "match p … _ =>");
}

#[test]
fn rule_b_flags_a_guarded_wildcard_arm() {
    let f = scan("match p { SnapshotPhase::Failed => 1, _ if n > 3 => 2, _ => 0 }");
    assert_eq!(f.len(), 2, "both wildcard heads: {f:?}");
    assert!(f.iter().any(|x| x.snippet.ends_with("_ if n > 3 =>")));
}

#[test]
fn rule_b_ignores_an_exhaustive_match() {
    assert!(scan("match p { SnapshotPhase::Failed => 1, SnapshotPhase::Running => 0 }").is_empty());
}

#[test]
fn rule_b_ignores_a_wildcard_over_an_unrelated_enum() {
    assert!(scan("match k { RepositoryKind::Repository => 1, _ => 0 }").is_empty());
}

#[test]
fn rule_b_charges_a_wrapper_wildcard_because_every_phase_is_an_option() {
    // THE case this rule exists for after #351: nothing in this repo matches a
    // bare `Phase`, it is always `Option<&Phase>`, so `Some(_)` — not `_` — is
    // the natural way to write the next swallowed variant.
    let f = scan("match p { Some(SnapshotPhase::Failed) => 1, Some(_) => 0, None => 2 }");
    assert_eq!(rules(&f), vec![Rule::WildcardArm], "got {f:?}");
    assert_eq!(f[0].snippet, "match p … Some(_) =>");
}

#[test]
fn rule_b_charges_other_single_payload_wrappers_too() {
    for arm in [
        "Ok(_) => 0",
        "&_ => 0",
        "Some(&_) => 0",
        "Some(_) if n > 1 => 0",
    ] {
        let src = format!("match p {{ Some(SnapshotPhase::Failed) => 1, {arm}, Other => 2 }}");
        assert_eq!(
            rules(&scan(&src)),
            vec![Rule::WildcardArm],
            "wrapper wildcard not charged: {arm}"
        );
    }
}

#[test]
fn rule_b_does_not_charge_a_named_variant_with_a_binding_hole() {
    // `SnapshotPhase::Unknown(_)` is shaped like `Ctor(_)` but names a SPECIFIC
    // variant — it is exhaustive-match code, not a catch-all. Charging it would
    // make the rule fire on exactly the code it exists to encourage. That is why
    // shape alone is not enough and the enum check is the second half.
    let f = scan("match p { SnapshotPhase::Unknown(_) => 1, SnapshotPhase::Failed => 0 }");
    assert!(f.is_empty(), "got {f:?}");
    let f =
        scan("match p { Some(SnapshotPhase::Unknown(_)) => 1, Some(SnapshotPhase::Failed) => 0 }");
    assert!(f.is_empty(), "got {f:?}");
    // …and the two halves really are pulling in opposite directions here.
    assert!(pattern_is_wildcard_shape("SnapshotPhase::Unknown(_)"));
    assert!(mentions_enum("SnapshotPhase::Unknown(_)", &enums()));
}

#[test]
fn rule_b_does_not_charge_a_tuple_hole() {
    // A tuple's holes are positional, not a catch-all. Pinned because the
    // wrapper rule above is the obvious way to break this.
    assert!(!pattern_is_wildcard_shape("(_, _)"));
    let f = scan("match (p, j) { (SnapshotPhase::Failed, _) => 1, (_, _) => 0 }");
    assert!(f.is_empty(), "got {f:?}");
}

#[test]
fn rule_b_does_not_mistake_a_binding_hole_for_a_wildcard_arm() {
    // `_` inside a pattern, and `_` as an ignored binding, are not arms.
    assert!(
        scan("match p { SnapshotPhase::Unknown(_) => 1, SnapshotPhase::Failed => 0 }").is_empty()
    );
    assert!(
        scan("match p { (SnapshotPhase::Failed, _) => 1, (SnapshotPhase::Running, _) => 0 }")
            .is_empty()
    );
    assert!(scan("match p { SnapshotPhase::Failed => { let _unused = 1; } }").is_empty());
}

#[test]
fn rule_b_masks_a_nested_match_so_the_wildcard_is_charged_to_the_inner_one() {
    // The outer match must NOT be reported for the inner match's `_ =>`, and
    // the inner one must be — and only once.
    let src = "match a { Outer::X => { match p { SnapshotPhase::Failed => 1, _ => 0 } } }";
    let f = scan(src);
    assert_eq!(f.len(), 1, "got {f:?}");
    assert_eq!(f[0].snippet, "match p … _ =>");
}

#[test]
fn rule_b_reports_the_outer_match_when_the_wildcard_is_its_own() {
    let src =
        "match p { SnapshotPhase::Failed => { match q { Inner::A => 1, Inner::B => 2 } } _ => 0 }";
    let f = scan(src);
    assert_eq!(f.len(), 1, "got {f:?}");
    assert_eq!(f[0].snippet, "match p … _ =>");
}

#[test]
fn rule_b_sees_an_arm_that_follows_a_braced_body_without_a_comma() {
    // `A => { … } _ => …` — the arm boundary is the closing brace, not a comma.
    let f = scan("match p { SnapshotPhase::Failed => { go(); } _ => 0 }");
    assert_eq!(f.len(), 1, "got {f:?}");
}

#[test]
fn rule_b_flags_an_or_pattern_wildcard() {
    let f = scan("match p { SnapshotPhase::Failed | _ => 1 }");
    assert_eq!(rules(&f), vec![Rule::WildcardArm]);
}

#[test]
fn rule_b_reports_the_line_of_the_wildcard_not_of_the_match() {
    let src =
        "fn f() {\n    match p {\n        SnapshotPhase::Failed => 1,\n        _ => 0,\n    }\n}\n";
    let f = scan(src);
    assert_eq!(f.len(), 1, "got {f:?}");
    assert_eq!(f[0].line, 4);
}

#[test]
fn rule_b_ignores_a_scrutinee_brace_inside_a_closure() {
    // `match v.iter().map(|x| { x }).count() { … }` — the block's `{` is the one
    // after the call, not the one in the closure.
    let src = "match v.iter().map(|x| { x }).count() { SnapshotPhase::Failed => 1, _ => 0 }";
    let f = scan(src);
    assert_eq!(f.len(), 1, "got {f:?}");
    assert!(f[0].snippet.starts_with("match v.iter()"), "got {f:?}");
}

// --- Rule C: controller-side condition consts -------------------------------

#[test]
fn rule_c_finds_condition_consts_and_only_those() {
    let hits = condition_consts(
        "pub const PINNED_CONDITION: &str = ;\n\
         pub const PIN_JOB_FAILED_REASON: &str = ;\n\
         const PRIVATE_CONDITION: &str = ;\n\
         pub const HOOKS_SUCCEEDED_CONDITION: &str = ;\n",
    );
    let names: Vec<&str> = hits.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(names, vec!["PINNED_CONDITION", "HOOKS_SUCCEEDED_CONDITION"]);
}

#[test]
fn rule_c_charges_the_real_controller_consts_and_nothing_hoisted() {
    // The exempt half: still controller-side, so still flagged.
    let findings = collect().expect("collect");
    let flagged: BTreeSet<&str> = findings
        .iter()
        .filter(|f| f.rule == Rule::ControllerCondition)
        .map(|f| f.snippet.as_str())
        .collect();
    assert!(flagged.contains("PINNED_CONDITION"), "{flagged:?}");
    assert!(flagged.contains("SOURCE_STAGED_CONDITION"), "{flagged:?}");

    // The hoisted half: these moved to `kopiur_api::consts` precisely so the CLI
    // can read the same row, so Rule C must NOT see them any more. If one of
    // these ever reappears here, it was un-hoisted and #359 is back.
    for hoisted in [
        "MOVER_PERMITTED_CONDITION",
        "CREDENTIALS_AVAILABLE_CONDITION",
        "SCHEDULE_RUNNABLE_CONDITION",
        "DELETION_HELD_CONDITION",
        "REPOSITORY_WRITABLE_CONDITION",
        "MASS_DELETION_HELD_CONDITION",
    ] {
        assert!(
            !flagged.contains(hoisted),
            "`{hoisted}` is defined controller-side again — it belongs in kopiur_api::consts"
        );
    }
}

#[test]
fn every_registered_gate_condition_lives_in_the_api_crate() {
    // The other half of the same guarantee, asserted from the registry's side:
    // a row in `kopiur_api::gates` naming a condition that Rule C still sees
    // controller-side would mean the two sides disagree about where it lives.
    let findings = collect().expect("collect");
    let flagged: BTreeSet<&str> = findings
        .iter()
        .filter(|f| f.rule == Rule::ControllerCondition)
        .map(|f| f.snippet.as_str())
        .collect();
    for gate in kopiur_api::gates::STRUCTURAL_GATES {
        let screaming = gate
            .condition
            .chars()
            .flat_map(|c| {
                if c.is_ascii_uppercase() {
                    vec!['_', c]
                } else {
                    vec![c.to_ascii_uppercase()]
                }
            })
            .collect::<String>();
        let name = format!("{}_CONDITION", screaming.trim_start_matches('_'));
        assert!(
            !flagged.contains(name.as_str()),
            "gate condition `{}` is still defined controller-side as `{name}`",
            gate.condition
        );
    }
}

// --- Rule D: `==` / `!=` compares -------------------------------------------

#[test]
fn rule_d_flags_a_compare_against_a_phase_variant() {
    let f = scan("if phase != Some(&RestorePhase::Failed) { }");
    assert_eq!(rules(&f), vec![Rule::PhaseCompare]);
    assert_eq!(f[0].snippet, "phase != Some(&RestorePhase::Failed)");
}

#[test]
fn rule_d_extracts_a_reflowed_left_operand_whole() {
    let src = "if repo.status\n    .as_ref()\n    .and_then(|s| s.phase.as_ref())\n    != Some(&RepositoryPhase::Degraded)\n{ }";
    let f = scan(src);
    assert_eq!(f.len(), 1, "got {f:?}");
    assert_eq!(
        normalize_code(&f[0].snippet),
        normalize_code(
            "repo.status.as_ref().and_then(|s| s.phase.as_ref()) != Some(&RepositoryPhase::Degraded)"
        )
    );
}

#[test]
fn rule_d_stops_the_left_operand_at_a_boolean_conjunction() {
    let f = scan("if ok && phase == Some(&SnapshotPhase::Failed) { }");
    assert_eq!(f.len(), 1, "got {f:?}");
    assert_eq!(f[0].snippet, "phase == Some(&SnapshotPhase::Failed)");
}

#[test]
fn rule_d_ignores_operators_that_merely_end_in_equals() {
    for src in [
        "if n >= 3 { }",
        "if n <= 3 { }",
        "let x = SnapshotPhase::Failed;",
        "match p { SnapshotPhase::Failed => 1, SnapshotPhase::Running => 0 }",
    ] {
        assert!(scan(src).is_empty(), "false hit on {src:?}");
    }
}

#[test]
fn rule_d_ignores_a_compare_against_an_unrelated_enum() {
    assert!(scan("if kind == RepositoryKind::Repository { }").is_empty());
}

#[test]
fn rule_d_is_immune_to_a_scrubbed_comment() {
    let raw = "// was `phase == Some(&SnapshotPhase::Failed)`, now exhaustive\nlet a = 1;\n";
    assert!(scan(&crate::scan::scrub_lines(raw)).is_empty());
}

// --- Rule E: `if let` probes ------------------------------------------------

#[test]
fn rule_e_flags_an_if_let_naming_a_phase_variant() {
    let f = scan("if let Some(SnapshotPhase::Unknown(raw)) = phase { warn(raw); }");
    assert_eq!(rules(&f), vec![Rule::IfLetProbe]);
    assert_eq!(f[0].snippet, "if let Some(SnapshotPhase::Unknown(raw)) =");
}

#[test]
fn rule_e_covers_the_spellings_the_other_rules_cannot_see() {
    // The whole point: none of these contains a `matches!`, a `match` block or
    // an `==`, so each would otherwise be invisible to A, B and D alike.
    for src in [
        "if let Some(p @ RepositoryPhase::Unknown(_)) = phase { }",
        "while let Some(RestorePhase::Restoring) = next() { }",
        "let Some(SnapshotPhase::Failed) = phase else { return; };",
    ] {
        assert_eq!(rules(&scan(src)), vec![Rule::IfLetProbe], "{src}");
    }
}

#[test]
fn rule_e_ignores_a_let_whose_pattern_names_no_phase() {
    for src in [
        "let x = SnapshotPhase::Failed;",
        "if let Some(kind) = repo.kind { }",
        "if let Some(RepositoryKind::Repository) = k { }",
        // The named-predicate form, which moves the exhaustive match into
        // `crates/api`. Invisible here, and that is the documented trade.
        "if let Some(p) = phase.filter(|p| p.is_unknown()) { }",
    ] {
        assert!(scan(src).is_empty(), "false hit on {src:?}");
    }
}

#[test]
fn rule_e_stops_the_pattern_at_the_binding_equals() {
    // Not at a comparison, and not at a struct pattern's braces.
    assert_eq!(let_patterns("if let Some(p) = a == b { }")[0].1, "Some(p)");
    assert_eq!(
        let_patterns("let Foo { a, b } = mk();")[0].1,
        "Foo { a, b }"
    );
}

// --- evaluate ---------------------------------------------------------------

#[test]
fn an_uncovered_finding_fails() {
    let findings = scan("let t = matches!(p, Some(SnapshotPhase::Failed));");
    let r = evaluate(&findings, &Allowlist::default());
    assert!(!r.ok());
    assert_eq!(r.uncovered.len(), 1);
    assert!(r.stale.is_empty());
}

#[test]
fn an_allowlisted_finding_passes() {
    let findings = scan("let t = matches!(p, Some(SnapshotPhase::Failed));");
    let allow = allow_of(&[(
        "crates/fixture/src/lib.rs",
        "matches!(p, Some(SnapshotPhase::Failed))",
    )]);
    let r = evaluate(&findings, &allow);
    assert!(r.ok(), "{r:?}");
}

#[test]
fn an_allowlist_entry_matching_nothing_is_stale() {
    // The ratchet direction that forces drainage: once the construct is
    // rewritten, the exemption has to go or the list rots into a rubber stamp.
    let allow = allow_of(&[("crates/fixture/src/lib.rs", "matches!(p, Some(A::B))")]);
    let r = evaluate(&[], &allow);
    assert!(!r.ok());
    assert_eq!(r.stale.len(), 1);
    assert!(r.uncovered.is_empty());
}

#[test]
fn an_entry_for_the_right_snippet_in_the_wrong_file_does_not_cover_it() {
    // An exemption is reviewed in a file; it must not travel.
    let findings = scan("let t = matches!(p, Some(SnapshotPhase::Failed));");
    let allow = allow_of(&[(
        "crates/other/src/lib.rs",
        "matches!(p, Some(SnapshotPhase::Failed))",
    )]);
    let r = evaluate(&findings, &allow);
    assert_eq!(r.uncovered.len(), 1);
    assert_eq!(r.stale.len(), 1);
}

#[test]
fn one_entry_covers_every_identical_construct_in_the_same_file() {
    // Deliberate: `phase != Some(&RestorePhase::Failed)` appears four times in
    // `restore/mod.rs` for the same reason. One reviewed entry covers them all,
    // and it still goes stale the moment the last one is rewritten.
    let findings = scan(
        "fn a() { if phase != Some(&RestorePhase::Failed) { } }\n\
         fn b() { if phase != Some(&RestorePhase::Failed) { } }\n",
    );
    assert_eq!(findings.len(), 2);
    let allow = allow_of(&[(
        "crates/fixture/src/lib.rs",
        "phase != Some(&RestorePhase::Failed)",
    )]);
    assert!(evaluate(&findings, &allow).ok());
    assert_eq!(evaluate(&[], &allow).stale.len(), 1);
}

#[test]
fn a_duplicated_entry_fails_rather_than_silently_collapsing() {
    // Two entries, one key: only the first is ever consulted, so the second is a
    // reason nobody reads — and deleting the live one would silently promote the
    // duplicate instead of failing.
    let findings = scan("let t = matches!(p, Some(SnapshotPhase::Failed));");
    let allow = allow_of(&[
        (
            "crates/fixture/src/lib.rs",
            "matches!(p, Some(SnapshotPhase::Failed))",
        ),
        (
            "crates/fixture/src/lib.rs",
            "matches!(p,\n    Some(SnapshotPhase::Failed))",
        ),
    ]);
    let r = evaluate(&findings, &allow);
    assert!(!r.ok());
    assert_eq!(r.duplicates.len(), 1);
    assert!(r.uncovered.is_empty() && r.stale.is_empty());
}

#[test]
fn an_entry_written_with_different_whitespace_still_covers() {
    let findings = scan("if phase != Some(&RestorePhase::Failed) { }");
    let allow = allow_of(&[(
        "crates/fixture/src/lib.rs",
        "phase\n  !=\n  Some( &RestorePhase::Failed )",
    )]);
    assert!(evaluate(&findings, &allow).ok(), "reflow-proof matching");
}

// --- enum self-discovery ----------------------------------------------------

#[test]
fn the_api_phase_enum_list_matches_the_real_source() {
    // The self-ratchet: a SIXTH CR phase enum cannot be added without this
    // failing, because a phase enum the list does not name is a phase enum none
    // of the four rules cover.
    let discovered = discover_api_phase_enums().expect("api sources");
    let expected: BTreeSet<String> = API_PHASE_ENUMS.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(discovered, expected);
}

#[test]
fn discovery_stays_api_scoped_because_of_the_hook_phase_name_collision() {
    // `crates/controller/src/hooks.rs` defines `HookPhase`, a Before/After hook
    // slot with nothing to do with CR status. A workspace-wide `pub enum
    // \w*Phase` sweep would pull it in and flag every hook dispatch. Pin both
    // halves of that reasoning so the scope is not "simplified" away.
    assert!(!phase_enums().contains(&"HookPhase"));
    let controller = crate::scan::sources(&["controller"]).expect("controller sources");
    let found: Vec<String> = controller
        .iter()
        .flat_map(|(_, raw)| declared_phase_enums(&crate::scan::scrub(raw)))
        .collect();
    assert!(
        found.contains(&"HookPhase".to_string()),
        "fixture assumption: controller declares HookPhase; got {found:?}"
    );
}

#[test]
fn the_mover_phase_is_named_explicitly_because_it_is_not_in_the_api_crate() {
    assert!(phase_enums().contains(&MOVER_PHASE));
    let mover = crate::scan::sources(&["mover"]).expect("mover sources");
    let found: Vec<String> = mover
        .iter()
        .flat_map(|(_, raw)| declared_phase_enums(&crate::scan::scrub(raw)))
        .collect();
    assert!(
        found.contains(&MOVER_PHASE.to_string()),
        "MoverPhase is no longer declared in crates/mover; got {found:?}"
    );
}

// --- the real working tree --------------------------------------------------

/// The test that actually fails CI when someone adds a non-exhaustive phase
/// construct. `cargo xtask check-phases` is the same computation with a report.
#[test]
fn every_phase_construct_is_exhaustive_or_allowlisted() {
    let allow = Allowlist::load().expect("phase-allowlist.yaml");
    let findings = collect().expect("workspace sources");
    let report = evaluate(&findings, &allow);
    assert!(
        report.ok(),
        "phase ratchet failed.\n\
         uncovered (flagged, no reviewed exemption): {:#?}\n\
         stale (exemption matches nothing — delete it): {:#?}\n\
         Run `cargo xtask check-phases` for the full explanation.",
        report.uncovered,
        report.stale
    );
}

#[test]
fn rules_a_and_b_are_fully_paid_down() {
    // The #359 paydown left ZERO `matches!` and ZERO `_ =>` over a phase enum.
    // The last two Rule A sites became `SnapshotPhase::is_unknown` /
    // `RestorePhase::is_unknown`, which moves the exhaustive `match` into
    // `crates/api` where the compiler — not this ratchet — enforces it. Both
    // counts are asserted at zero rather than allowlisted, because an exemption
    // that can be zero should be.
    let findings = collect().expect("workspace sources");
    for rule in [Rule::NonExhaustiveMatches, Rule::WildcardArm] {
        let hits: Vec<&Finding> = findings.iter().filter(|f| f.rule == rule).collect();
        assert!(
            hits.is_empty(),
            "Rule {} is meant to stay empty; got {hits:#?}",
            rule.id()
        );
    }
}

#[test]
fn every_allowlist_entry_carries_a_real_reason() {
    let allow = Allowlist::load().expect("phase-allowlist.yaml");
    assert!(!allow.allow.is_empty());
    for e in &allow.allow {
        assert!(
            e.reason.trim().len() > 40,
            "entry `{}` / `{}` needs a real reason, got {:?}",
            e.file,
            e.snippet,
            e.reason
        );
        assert!(
            e.file.starts_with("crates/") && !e.file.contains('\\'),
            "entry file must be workspace-relative with `/`: {:?}",
            e.file
        );
    }
}
