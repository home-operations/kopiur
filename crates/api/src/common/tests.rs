use super::*;
use k8s_openapi::api::core::v1::{PodSecurityContext, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

fn ref_of(kind: RepositoryKind, name: &str, namespace: Option<&str>) -> RepositoryRef {
    RepositoryRef {
        kind,
        name: name.into(),
        namespace: namespace.map(str::to_string),
    }
}

#[test]
fn resolves_to_same_namespace_when_ref_omits_it() {
    // A Maintenance in `apps` referencing `{ kind: Repository, name: nas }`
    // (no namespace) points at Repository apps/nas.
    let r = ref_of(RepositoryKind::Repository, "nas", None);
    assert!(r.resolves_to("apps", RepositoryKind::Repository, "nas", Some("apps")));
    assert!(!r.resolves_to("apps", RepositoryKind::Repository, "nas", Some("other")));
}

#[test]
fn resolves_to_honors_explicit_cross_namespace_ref() {
    let r = ref_of(RepositoryKind::Repository, "nas", Some("backups"));
    // Owner namespace is irrelevant once the ref pins one.
    assert!(r.resolves_to("apps", RepositoryKind::Repository, "nas", Some("backups")));
    assert!(!r.resolves_to("apps", RepositoryKind::Repository, "nas", Some("apps")));
}

#[test]
fn resolves_to_name_mismatch_is_false() {
    let r = ref_of(RepositoryKind::Repository, "nas", None);
    assert!(!r.resolves_to("apps", RepositoryKind::Repository, "other", Some("apps")));
}

#[test]
fn resolves_to_kind_mismatch_is_false_even_with_same_name() {
    // A `Repository` ref must never satisfy a `ClusterRepository` target and
    // vice versa, even when the names collide.
    let r = ref_of(RepositoryKind::Repository, "shared", None);
    assert!(!r.resolves_to("apps", RepositoryKind::ClusterRepository, "shared", None));

    let cr = ref_of(RepositoryKind::ClusterRepository, "shared", None);
    assert!(!cr.resolves_to("apps", RepositoryKind::Repository, "shared", Some("apps")));
}

#[test]
fn resolves_to_cluster_repository_ignores_namespace() {
    let cr = ref_of(RepositoryKind::ClusterRepository, "hetzner", None);
    assert!(cr.resolves_to("apps", RepositoryKind::ClusterRepository, "hetzner", None));
    // Even a stray namespace on the ref (webhook normally forbids it) still
    // resolves cluster-scoped.
    let stray = ref_of(RepositoryKind::ClusterRepository, "hetzner", Some("oops"));
    assert!(stray.resolves_to("apps", RepositoryKind::ClusterRepository, "hetzner", None));
}

// --- cache-defaults merge (repository cacheDefaults ← mover.cache) ---

#[test]
fn cache_defaults_merge_overlays_field_by_field() {
    // Neither side → nothing to apply.
    assert_eq!(CacheDefaults::merge(None, None), None);

    let repo = CacheDefaults {
        capacity: Some("8Gi".into()),
        storage_class_name: Some("standard".into()),
        metadata_cache_size_mb: Some(1024),
        content_cache_size_mb: Some(4096),
        mode: Some(CacheVolumeMode::Ephemeral),
    };
    // Only base → base verbatim.
    assert_eq!(CacheDefaults::merge(Some(&repo), None), Some(repo.clone()));

    // Override wins per-field; unset override fields fall back to base.
    let mover = CacheDefaults {
        capacity: Some("32Gi".into()),
        storage_class_name: None,
        metadata_cache_size_mb: None,
        content_cache_size_mb: Some(16384),
        mode: Some(CacheVolumeMode::Persistent),
    };
    let merged = CacheDefaults::merge(Some(&repo), Some(&mover)).unwrap();
    assert_eq!(merged.capacity.as_deref(), Some("32Gi")); // override
    assert_eq!(merged.storage_class_name.as_deref(), Some("standard")); // base
    assert_eq!(merged.metadata_cache_size_mb, Some(1024)); // base
    assert_eq!(merged.content_cache_size_mb, Some(16384)); // override
    assert_eq!(merged.mode, Some(CacheVolumeMode::Persistent)); // override
    assert_eq!(merged.effective_mode(), CacheVolumeMode::Persistent);

    // Unset mode defaults to Ephemeral.
    assert_eq!(
        CacheDefaults::default().effective_mode(),
        CacheVolumeMode::Ephemeral
    );
}

#[test]
fn scratch_defaults_merge_overlays_field_by_field() {
    // Neither side → nothing to apply.
    assert_eq!(ScratchDefaults::merge(None, None), None);

    let repo = ScratchDefaults {
        storage_class_name: Some("fast-ssd".into()),
        capacity: Some("100Gi".into()),
    };
    // Only base → base verbatim.
    assert_eq!(
        ScratchDefaults::merge(Some(&repo), None),
        Some(repo.clone())
    );
    // Only override → override verbatim.
    let over_only = ScratchDefaults {
        storage_class_name: Some("slow-hdd".into()),
        capacity: None,
    };
    assert_eq!(
        ScratchDefaults::merge(None, Some(&over_only)),
        Some(over_only.clone())
    );

    // Override wins per-field; unset override fields fall back to base.
    let recipe = ScratchDefaults {
        storage_class_name: None,
        capacity: Some("200Gi".into()),
    };
    let merged = ScratchDefaults::merge(Some(&repo), Some(&recipe)).unwrap();
    assert_eq!(merged.storage_class_name.as_deref(), Some("fast-ssd")); // base
    assert_eq!(merged.capacity.as_deref(), Some("200Gi")); // override

    // Mixed the other way: storageClass from the recipe, capacity from the repo.
    let recipe2 = ScratchDefaults {
        storage_class_name: Some("fast-ssd".into()),
        capacity: None,
    };
    let merged2 = ScratchDefaults::merge(Some(&repo), Some(&recipe2)).unwrap();
    assert_eq!(merged2.storage_class_name.as_deref(), Some("fast-ssd"));
    assert_eq!(merged2.capacity.as_deref(), Some("100Gi")); // base
}

#[test]
fn mover_defaults_scratch_round_trips() {
    let yaml = r#"
scratch:
  storageClassName: fast-ssd
  capacity: 100Gi
"#;
    let md: MoverDefaults = crate::testutil::from_yaml(yaml);
    let scratch = md.scratch.expect("scratch present");
    assert_eq!(scratch.storage_class_name.as_deref(), Some("fast-ssd"));
    assert_eq!(scratch.capacity.as_deref(), Some("100Gi"));
}

// --- privileged-mover detection (ADR §4.11/§G16, namespace-gated). ---

use k8s_openapi::api::core::v1::{Capabilities, SecurityContext};

fn mover_with(sc: Option<SecurityContext>, privileged_mode: Option<bool>) -> MoverSpec {
    MoverSpec {
        security_context: sc,
        privileged_mode,
        ..Default::default()
    }
}

#[test]
fn default_mover_is_unprivileged() {
    assert!(!MoverSpec::default().requires_privilege());
    // A benign securityContext (non-root, no escalation) is not privileged.
    let benign = SecurityContext {
        run_as_user: Some(1000),
        run_as_non_root: Some(true),
        allow_privilege_escalation: Some(false),
        ..Default::default()
    };
    assert!(!mover_with(Some(benign), None).requires_privilege());
}

#[test]
fn run_as_root_requires_privilege() {
    // The trilium-rain case: mover.securityContext.runAsUser: 0.
    let root = SecurityContext {
        run_as_user: Some(0),
        ..Default::default()
    };
    assert!(mover_with(Some(root), None).requires_privilege());
}

#[test]
fn privileged_flag_and_escalation_and_caps_and_nonroot_false_all_count() {
    let priv_ctx = SecurityContext {
        privileged: Some(true),
        ..Default::default()
    };
    assert!(mover_with(Some(priv_ctx), None).requires_privilege());

    let escalate = SecurityContext {
        allow_privilege_escalation: Some(true),
        ..Default::default()
    };
    assert!(mover_with(Some(escalate), None).requires_privilege());

    let caps = SecurityContext {
        capabilities: Some(Capabilities {
            add: Some(vec!["SYS_ADMIN".into()]),
            drop: None,
        }),
        ..Default::default()
    };
    assert!(mover_with(Some(caps), None).requires_privilege());

    let nonroot_false = SecurityContext {
        run_as_non_root: Some(false),
        ..Default::default()
    };
    assert!(mover_with(Some(nonroot_false), None).requires_privilege());
}

#[test]
fn privileged_mode_flag_alone_requires_privilege() {
    assert!(mover_with(None, Some(true)).requires_privilege());
    assert!(!mover_with(None, Some(false)).requires_privilege());
}

#[test]
fn empty_added_capabilities_is_not_privileged() {
    let caps = SecurityContext {
        capabilities: Some(Capabilities {
            add: Some(vec![]),
            drop: Some(vec!["ALL".into()]),
        }),
        ..Default::default()
    };
    assert!(!mover_with(Some(caps), None).requires_privilege());
}

#[test]
fn pod_level_fsgroup_is_not_privileged_but_pod_level_root_is() {
    // fsGroup (the headline use) is NOT elevation — an unprivileged mover with
    // fsGroup must run without a namespace opt-in.
    let fsgroup = MoverSpec {
        pod_security_context: Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(!fsgroup.requires_privilege());

    // ...but a pod-level runAsUser: 0 / runAsNonRoot: false IS gated, so it can't
    // slip past the container-only check.
    let pod_root = MoverSpec {
        pod_security_context: Some(PodSecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(pod_root.requires_privilege());
    let pod_nonroot_false = MoverSpec {
        pod_security_context: Some(PodSecurityContext {
            run_as_non_root: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(pod_nonroot_false.requires_privilege());
}

#[test]
fn requires_privilege_resolved_covers_the_gate_inputs() {
    let root = SecurityContext {
        run_as_user: Some(0),
        ..Default::default()
    };
    let benign = SecurityContext {
        run_as_user: Some(1000),
        run_as_non_root: Some(true),
        ..Default::default()
    };
    let fsgroup = PodSecurityContext {
        fs_group: Some(1000),
        ..Default::default()
    };
    let pod_root = PodSecurityContext {
        run_as_user: Some(0),
        ..Default::default()
    };

    // Nothing set → not privileged.
    assert!(!requires_privilege_resolved(None, None, None));
    // Benign container + fsGroup pod context → still not privileged.
    assert!(!requires_privilege_resolved(
        Some(&benign),
        Some(&fsgroup),
        None
    ));
    // An (e.g. inherited) root CONTAINER context → privileged.
    assert!(requires_privilege_resolved(Some(&root), None, None));
    // A root POD context with a benign container → privileged (can't slip past).
    assert!(requires_privilege_resolved(
        Some(&benign),
        Some(&pod_root),
        None
    ));
    // privilegedMode alone → privileged.
    assert!(requires_privilege_resolved(None, None, Some(true)));
    // The pure helpers agree.
    assert!(security_context_is_elevated(&root));
    assert!(!security_context_is_elevated(&benign));
    assert!(pod_security_context_is_elevated(&pod_root));
    assert!(!pod_security_context_is_elevated(&fsgroup));
}

// --- moverDefaults field-wise merge (ADR-0004 §1/§2) ---

#[test]
fn resolve_mover_with_no_layers_is_the_hardened_default() {
    let m = resolve_mover(None, None, None, None, None, None);
    let sc = m.security_context;
    assert_eq!(sc.run_as_non_root, Some(true));
    assert_eq!(sc.allow_privilege_escalation, Some(false));
    assert_eq!(sc.capabilities.unwrap().drop.unwrap(), vec!["ALL"]);
    assert_eq!(sc.seccomp_profile.unwrap().type_, "RuntimeDefault");
    // The pod context is now hardened too (not None): fsGroup matches the mover
    // image's nonroot gid so the cache is writable on PVC-backed storage, with
    // OnRootMismatch so an already-correct volume isn't re-chowned every run.
    let psc = m
        .pod_security_context
        .expect("hardened pod context is always present");
    assert_eq!(psc.fs_group, Some(MOVER_NONROOT_ID));
    assert_eq!(
        psc.fs_group_change_policy.as_deref(),
        Some("OnRootMismatch")
    );
    assert!(m.resources.is_none());
    assert!(m.cache.is_none());
}

#[test]
fn recipe_or_defaults_can_override_the_hardened_fsgroup() {
    // The hardened fsGroup is a floor, not a ceiling: a moverDefaults fsGroup wins
    // over it, and a recipe fsGroup wins over moverDefaults — while unset pod
    // fields (here fsGroupChangePolicy) keep the hardened default. This is what
    // lets a restore own files as the app's UID/GID.
    let defaults = MoverDefaults {
        pod_security_context: Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
        ..Default::default()
    };
    let recipe_psc = PodSecurityContext {
        fs_group: Some(3000),
        run_as_user: Some(3000),
        ..Default::default()
    };

    // moverDefaults overrides the hardened fsGroup; change policy still inherited.
    let only_defaults = resolve_mover(Some(&defaults), None, None, None, None, None);
    let psc = only_defaults.pod_security_context.unwrap();
    assert_eq!(psc.fs_group, Some(1000));
    assert_eq!(
        psc.fs_group_change_policy.as_deref(),
        Some("OnRootMismatch")
    );

    // recipe wins over moverDefaults, which wins over hardened.
    let m = resolve_mover(Some(&defaults), None, Some(&recipe_psc), None, None, None);
    let psc = m.pod_security_context.unwrap();
    assert_eq!(psc.fs_group, Some(3000), "recipe fsGroup must win");
    assert_eq!(psc.run_as_user, Some(3000));
    assert_eq!(
        psc.fs_group_change_policy.as_deref(),
        Some("OnRootMismatch")
    );
}

#[test]
fn recipe_partial_override_only_tightens_keeping_hardening() {
    // The de-hardening bug ADR-0004 §2 cites: a recipe that sets only runAsUser
    // must NOT wipe the hardened drop:[ALL]/seccomp/escalation defaults.
    let recipe = SecurityContext {
        run_as_user: Some(1000),
        ..Default::default()
    };
    let m = resolve_mover(None, Some(&recipe), None, None, None, None);
    let sc = m.security_context;
    assert_eq!(sc.run_as_user, Some(1000)); // recipe wins
    assert_eq!(sc.run_as_non_root, Some(true)); // hardened preserved
    assert_eq!(sc.allow_privilege_escalation, Some(false)); // hardened preserved
    assert_eq!(sc.capabilities.unwrap().drop.unwrap(), vec!["ALL"]); // never lost
    assert_eq!(sc.seccomp_profile.unwrap().type_, "RuntimeDefault");
}

#[test]
fn three_layer_precedence_hardened_then_defaults_then_recipe() {
    let defaults = MoverDefaults {
        security_context: Some(SecurityContext {
            run_as_group: Some(568),
            run_as_user: Some(568),
            ..Default::default()
        }),
        ..Default::default()
    };
    let recipe = SecurityContext {
        run_as_user: Some(1000), // recipe overrides the moverDefaults runAsUser
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), Some(&recipe), None, None, None, None);
    let sc = m.security_context;
    assert_eq!(sc.run_as_user, Some(1000)); // recipe wins over defaults
    assert_eq!(sc.run_as_group, Some(568)); // from moverDefaults
    assert_eq!(sc.run_as_non_root, Some(true)); // from hardened base
    assert_eq!(sc.capabilities.unwrap().drop.unwrap(), vec!["ALL"]);
}

#[test]
fn add_only_capabilities_override_keeps_hardened_drop_all() {
    // Deep-merge: a recipe adding NET_BIND_SERVICE (with no `drop`) must keep the
    // hardened drop:[ALL] (the precise bug ADR-0004 §2 calls out).
    let recipe = SecurityContext {
        capabilities: Some(Capabilities {
            add: Some(vec!["NET_BIND_SERVICE".into()]),
            drop: None,
        }),
        ..Default::default()
    };
    let m = resolve_mover(None, Some(&recipe), None, None, None, None);
    let caps = m.security_context.capabilities.unwrap();
    assert_eq!(caps.add.unwrap(), vec!["NET_BIND_SERVICE"]);
    assert_eq!(caps.drop.unwrap(), vec!["ALL"]); // hardened drop survives
}

#[test]
fn inherited_root_uid_clears_hardened_run_as_non_root() {
    // The production wedge: inheritSecurityContextFrom copies `runAsUser: 0` off a
    // root workload (matrix/synapse, mssql). Merged under the hardened base it would
    // be `{ runAsNonRoot: true, runAsUser: 0 }` — which the kubelet rejects, parking
    // the pod in CreateContainerConfigError forever. resolve_mover must normalize it
    // to a VALID root context so the mover can run (gated by the privileged check).
    let inherited = SecurityContext {
        run_as_user: Some(0),
        run_as_group: Some(0),
        ..Default::default()
    };
    let m = resolve_mover(None, Some(&inherited), None, None, None, None);
    let sc = m.security_context;
    assert_eq!(sc.run_as_user, Some(0), "inherited root UID preserved");
    assert_eq!(
        sc.run_as_non_root,
        Some(false),
        "the contradictory runAsNonRoot:true MUST be cleared for a root UID"
    );
    // The hardened tightening is still intact — this is a *valid* root mover, not a
    // de-hardened one.
    assert_eq!(sc.allow_privilege_escalation, Some(false));
    assert_eq!(sc.capabilities.unwrap().drop.unwrap(), vec!["ALL"]);
    // And it is still recognized as elevated → the privileged-mover gate applies.
    assert!(super::security_context_is_elevated(&SecurityContext {
        run_as_user: Some(0),
        run_as_non_root: Some(false),
        ..Default::default()
    }));
}

#[test]
fn pod_level_root_uid_clears_container_run_as_non_root() {
    // The cross-level case: the inherited *pod* context sets `runAsUser: 0` while the
    // container keeps the hardened `runAsNonRoot: true`. The kubelet's effective UID
    // is `container.runAsUser ?? pod.runAsUser`, so this is the same contradiction —
    // normalization must clear runAsNonRoot at the container level too.
    let inherited_psc = PodSecurityContext {
        run_as_user: Some(0),
        ..Default::default()
    };
    let m = resolve_mover(None, None, Some(&inherited_psc), None, None, None);
    assert_eq!(
        m.security_context.run_as_non_root,
        Some(false),
        "pod-level root UID must clear the container's hardened runAsNonRoot:true"
    );
    let psc = m.pod_security_context.unwrap();
    assert_eq!(psc.run_as_user, Some(0));
    // The hardened pod context never set runAsNonRoot, so there is no contradiction to
    // clear at the pod level — it stays unset (valid: the kubelet permits runAsUser:0
    // when runAsNonRoot is not true). What matters is it is never left `Some(true)`.
    assert_ne!(psc.run_as_non_root, Some(true));
}

#[test]
fn nonroot_inherited_uid_keeps_run_as_non_root_true() {
    // The common, non-root inherit (e.g. runAsUser: 2000) is untouched: runAsNonRoot
    // stays true and the contexts remain mutually consistent — no over-normalization.
    let inherited = SecurityContext {
        run_as_user: Some(2000),
        run_as_group: Some(2000),
        run_as_non_root: Some(true),
        ..Default::default()
    };
    let m = resolve_mover(None, Some(&inherited), None, None, None, None);
    assert_eq!(m.security_context.run_as_user, Some(2000));
    assert_eq!(m.security_context.run_as_non_root, Some(true));
}

#[test]
fn pod_startup_deadline_defaults_to_five_minutes() {
    // The contract every reconciler relies on: unset → 5 minutes. Pinned so a careless
    // change to the constant fails a test rather than silently shifting every mover's
    // fail-fast window.
    assert_eq!(DEFAULT_POD_STARTUP_DEADLINE_SECONDS, 300);
    // No failurePolicy at all → default.
    assert_eq!(pod_startup_deadline_seconds(None), 300);
    // failurePolicy present but field unset → default.
    let fp_unset = FailurePolicy {
        backoff_limit: Some(2),
        ..Default::default()
    };
    assert_eq!(pod_startup_deadline_seconds(Some(&fp_unset)), 300);
    // Explicit value wins.
    let fp_set = FailurePolicy {
        pod_startup_deadline_seconds: Some(900),
        ..Default::default()
    };
    assert_eq!(pod_startup_deadline_seconds(Some(&fp_set)), 900);
}

#[test]
fn pod_security_context_merges_fsgroup_from_defaults_with_recipe() {
    let defaults = MoverDefaults {
        pod_security_context: Some(PodSecurityContext {
            fs_group: Some(568),
            fs_group_change_policy: Some("OnRootMismatch".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let recipe_psc = PodSecurityContext {
        run_as_user: Some(1000),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, Some(&recipe_psc), None, None, None);
    let psc = m.pod_security_context.unwrap();
    assert_eq!(psc.fs_group, Some(568)); // from defaults
    assert_eq!(
        psc.fs_group_change_policy.as_deref(),
        Some("OnRootMismatch")
    );
    assert_eq!(psc.run_as_user, Some(1000)); // from recipe
}

#[test]
fn resources_merge_per_key_with_recipe_winning() {
    use std::collections::BTreeMap;
    let defaults = MoverDefaults {
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity("100m".into())),
                ("memory".to_string(), Quantity("128Mi".into())),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    };
    let recipe_res = ResourceRequirements {
        requests: Some(BTreeMap::from([(
            "cpu".to_string(),
            Quantity("500m".into()),
        )])),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, None, Some(&recipe_res), None, None);
    let req = m.resources.unwrap().requests.unwrap();
    assert_eq!(req["cpu"].0, "500m"); // recipe wins
    assert_eq!(req["memory"].0, "128Mi"); // defaults fills
}

#[test]
fn privileged_gate_fires_on_merged_root_from_defaults_but_not_benign() {
    // moverDefaults setting runAsUser:0 produces a privileged merged context even
    // with no recipe override — the gate must see the merged result.
    let root_defaults = MoverDefaults {
        security_context: Some(SecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };
    let m = resolve_mover(Some(&root_defaults), None, None, None, None, None);
    assert!(requires_privilege_resolved(
        Some(&m.security_context),
        m.pod_security_context.as_ref(),
        None
    ));

    // A benign merge (hardened base only) must NOT trip the gate.
    let benign = resolve_mover(None, None, None, None, None, None);
    assert!(!requires_privilege_resolved(
        Some(&benign.security_context),
        benign.pod_security_context.as_ref(),
        None
    ));
}

#[test]
fn mover_defaults_flows_cache_node_selector_and_ttl() {
    let defaults = MoverDefaults {
        cache: Some(CacheDefaults {
            capacity: Some("10Gi".into()),
            ..Default::default()
        }),
        node_selector: Some(std::collections::BTreeMap::from([(
            "disktype".to_string(),
            "ssd".to_string(),
        )])),
        ttl_seconds_after_finished: Some(3600),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, None, None, None, None);
    assert_eq!(m.cache.unwrap().capacity.as_deref(), Some("10Gi"));
    assert_eq!(m.node_selector.unwrap()["disktype"], "ssd");
    assert_eq!(m.ttl_seconds_after_finished, Some(3600));
}

// --- RWO multi-attach: sourceColocation flows from moverDefaults, defaults to Auto ---

#[test]
fn source_colocation_defaults_to_auto_when_unset() {
    // No moverDefaults at all → Auto (the bug-fixing default).
    let none = resolve_mover(None, None, None, None, None, None);
    assert_eq!(none.source_colocation, SourceColocationMode::Auto);
    // moverDefaults present but sourceColocation unset → still Auto.
    let defaults = MoverDefaults {
        node_selector: Some(std::collections::BTreeMap::from([(
            "disktype".to_string(),
            "ssd".to_string(),
        )])),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, None, None, None, None);
    assert_eq!(m.source_colocation, SourceColocationMode::Auto);
}

#[test]
fn source_colocation_mode_flows_from_defaults() {
    let defaults = MoverDefaults {
        source_colocation: Some(SourceColocation {
            mode: Some(SourceColocationMode::Disabled),
        }),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, None, None, None, None);
    assert_eq!(m.source_colocation, SourceColocationMode::Disabled);
}

#[test]
fn source_colocation_parses_the_cluster_way() {
    // YAML → serde_json::Value → typed (the cluster's path), never serde_yaml direct.
    let defaults: MoverDefaults = crate::testutil::from_yaml(
        r#"
            sourceColocation:
              mode: Required
            "#,
    );
    assert_eq!(
        defaults.source_colocation,
        Some(SourceColocation {
            mode: Some(SourceColocationMode::Required),
        })
    );
    // An empty sub-object resolves to Auto (mode unset).
    let bare: MoverDefaults = crate::testutil::from_yaml(
        r#"
            sourceColocation: {}
            "#,
    );
    assert_eq!(
        resolve_mover(Some(&bare), None, None, None, None, None).source_colocation,
        SourceColocationMode::Auto,
    );
}

#[test]
fn inherit_security_context_from_parses_both_variants_the_cluster_way() {
    // Externally tagged: { workloadSelector: {...} } vs { pvcConsumer: {...} }, parsed via
    // YAML → serde_json::Value → typed (never serde_yaml, which mis-encodes external tags).
    let selector: MoverSpec = crate::testutil::from_yaml(
        r#"
            inheritSecurityContextFrom:
              workloadSelector:
                podSelector:
                  matchLabels:
                    app: pg
                container: postgres
            "#,
    );
    match selector.inherit_security_context_from {
        Some(InheritSecurityContextFrom::WorkloadSelector(s)) => {
            assert_eq!(s.container.as_deref(), Some("postgres"));
        }
        other => panic!("expected WorkloadSelector, got {other:?}"),
    }

    let consumer: MoverSpec = crate::testutil::from_yaml(
        r#"
            inheritSecurityContextFrom:
              pvcConsumer: {}
            "#,
    );
    assert!(matches!(
        consumer.inherit_security_context_from,
        Some(InheritSecurityContextFrom::PvcConsumer(
            PvcConsumerInherit { container: None }
        )),
    ));

    let consumer_named: MoverSpec = crate::testutil::from_yaml(
        r#"
            inheritSecurityContextFrom:
              pvcConsumer:
                container: app
            "#,
    );
    assert!(matches!(
        consumer_named.inherit_security_context_from,
        Some(InheritSecurityContextFrom::PvcConsumer(PvcConsumerInherit { container }))
            if container.as_deref() == Some("app"),
    ));
}

// --- §12 mover Job TTL precedence (recipe over default over built-in) ---

#[test]
fn ttl_precedence_recipe_over_default_over_builtin() {
    // Built-in default when neither sets one (so finished Jobs always self-GC).
    let none = resolve_mover(None, None, None, None, None, None);
    assert_eq!(
        none.ttl_seconds_after_finished,
        Some(DEFAULT_JOB_TTL_SECONDS)
    );

    // moverDefaults sets it → used when the recipe doesn't override.
    let defaults = MoverDefaults {
        ttl_seconds_after_finished: Some(7200),
        ..Default::default()
    };
    let from_default = resolve_mover(Some(&defaults), None, None, None, None, None);
    assert_eq!(from_default.ttl_seconds_after_finished, Some(7200));

    // Recipe override wins over the repo default.
    let from_recipe = resolve_mover(Some(&defaults), None, None, None, None, Some(900));
    assert_eq!(from_recipe.ttl_seconds_after_finished, Some(900));

    // Recipe override alone (no repo default) also wins over the built-in.
    let recipe_only = resolve_mover(None, None, None, None, None, Some(120));
    assert_eq!(recipe_only.ttl_seconds_after_finished, Some(120));
}

// --- §13(e) throttle flows from moverDefaults into ResolvedMover ---

#[test]
fn resolve_mover_carries_throttle_from_defaults() {
    let defaults = MoverDefaults {
        throttle: Some(Throttle {
            upload_bytes_per_second: Some(10_000_000),
            download_bytes_per_second: None,
            read_ops_per_second: Some(50),
            write_ops_per_second: None,
        }),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, None, None, None, None);
    let t = m.throttle.expect("throttle");
    assert_eq!(t.upload_bytes_per_second, Some(10_000_000));
    assert_eq!(t.read_ops_per_second, Some(50));
    // Absent on a repo with no throttle.
    assert!(
        resolve_mover(None, None, None, None, None, None)
            .throttle
            .is_none()
    );
}
