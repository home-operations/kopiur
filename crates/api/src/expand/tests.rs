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
    // THE silent-data-loss case: both resolve to `/pvc/data`, so two volumes'
    // histories merge into one kopia source and prune each other.
    // `detect_identity_collision` cannot see it — it compares ACROSS policies
    // and skips self.
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
