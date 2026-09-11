//! Tests for the generated `kopiur-ui` RBAC.
//!
//! These are the specification, not a description. `kopiur-ui` turns an HTTP
//! request into an *impersonated* apiserver call, so its own ServiceAccount is
//! the one identity in the system whose over-grant is silently catastrophic: a
//! ServiceAccount that may impersonate AND read Secrets is a ServiceAccount that
//! may read every repository credential as anyone. Every assertion below that
//! starts with `!` is therefore load-bearing — it asserts an ABSENCE, which is
//! the half of an RBAC review a human reader reliably skips.

use std::collections::BTreeSet;

use k8s_openapi::api::rbac::v1::{ClusterRole, PolicyRule, Role};
use xtask::artifact::{Artifact, RBAC_HEADER};
use xtask::rbac::{
    UI_AGGREGATE_LABEL, UiAnonymous, ui_browse_rules, ui_doctor_role_rules, ui_editor_rules,
    ui_rules, ui_viewer_rules,
};

/// The nine Kopiur CRD plurals, as the UI cache and the viewer role see them.
const ALL_CRDS: &[&str] = &[
    "repositories",
    "snapshotpolicies",
    "snapshots",
    "snapshotschedules",
    "restores",
    "maintenances",
    "repositoryreplications",
    "snapshotreplications",
    "clusterrepositories",
];

/// The seven kinds a UI action PATCHes (suspend/resume, maintenance run,
/// replication run, catalog scan). Mirrors the probe table in
/// `crates/ui/src/api/me.rs` — if one side grows a kind, the other must too.
const PATCHED_CRDS: &[&str] = &[
    "snapshotpolicies",
    "snapshotschedules",
    "repositories",
    "clusterrepositories",
    "maintenances",
    "repositoryreplications",
    "snapshotreplications",
];

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn artifact<'a>(artifacts: &'a [Artifact], rel: &str) -> &'a Artifact {
    artifacts
        .iter()
        .find(|a| a.rel_path == rel)
        .unwrap_or_else(|| panic!("missing generated artifact {rel}"))
}

fn docs(content: &str) -> Vec<String> {
    let body = content.strip_prefix(RBAC_HEADER).unwrap_or(content);
    body.split("\n---\n").map(str::to_string).collect()
}

/// Every YAML document in a generated file, as an untyped value.
fn values(content: &str) -> Vec<serde_yaml::Value> {
    docs(content)
        .into_iter()
        .filter_map(|d| serde_yaml::from_str::<serde_yaml::Value>(&d).ok())
        .collect()
}

fn doc_named(content: &str, kind: &str, name: &str) -> serde_yaml::Value {
    values(content)
        .into_iter()
        .find(|v| {
            v.get("kind").and_then(|k| k.as_str()) == Some(kind)
                && v.get("metadata")
                    .and_then(|m| m.get("name"))
                    .and_then(|n| n.as_str())
                    == Some(name)
        })
        .unwrap_or_else(|| panic!("no {kind} named {name} in the generated ui RBAC"))
}

fn clusterrole_named(content: &str, name: &str) -> ClusterRole {
    serde_yaml::from_value(doc_named(content, "ClusterRole", name))
        .unwrap_or_else(|e| panic!("ClusterRole {name} must parse: {e}"))
}

fn role_named(content: &str, name: &str) -> Role {
    serde_yaml::from_value(doc_named(content, "Role", name))
        .unwrap_or_else(|e| panic!("Role {name} must parse: {e}"))
}

/// Whether some rule grants `verb` on `(group, resource)`.
fn grants(rules: &[PolicyRule], group: &str, resource: &str, verb: &str) -> bool {
    rules.iter().any(|r| {
        r.api_groups
            .as_ref()
            .is_some_and(|g| g.iter().any(|x| x == group))
            && r.resources
                .as_ref()
                .is_some_and(|res| res.iter().any(|x| x == resource))
            && r.verbs.iter().any(|v| v == verb)
    })
}

/// Whether ANY rule mentions `(group, resource)` at all, whatever the verb.
/// Used for the absence assertions, where "mentioned at all" is already a bug.
fn mentions(rules: &[PolicyRule], group: &str, resource: &str) -> bool {
    rules.iter().any(|r| {
        r.api_groups
            .as_ref()
            .is_some_and(|g| g.iter().any(|x| x == group))
            && r.resources
                .as_ref()
                .is_some_and(|res| res.iter().any(|x| x == resource))
    })
}

/// Every resource string any rule names, for the `/*` sweep.
fn all_resources(rules: &[PolicyRule]) -> Vec<String> {
    rules
        .iter()
        .filter_map(|r| r.resources.clone())
        .flatten()
        .collect()
}

/// The `resourceNames` of the (single) rule naming `resource`.
fn names_for(rules: &[PolicyRule], resource: &str) -> Option<Vec<String>> {
    rules
        .iter()
        .find(|r| {
            r.resources
                .as_ref()
                .is_some_and(|res| res.iter().any(|x| x == resource))
        })
        .and_then(|r| r.resource_names.clone())
}

/// A rule reduced to its comparable parts: (apiGroups, resources, verbs,
/// resourceNames). Comparing whole SETS of these is what lets a test fail on a
/// WIDENED grant — a `contains`-style assertion never can.
type Shape = (Vec<String>, Vec<String>, Vec<String>, Vec<String>);

fn shape(r: &PolicyRule) -> Shape {
    (
        r.api_groups.clone().unwrap_or_default(),
        r.resources.clone().unwrap_or_default(),
        r.verbs.clone(),
        r.resource_names.clone().unwrap_or_default(),
    )
}

fn shapes(rules: &[PolicyRule]) -> BTreeSet<Shape> {
    rules.iter().map(shape).collect()
}

// --- the ServiceAccount's own ClusterRole -----------------------------------

/// The whole point of the component: impersonate, and nothing that would make
/// impersonation a shortcut to the data itself.
#[test]
fn ui_service_account_may_impersonate_and_never_reads_secrets_or_execs() {
    let rules = ui_rules(&strs(&["scopes", "oid"]), &[], None, true);

    assert!(
        grants(&rules, "", "users", "impersonate"),
        "the UI must be able to impersonate users"
    );
    assert!(
        grants(&rules, "", "groups", "impersonate"),
        "the UI must be able to impersonate groups"
    );
    for key in ["scopes", "oid"] {
        assert!(
            grants(
                &rules,
                "authentication.k8s.io",
                &format!("userextras/{key}"),
                "impersonate"
            ),
            "each configured extra key must get its OWN userextras/<key> rule ({key})"
        );
    }

    // The two absences that matter. A UI ServiceAccount holding either would be
    // able to read every repository credential in the cluster directly — without
    // impersonating anyone, so without a single RBAC decision recording it.
    assert!(
        !mentions(&rules, "", "secrets"),
        "the UI ServiceAccount must NEVER be granted secrets"
    );
    assert!(
        !mentions(&rules, "", "pods/exec"),
        "the UI ServiceAccount must NEVER be granted pods/exec (browse execs as the USER)"
    );
    assert!(
        !mentions(&rules, "", "pods"),
        "the UI ServiceAccount has no business reading pods under its own identity"
    );
}

/// Kubernetes RBAC has no wildcard for a user-extra key: `userextras/*` parses,
/// binds, and authorizes NOTHING, so the failure is a confusing runtime 403
/// rather than an install-time error. The generated form must name each key.
#[test]
fn no_generated_ui_resource_is_a_subresource_wildcard() {
    let mut everything = ui_rules(&strs(&["scopes"]), &[], None, true);
    everything.extend(ui_viewer_rules());
    everything.extend(ui_editor_rules());
    everything.extend(ui_browse_rules());
    everything.extend(ui_doctor_role_rules());

    for resource in all_resources(&everything) {
        assert!(
            !resource.ends_with("/*") && resource != "*",
            "resource {resource:?} is a wildcard: RBAC does not expand it, so the rule \
             grants nothing and fails at runtime instead of at install"
        );
    }
}

/// Anonymous mode has one fixed identity by definition, so the apiserver — not
/// only the UI's header parser — should be the thing that says so. Even if the
/// parser were fooled into asserting a different user, an impersonate rule
/// scoped by `resourceNames` denies it.
#[test]
fn anonymous_only_mode_pins_impersonation_to_that_one_identity() {
    let anon = UiAnonymous {
        user: "kopiur-anonymous".into(),
        groups: strs(&["kopiur-readers"]),
    };
    let rules = ui_rules(&[], &[], Some(&anon), true);

    assert_eq!(
        names_for(&rules, "users").as_deref(),
        Some(["kopiur-anonymous".to_string()].as_slice()),
        "anonymous mode must pin the users impersonate rule to the one anonymous user"
    );
    let groups = names_for(&rules, "groups").expect("groups rule must be name-scoped");
    assert!(
        groups.contains(&"kopiur-readers".to_string()),
        "the anonymous identity's own groups must be impersonatable: {groups:?}"
    );
    assert!(
        groups.contains(&"system:authenticated".to_string()),
        "system:authenticated rides along on every impersonated identity, so it must be \
         named or every anonymous request 403s: {groups:?}"
    );
    assert!(
        !groups.iter().any(|g| g == "system:masters"),
        "system:masters must never be nameable: {groups:?}"
    );
}

/// Header mode cannot be name-scoped (the proxy asserts whoever authenticated),
/// so the rules are deliberately unscoped there — the guard is the proxy secret
/// and the identity deny-list, not `resourceNames`.
#[test]
fn header_mode_leaves_impersonation_unscoped() {
    let rules = ui_rules(&[], &[], None, true);
    assert_eq!(names_for(&rules, "users"), None);
    assert_eq!(names_for(&rules, "groups"), None);
}

/// `KOPIUR_UI_ALLOWED_GROUPS` is enforced twice: once by the UI's parser and
/// once by the apiserver, so a bug in the former is still caught by the latter.
#[test]
fn allowed_groups_render_as_resource_names_on_the_groups_rule() {
    let rules = ui_rules(&[], &strs(&["platform", "sre"]), None, true);
    let groups = names_for(&rules, "groups").expect("allowedGroups must scope the groups rule");
    assert!(groups.contains(&"platform".to_string()));
    assert!(groups.contains(&"sre".to_string()));
    assert!(
        groups.contains(&"system:authenticated".to_string()),
        "system:authenticated must stay nameable or every request 403s: {groups:?}"
    );
    // The USERS rule is untouched: an allow-list of groups says nothing about
    // which humans may log in.
    assert_eq!(names_for(&rules, "users"), None);
}

/// The cache is the only reason the UI ever reads a Kopiur object under its OWN
/// identity. With it off, every read is impersonated, and the grant must vanish
/// rather than linger as an unused-but-present permission.
#[test]
fn cache_rules_appear_only_when_the_cache_is_enabled() {
    let on = ui_rules(&[], &[], None, true);
    for crd in ALL_CRDS {
        for verb in ["get", "list", "watch"] {
            assert!(
                grants(&on, "kopiur.home-operations.com", crd, verb),
                "cache mode must grant {verb} on {crd}"
            );
        }
    }
    assert!(
        grants(
            &on,
            "authorization.k8s.io",
            "subjectaccessreviews",
            "create"
        ),
        "cached reads are SAR-gated per identity, so the UI must be able to create SARs"
    );
    // Reads only — the cache never writes, and the UI's own identity must never
    // be able to.
    for verb in ["create", "update", "patch", "delete"] {
        assert!(
            !grants(&on, "kopiur.home-operations.com", "snapshots", verb),
            "the cache is read-only: {verb} on snapshots must not be granted"
        );
    }

    let off = ui_rules(&[], &[], None, false);
    for crd in ALL_CRDS {
        assert!(
            !mentions(&off, "kopiur.home-operations.com", crd),
            "with the cache off, {crd} must not be readable by the UI ServiceAccount"
        );
    }
    assert!(
        !mentions(&off, "authorization.k8s.io", "subjectaccessreviews"),
        "with the cache off there is nothing to SAR-gate"
    );
    // ...but impersonation, the component's whole reason to exist, stays.
    assert!(grants(&off, "", "users", "impersonate"));
    assert!(grants(&off, "", "groups", "impersonate"));
}

// --- the human roles --------------------------------------------------------

/// The viewer's exact rule set. Asserted whole: a subset assertion cannot fail
/// when someone widens a grant, which is the only direction that matters here.
#[test]
fn viewer_role_is_exactly_read_only() {
    let expected = vec![
        (
            strs(&["kopiur.home-operations.com"]),
            strs(ALL_CRDS),
            strs(&["get", "list", "watch"]),
            vec![],
        ),
        (
            strs(&["apiextensions.k8s.io"]),
            strs(&["customresourcedefinitions"]),
            strs(&["get", "list"]),
            vec![],
        ),
        (
            strs(&["events.k8s.io"]),
            strs(&["events"]),
            strs(&["list"]),
            vec![],
        ),
    ];
    assert_eq!(
        shapes(&ui_viewer_rules()),
        expected.into_iter().collect::<BTreeSet<_>>(),
        "the viewer role's rule set must be exactly this"
    );
}

/// The editor's exact verb table. Every entry here is a button in the UI; an
/// entry that is NOT here is a button the UI does not have, and granting it
/// would hand a human more than the console can even ask for.
#[test]
fn editor_role_verb_table_is_exact() {
    let expected = vec![
        (
            strs(&["kopiur.home-operations.com"]),
            strs(&["snapshots"]),
            strs(&["create", "delete"]),
            vec![],
        ),
        (
            strs(&["kopiur.home-operations.com"]),
            strs(&["restores"]),
            strs(&["create"]),
            vec![],
        ),
        (
            strs(&["kopiur.home-operations.com"]),
            strs(PATCHED_CRDS),
            strs(&["patch"]),
            vec![],
        ),
        (
            strs(&[""]),
            strs(&["persistentvolumeclaims"]),
            strs(&["list"]),
            vec![],
        ),
    ];
    assert_eq!(
        shapes(&ui_editor_rules()),
        expected.into_iter().collect::<BTreeSet<_>>(),
        "the editor role's rule set must be exactly this"
    );

    // The absences an exact-set assertion already implies, spelled out because
    // they are the ones a reviewer would look for.
    let rules = ui_editor_rules();
    assert!(!mentions(&rules, "", "secrets"));
    assert!(!mentions(&rules, "", "pods/exec"));
    assert!(!mentions(&rules, "apps", "deployments"));
}

/// Browse is the role that grants `pods/exec` in a namespace, and a session pod
/// loads its repository credentials from its own environment — so anyone who can
/// exec into that namespace can print them. It is therefore shipped UNaggregated
/// and bound deliberately, never folded into the everyday user role.
#[test]
fn browse_role_carries_exec_and_drops_the_deployment_lookup() {
    let rules = ui_browse_rules();
    assert!(
        grants(&rules, "", "pods/exec", "create"),
        "browse is exec: without it the data plane cannot read a file"
    );
    assert!(
        !mentions(&rules, "apps", "deployments"),
        "the UI passes the mover image through KOPIUR_MOVER_IMAGE, so a browsing user must \
         not need (or get) a cluster-wide Deployment read"
    );
    assert!(
        !mentions(&rules, "", "secrets"),
        "the session pod loads the credentials; the browsing user never reads them directly"
    );
    // The session lifecycle it does need.
    for verb in ["create", "get", "list", "delete"] {
        assert!(
            grants(&rules, "batch", "jobs", verb),
            "browse needs {verb} on jobs"
        );
    }
    assert!(grants(&rules, "", "pods/log", "get"));
}

/// Doctor's `ControllerRunning` check reads the controller Deployment. That is
/// the ONLY reason any UI role touches `apps`, and it is confined to a
/// namespaced Role in the operator's namespace rather than a cluster-wide read.
#[test]
fn doctor_role_is_a_namespaced_deployment_read_and_nothing_else() {
    let expected = vec![(
        strs(&["apps"]),
        strs(&["deployments"]),
        strs(&["get", "list"]),
        vec![],
    )];
    assert_eq!(
        shapes(&ui_doctor_role_rules()),
        expected.into_iter().collect::<BTreeSet<_>>(),
        "the doctor Role must grant exactly apps/deployments get,list"
    );
}

// --- the generated artifact -------------------------------------------------

#[test]
fn ui_artifact_ships_every_role_with_the_right_aggregation() {
    let artifacts = xtask::rbac::artifacts().expect("generate RBAC artifacts");
    let a = artifact(&artifacts, "rbac/ui.yaml");
    assert!(
        a.content.starts_with(RBAC_HEADER),
        "missing generated header"
    );

    // Viewer and editor aggregate into the everyday user role...
    for name in ["kopiur-ui-viewer", "kopiur-ui-editor"] {
        let cr = clusterrole_named(&a.content, name);
        let labels = cr.metadata.labels.unwrap_or_default();
        assert_eq!(
            labels.get(UI_AGGREGATE_LABEL).map(String::as_str),
            Some("true"),
            "{name} must carry the aggregation label {UI_AGGREGATE_LABEL}"
        );
    }
    // ...and browse deliberately does NOT, because aggregating it would hand
    // every `kopiur-ui-user` subject `pods/exec` — and with it the repository
    // credentials of every namespace they can reach.
    let browse = clusterrole_named(&a.content, "kopiur-ui-browse");
    let browse_labels = browse.metadata.labels.unwrap_or_default();
    assert!(
        !browse_labels.contains_key(UI_AGGREGATE_LABEL),
        "kopiur-ui-browse must NEVER aggregate: granting browse grants that namespace's \
         repository credentials, so it has to be bound on purpose"
    );

    // The umbrella role exists, selects the label, and carries no rules of its
    // own (the apiserver fills them in).
    let user = clusterrole_named(&a.content, "kopiur-ui-user");
    let selectors = user
        .aggregation_rule
        .and_then(|r| r.cluster_role_selectors)
        .unwrap_or_default();
    assert!(
        selectors.iter().any(|s| s
            .match_labels
            .as_ref()
            .is_some_and(|m| m.get(UI_AGGREGATE_LABEL).map(String::as_str) == Some("true"))),
        "kopiur-ui-user must aggregate on {UI_AGGREGATE_LABEL}"
    );

    // The doctor Role is namespaced; everything else in the file is not.
    let doctor = role_named(&a.content, "kopiur-ui-doctor");
    assert_eq!(doctor.metadata.namespace.as_deref(), Some("kopiur-system"));
}

/// The shipped artifact is the "everything on" flavour (cache enabled), exactly
/// like the operator's own — the chart is what narrows it per install. It must
/// still never carry Secrets or exec.
#[test]
fn ui_artifact_service_account_role_stays_least_privilege() {
    let artifacts = xtask::rbac::artifacts().expect("generate RBAC artifacts");
    let a = artifact(&artifacts, "rbac/ui.yaml");
    let rules = clusterrole_named(&a.content, "kopiur-ui")
        .rules
        .expect("the UI ClusterRole must have rules");

    assert!(grants(&rules, "", "users", "impersonate"));
    assert!(grants(&rules, "", "groups", "impersonate"));
    assert!(!mentions(&rules, "", "secrets"));
    assert!(!mentions(&rules, "", "pods/exec"));
    for resource in all_resources(&rules) {
        assert!(
            !resource.ends_with("/*") && resource != "*",
            "{resource} is a wildcard"
        );
    }
}
