//! Tests for generated RBAC YAML.

use k8s_openapi::api::rbac::v1::{ClusterRole, PolicyRule, Role};
use xtask::artifact::{Artifact, RBAC_HEADER};

fn artifact<'a>(artifacts: &'a [Artifact], rel: &str) -> &'a Artifact {
    artifacts
        .iter()
        .find(|a| a.rel_path == rel)
        .unwrap_or_else(|| panic!("missing generated artifact {rel}"))
}

/// Split a multi-doc RBAC file into its individual YAML documents (header off).
fn docs(content: &str) -> Vec<String> {
    let body = content.strip_prefix(RBAC_HEADER).unwrap_or(content);
    body.split("\n---\n").map(str::to_string).collect()
}

fn rule_grants(rules: &[PolicyRule], group: &str, resource: &str) -> bool {
    rules.iter().any(|r| {
        r.api_groups
            .as_ref()
            .is_some_and(|g| g.iter().any(|x| x == group))
            && r.resources
                .as_ref()
                .is_some_and(|res| res.iter().any(|x| x == resource))
    })
}

/// Whether some rule grants `verb` on `(group, resource)`. Stricter than
/// [`rule_grants`]: a rule that lists the resource but lacks the verb does NOT
/// satisfy this (the bug that shipped `serviceaccounts: [create, get]` while the
/// controller mints via server-side apply, which needs `patch`).
fn rule_grants_verb(rules: &[PolicyRule], group: &str, resource: &str, verb: &str) -> bool {
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

#[test]
fn clusterrole_parses_and_grants_expected_rules() {
    let artifacts = xtask::rbac::artifacts().expect("generate RBAC artifacts");
    let a = artifact(&artifacts, "rbac/operator-clusterrole.yaml");
    assert!(a.content.starts_with(RBAC_HEADER));

    // The ClusterRole is one of the documents in the file.
    let clusterrole = docs(&a.content)
        .into_iter()
        .find_map(|d| {
            let v: serde_yaml::Value = serde_yaml::from_str(&d).ok()?;
            if v.get("kind").and_then(|k| k.as_str()) == Some("ClusterRole") {
                serde_yaml::from_str::<ClusterRole>(&d).ok()
            } else {
                None
            }
        })
        .expect("ClusterRole document must parse");

    let rules = clusterrole.rules.expect("ClusterRole must have rules");

    assert!(
        rule_grants(&rules, "kopiur.home-operations.com", "snapshots"),
        "must grant backups under kopiur.home-operations.com"
    );
    assert!(
        rule_grants(&rules, "kopiur.home-operations.com", "clusterrepositories"),
        "cluster role must include cluster-scoped clusterrepositories"
    );
    assert!(
        rule_grants(&rules, "batch", "jobs"),
        "must grant jobs under batch"
    );
    assert!(
        rule_grants(&rules, "", "serviceaccounts"),
        "cluster role must allow minting per-namespace serviceaccounts"
    );
    // kube's Recorder writes events.k8s.io/v1 Events — without this group the
    // create is 403'd and reconcile-outcome Events (e.g. MaintenanceNotConfigured,
    // SnapshotOrphaned) are silently dropped.
    assert!(
        rule_grants(&rules, "events.k8s.io", "events"),
        "must grant events under events.k8s.io (kube Recorder target)"
    );
    assert!(
        rule_grants(&rules, "", "events"),
        "must also grant legacy core events"
    );
}

#[test]
fn namespaced_role_omits_cluster_crds_but_keeps_mover_minting() {
    let artifacts = xtask::rbac::artifacts().expect("generate RBAC artifacts");
    let a = artifact(&artifacts, "rbac/operator-role.yaml");

    let role = docs(&a.content)
        .into_iter()
        .find_map(|d| {
            let v: serde_yaml::Value = serde_yaml::from_str(&d).ok()?;
            if v.get("kind").and_then(|k| k.as_str()) == Some("Role") {
                serde_yaml::from_str::<Role>(&d).ok()
            } else {
                None
            }
        })
        .expect("Role document must parse");

    let rules = role.rules.expect("Role must have rules");

    // Same core grants...
    assert!(rule_grants(
        &rules,
        "kopiur.home-operations.com",
        "snapshots"
    ));
    assert!(rule_grants(&rules, "batch", "jobs"));
    // Events surfacing works in namespaced mode too (Recorder → events.k8s.io/v1).
    assert!(
        rule_grants(&rules, "events.k8s.io", "events"),
        "namespaced role must grant events under events.k8s.io"
    );
    // ...the cluster-scoped CRD is dropped in namespaced mode.
    assert!(
        !rule_grants(&rules, "kopiur.home-operations.com", "clusterrepositories"),
        "namespaced role must NOT include cluster-scoped clusterrepositories"
    );
    // ...but the mover SA + RoleBinding minting rules ARE retained: even in
    // namespaced mode the controller mints the least-privilege mover RBAC in the
    // (in-scope) workload namespace before each mover Job (ADR §4.12).
    assert!(
        rule_grants(&rules, "", "serviceaccounts"),
        "namespaced role must mint the mover ServiceAccount in its own namespace"
    );
    assert!(
        rule_grants(&rules, "rbac.authorization.k8s.io", "rolebindings"),
        "namespaced role must mint the mover RoleBinding in its own namespace"
    );
}

/// Regression: the controller mints the per-namespace mover SA + RoleBinding via
/// server-side apply (`io::ensure_mover_rbac` → `PatchParams::apply`), which the
/// apiserver authorizes as a `patch` (plus `create` when the object is new). The
/// operator role shipped `serviceaccounts: [create, get]` — missing `patch` — so
/// every mint 403'd, the reconcile errored before launching the mover Job, and a
/// failing Repository never published its Warning Event (and real-cluster mover
/// Jobs FailedCreate'd). Both install modes must grant `patch` on the minted kinds.
#[test]
fn operator_role_can_server_side_apply_the_minted_mover_rbac() {
    let artifacts = xtask::rbac::artifacts().expect("generate RBAC artifacts");
    for (rel, kind) in [
        ("rbac/operator-clusterrole.yaml", "ClusterRole"),
        ("rbac/operator-role.yaml", "Role"),
    ] {
        let a = artifact(&artifacts, rel);
        let rules = docs(&a.content)
            .into_iter()
            .find_map(|d| {
                let v: serde_yaml::Value = serde_yaml::from_str(&d).ok()?;
                (v.get("kind").and_then(|k| k.as_str()) == Some(kind)).then_some(())?;
                if kind == "ClusterRole" {
                    serde_yaml::from_str::<ClusterRole>(&d).ok()?.rules
                } else {
                    serde_yaml::from_str::<Role>(&d).ok()?.rules
                }
            })
            .unwrap_or_else(|| panic!("{rel}: {kind} rules must parse"));

        // Server-side apply needs `patch` (and `create` for first-write) on BOTH
        // minted kinds — assert the verb, not just the resource's presence.
        for (group, resource) in [
            ("", "serviceaccounts"),
            ("rbac.authorization.k8s.io", "rolebindings"),
        ] {
            assert!(
                rule_grants_verb(&rules, group, resource, "patch"),
                "{rel}: must grant `patch` on {resource} (controller mints via server-side apply)"
            );
            assert!(
                rule_grants_verb(&rules, group, resource, "create"),
                "{rel}: must grant `create` on {resource} (first-time mint)"
            );
        }
    }
}

/// The dedicated, least-privilege mover role is generated for both install modes
/// and grants ONLY what the mover uses (status patch on the owning CRDs + the
/// bootstrap-result configmap patch) — never the operator's broad rule set.
/// The rules of the named (Cluster)Role document within a multi-doc RBAC file
/// (the mover files now carry BOTH the generic and the snapshot-replication
/// mover roles, so "first role in the file" is no longer a selector).
fn named_role_rules(content: &str, name: &str) -> Vec<PolicyRule> {
    docs(content)
        .into_iter()
        .find_map(|d| {
            let v: serde_yaml::Value = serde_yaml::from_str(&d).ok()?;
            let doc_name = v
                .get("metadata")
                .and_then(|m| m.get("name"))
                .and_then(|n| n.as_str());
            (doc_name == Some(name)).then_some(())?;
            match v.get("kind").and_then(|k| k.as_str()) {
                Some("ClusterRole") => serde_yaml::from_str::<ClusterRole>(&d).ok()?.rules,
                Some("Role") => serde_yaml::from_str::<Role>(&d).ok()?.rules,
                _ => None,
            }
        })
        .unwrap_or_else(|| panic!("no (Cluster)Role named {name} with rules"))
}

#[test]
fn mover_role_is_least_privilege() {
    let artifacts = xtask::rbac::artifacts().expect("generate RBAC artifacts");
    for rel in ["rbac/mover-clusterrole.yaml", "rbac/mover-role.yaml"] {
        let a = artifact(&artifacts, rel);
        assert!(a.content.starts_with(RBAC_HEADER), "{rel} missing header");
        let role_rules = named_role_rules(&a.content, "kopiur-mover");

        // Grants the mover's actual API surface.
        assert!(
            rule_grants(
                &role_rules,
                "kopiur.home-operations.com",
                "snapshots/status"
            ),
            "{rel} must grant snapshots/status"
        );
        assert!(
            rule_grants(&role_rules, "", "configmaps"),
            "{rel} must grant configmaps (bootstrap result write)"
        );
        // #401: `get` on `{crd}/status` is LOAD-BEARING, not an unused verb —
        // it authorizes the mover's `read_resolved` GET on the status
        // subresource (restore-retry determinism). A "trim unused verbs" pass
        // dropping it would silently kill that read again.
        for verb in ["get", "patch"] {
            assert!(
                rule_grants_verb(
                    &role_rules,
                    "kopiur.home-operations.com",
                    "restores/status",
                    verb
                ),
                "{rel} must grant `{verb}` on restores/status (#401: get backs read_resolved)"
            );
        }
        // ...and the base resource stays UNgranted: the mover reads through the
        // subresource precisely so it never needs (and must never get) this.
        assert!(
            !rule_grants(&role_rules, "kopiur.home-operations.com", "restores"),
            "{rel}: the mover must NOT be granted the base restores resource (#401)"
        );
        // Least privilege: NONE of the operator's broad grants leak into the mover.
        for (g, r) in [
            ("batch", "jobs"),
            ("", "secrets"),
            ("", "pods"),
            ("kopiur.home-operations.com", "snapshotschedules"),
        ] {
            assert!(
                !rule_grants(&role_rules, g, r),
                "{rel} must NOT grant {g}/{r} (mover is least-privilege)"
            );
        }
        // The rationale for the SEPARATE snapshot-replication role: the generic
        // mover SA must never hold namespace-wide Snapshot create/delete — a
        // compromised generic mover pod could otherwise erase every backup
        // record in its namespace.
        assert!(
            !rule_grants(&role_rules, "kopiur.home-operations.com", "snapshots"),
            "{rel}: the GENERIC mover role must never grant the snapshots primary resource"
        );
    }
}

/// The dedicated snapshot-replication mover role (issue #368): ships in both
/// install modes, grants the copy-CR lifecycle (get/list/create/patch/delete
/// on `snapshots` — SSA needs `patch`+`create`, m:217cd0be) plus the status
/// patches and configmap parity — and NOTHING of the operator's broad set.
#[test]
fn snapshot_replication_mover_role_grants_the_copy_cr_lifecycle() {
    let artifacts = xtask::rbac::artifacts().expect("generate RBAC artifacts");
    for rel in ["rbac/mover-clusterrole.yaml", "rbac/mover-role.yaml"] {
        let a = artifact(&artifacts, rel);
        let rules = named_role_rules(&a.content, "kopiur-snapshot-replication-mover");
        for verb in ["get", "list", "create", "patch", "delete"] {
            assert!(
                rule_grants_verb(&rules, "kopiur.home-operations.com", "snapshots", verb),
                "{rel}: dedicated role must grant `{verb}` on snapshots"
            );
        }
        for resource in ["snapshots/status", "snapshotreplications/status"] {
            for verb in ["get", "patch"] {
                assert!(
                    rule_grants_verb(&rules, "kopiur.home-operations.com", resource, verb),
                    "{rel}: dedicated role must grant `{verb}` on {resource}"
                );
            }
        }
        assert!(
            rule_grants_verb(&rules, "", "configmaps", "patch"),
            "{rel}: dedicated role keeps the generic mover's configmaps surface"
        );
        // Still a mover role: none of the operator's broad grants.
        for (g, r) in [
            ("batch", "jobs"),
            ("", "secrets"),
            ("", "pods"),
            ("kopiur.home-operations.com", "snapshotpolicies"),
        ] {
            assert!(
                !rule_grants(&rules, g, r),
                "{rel}: dedicated role must NOT grant {g}/{r}"
            );
        }
    }
}
