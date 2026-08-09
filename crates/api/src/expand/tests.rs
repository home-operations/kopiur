//! Unit tests for selector expansion (#346).
//!
//! Everything here is pure: the cluster IO lives in `match_pvcs`, and the
//! decisions — which source governs, what the kopia path is, what the child is
//! called, and whether two matched PVCs would collide — are all testable
//! off-cluster.

use super::*;
use crate::snapshot_policy::{NamespaceSelector, PvcSelector, PvcSource};

fn policy_with(sources: Vec<Source>) -> SnapshotPolicy {
    let mut p: SnapshotPolicy = serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "app", "namespace": "billing" },
        "spec": {
            "repository": { "kind": "Repository", "name": "nas" },
            "sources": [],
        }
    }))
    .expect("valid SnapshotPolicy");
    p.spec.sources = sources;
    p
}

fn selector_source(strategy: Option<SourcePathStrategy>, namespaces: Vec<&str>) -> Source {
    Source {
        pvc: None,
        pvc_selector: Some(PvcSelector {
            namespace_selector: if namespaces.is_empty() {
                None
            } else {
                Some(NamespaceSelector {
                    match_names: namespaces.into_iter().map(String::from).collect(),
                })
            },
            label_selector: None,
        }),
        nfs: None,
        read_only: None,
        acknowledge_live_mutation: None,
        source_path_override: None,
        source_path_strategy: strategy,
    }
}

fn pvc_source(name: &str) -> Source {
    Source {
        pvc: Some(PvcSource {
            name: name.to_string(),
        }),
        pvc_selector: None,
        nfs: None,
        read_only: None,
        acknowledge_live_mutation: None,
        source_path_override: None,
        source_path_strategy: None,
    }
}

fn target(ns: &str, name: &str) -> PvcTargetRef {
    PvcTargetRef {
        namespace: ns.to_string(),
        name: name.to_string(),
    }
}

// --- effective_source -------------------------------------------------------

#[test]
fn an_unpinned_snapshot_resolves_the_first_source_exactly_as_before() {
    // The single-source path must be byte-for-byte unchanged: any drift here
    // re-identifies every existing policy's kopia source.
    let p = policy_with(vec![pvc_source("data")]);
    let eff = effective_source(&p, None).expect("resolves");
    assert_eq!(eff.index, 0);
    assert_eq!(eff.pvc.as_ref().map(|t| t.name.as_str()), Some("data"));
    assert_eq!(
        eff.kopia_source_path(SourcePathStrategy::PvcName)
            .as_deref(),
        Some("/pvc/data")
    );
}

#[test]
fn a_pinned_child_uses_its_own_pvc_and_the_indexed_sources_knobs() {
    let mut sel = selector_source(Some(SourcePathStrategy::PvcNamespacedName), vec![]);
    sel.read_only = Some(false);
    let p = policy_with(vec![pvc_source("ignored"), sel]);
    let pin = SnapshotSourceRef {
        source_index: 1,
        target: SnapshotSourceTarget::Pvc(target("web", "assets")),
        group: None,
    };
    let eff = effective_source(&p, Some(&pin)).expect("resolves");
    assert_eq!(eff.index, 1);
    assert_eq!(eff.pvc.as_ref().unwrap().namespace, "web");
    assert!(!eff.read_only, "knobs come from sources[sourceIndex]");
    assert_eq!(
        eff.kopia_source_path(SourcePathStrategy::PvcNamespacedName)
            .as_deref(),
        Some("/pvc/web/assets")
    );
}

#[test]
fn an_out_of_range_source_index_fails_loudly_instead_of_falling_back() {
    // Silently backing up a DIFFERENT volume than the CR names is the worst
    // possible failure mode for a backup operator, so this must never degrade
    // to `sources[0]`.
    let p = policy_with(vec![pvc_source("data")]);
    let pin = SnapshotSourceRef {
        source_index: 7,
        target: SnapshotSourceTarget::Pvc(target("billing", "gone")),
        group: None,
    };
    let err = effective_source(&p, Some(&pin)).expect_err("must not fall back");
    let msg = err.to_string();
    assert!(msg.contains("sourceIndex"), "got: {msg}");
    assert!(msg.contains("edited"), "message must say why: {msg}");
}

#[test]
fn source_path_override_wins_over_every_strategy() {
    let mut s = selector_source(Some(SourcePathStrategy::PvcNamespacedName), vec![]);
    s.source_path_override = Some("/mnt/custom".into());
    let p = policy_with(vec![s]);
    let pin = SnapshotSourceRef {
        source_index: 0,
        target: SnapshotSourceTarget::Pvc(target("billing", "d")),
        group: None,
    };
    let eff = effective_source(&p, Some(&pin)).unwrap();
    assert_eq!(
        eff.kopia_source_path(SourcePathStrategy::PvcNamespacedName)
            .as_deref(),
        Some("/mnt/custom")
    );
}

// --- strategy_for -----------------------------------------------------------

#[test]
fn strategy_is_ignored_for_a_plain_pvc_source() {
    // Honoring it there would change `/pvc/<name>` for existing single-PVC
    // policies, re-identifying their kopia source and orphaning every manifest.
    let mut s = pvc_source("data");
    s.source_path_strategy = Some(SourcePathStrategy::PvcNamespacedName);
    assert_eq!(strategy_for(&s), SourcePathStrategy::PvcName);
}

#[test]
fn strategy_is_honored_for_a_selector_source() {
    let s = selector_source(Some(SourcePathStrategy::PvcNamespacedName), vec![]);
    assert_eq!(strategy_for(&s), SourcePathStrategy::PvcNamespacedName);
    let s = selector_source(None, vec![]);
    assert_eq!(strategy_for(&s), SourcePathStrategy::PvcName);
}

// --- naming -----------------------------------------------------------------

#[test]
fn child_names_are_deterministic_legible_and_injective() {
    let a = fanout_child_name("nightly-20260805020000", "billing", "billing", "pgdata");
    let b = fanout_child_name("nightly-20260805020000", "billing", "billing", "pgdata");
    assert_eq!(a, b, "must be deterministic — it is the idempotency key");
    assert!(a.contains("-pvc-pgdata-"), "human-legible: {a}");

    let other = fanout_child_name("nightly-20260805020000", "billing", "billing", "assets");
    assert_ne!(a, other);
}

#[test]
fn two_slots_of_the_same_schedule_and_pvc_never_collide() {
    // The clip budget eats `base` first, and `base` carries the slot stamp — so
    // a tag hashing only the PVC leaves two runs with byte-identical names. The
    // schedule force-SSAs and only skips *terminating* twins, so the second fire
    // would re-apply onto the already-Succeeded first Snapshot, `run_decision`
    // would say SucceededSteadyState, and no mover Job would ever launch: a
    // whole backup slot vanishing with no error anywhere.
    let long_schedule = "nightly-database-backups";
    let a = fanout_child_name(
        &format!("{long_schedule}-20260805020000"),
        "db",
        "db",
        "postgres-data-vol",
    );
    let b = fanout_child_name(
        &format!("{long_schedule}-20260805140000"),
        "db",
        "db",
        "postgres-data-vol",
    );
    assert!(a.len() <= 63 && b.len() <= 63);
    assert_ne!(
        a, b,
        "two slots must produce distinct names even when the stamp is clipped"
    );
}

#[test]
fn a_cross_namespace_pvc_gets_the_namespace_in_its_slug() {
    let same = fanout_child_name("n-1", "billing", "billing", "data");
    let other = fanout_child_name("n-1", "billing", "web", "data");
    assert_ne!(same, other, "same PVC name in two namespaces must differ");
    assert!(other.contains("web-data"), "got: {other}");
}

#[test]
fn child_names_never_exceed_the_63_char_job_label_cap() {
    // >63 silently breaks staged-PVC teardown: `cleanup_staged_source` finds the
    // mover pods by the `batch.kubernetes.io/job-name` LABEL VALUE, and
    // Kubernetes caps a label value at 63 bytes.
    let long_base = "a".repeat(200);
    let long_pvc = "b".repeat(200);
    let n = fanout_child_name(&long_base, "ns", "other-namespace-that-is-long", &long_pvc);
    assert!(n.len() <= 63, "len {} for {n}", n.len());
    assert!(
        !n.starts_with('-') && !n.ends_with('-'),
        "not DNS-1123: {n}"
    );
    // The hash survives clipping, so two long names still differ.
    let m = fanout_child_name(
        &long_base,
        "ns",
        "other-namespace-that-is-long",
        "c".repeat(200).as_str(),
    );
    assert_ne!(n, m, "the injectivity tag must never be clipped");
}

#[test]
fn child_names_cannot_collide_with_the_unfanned_schemes() {
    // Every un-fanned name ends in a dash-free 14-digit slot stamp; a fanned
    // name's tail after `-pvc-` always contains a `-`.
    let n = fanout_child_name("sched-20260805020000", "ns", "ns", "data");
    let tail = n.rsplit_once("-pvc-").expect("marker present").1;
    assert!(
        tail.contains('-'),
        "tail {tail} must not look like a slot stamp"
    );
}

#[test]
fn a_pvc_name_with_illegal_characters_is_sanitized() {
    let n = fanout_child_name("base", "ns", "ns", "Data_Vol.1");
    assert!(
        n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "not DNS-1123: {n}"
    );
}

// --- expand_sources ---------------------------------------------------------

#[test]
fn a_policy_with_no_selector_expands_to_none_not_to_an_empty_list() {
    // `None` means "mint one child, unpinned" (today's behavior); an empty Vec
    // would mean "a selector matched nothing", which is a different and much
    // louder situation.
    let p = policy_with(vec![pvc_source("data")]);
    assert_eq!(expand_sources(&p, "base", &BTreeMap::new()).unwrap(), None);
}

#[test]
fn a_selector_expands_to_one_pinned_member_per_matched_pvc() {
    let p = policy_with(vec![selector_source(None, vec![])]);
    let matched = BTreeMap::from([(0, vec![target("billing", "a"), target("billing", "b")])]);
    let members = expand_sources(&p, "nightly-1", &matched)
        .unwrap()
        .expect("selector present");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].source.source_index, 0);
    match &members[0].source.target {
        SnapshotSourceTarget::Pvc(t) => assert_eq!(t.name, "a"),
    }
    assert_ne!(members[0].name, members[1].name);
}

#[test]
fn a_selector_that_matches_nothing_expands_to_an_empty_list() {
    let p = policy_with(vec![selector_source(None, vec![])]);
    let matched = BTreeMap::from([(0, vec![])]);
    let members = expand_sources(&p, "nightly-1", &matched).unwrap().unwrap();
    assert!(members.is_empty());
}

#[test]
fn same_named_pvcs_in_two_namespaces_are_refused_under_pvcname() {
    // Defense in depth: a cross-namespace selector is now refused outright
    // (`validate_source`), because a mover Pod cannot mount a PVC from another
    // namespace anyway. This guard stays because the failure it prevents is
    // silent data loss — both PVCs resolve to `/pvc/data`, so two volumes'
    // histories would merge into one kopia source and prune each other — and
    // `detect_identity_collision` cannot see it (it compares ACROSS policies
    // and skips self).
    let p = policy_with(vec![selector_source(None, vec!["billing", "web"])]);
    let matched = BTreeMap::from([(0, vec![target("billing", "data"), target("web", "data")])]);
    let err = expand_sources(&p, "nightly-1", &matched).expect_err("must refuse");
    let msg = err.to_string();
    assert!(msg.contains("/pvc/data"), "must name the path: {msg}");
    assert!(
        msg.contains("PvcNamespacedName"),
        "must name the fix: {msg}"
    );
}

#[test]
fn pvcnamespacedname_resolves_the_collision() {
    let p = policy_with(vec![selector_source(
        Some(SourcePathStrategy::PvcNamespacedName),
        vec!["billing", "web"],
    )]);
    let matched = BTreeMap::from([(0, vec![target("billing", "data"), target("web", "data")])]);
    let members = expand_sources(&p, "nightly-1", &matched)
        .expect("no collision")
        .unwrap();
    assert_eq!(members.len(), 2);
}

#[test]
fn the_same_pvc_matched_twice_is_not_a_collision() {
    // Idempotence: re-listing the same PVC must not read as two volumes
    // fighting over one path.
    let p = policy_with(vec![selector_source(None, vec![])]);
    let matched = BTreeMap::from([(
        0,
        vec![target("billing", "data"), target("billing", "data")],
    )]);
    assert!(expand_sources(&p, "n", &matched).is_ok());
}

// --- groupBy: VolumeGroupSnapshot -------------------------------------------

fn grouped_policy(sources: Vec<Source>) -> SnapshotPolicy {
    let mut p = policy_with(sources);
    p.spec.group_by = Some(crate::snapshot_policy::GroupBy::VolumeGroupSnapshot);
    p
}

#[test]
fn a_multi_member_group_pins_one_shared_volumegroupsnapshot_per_namespace() {
    // A VolumeGroupSnapshot is namespaced and its source.selector is
    // namespace-local, so a selector spanning namespaces yields ONE GROUP PER
    // NAMESPACE — the consistency guarantee is per-namespace, and pretending
    // otherwise would promise crash-consistency the CSI layer cannot give.
    let p = grouped_policy(vec![selector_source(
        Some(SourcePathStrategy::PvcNamespacedName),
        vec!["billing", "web"],
    )]);
    let matched = BTreeMap::from([(
        0,
        vec![
            target("billing", "a"),
            target("billing", "b"),
            target("web", "c"),
            target("web", "d"),
        ],
    )]);
    let members = expand_sources(&p, "nightly-1", &matched).unwrap().unwrap();
    assert_eq!(members.len(), 4);

    let group_of = |ns: &str| -> String {
        members
            .iter()
            .find(|m| match &m.source.target {
                SnapshotSourceTarget::Pvc(t) => t.namespace == ns,
            })
            .and_then(|m| m.source.group.as_ref())
            .map(|g| g.volume_group_snapshot_name.clone())
            .expect("group present")
    };
    let billing = group_of("billing");
    let web = group_of("web");
    assert_ne!(billing, web, "one group per namespace, not one overall");

    // Every member of a namespace must pin the IDENTICAL name — that is what
    // makes their racing server-side-applies converge on one object.
    for m in &members {
        let SnapshotSourceTarget::Pvc(t) = &m.source.target;
        let g = m.source.group.as_ref().expect("group present");
        assert_eq!(g.namespace, t.namespace);
        let want = if t.namespace == "billing" {
            &billing
        } else {
            &web
        };
        assert_eq!(&g.volume_group_snapshot_name, want);
    }
}

#[test]
fn a_single_member_group_degrades_to_the_plain_per_pvc_path() {
    // A one-PVC "group" buys nothing and costs a VolumeGroupSnapshotClass — plus
    // a Beta API group most clusters do not serve. Requiring it there would make
    // `groupBy: VolumeGroupSnapshot` fail on setups where it is meaningless.
    let p = grouped_policy(vec![selector_source(None, vec![])]);
    let matched = BTreeMap::from([(0, vec![target("billing", "only")])]);
    let members = expand_sources(&p, "nightly-1", &matched).unwrap().unwrap();
    assert_eq!(members.len(), 1);
    assert!(members[0].source.group.is_none());
}

#[test]
fn group_by_none_never_pins_a_group() {
    let mut p = policy_with(vec![selector_source(None, vec![])]);
    p.spec.group_by = Some(crate::snapshot_policy::GroupBy::None);
    let matched = BTreeMap::from([(0, vec![target("billing", "a"), target("billing", "b")])]);
    let members = expand_sources(&p, "n", &matched).unwrap().unwrap();
    assert!(members.iter().all(|m| m.source.group.is_none()));
}

#[test]
fn group_names_are_deterministic_and_bounded() {
    assert_eq!(
        group_name("base", "ns", 0, None),
        group_name("base", "ns", 0, None)
    );
    assert_ne!(
        group_name("base", "ns", 0, None),
        group_name("base", "other", 0, None)
    );
    // Two selector sources in ONE namespace are two separate captures, built
    // from two different label selectors — they must not share an object.
    assert_ne!(
        group_name("base", "ns", 0, None),
        group_name("base", "ns", 1, None)
    );
    let n = group_name(&"a".repeat(200), "ns", 0, None);
    assert!(n.len() <= 63, "len {} for {n}", n.len());
    assert!(n.ends_with("-grp"), "{n}");
}

#[test]
fn two_sources_matching_one_pvc_are_refused() {
    // Both land on the same kopia path AND the same child name, so the second
    // would force-server-side-apply over the first and one backup would vanish
    // with no error — the same silent-overwrite class as a clipped slot stamp.
    let p = policy_with(vec![
        selector_source(None, vec![]),
        selector_source(None, vec![]),
    ]);
    let matched = BTreeMap::from([
        (0, vec![target("billing", "shared")]),
        (1, vec![target("billing", "shared")]),
    ]);
    let err = expand_sources(&p, "n", &matched).expect_err("overlapping selectors must be refused");
    let msg = err.to_string();
    assert!(msg.contains("billing/shared"), "must name the PVC: {msg}");
    assert!(
        msg.contains("Narrow the selectors"),
        "must name the fix: {msg}"
    );
}

#[test]
fn a_cross_namespace_namespace_selector_is_refused_at_admission() {
    // Structural, not a kopiur limitation: a mover Pod can only mount PVCs in
    // its OWN namespace, and the Job runs in the Snapshot's (= the policy's).
    // Accepting it would either fail at reconcile with "source PVC not found"
    // or — with a same-named PVC present locally — silently snapshot the WRONG
    // volume under the matched one's identity.
    let src = selector_source(None, vec!["other"]);
    let err = crate::validate::validate_source(&src).expect_err("must be refused");
    let msg = err.to_string();
    assert!(msg.contains("own namespace"), "must say why: {msg}");
    assert!(
        msg.contains("one SnapshotPolicy per namespace"),
        "must name the fix: {msg}"
    );
    // The policy's own namespace is fine (it is the only reachable one).
    assert!(crate::validate::validate_source(&selector_source(None, vec![])).is_ok());
}

#[test]
fn an_unusable_match_expression_is_refused_not_dropped() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, LabelSelectorRequirement};
    let with_expr = |op: &str, values: Option<Vec<String>>| {
        let mut s = selector_source(None, vec![]);
        s.pvc_selector = Some(PvcSelector {
            namespace_selector: None,
            label_selector: Some(LabelSelector {
                match_labels: None,
                match_expressions: Some(vec![LabelSelectorRequirement {
                    key: "tier".into(),
                    operator: op.into(),
                    values,
                }]),
            }),
        });
        s
    };
    // A typo would otherwise be silently dropped — WIDENING the selector, so
    // PVCs the user meant to exclude get backed up.
    let err = crate::validate::validate_source(&with_expr("in", Some(vec!["db".into()])))
        .expect_err("a bad operator must be refused");
    assert!(err.to_string().contains("WIDENING"), "{err}");

    // `In` with no values renders as `tier in ()`, which the API server rejects
    // with a 400 that would abort the whole schedule fire.
    let err = crate::validate::validate_source(&with_expr("In", Some(vec![])))
        .expect_err("an empty value list must be refused");
    assert!(err.to_string().contains("at least one value"), "{err}");

    assert!(crate::validate::validate_source(&with_expr("Exists", None)).is_ok());
    assert!(crate::validate::validate_source(&with_expr("NotIn", Some(vec!["db".into()]))).is_ok());
}

// --- M7 multi-repo naming: golden byte-compat + injectivity ------------------

/// Shorthand ref builder for the naming tests.
fn rref(kind: crate::common::RepositoryKind, name: &str) -> crate::common::RepositoryRef {
    crate::common::RepositoryRef {
        kind,
        name: name.into(),
        namespace: None,
    }
}

/// THE byte-compat proof for `fanout_child_name_for`: the legacy forms are
/// pinned as golden strings computed from the PRE-multi-repo algorithm, so any
/// drift in the hash input, slug rules, or clipping fails loudly here.
#[test]
fn legacy_child_names_are_byte_identical_golden() {
    // (member, no repo) — the legacy `-pvc-` scheme, both spellings agree.
    assert_eq!(
        fanout_child_name("nightly-20260805020000", "db", "db", "pgdata"),
        "nightly-20260805020000-pvc-pgdata-9c07500e"
    );
    assert_eq!(
        fanout_child_name_for("nightly-20260805020000", "db", Some(("db", "pgdata")), None),
        "nightly-20260805020000-pvc-pgdata-9c07500e"
    );
    // Cross-namespace slug form.
    assert_eq!(
        fanout_child_name("nightly-20260805020000", "db", "other", "pgdata"),
        "nightly-20260805020000-pvc-other-pgdata-e2e6bfb6"
    );
    // Clipping path (base + slug over budget) — pre-change clip split pinned.
    assert_eq!(
        fanout_child_name(
            &format!("{}-20260805020000", "a".repeat(40)),
            "db",
            "db",
            &"p".repeat(40)
        ),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-pvc-pppppppppppppppp-76e51736"
    );
    // (no member, no repo) — a non-fanned child IS its base (scheduled
    // `<schedule>-<slot>` / manual `<policy>-manual-<slot>` names unchanged).
    assert_eq!(
        fanout_child_name_for("nightly-20260805020000", "db", None, None),
        "nightly-20260805020000"
    );
    assert_eq!(
        fanout_child_name_for("pg-manual-20260805020000", "db", None, None),
        "pg-manual-20260805020000"
    );
}

#[test]
fn group_name_without_repo_is_byte_identical_golden() {
    assert_eq!(
        group_name("nightly-20260805020000", "db", 0, None),
        "nightly-20260805020000-2a7a7950-grp"
    );
    // Long-base clipping pinned too.
    assert_eq!(
        group_name(&"a".repeat(80), "db", 1, None),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-2a7a7b03-grp"
    );
    // The repo dimension changes the tag (one VolumeGroupSnapshot per
    // (repo, slot)) and distinct repos get distinct groups.
    let none = group_name("base", "ns", 0, None);
    let a = group_name("base", "ns", 0, Some("Repository/ns/a"));
    let b = group_name("base", "ns", 0, Some("Repository/ns/b"));
    assert_ne!(none, a);
    assert_ne!(a, b);
    assert!(a.len() <= 63 && a.ends_with("-grp"));
}

#[test]
fn repo_child_name_forms_carry_markers_and_stay_bounded() {
    use crate::common::RepositoryKind;
    let repo = rref(RepositoryKind::Repository, "offsite");

    // (no member, repo): `<base>-repo-<rslug>-<h8>`.
    let n = fanout_child_name_for("nightly-20260805020000", "db", None, Some(&repo));
    assert!(n.starts_with("nightly-20260805020000-repo-offsite-"), "{n}");
    assert!(n.len() <= 63);

    // (member, repo): combined `-pvc-…-repo-…` form.
    let c = fanout_child_name_for(
        "nightly-20260805020000",
        "db",
        Some(("db", "pgdata")),
        Some(&repo),
    );
    assert!(
        c.starts_with("nightly-20260805020000-pvc-pgdata-repo-offsite-"),
        "{c}"
    );
    assert!(c.len() <= 63);
    // …and differs from the legacy no-repo name (the repo_key is hashed).
    assert_ne!(
        c,
        fanout_child_name_for("nightly-20260805020000", "db", Some(("db", "pgdata")), None)
    );

    // Same repo NAME under a different KIND is a different repo_key → a
    // different hash even though the legible rslug is identical.
    let cluster = rref(RepositoryKind::ClusterRepository, "offsite");
    let n2 = fanout_child_name_for("nightly-20260805020000", "db", None, Some(&cluster));
    assert_ne!(n, n2);
    assert_eq!(
        n.rsplit_once('-').unwrap().0,
        n2.rsplit_once('-').unwrap().0,
        "same visible prefix — only the never-clipped hash separates them"
    );
}

#[test]
fn marker_ambiguity_pslug_containing_repo_marker_stays_injective() {
    use crate::common::RepositoryKind;
    // pslug "x-repo-y" with NO repo vs pslug "x" + rslug "y": both render as
    // `…-pvc-x-repo-y-<h8>` up to the hash. Injectivity must come from the
    // newline-framed hash tuple, never from parsing the markers back.
    let a = fanout_child_name_for("base", "ns", Some(("ns", "x-repo-y")), None);
    let b = fanout_child_name_for(
        "base",
        "ns",
        Some(("ns", "x")),
        Some(&rref(RepositoryKind::Repository, "y")),
    );
    assert_eq!(
        a.rsplit_once('-').unwrap().0,
        b.rsplit_once('-').unwrap().0,
        "fixture precondition: identical visible prefix `base-pvc-x-repo-y`"
    );
    assert_ne!(a, b, "the hash tuple must disambiguate the two shapes");
}

#[test]
fn fanout_child_name_for_length_and_injectivity_property() {
    use crate::common::RepositoryKind;
    use std::collections::BTreeMap;

    // Deterministic pseudo-random tuples (hermetic — no proptest dep): a
    // xorshift over a fixed seed drives lengths/shape choices.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rand = move |bound: usize| -> usize {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state as usize) % bound.max(1)
    };
    let frag = |n: usize, c: u8| -> String { std::iter::repeat_n(c as char, n).collect() };

    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for i in 0..2000 {
        let base = format!("{}-2026080502{:04}", frag(1 + rand(50), b'b'), i);
        let member_owned;
        let member = match rand(3) {
            0 => None,
            _ => {
                member_owned = (
                    format!("ns{}", rand(4)),
                    format!("{}-{i}", frag(1 + rand(60), b'p')),
                );
                Some((member_owned.0.as_str(), member_owned.1.as_str()))
            }
        };
        let repo_owned;
        let repo = match rand(3) {
            0 => None,
            k => {
                repo_owned = rref(
                    if k == 1 {
                        RepositoryKind::Repository
                    } else {
                        RepositoryKind::ClusterRepository
                    },
                    &format!("{}{}", frag(1 + rand(40), b'r'), rand(8)),
                );
                Some(&repo_owned)
            }
        };
        // The (None, None) arm is the legacy "name IS the base" contract:
        // every real caller produces a ≤63 base there (schedule/manual
        // naming), and with no member/repo there is no hash to disambiguate a
        // clipped base. Mirror the contract in the generator.
        let base = if member.is_none() && repo.is_none() {
            base.chars().take(63).collect::<String>()
        } else {
            base
        };
        let key = format!("{base}|{member:?}|{repo:?}");
        let name = fanout_child_name_for(&base, "ns0", member, repo);
        // Length + DNS-1123 shape, always.
        assert!(name.len() <= 63, "len {} for {name} ({key})", name.len());
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{name}"
        );
        assert!(!name.starts_with('-') && !name.ends_with('-'), "{name}");
        // Injectivity: two distinct tuples never share a name.
        if let Some(prev) = seen.insert(name.clone(), key.clone()) {
            assert_eq!(prev, key, "collision on {name}");
        }
    }
}

#[test]
fn cache_pvc_name_single_repo_is_the_legacy_name_byte_identical() {
    // The `repo: None` arm must reproduce `kopiur-cache-<policy>` EXACTLY —
    // an existing warm-cache PVC is found by name, so any drift silently
    // abandons every fleet's cache.
    assert_eq!(
        cache_pvc_name("nightly", "db", None),
        "kopiur-cache-nightly"
    );
    // No length cap on the legacy arm (historical behavior preserved).
    let long = "p".repeat(80);
    assert_eq!(
        cache_pvc_name(&long, "db", None),
        format!("kopiur-cache-{long}")
    );
}

#[test]
fn cache_pvc_name_multi_repo_golden_and_bounded() {
    use crate::common::RepositoryKind;
    let repo = rref(RepositoryKind::Repository, "offsite");
    let n = cache_pvc_name("nightly", "db", Some(&repo));
    // Golden: `kopiur-cache-<policy>-<rslug>-<h6>` where h6 is the first 6 hex
    // of FNV-1a over the normalized repo_key `Repository/db/offsite`.
    assert_eq!(n, "kopiur-cache-nightly-offsite-87ceb3");
    assert!(n.len() <= 63);

    // Same name, different KIND → different repo_key → different tag.
    let cluster = rref(RepositoryKind::ClusterRepository, "offsite");
    let c = cache_pvc_name("nightly", "db", Some(&cluster));
    assert_ne!(n, c);
    assert!(c.starts_with("kopiur-cache-nightly-offsite-"), "{c}");

    // Same ref keyed from a different policy namespace → different effective
    // namespace → different tag (two namespaces' caches must not collide).
    let other_ns = cache_pvc_name("nightly", "media", Some(&repo));
    assert_ne!(n, other_ns);
}

#[test]
fn cache_pvc_name_multi_repo_clips_slugs_never_the_tag() {
    use crate::common::RepositoryKind;
    let repo = rref(
        RepositoryKind::Repository,
        "a-very-long-repository-name-that-needs-clipping-somewhere",
    );
    let n = cache_pvc_name(
        &"policy-with-an-extremely-long-name-".repeat(3),
        "ns",
        Some(&repo),
    );
    assert!(n.len() <= 63, "len {} for {n}", n.len());
    assert!(n.starts_with("kopiur-cache-"), "{n}");
    // RFC 1123 label shape.
    assert!(
        n.chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
        "{n}"
    );
    assert!(!n.ends_with('-') && !n.contains("--"), "{n}");
    // The 6-hex tag survives at the end.
    let tag = n.rsplit('-').next().unwrap();
    assert_eq!(tag.len(), 6, "{n}");
    assert!(tag.chars().all(|ch| ch.is_ascii_hexdigit()), "{n}");
}

// --- mint_cells: the members × repositories cross product (#368) ------------

/// A multi-repo policy fixture in `billing`, spec constructed directly (the
/// same shape admission now accepts — the M7 feature gate is lifted).
fn multi_repo_policy() -> SnapshotPolicy {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "app", "namespace": "billing" },
        "spec": {
            "repositories": [
                { "kind": "Repository", "name": "nas" },
                { "kind": "ClusterRepository", "name": "offsite" },
            ],
            "sources": [ { "pvc": { "name": "data" } } ],
        }
    }))
    .expect("multi-repo policy fixture")
}

#[test]
fn mint_cells_single_repo_is_byte_identical_legacy() {
    let policy = policy_with(vec![Source {
        pvc: Some(PvcSource {
            name: "data".into(),
        }),
        ..Default::default()
    }]);
    // No selector: exactly the one bare, pin-free cell.
    assert_eq!(
        mint_cells(&policy, "nightly-20260805020000", None),
        vec![MintCell {
            name: "nightly-20260805020000".into(),
            source: None,
            repository: None,
        }]
    );
    // Selector members: names + sources pass through verbatim, no pins.
    let member = ExpandedMember {
        name: fanout_child_name("base", "billing", "billing", "pgdata"),
        source: SnapshotSourceRef {
            source_index: 0,
            target: SnapshotSourceTarget::Pvc(PvcTargetRef {
                namespace: "billing".into(),
                name: "pgdata".into(),
            }),
            group: None,
        },
    };
    let cells = mint_cells(&policy, "base", Some(vec![member.clone()]));
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].name, member.name);
    assert_eq!(cells[0].source.as_ref(), Some(&member.source));
    assert_eq!(cells[0].repository, None);
    // A selector that matched nothing yields no cells (the caller warns).
    assert!(mint_cells(&policy, "base", Some(Vec::new())).is_empty());
}

#[test]
fn mint_cells_multi_repo_crosses_and_pins_normalized() {
    let policy = multi_repo_policy();
    // No selector: one cell per repository, `-repo-` names, normalized pins.
    let cells = mint_cells(&policy, "nightly-20260805020000", None);
    assert_eq!(cells.len(), 2);
    assert!(
        cells[0]
            .name
            .starts_with("nightly-20260805020000-repo-nas-"),
        "{}",
        cells[0].name
    );
    assert!(
        cells[1]
            .name
            .starts_with("nightly-20260805020000-repo-offsite-"),
        "{}",
        cells[1].name
    );
    // Normalization: the namespaced Repository pin carries its EFFECTIVE
    // namespace explicitly; the ClusterRepository pin carries none.
    let nas = cells[0].repository.as_ref().unwrap();
    assert_eq!(nas.namespace.as_deref(), Some("billing"));
    let offsite = cells[1].repository.as_ref().unwrap();
    assert_eq!(offsite.namespace, None);
    assert!(cells.iter().all(|c| c.source.is_none()));

    // Members × repos: 2 members × 2 repos = 4 distinct names, each pinned.
    let member = |pvc: &str| ExpandedMember {
        name: fanout_child_name("base", "billing", "billing", pvc),
        source: SnapshotSourceRef {
            source_index: 0,
            target: SnapshotSourceTarget::Pvc(PvcTargetRef {
                namespace: "billing".into(),
                name: pvc.into(),
            }),
            group: None,
        },
    };
    let cells = mint_cells(&policy, "base", Some(vec![member("a"), member("b")]));
    assert_eq!(cells.len(), 4);
    let mut names: Vec<&str> = cells.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 4, "all four (member, repo) names are distinct");
    assert!(cells.iter().all(|c| c.repository.is_some()));
    assert!(
        cells
            .iter()
            .all(|c| c.name.contains("-pvc-") && c.name.contains("-repo-")),
        "combined form carries both markers"
    );
}

#[test]
fn mint_cells_multi_repo_rederives_group_names_per_repository() {
    let policy = multi_repo_policy();
    let member = ExpandedMember {
        name: fanout_child_name("base", "billing", "billing", "pgdata"),
        source: SnapshotSourceRef {
            source_index: 0,
            target: SnapshotSourceTarget::Pvc(PvcTargetRef {
                namespace: "billing".into(),
                name: "pgdata".into(),
            }),
            group: Some(SnapshotSourceGroup {
                namespace: "billing".into(),
                // The legacy repo-less group name, as expand_sources builds it.
                volume_group_snapshot_name: group_name("base", "billing", 0, None),
            }),
        },
    };
    let cells = mint_cells(&policy, "base", Some(vec![member]));
    assert_eq!(cells.len(), 2);
    let g0 = cells[0].source.as_ref().unwrap().group.as_ref().unwrap();
    let g1 = cells[1].source.as_ref().unwrap().group.as_ref().unwrap();
    // Each repo's members are an independent capture wave: N repos = N groups,
    // and neither reuses the repo-less legacy name.
    assert_ne!(g0.volume_group_snapshot_name, g1.volume_group_snapshot_name);
    let legacy = group_name("base", "billing", 0, None);
    assert_ne!(g0.volume_group_snapshot_name, legacy);
    assert_ne!(g1.volume_group_snapshot_name, legacy);
}
