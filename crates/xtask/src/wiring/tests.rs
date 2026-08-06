//! Unit tests for the wiring ratchet.
//!
//! The decision half (`evaluate`) is pure, so the whole ratchet is testable
//! without touching disk. The scanning half (`scrub` / `strip_cfg_test` /
//! `mentions`) is where false "this field IS wired" answers would come from, so
//! it gets the most coverage — a doc comment or an error message naming a field
//! must NOT make it look consumed.

use super::*;

fn field(path: &str, ident: &str) -> Field {
    Field {
        path: path.to_string(),
        ident: ident.to_string(),
        variant: camel_to_pascal(path.rsplit('.').next().unwrap_or(path)),
    }
}

fn allow_of(allow: &[(&str, &str)], prune: &[(&str, &str)]) -> Allowlist {
    let mk = |v: &[(&str, &str)]| {
        v.iter()
            .map(|(p, r)| Entry {
                path: (*p).to_string(),
                reason: (*r).to_string(),
            })
            .collect()
    };
    Allowlist {
        allow: mk(allow),
        prune: mk(prune),
        rename: Vec::new(),
    }
}

// --- camel_to_snake --------------------------------------------------------

#[test]
fn camel_to_snake_handles_acronym_runs() {
    // The naive "underscore before every capital" rule would produce
    // `kopia_snapshot_i_d` and silently fail to find the real Rust field.
    assert_eq!(camel_to_snake("kopiaSnapshotID"), "kopia_snapshot_id");
    assert_eq!(camel_to_snake("pvcName"), "pvc_name");
    assert_eq!(
        camel_to_snake("indexBlobWarnThreshold"),
        "index_blob_warn_threshold"
    );
    assert_eq!(camel_to_snake("uploadLimitMB"), "upload_limit_mb");
    // A capital run followed by a lowercase starts a new segment at the LAST
    // capital: `CSIDriver` -> `csi_driver`.
    assert_eq!(camel_to_snake("csiDriverName"), "csi_driver_name");
    assert_eq!(camel_to_snake("path"), "path");
    assert_eq!(camel_to_snake(""), "");
}

#[test]
fn camel_to_snake_matches_the_real_field_names_we_care_about() {
    // The four fields whose inertness shipped as bugs.
    assert_eq!(camel_to_snake("pvcSelector"), "pvc_selector");
    assert_eq!(
        camel_to_snake("ignoreIdenticalSnapshots"),
        "ignore_identical_snapshots"
    );
    assert_eq!(camel_to_snake("sourcePathStrategy"), "source_path_strategy");
    assert_eq!(camel_to_snake("groupBy"), "group_by");
}

// --- scrub -----------------------------------------------------------------

#[test]
fn scrub_removes_line_and_doc_comments() {
    // This is the case that would otherwise have marked `pvc_selector` wired:
    // `io/staging.rs` carries a comment reading "NFS / pvcSelector — nothing to
    // snapshot" while the code never touches the field.
    let src =
        "let x = 1; // pvc_selector\n/// doc pvc_selector\n//! inner pvc_selector\nlet y = 2;";
    let out = scrub(src);
    assert!(!out.contains("pvc_selector"), "got: {out}");
    assert!(out.contains("let x = 1;"));
    assert!(out.contains("let y = 2;"));
}

#[test]
fn scrub_removes_block_comments_including_nested() {
    let out = scrub("a /* pvc_selector /* deeper pvc_selector */ still */ b");
    assert!(!out.contains("pvc_selector"), "got: {out}");
    assert!(out.contains('a') && out.contains('b'));
}

#[test]
fn scrub_removes_string_and_raw_string_literals() {
    // An error message naming a field is not a consumer of it.
    assert!(!scrub(r#"bail!("pvc_selector is unsupported")"#).contains("pvc_selector"));
    assert!(!scrub(r##"let s = r#"pvc_selector"#;"##).contains("pvc_selector"));
    assert!(!scrub(r#"let s = "esc \" pvc_selector";"#).contains("pvc_selector"));
    // But an identifier outside a literal survives.
    assert!(scrub("cfg.pvc_selector.is_some()").contains("pvc_selector"));
}

#[test]
fn scrub_leaves_ordinary_code_intact() {
    let src = "if let Some(s) = spec.files.as_ref() { s.ignore_identical_snapshots }";
    assert!(scrub(src).contains("ignore_identical_snapshots"));
}

// --- strip_cfg_test --------------------------------------------------------

#[test]
fn strip_cfg_test_removes_the_module_but_keeps_real_code() {
    let src = "fn real() { wired_field(); }\n\
               #[cfg(test)]\n\
               mod tests { fn f() { inert_field(); } }\n\
               fn also_real() { second_field(); }\n";
    let out = strip_cfg_test(src);
    assert!(out.contains("wired_field"));
    assert!(out.contains("second_field"));
    assert!(!out.contains("inert_field"), "got: {out}");
}

#[test]
fn strip_cfg_test_handles_nested_braces_in_the_test_module() {
    let src = "fn real() { keep_me(); }\n\
               #[cfg(test)]\n\
               mod tests { fn f() { if x { g(inert_field); } } }\n\
               fn tail() { also_keep(); }\n";
    let out = strip_cfg_test(src);
    assert!(!out.contains("inert_field"), "got: {out}");
    assert!(
        out.contains("keep_me") && out.contains("also_keep"),
        "got: {out}"
    );
}

// --- mentions --------------------------------------------------------------

#[test]
fn mentions_is_whole_word_only() {
    assert!(mentions("a.pvc_selector = b", "pvc_selector"));
    assert!(mentions("pvc_selector", "pvc_selector"));
    // A longer identifier that merely CONTAINS the name is not a consumer.
    assert!(!mentions("let pvc_selector_kind = 1;", "pvc_selector"));
    assert!(!mentions("let my_pvc_selector = 1;", "pvc_selector"));
    assert!(!mentions("", "pvc_selector"));
    assert!(!mentions("anything", ""));
}

#[test]
fn mentions_finds_a_later_occurrence_after_a_partial_one() {
    // The scan must not stop at the first (rejected) substring hit.
    assert!(mentions("xx_path_yy; let path = 1;", "path"));
}

// --- evaluate --------------------------------------------------------------

#[test]
fn unwired_and_unlisted_field_is_an_offender() {
    let fields = vec![field(
        "SnapshotPolicy.spec.files.ignoreIdenticalSnapshots",
        "ignore_identical_snapshots",
    )];
    let r = evaluate(&fields, "fn main() {}", &Allowlist::default());
    assert!(!r.ok());
    assert_eq!(r.offenders.len(), 1);
    assert_eq!(
        r.offenders[0].path,
        "SnapshotPolicy.spec.files.ignoreIdenticalSnapshots"
    );
}

#[test]
fn wired_field_passes() {
    let fields = vec![field(
        "SnapshotPolicy.spec.files.ignoreIdenticalSnapshots",
        "ignore_identical_snapshots",
    )];
    let r = evaluate(
        &fields,
        "let v = f.ignore_identical_snapshots;",
        &Allowlist::default(),
    );
    assert!(r.ok(), "{r:?}");
}

#[test]
fn allowlisted_unwired_field_passes() {
    let fields = vec![field("Snapshot.status.stats.bytesNew", "bytes_new")];
    let allow = allow_of(&[("Snapshot.status.stats.bytesNew", "never written")], &[]);
    let r = evaluate(&fields, "fn main() {}", &allow);
    assert!(r.ok(), "{r:?}");
}

#[test]
fn allowlisted_field_that_became_wired_is_stale() {
    // The ratchet direction that forces drainage: once you implement a field,
    // the exemption must go, or the list silently rots.
    let fields = vec![field("SnapshotPolicy.spec.groupBy", "group_by")];
    let allow = allow_of(
        &[("SnapshotPolicy.spec.groupBy", "not implemented yet")],
        &[],
    );
    let r = evaluate(&fields, "match policy.spec.group_by { _ => () }", &allow);
    assert!(!r.ok());
    assert_eq!(r.stale, vec!["SnapshotPolicy.spec.groupBy".to_string()]);
    assert!(r.offenders.is_empty());
}

#[test]
fn allowlist_entry_for_a_vanished_field_is_unknown() {
    let allow = allow_of(&[("Snapshot.spec.longGone", "renamed away")], &[]);
    let r = evaluate(&[], "", &allow);
    assert!(!r.ok());
    assert_eq!(r.unknown, vec!["Snapshot.spec.longGone".to_string()]);
}

#[test]
fn a_prune_entry_matching_no_field_is_unknown() {
    // Dead weight: the upstream type moved and the exemption was left behind.
    let allow = allow_of(&[], &[("Snapshot.spec.goneAway", "upstream type")]);
    let r = evaluate(&[], "", &allow);
    assert_eq!(r.unknown, vec!["Snapshot.spec.goneAway".to_string()]);
}

#[test]
fn a_glob_allow_entry_covers_every_matching_path() {
    // The same field repeats across Repository / ClusterRepository /
    // RepositoryReplication; one reviewed entry should cover all three.
    let fields = vec![
        field(
            "Repository.spec.backend.gdrive.credentialsSecretRef",
            "credentials_secret_ref",
        ),
        field(
            "ClusterRepository.spec.backend.gdrive.credentialsSecretRef",
            "credentials_secret_ref",
        ),
    ];
    let allow = allow_of(
        &[("*.gdrive.credentialsSecretRef", "api::creds resolves it")],
        &[],
    );
    let r = evaluate(&fields, "fn main() {}", &allow);
    assert!(r.ok(), "{r:?}");
}

#[test]
fn a_glob_allow_entry_still_goes_stale_when_one_member_becomes_wired() {
    // A glob must not be able to absorb a sibling silently: if ANY covered
    // field becomes wired, the entry has to be narrowed.
    let fields = vec![field(
        "Repository.spec.backend.gdrive.credentialsSecretRef",
        "credentials_secret_ref",
    )];
    let allow = allow_of(
        &[("*.gdrive.credentialsSecretRef", "api::creds resolves it")],
        &[],
    );
    let r = evaluate(&fields, "let s = b.credentials_secret_ref;", &allow);
    assert!(!r.ok());
    assert_eq!(r.stale.len(), 1);
}

// --- strip_use_stmts -------------------------------------------------------

#[test]
fn strip_use_stmts_removes_imports_so_they_cannot_count_as_dispatch() {
    // An import contains `::GroupBy` and would otherwise satisfy
    // `mentions_variant` without anyone ever matching on the type. Handled
    // here rather than in `mentions_variant`, which only knows about one name.
    let src = "use kopiur_api::GroupBy;\npub use crate::snapshot_policy::GroupBy;\nlet a = 1;";
    let out = strip_use_stmts(src);
    assert!(!out.contains("GroupBy"), "got: {out}");
    assert!(out.contains("let a = 1;"));
}

#[test]
fn strip_use_stmts_handles_multi_line_brace_groups() {
    let src = "use kopiur_api::{\n    GroupBy,\n    SourcePathStrategy,\n};\nmatch x { GroupBy::None => () }";
    let out = strip_use_stmts(src);
    // The import block is gone, the real dispatch survives.
    assert!(out.contains("GroupBy::None"), "got: {out}");
    assert!(!out.contains("SourcePathStrategy"), "got: {out}");
}

#[test]
fn strip_use_stmts_keeps_a_line_that_merely_mentions_use() {
    let src = "let used = compute();\nfn misuse() {}\n";
    assert_eq!(strip_use_stmts(src), src);
}

// --- mentions_variant ------------------------------------------------------

#[test]
fn mentions_variant_requires_a_path_use_not_a_declaration() {
    // This is the distinction that decides whether the ratchet catches #346.
    // `pub enum GroupBy` / `pub use ... GroupBy` prove only that the type
    // exists; `GroupBy::VolumeGroupSnapshot` proves someone dispatches on it.
    assert!(mentions_variant("match b { Backend::S3(c) => () }", "S3"));
    assert!(mentions_variant(
        "Hook::HttpRequest(h) => run(h)",
        "HttpRequest"
    ));
    assert!(!mentions_variant(
        "pub enum GroupBy { VolumeGroupSnapshot }",
        "GroupBy"
    ));
    assert!(!mentions_variant("group_by: Option<GroupBy>", "GroupBy"));
    // Whole-word on the trailing side.
    assert!(!mentions_variant("Backend::S3Compatible", "S3"));
    assert!(!mentions_variant("anything", ""));
}

#[test]
fn an_externally_tagged_variant_counts_as_wired() {
    // `backend.s3` is `Backend::S3`, not a field named `s3` — without variant
    // matching the repo's whole discriminated-union convention reads as inert.
    let fields = vec![field("Repository.spec.backend.s3", "s3")];
    let r = evaluate(
        &fields,
        "match &repo.backend { Backend::S3(c) => c }",
        &Allowlist::default(),
    );
    assert!(r.ok(), "{r:?}");
}

#[test]
fn a_serde_rename_override_is_honored() {
    let fields = vec![field(
        "Repository.spec.parameters.epoch.advanceOnSizeMiB",
        "advance_on_size_mi_b",
    )];
    let mut allow = Allowlist::default();
    allow.rename.push(Rename {
        path: "Repository.spec.parameters.epoch.advanceOnSizeMiB".into(),
        ident: "advance_on_size_mb".into(),
        reason: "serde rename".into(),
    });
    // The derived ident finds nothing; the override finds the real field.
    let r = evaluate(&fields, "let v = e.advance_on_size_mb;", &allow);
    assert!(r.ok(), "{r:?}");
}

// --- the real schema -------------------------------------------------------

#[test]
fn schema_walk_finds_the_fields_from_the_two_shipped_bugs() {
    // Guards the walk itself: array-of-object descent (`sources[]`) and nested
    // objects both have to work, or the ratchet would silently examine nothing.
    let fields = schema_fields(&Allowlist::default());
    let paths: BTreeSet<&str> = fields.iter().map(|f| f.path.as_str()).collect();
    for want in [
        "SnapshotPolicy.spec.sources.pvcSelector",
        "SnapshotPolicy.spec.files.ignoreIdenticalSnapshots",
        "SnapshotPolicy.spec.groupBy",
        "SnapshotPolicy.spec.sources.sourcePathStrategy",
    ] {
        assert!(paths.contains(want), "missing {want}; walk is broken");
    }
    assert!(fields.len() > 200, "only {} fields walked", fields.len());
}

#[test]
fn prune_stops_the_walk_at_the_named_subtree() {
    let all = schema_fields(&Allowlist::default());
    let had_children = all.iter().any(|f| {
        f.path
            .starts_with("SnapshotPolicy.spec.sources.pvcSelector.")
    });
    assert!(had_children, "fixture assumption: pvcSelector has children");

    let prune = allow_of(&[], &[("SnapshotPolicy.spec.sources.pvcSelector", "test")]);
    let pruned = schema_fields(&prune);
    // The field ITSELF is still recorded — pruning stops the descent, it is not
    // an exemption — but nothing below it is.
    assert!(
        pruned
            .iter()
            .any(|f| f.path == "SnapshotPolicy.spec.sources.pvcSelector"),
        "pruning must not hide the kopiur-owned field that holds the type"
    );
    assert!(
        !pruned.iter().any(|f| f
            .path
            .starts_with("SnapshotPolicy.spec.sources.pvcSelector.")),
        "prune did not stop the walk"
    );
}

/// The ratchet, run for real against the working tree.
///
/// This is the test that actually fails CI when someone adds an inert field.
#[test]
fn every_crd_field_is_wired_or_allowlisted() {
    let allow = Allowlist::load().expect("wiring-allowlist.yaml");
    let fields = schema_fields(&allow);
    let corpus = consumer_corpus().expect("consumer sources");
    let report = evaluate(&fields, &corpus, &allow);
    assert!(
        report.ok(),
        "wiring ratchet failed.\n\
         offenders (defined + schema'd but read by nobody): {:#?}\n\
         stale (allowlisted but now wired — delete the entry): {:#?}\n\
         unknown (allowlist entry matches no field): {:#?}\n\
         Run `cargo xtask check-wiring` for the full explanation.",
        report.offenders,
        report.stale,
        report.unknown
    );
}

#[test]
fn every_allowlist_entry_carries_a_reason() {
    let allow = Allowlist::load().expect("wiring-allowlist.yaml");
    for e in allow.allow.iter().chain(allow.prune.iter()) {
        assert!(
            e.reason.trim().len() > 20,
            "allowlist entry `{}` needs a real reason, got {:?}",
            e.path,
            e.reason
        );
    }
}
