//! Hermetic tests for the snapshot-replication pure kernels: identity
//! selection (component globs), the migrate post-verify, copy-CR
//! naming/building, and the prune selections. No cluster, no kopia binary.

use super::*;

fn entry(
    id: &str,
    user: &str,
    host: &str,
    path: &str,
    start: &str,
    end: &str,
) -> SnapshotListEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "source": { "host": host, "userName": user, "path": path },
        "startTime": start,
        "endTime": end,
        "stats": { "totalSize": 4096, "fileCount": 3 },
    }))
    .expect("entry fixture")
}

fn t(user: &str, host: &str, path: &str) -> IdentityTriple {
    (user.into(), host.into(), path.into())
}

fn matcher(
    username: Option<&str>,
    hostname: Option<&str>,
    source_path: Option<&str>,
) -> IdentityMatcherSpec {
    IdentityMatcherSpec {
        username: username.map(str::to_string),
        hostname: hostname.map(str::to_string),
        source_path: source_path.map(str::to_string),
    }
}

fn ts(s: &str) -> DateTime<Utc> {
    s.parse().expect("timestamp fixture")
}

// --- component_glob_matches --------------------------------------------------

#[test]
fn component_glob_literal_star_and_question() {
    assert!(component_glob_matches("mydb", "mydb"));
    assert!(!component_glob_matches("mydb", "mydb2"));
    assert!(component_glob_matches("my*", "mydb"));
    assert!(component_glob_matches("*db", "mydb"));
    assert!(component_glob_matches("m*b", "mydb"));
    assert!(component_glob_matches("*", "anything"));
    assert!(component_glob_matches("my?b", "mydb"));
    assert!(!component_glob_matches("my?b", "mydddb"));
    // Empty pattern matches only the empty value.
    assert!(component_glob_matches("", ""));
    assert!(!component_glob_matches("", "x"));
    // Multiple stars.
    assert!(component_glob_matches("*a*c*", "xxaxxcxx"));
    assert!(!component_glob_matches("*a*c*", "xxaxxbxx"));
}

#[test]
fn component_glob_never_crosses_path_separators() {
    // `*` stays within one component: /pvc/* matches one level, not two.
    assert!(component_glob_matches("/pvc/*", "/pvc/mydb"));
    assert!(!component_glob_matches("/pvc/*", "/pvc/mydb/data"));
    assert!(component_glob_matches("/pvc/*/data", "/pvc/mydb/data"));
    // `?` never matches `/` either (segment counts must agree).
    assert!(!component_glob_matches("/pvc?mydb", "/pvc/mydb"));
    // Literal `/` must align.
    assert!(component_glob_matches("/a/b", "/a/b"));
    assert!(!component_glob_matches("/a/b", "/a/b/c"));
    assert!(!component_glob_matches("/a/b/c", "/a/b"));
}

// --- matcher_matches ---------------------------------------------------------

#[test]
fn matcher_requires_every_present_field_and_absent_is_wildcard() {
    let triple = t("mydb", "prod", "/pvc/mydb");
    // hostname-only matcher: username/path are wildcards.
    assert!(matcher_matches(&matcher(None, Some("prod"), None), &triple));
    // Every present field must match.
    assert!(!matcher_matches(
        &matcher(Some("other"), Some("prod"), None),
        &triple
    ));
    assert!(matcher_matches(
        &matcher(Some("my*"), Some("pro?"), Some("/pvc/*")),
        &triple
    ));
}

#[test]
fn all_absent_matcher_matches_nothing() {
    // Webhook-refused upstream; the defensive reading is match-NOTHING so an
    // invalid matcher can never select — or exclude — everything.
    assert!(!matcher_matches(
        &matcher(None, None, None),
        &t("u", "h", "/p")
    ));
}

#[test]
fn matching_is_on_the_structured_triple_not_a_rendered_string() {
    // A username that CONTAINS '@' must be matched as one component — a
    // rendered-string matcher would split it wrong.
    let odd = t("a@b", "c", "/p");
    assert!(matcher_matches(&matcher(Some("a@b"), None, None), &odd));
    assert!(!matcher_matches(&matcher(Some("a"), None, None), &odd));
    assert!(!matcher_matches(&matcher(None, Some("b"), None), &odd));
}

// --- select_identities -------------------------------------------------------

fn three_entries() -> Vec<SnapshotListEntry> {
    vec![
        entry(
            "s1",
            "mydb",
            "prod",
            "/pvc/mydb",
            "2026-08-01T02:00:00Z",
            "2026-08-01T02:05:00Z",
        ),
        entry(
            "s2",
            "otherdb",
            "prod",
            "/pvc/otherdb",
            "2026-08-01T03:00:00Z",
            "2026-08-01T03:05:00Z",
        ),
        entry(
            "s3",
            "mydb",
            "staging",
            "/pvc/mydb",
            "2026-08-01T04:00:00Z",
            "2026-08-01T04:05:00Z",
        ),
    ]
}

#[test]
fn empty_include_selects_every_identity() {
    let selected = select_identities(&[], &[], &three_entries());
    assert_eq!(selected.len(), 3);
}

#[test]
fn include_narrows_and_exclude_wins() {
    let entries = three_entries();
    // Include everything on prod...
    let selected = select_identities(&[matcher(None, Some("prod"), None)], &[], &entries);
    assert_eq!(
        selected,
        BTreeSet::from([
            t("mydb", "prod", "/pvc/mydb"),
            t("otherdb", "prod", "/pvc/otherdb")
        ])
    );
    // ...but an exclude beats an include that also matches.
    let selected = select_identities(
        &[matcher(None, Some("prod"), None)],
        &[matcher(Some("other*"), None, None)],
        &entries,
    );
    assert_eq!(selected, BTreeSet::from([t("mydb", "prod", "/pvc/mydb")]));
}

#[test]
fn exclude_alone_subtracts_from_the_implicit_all() {
    let selected = select_identities(
        &[],
        &[matcher(None, Some("staging"), None)],
        &three_entries(),
    );
    assert_eq!(selected.len(), 2);
    assert!(!selected.contains(&t("mydb", "staging", "/pvc/mydb")));
}

// --- expected_keys / missing_after_migrate -----------------------------------

#[test]
fn missing_after_migrate_reports_only_absent_expected_pairs() {
    let source = three_entries();
    let selected = select_identities(&[], &[], &source);
    // Destination holds s1's pair (under a NEW dest manifest id — the id is
    // irrelevant, the key is (identity, startTime)) but not s2's / s3's.
    let dest = vec![entry(
        "d1",
        "mydb",
        "prod",
        "/pvc/mydb",
        "2026-08-01T02:00:00Z",
        "2026-08-01T02:05:00Z",
    )];
    let missing = missing_after_migrate(&source, &selected, &dest, false);
    assert_eq!(missing.len(), 2);
    assert!(missing.contains(&(
        t("otherdb", "prod", "/pvc/otherdb"),
        ts("2026-08-01T03:00:00Z")
    )));
    assert!(missing.contains(&(
        t("mydb", "staging", "/pvc/mydb"),
        ts("2026-08-01T04:00:00Z")
    )));

    // Everything present ⇒ nothing missing.
    let dest_full: Vec<_> = source
        .iter()
        .enumerate()
        .map(|(i, e)| {
            entry(
                &format!("d{i}"),
                &e.source.user_name,
                &e.source.host,
                &e.source.path,
                &e.start_time.to_rfc3339(),
                &e.end_time.to_rfc3339(),
            )
        })
        .collect();
    assert!(missing_after_migrate(&source, &selected, &dest_full, false).is_empty());
}

#[test]
fn latest_only_expects_only_the_newest_snapshot_per_identity() {
    let source = vec![
        entry(
            "old",
            "mydb",
            "prod",
            "/pvc/mydb",
            "2026-08-01T02:00:00Z",
            "2026-08-01T02:05:00Z",
        ),
        entry(
            "new",
            "mydb",
            "prod",
            "/pvc/mydb",
            "2026-08-02T02:00:00Z",
            "2026-08-02T02:05:00Z",
        ),
    ];
    let selected = select_identities(&[], &[], &source);
    let expected = expected_keys(&source, &selected, true);
    assert_eq!(
        expected,
        BTreeSet::from([(t("mydb", "prod", "/pvc/mydb"), ts("2026-08-02T02:00:00Z"))])
    );
    // A destination holding only the newest is complete under latest-only…
    let dest = vec![entry(
        "d",
        "mydb",
        "prod",
        "/pvc/mydb",
        "2026-08-02T02:00:00Z",
        "2026-08-02T02:05:00Z",
    )];
    assert!(missing_after_migrate(&source, &selected, &dest, true).is_empty());
    // …but incomplete under full-history.
    assert_eq!(
        missing_after_migrate(&source, &selected, &dest, false).len(),
        1
    );
}

#[test]
fn unselected_identities_never_count_as_expected_or_present() {
    let source = three_entries();
    let selected = select_identities(&[matcher(Some("mydb"), Some("prod"), None)], &[], &source);
    // A dest holding ONLY an unselected identity contributes nothing.
    let dest = vec![entry(
        "d2",
        "otherdb",
        "prod",
        "/pvc/otherdb",
        "2026-08-01T03:00:00Z",
        "2026-08-01T03:05:00Z",
    )];
    let missing = missing_after_migrate(&source, &selected, &dest, false);
    assert_eq!(
        missing,
        vec![(t("mydb", "prod", "/pvc/mydb"), ts("2026-08-01T02:00:00Z"))]
    );
}

#[test]
fn missing_sample_caps_and_renders_identity_at_start_time() {
    let missing: Vec<SnapKey> = (0..15)
        .map(|i| (t("u", "h", &format!("/p{i}")), ts("2026-08-01T02:00:00Z")))
        .collect();
    let sample = missing_sample(&missing, 10);
    assert_eq!(sample.matches("u@h:").count(), 10, "capped at 10: {sample}");
    assert!(
        sample.contains("u@h:/p0@2026-08-01T02:00:00+00:00"),
        "{sample}"
    );
}

// --- copy_cr_name ------------------------------------------------------------

#[test]
fn copy_cr_name_is_deterministic_and_capped() {
    let a = copy_cr_name("offsite-mirror", "k1f1ec0a8deadbeefcafe0123456789ab");
    let b = copy_cr_name("offsite-mirror", "k1f1ec0a8deadbeefcafe0123456789ab");
    assert_eq!(a, b, "same inputs must mint the same name (SSA resume)");
    assert!(a.starts_with("offsite-mirror-copy-k1f1ec0a8deadbee"), "{a}");
    assert!(a.len() <= 63);

    // A very long replication name still yields a valid, deterministic name.
    let long = "r".repeat(80);
    let capped = copy_cr_name(&long, "k1f1ec0a8deadbeefcafe0123456789ab");
    assert!(capped.len() <= 63, "{} chars", capped.len());
    assert_eq!(
        capped,
        copy_cr_name(&long, "k1f1ec0a8deadbeefcafe0123456789ab")
    );
}

#[test]
fn copy_cr_names_differ_for_ids_sharing_a_16_char_prefix() {
    // The first-16 prefix collides; the trailing hash of the FULL id must
    // still keep the names distinct (the adoption-name lesson).
    let shared = "aaaaaaaaaaaaaaaa";
    let a = copy_cr_name("repl", &format!("{shared}111"));
    let b = copy_cr_name("repl", &format!("{shared}222"));
    assert_ne!(a, b);
}

// --- build_copy_snapshot -----------------------------------------------------

fn dest_repo() -> ReplicationRepositoryRef {
    ReplicationRepositoryRef {
        kind: "ClusterRepository".into(),
        name: "offsite".into(),
        namespace: None,
        uid: "dest-uid-1".into(),
    }
}

fn source_repo() -> ReplicationSourceRef {
    ReplicationSourceRef {
        kind: "Repository".into(),
        name: "nas-primary".into(),
        namespace: Some("backups".into()),
    }
}

/// One canonical build shared by the per-facet tests below (kept apart so no
/// single test trips the cognitive-complexity ratchet).
fn built_copy() -> (kopiur_api::snapshot::Snapshot, SnapshotStatus) {
    let e = entry(
        "destid1",
        "mydb",
        "prod",
        "/pvc/mydb",
        "2026-08-01T02:00:00Z",
        "2026-08-01T02:05:00Z",
    );
    build_copy_snapshot(
        "offsite-mirror",
        "backups",
        &dest_repo(),
        &source_repo(),
        &e,
        "srcid9",
    )
}

#[test]
fn build_copy_snapshot_metadata_and_labels() {
    let (snap, _) = built_copy();
    assert_eq!(
        snap.metadata.name.as_deref(),
        Some(copy_cr_name("offsite-mirror", "destid1").as_str())
    );
    assert_eq!(snap.metadata.namespace.as_deref(), Some("backups"));
    let labels = snap.metadata.labels.as_ref().expect("labels");
    assert_eq!(labels[ORIGIN_LABEL], "replicated");
    assert_eq!(labels[SNAPSHOT_ID_LABEL], "destid1");
    assert_eq!(labels[REPOSITORY_UID_LABEL], "dest-uid-1");
    assert_eq!(labels[SNAPSHOT_REPLICATION_LABEL], "offsite-mirror");
    assert_eq!(labels.len(), 4);
    // NO ownerReferences: deleting the SnapshotReplication never cascades.
    assert!(snap.metadata.owner_references.is_none());
}

#[test]
fn build_copy_snapshot_spec_pin_and_deletion_policy() {
    let (snap, _) = built_copy();
    let pin = snap.spec.repository.as_ref().expect("spec.repository pin");
    assert_eq!(pin.kind, RepositoryKind::ClusterRepository);
    assert_eq!(pin.name, "offsite");
    assert!(snap.spec.policy_ref.is_none());
    assert_eq!(snap.spec.deletion_policy, Some(DeletionPolicy::Delete));
    assert!(!snap.spec.pin);
}

#[test]
fn build_copy_snapshot_status_core() {
    let (_, status) = built_copy();
    assert_eq!(status.phase, Some(SnapshotPhase::Succeeded));
    assert_eq!(status.origin, Some(Origin::Replicated));
    let info = status.snapshot.as_ref().expect("status.snapshot");
    assert_eq!(info.kopia_snapshot_id, "destid1");
    assert_eq!(info.identity.username, "mydb");
    assert_eq!(info.identity.hostname, "prod");
    assert_eq!(info.identity.source_path.as_deref(), Some("/pvc/mydb"));
    let resolved = status.resolved.as_ref().expect("status.resolved");
    assert_eq!(resolved.repository.as_ref().unwrap().name, "offsite");
}

#[test]
fn build_copy_snapshot_lineage_and_timing() {
    let (_, status) = built_copy();
    let cf = status.copied_from.as_ref().expect("status.copiedFrom");
    assert_eq!(cf.repository.kind, RepositoryKind::Repository);
    assert_eq!(cf.repository.name, "nas-primary");
    assert_eq!(cf.repository.namespace.as_deref(), Some("backups"));
    assert_eq!(cf.source_manifest_id, "srcid9");
    assert_eq!(cf.start_time, "2026-08-01T02:00:00+00:00");
    let timing = status.timing.as_ref().expect("timing");
    assert_eq!(timing.duration_seconds, Some(300));
    assert_eq!(status.stats.as_ref().unwrap().size_bytes, Some(4096));
}

#[test]
fn build_copy_snapshot_truncates_a_foreign_description() {
    let mut e = entry(
        "destid2",
        "u",
        "h",
        "/p",
        "2026-08-01T02:00:00Z",
        "2026-08-01T02:05:00Z",
    );
    e.description = "☃".repeat(1000); // 3000 bytes of snowmen
    let (_, status) = build_copy_snapshot("r", "ns", &dest_repo(), &source_repo(), &e, "src");
    let desc = status
        .snapshot
        .as_ref()
        .unwrap()
        .description
        .as_ref()
        .expect("description carried");
    assert!(desc.len() <= 1024);
    assert!(desc.chars().all(|c| c == '☃'), "char-boundary-safe cut");

    // Empty description is elided, not Some("").
    e.description = String::new();
    let (_, status) = build_copy_snapshot("r", "ns", &dest_repo(), &source_repo(), &e, "src");
    assert!(status.snapshot.as_ref().unwrap().description.is_none());
}

// --- correspondence_set ------------------------------------------------------

#[test]
fn correspondence_covers_only_selected_source_present_dest_manifests() {
    let source = three_entries();
    let selected = select_identities(&[matcher(None, Some("prod"), None)], &[], &source);
    let dest = vec![
        // Selected + present in source ⇒ in the set, with s1's source id.
        entry(
            "d1",
            "mydb",
            "prod",
            "/pvc/mydb",
            "2026-08-01T02:00:00Z",
            "2026-08-01T02:05:00Z",
        ),
        // Selected identity but a startTime the source does not have (e.g. a
        // direct backup into the dest) ⇒ NOT a copy, never a copy CR.
        entry(
            "d2",
            "mydb",
            "prod",
            "/pvc/mydb",
            "2027-01-01T00:00:00Z",
            "2027-01-01T00:05:00Z",
        ),
        // Unselected identity ⇒ ignored.
        entry(
            "d3",
            "mydb",
            "staging",
            "/pvc/mydb",
            "2026-08-01T04:00:00Z",
            "2026-08-01T04:05:00Z",
        ),
    ];
    let set = correspondence_set(&source, &selected, &dest);
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].dest_entry.id, "d1");
    assert_eq!(set[0].source_manifest_id, "s1");
}

// --- prune selections --------------------------------------------------------

fn row(name: &str, identity: IdentityTriple, start: &str, end: &str, pinned: bool) -> CopyRow {
    CopyRow {
        name: name.into(),
        identity,
        start_time: Some(ts(start)),
        end_time: Some(ts(end)),
        pinned,
    }
}

#[test]
fn retention_prunes_per_identity_buckets_independently() {
    // keepLatest: 1 with TWO identities must keep one EACH (per-identity
    // bucketing), never one overall.
    let a = t("a", "h", "/pa");
    let b = t("b", "h", "/pb");
    let rows = vec![
        row(
            "a-new",
            a.clone(),
            "2026-08-02T00:00:00Z",
            "2026-08-02T00:05:00Z",
            false,
        ),
        row(
            "a-old",
            a.clone(),
            "2026-08-01T00:00:00Z",
            "2026-08-01T00:05:00Z",
            false,
        ),
        row(
            "b-new",
            b.clone(),
            "2026-08-02T01:00:00Z",
            "2026-08-02T01:05:00Z",
            false,
        ),
        row(
            "b-old",
            b.clone(),
            "2026-08-01T01:00:00Z",
            "2026-08-01T01:05:00Z",
            false,
        ),
    ];
    let retention: Retention =
        serde_json::from_value(serde_json::json!({ "keepLatest": 1 })).unwrap();
    let delete = retention_prune_names(&rows, &retention);
    assert_eq!(delete, vec!["a-old", "b-old"]);
}

#[test]
fn retention_exempts_pinned_and_keeps_rows_without_end_time() {
    let a = t("a", "h", "/pa");
    let mut no_end = row(
        "a-no-end",
        a.clone(),
        "2026-07-01T00:00:00Z",
        "2026-07-01T00:05:00Z",
        false,
    );
    no_end.end_time = None;
    let rows = vec![
        row(
            "a-new",
            a.clone(),
            "2026-08-02T00:00:00Z",
            "2026-08-02T00:05:00Z",
            false,
        ),
        row(
            "a-pinned",
            a.clone(),
            "2026-08-01T00:00:00Z",
            "2026-08-01T00:05:00Z",
            true,
        ),
        row(
            "a-old",
            a.clone(),
            "2026-07-31T00:00:00Z",
            "2026-07-31T00:05:00Z",
            false,
        ),
        no_end,
    ];
    let retention: Retention =
        serde_json::from_value(serde_json::json!({ "keepLatest": 1 })).unwrap();
    let delete = retention_prune_names(&rows, &retention);
    // Pinned exempt; missing end_time conservatively kept; only a-old goes.
    assert_eq!(delete, vec!["a-old"]);
}

#[test]
fn mirror_source_prunes_exactly_the_vanished_pairs() {
    let a = t("a", "h", "/pa");
    let source_keys = BTreeSet::from([(a.clone(), ts("2026-08-02T00:00:00Z"))]);
    let mut no_start = row(
        "a-no-start",
        a.clone(),
        "2026-08-01T00:00:00Z",
        "2026-08-01T00:05:00Z",
        false,
    );
    no_start.start_time = None;
    let rows = vec![
        // Still on the source ⇒ kept.
        row(
            "a-live",
            a.clone(),
            "2026-08-02T00:00:00Z",
            "2026-08-02T00:05:00Z",
            false,
        ),
        // Vanished from the source ⇒ pruned.
        row(
            "a-gone",
            a.clone(),
            "2026-08-01T00:00:00Z",
            "2026-08-01T00:05:00Z",
            false,
        ),
        // Vanished but pinned ⇒ exempt.
        row(
            "a-gone-pinned",
            a.clone(),
            "2026-07-01T00:00:00Z",
            "2026-07-01T00:05:00Z",
            true,
        ),
        // No parseable startTime ⇒ conservatively kept.
        no_start,
    ];
    let delete = mirror_prune_names(&rows, &source_keys);
    assert_eq!(delete, vec!["a-gone"]);
}

// --- copy_row_from_snapshot --------------------------------------------------

#[test]
fn copy_row_extracts_identity_timing_and_pin() {
    let e = entry(
        "destid1",
        "mydb",
        "prod",
        "/pvc/mydb",
        "2026-08-01T02:00:00Z",
        "2026-08-01T02:05:00Z",
    );
    let (mut snap, status) =
        build_copy_snapshot("r", "ns", &dest_repo(), &source_repo(), &e, "src");
    snap.status = Some(status);
    let row = copy_row_from_snapshot(&snap).expect("row");
    assert_eq!(row.identity, t("mydb", "prod", "/pvc/mydb"));
    assert_eq!(row.start_time, Some(ts("2026-08-01T02:00:00Z")));
    assert_eq!(row.end_time, Some(ts("2026-08-01T02:05:00Z")));
    assert!(!row.pinned);

    // A row with no status (or no identity) is never prunable.
    snap.status = None;
    assert!(copy_row_from_snapshot(&snap).is_none());
}

// --- repository ref mapping --------------------------------------------------

#[test]
fn wire_repo_refs_map_kinds_via_the_serde_values() {
    let d = dest_repository_ref(&dest_repo());
    assert_eq!(d.kind, RepositoryKind::ClusterRepository);
    assert_eq!(d.name, "offsite");
    assert_eq!(d.namespace, None);
    let s = source_repository_ref(&source_repo());
    assert_eq!(s.kind, RepositoryKind::Repository);
    assert_eq!(s.namespace.as_deref(), Some("backups"));
}
