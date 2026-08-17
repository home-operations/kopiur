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

// --- cross-dimension identity precedence (the moverDefaults-shadows-inherited bug) ---

#[test]
fn moverdefaults_container_uid_cannot_shadow_an_inherited_pod_level_uid() {
    // The matter-server shape: the workload pins root at the POD level, so the
    // inherited context (folded into the recipe layer) carries `psc.runAsUser: 0`,
    // while the repository's moverDefaults pins `sc.runAsUser: 1000` at the
    // CONTAINER level. The ladder says inherited beats moverDefaults, but the
    // kubelet's effective UID is `container ?? pod` — without identity promotion
    // the lower layer's container value shadows the higher layer's pod value and
    // the mover silently runs as 1000.
    let defaults = MoverDefaults {
        security_context: Some(SecurityContext {
            run_as_user: Some(1000),
            run_as_group: Some(1000),
            ..Default::default()
        }),
        ..Default::default()
    };
    let inherited_psc = PodSecurityContext {
        run_as_user: Some(0),
        ..Default::default()
    };
    let m = resolve_mover(
        Some(&defaults),
        None,
        Some(&inherited_psc),
        None,
        None,
        None,
    );
    assert_eq!(
        super::effective_run_as_user(Some(&m.security_context), m.pod_security_context.as_ref()),
        Some(0),
        "the inherited pod-level uid is the higher layer and must win the \
         effective identity, regardless of which dimension each layer wrote"
    );
    // INV-1 keys on the effective UID: a root result must clear the hardened
    // runAsNonRoot:true or the kubelet wedges the pod in CreateContainerConfigError.
    assert_eq!(m.security_context.run_as_non_root, Some(false));
    // And the result is still recognized as elevated → privileged gate applies.
    assert!(requires_privilege_resolved(
        Some(&m.security_context),
        m.pod_security_context.as_ref(),
        None
    ));
}

#[test]
fn explicit_pod_level_uid_beats_moverdefaults_container_uid() {
    // The fully-silent variant of the same bug: no inheritance involved. A recipe
    // that pins its identity via `mover.podSecurityContext.runAsUser` is the top
    // layer and must win over `moverDefaults.securityContext.runAsUser`.
    let defaults = MoverDefaults {
        security_context: Some(SecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        }),
        ..Default::default()
    };
    let recipe_psc = PodSecurityContext {
        run_as_user: Some(2000),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, Some(&recipe_psc), None, None, None);
    assert_eq!(
        super::effective_run_as_user(Some(&m.security_context), m.pod_security_context.as_ref()),
        Some(2000),
        "the recipe's pod-level uid is the top layer and must win"
    );
    // Non-root result: the hardened runAsNonRoot:true must survive untouched.
    assert_eq!(m.security_context.run_as_non_root, Some(true));
}

#[test]
fn moverdefaults_container_gid_cannot_shadow_a_recipe_pod_level_gid() {
    // The runAsGroup analog: effective group is also `container ?? pod`, so a
    // moverDefaults container-level gid shadows a higher layer's pod-level gid
    // the same way.
    let defaults = MoverDefaults {
        security_context: Some(SecurityContext {
            run_as_group: Some(999),
            ..Default::default()
        }),
        ..Default::default()
    };
    let recipe_psc = PodSecurityContext {
        run_as_group: Some(568),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, Some(&recipe_psc), None, None, None);
    let psc = m.pod_security_context.as_ref();
    let effective_gid = m
        .security_context
        .run_as_group
        .or_else(|| psc.and_then(|p| p.run_as_group));
    assert_eq!(
        effective_gid,
        Some(568),
        "the recipe's pod-level gid is the higher layer and must win the \
         effective group"
    );
}

#[test]
fn identity_promotion_never_invents_an_identity() {
    // Layers that pin nothing must stay pinned-nothing: a moverDefaults that only
    // tweaks non-identity fields plus an identity-free recipe must not conjure a
    // runAsUser/runAsGroup from anywhere.
    let defaults = MoverDefaults {
        security_context: Some(SecurityContext {
            read_only_root_filesystem: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let m = resolve_mover(Some(&defaults), None, None, None, None, None);
    assert_eq!(
        super::effective_run_as_user(Some(&m.security_context), m.pod_security_context.as_ref()),
        None,
        "no layer pinned a uid, so the identity stays image-determined"
    );
    assert_eq!(m.security_context.run_as_group, None);
}

#[test]
fn within_one_layer_container_uid_still_beats_pod_uid() {
    // Intra-layer semantics are Kubernetes semantics: a single layer that sets
    // both `sc.runAsUser: 1000` and `psc.runAsUser: 0` means uid 1000 (container
    // wins within the layer). Promotion must respect that, not resurrect the
    // pod-level 0.
    let recipe_sc = SecurityContext {
        run_as_user: Some(1000),
        ..Default::default()
    };
    let recipe_psc = PodSecurityContext {
        run_as_user: Some(0),
        ..Default::default()
    };
    let m = resolve_mover(None, Some(&recipe_sc), Some(&recipe_psc), None, None, None);
    assert_eq!(
        super::effective_run_as_user(Some(&m.security_context), m.pod_security_context.as_ref()),
        Some(1000),
        "within one layer the container uid wins, exactly as the kubelet resolves it"
    );
}

#[test]
fn merge_context_pair_is_associative_and_resolves_the_highest_pinning_layer() {
    // The property the controller's pre-fold (inherited ⊂ explicit) and
    // `resolve_mover`'s fold (hardened ⊂ moverDefaults ⊂ recipe) both rely on:
    // any grouping of the layer chain merges to the identical pair, and the
    // effective identity is the highest layer's pinned one. This is also the
    // guard for `inherit_verdict`'s deleted moverDefaults branch — if promotion
    // ever regressed, a lower layer could displace an inherited uid again and
    // this property would break.
    let uids = [None, Some(0i64), Some(1000)];
    let gids = [None, Some(568i64)];
    let mut layers = Vec::new();
    for sc_uid in uids {
        for psc_uid in uids {
            for sc_gid in gids {
                for psc_gid in gids {
                    let sc = (sc_uid.is_some() || sc_gid.is_some()).then(|| SecurityContext {
                        run_as_user: sc_uid,
                        run_as_group: sc_gid,
                        ..Default::default()
                    });
                    let psc =
                        (psc_uid.is_some() || psc_gid.is_some()).then(|| PodSecurityContext {
                            run_as_user: psc_uid,
                            run_as_group: psc_gid,
                            ..Default::default()
                        });
                    layers.push((sc, psc));
                }
            }
        }
    }
    let pair = |base: &(Option<SecurityContext>, Option<PodSecurityContext>),
                over: &(Option<SecurityContext>, Option<PodSecurityContext>)| {
        super::merge_context_pair(
            base.0.as_ref(),
            base.1.as_ref(),
            over.0.as_ref(),
            over.1.as_ref(),
        )
    };
    let effective = |p: &(Option<SecurityContext>, Option<PodSecurityContext>)| {
        (
            super::effective_run_as_user(p.0.as_ref(), p.1.as_ref()),
            super::effective_run_as_group(p.0.as_ref(), p.1.as_ref()),
        )
    };
    for a in &layers {
        for b in &layers {
            for c in &layers {
                let left = pair(&pair(a, b), c);
                let right = pair(a, &pair(b, c));
                assert_eq!(left, right, "grouping must not change the merged pair");
                let (uid, gid) = effective(&left);
                let (ea, eb, ec) = (effective(a), effective(b), effective(c));
                assert_eq!(
                    uid,
                    ec.0.or(eb.0).or(ea.0),
                    "effective uid must be the highest layer's pinned one"
                );
                assert_eq!(
                    gid,
                    ec.1.or(eb.1).or(ea.1),
                    "effective gid must be the highest layer's pinned one"
                );
            }
        }
    }
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

#[test]
fn inherit_security_context_from_snapshot_variant_parses_the_cluster_way() {
    // Externally tagged `{ snapshot: {} }` — the restore-only mode that inherits the
    // identity RECORDED on the backup (`kopiur-meta` / `Snapshot.status.recorded`)
    // instead of reading a live pod.
    let snap: MoverSpec = crate::testutil::from_yaml(
        r#"
            inheritSecurityContextFrom:
              snapshot: {}
            "#,
    );
    assert!(matches!(
        snap.inherit_security_context_from,
        Some(InheritSecurityContextFrom::Snapshot(SnapshotInherit {})),
    ));
    // Round-trips through the cluster wire shape.
    let json = serde_json::to_value(&snap).unwrap();
    assert!(json["inheritSecurityContextFrom"]["snapshot"].is_object());
    let back: MoverSpec = serde_json::from_value(json).unwrap();
    assert_eq!(snap, back);

    // The YAML footgun: `snapshot:` (null) is NOT the empty sub-object — serde
    // rejects it rather than guessing. Doc examples must write `snapshot: {}`.
    let null_value: serde_json::Value =
        serde_yaml::from_str("inheritSecurityContextFrom:\n  snapshot:\n").unwrap();
    assert!(
        serde_json::from_value::<MoverSpec>(null_value).is_err(),
        "a null `snapshot:` must be rejected, not silently treated as `snapshot: {{}}`"
    );
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

// --- repo-level scheduleDefaults.timezone precedence (GitHub #174 item 3) ---

#[test]
fn resolve_tz_with_default_own_wins_over_repo_default() {
    assert_eq!(
        resolve_tz_with_default(Some("America/Chicago"), Some("Europe/Berlin")),
        "America/Chicago".parse::<chrono_tz::Tz>().unwrap()
    );
}

#[test]
fn resolve_tz_with_default_repo_default_fills_when_own_absent() {
    assert_eq!(
        resolve_tz_with_default(None, Some("America/New_York")),
        "America/New_York".parse::<chrono_tz::Tz>().unwrap()
    );
}

#[test]
fn resolve_tz_with_default_both_absent_is_utc() {
    assert_eq!(resolve_tz_with_default(None, None), chrono_tz::Tz::UTC);
}

#[test]
fn resolve_tz_with_default_invalid_own_falls_back_to_utc_per_resolve_tz_semantics() {
    // An unparseable own timezone falls straight back to UTC — same defensive
    // fallback as `resolve_tz` — even when a VALID repo default is present. The
    // admission webhook rejects bad names up front for both levels, so this
    // should never be observed in practice; the point is the fallback matches
    // `resolve_tz`'s existing (own-only) semantics, not a secondary retry.
    assert_eq!(
        resolve_tz_with_default(Some("America/Chicgo"), Some("Europe/Berlin")),
        chrono_tz::Tz::UTC
    );
}

#[test]
fn resolve_tz_with_default_invalid_repo_default_with_no_own_is_utc() {
    assert_eq!(
        resolve_tz_with_default(None, Some("Not/AZone")),
        chrono_tz::Tz::UTC
    );
}

#[test]
fn effective_timezone_own_wins_without_lookups() {
    // Own timezone is authoritative; matched-policy defaults are never consulted.
    let (tz, amb) = effective_timezone(
        Some("America/Los_Angeles"),
        &[Some("Europe/Berlin".to_string())],
    );
    assert_eq!(tz.name(), "America/Los_Angeles");
    assert!(amb.is_none());
}

#[test]
fn effective_timezone_unset_single_agreeing_default() {
    let defs = [Some("America/New_York".to_string())];
    let (tz, amb) = effective_timezone(None, &defs);
    assert_eq!(tz.name(), "America/New_York");
    assert!(amb.is_none());
}

#[test]
fn effective_timezone_unset_multiple_agreeing_defaults() {
    let defs = [
        Some("Europe/Berlin".to_string()),
        Some("Europe/Berlin".to_string()),
        Some("Europe/Berlin".to_string()),
    ];
    let (tz, amb) = effective_timezone(None, &defs);
    assert_eq!(tz.name(), "Europe/Berlin");
    assert!(amb.is_none());
}

#[test]
fn effective_timezone_unset_disagreeing_defaults_is_utc_with_ambiguity() {
    let defs = [
        Some("Europe/Berlin".to_string()),
        Some("America/Chicago".to_string()),
    ];
    let (tz, amb) = effective_timezone(None, &defs);
    assert_eq!(tz, chrono_tz::Tz::UTC);
    let amb = amb.expect("disagreeing defaults must surface an ambiguity signal");
    // Candidates are the distinct zones, sorted by IANA name.
    assert_eq!(amb.candidates, ["America/Chicago", "Europe/Berlin"]);
}

#[test]
fn effective_timezone_unset_mix_of_default_and_none_is_ambiguous() {
    // A repo with no default resolves to UTC, so "a zone" vs "no default" is a
    // genuine disagreement — reported, not silently one-sided.
    let defs = [Some("Europe/Berlin".to_string()), None];
    let (tz, amb) = effective_timezone(None, &defs);
    assert_eq!(tz, chrono_tz::Tz::UTC);
    let amb = amb.expect("a zone mixed with no-default must be ambiguous");
    assert_eq!(amb.candidates, ["Europe/Berlin", "UTC"]);
}

#[test]
fn effective_timezone_unset_no_policies_is_utc() {
    let (tz, amb) = effective_timezone(None, &[]);
    assert_eq!(tz, chrono_tz::Tz::UTC);
    assert!(amb.is_none());
}

#[test]
fn effective_timezone_unset_all_repos_without_default_agree_on_utc() {
    // Several matched policies, none of whose repos set a default → all UTC → agree.
    let defs = [None, None];
    let (tz, amb) = effective_timezone(None, &defs);
    assert_eq!(tz, chrono_tz::Tz::UTC);
    assert!(amb.is_none());
}

#[test]
fn effective_timezone_unset_invalid_default_falls_back_to_utc() {
    // An unparseable repo default resolves to UTC (resolve_tz semantics). Two invalid
    // strings both collapse to UTC and therefore agree.
    let defs = [
        Some("Not/AZone".to_string()),
        Some("Also/Bogus".to_string()),
    ];
    let (tz, amb) = effective_timezone(None, &defs);
    assert_eq!(tz, chrono_tz::Tz::UTC);
    assert!(amb.is_none());
}

// --- PvcAccessMode: closed schema, graceful legacy decode ---

#[test]
fn pvc_access_mode_schema_lists_only_canonical_values() {
    // `Unknown` is a decode-compat artifact for legacy stored data and must NEVER
    // be admissible: the schema enum is exactly the four canonical k8s strings.
    let schema = schemars::schema_for!(PvcAccessMode);
    let json = serde_json::to_value(&schema).unwrap();
    assert_eq!(
        json["enum"],
        serde_json::json!([
            "ReadWriteOnce",
            "ReadOnlyMany",
            "ReadWriteMany",
            "ReadWriteOncePod"
        ]),
        "schema enum must be exactly the canonical set; got {json}"
    );
    assert_eq!(json["type"], "string", "got {json}");
}

#[test]
fn pvc_access_mode_parse_and_mode_str_are_inverse_on_canonical() {
    for s in PvcAccessMode::CANONICAL {
        let parsed = PvcAccessMode::parse(s).expect("canonical value must parse");
        assert!(!matches!(parsed, PvcAccessMode::Unknown(_)));
        assert_eq!(parsed.mode_str(), s);
    }
    // Case matters (k8s wire strings are exact) — shorthand/typos don't parse.
    assert_eq!(PvcAccessMode::parse("rwo"), None);
    assert_eq!(PvcAccessMode::parse("readwriteonce"), None);
}

#[test]
fn pvc_access_mode_legacy_value_decodes_to_unknown_and_roundtrips() {
    // The load-bearing property: a bogus stored string must DECODE (a serde error
    // here would poison the typed watch stream for the whole Kind) and must
    // re-serialize unchanged, so a read-modify-write never mutates legacy data.
    let m: PvcAccessMode = serde_json::from_value(serde_json::json!("ReadWriteOnze")).unwrap();
    assert_eq!(m, PvcAccessMode::Unknown("ReadWriteOnze".into()));
    assert_eq!(m.mode_str(), "ReadWriteOnze");
    assert_eq!(serde_json::to_value(&m).unwrap(), "ReadWriteOnze");
}

#[test]
fn failure_block_roundtrips_with_op_via_the_api_server_path() {
    // YAML → JSON → typed (the cluster's decode path — never serde_yaml direct).
    let fb: FailureBlock = crate::testutil::from_yaml(
        r#"
kopiaErrorClass: RepositoryUnavailable
message: "repository connect failed"
stderrTail: "dial tcp: connection refused"
exitCode: 1
retryRecommended: true
op: repository connect
"#,
    );
    assert_eq!(fb.kopia_error_class, "RepositoryUnavailable");
    assert_eq!(fb.op.as_deref(), Some("repository connect"));
    // Structural round-trip: serialize → reparse must be identical, and the
    // wire field name must be the camelCase `op` the CRD schema declares.
    let v = serde_json::to_value(&fb).unwrap();
    assert_eq!(v["op"], "repository connect");
    let back: FailureBlock = serde_json::from_value(v).unwrap();
    assert_eq!(back, fb);
}

#[test]
fn failure_block_op_absent_stays_none_and_is_omitted_when_none() {
    // Every pre-M2 status block in the wild lacks `op`: it must decode to None
    // (no default-materialized value) …
    let fb: FailureBlock = crate::testutil::from_yaml(
        r#"
kopiaErrorClass: NotFound
message: "no such file or directory"
retryRecommended: false
"#,
    );
    assert_eq!(fb.op, None);
    // … and a None must serialize to NOTHING (skip_serializing_if), so a
    // re-PATCH of an old block can never write an explicit null.
    let v = serde_json::to_value(&fb).unwrap();
    assert!(v.get("op").is_none(), "op: None must be omitted, got {v}");
}

// --- phase-enum `Unknown` decode fallback (M2, #359 version-skew class) ------

/// Assert the full `phase_serde!` contract for one phase enum:
/// every canonical variant round-trips as a bare string via `label()`, an
/// unrecognized string decodes to `Unknown` (never a serde error that would
/// poison the typed watch/list for the whole Kind) and re-serializes verbatim,
/// and `ALL`/`canonical()` never leak `Unknown` into the CRD schema's value set.
fn assert_phase_contract<P>(unknown_wire: &str)
where
    P: PhaseLabel + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    for variant in P::ALL {
        let wire = serde_json::to_value(variant).unwrap();
        assert_eq!(
            wire,
            variant.label(),
            "{variant:?} must serialize to label()"
        );
        let back: P = serde_json::from_value(wire).unwrap();
        assert_eq!(&back, variant, "{variant:?} must round-trip");
        assert_eq!(P::parse(variant.label()).as_ref(), Some(variant));
    }
    // The schema's value set is exactly ALL's labels — `Unknown` is never in it.
    let canonical = P::canonical();
    assert_eq!(canonical.len(), P::ALL.len());
    assert!(!canonical.contains(&unknown_wire));

    // A phase written by a NEWER operator decodes instead of erroring…
    let decoded: P = serde_json::from_value(serde_json::json!(unknown_wire)).unwrap();
    assert_eq!(decoded, P::unknown(unknown_wire.to_string()));
    // …echoes the stored value verbatim (so a read-modify-write never mutates
    // a phase this build does not understand)…
    assert_eq!(decoded.label(), unknown_wire);
    assert_eq!(serde_json::to_value(&decoded).unwrap(), unknown_wire);
    // …and is NOT parseable as a canonical value.
    assert!(P::parse(unknown_wire).is_none());
    // Not in ALL: the metric label domain and the schema enum stay canonical.
    assert!(!P::ALL.contains(&decoded));
}

#[test]
fn every_phase_enum_decodes_an_unknown_string_instead_of_erroring() {
    use crate::maintenance::ManualRunPhase;
    use crate::repository::RepositoryPhase;
    use crate::repository_replication::RepositoryReplicationPhase;
    use crate::{RestorePhase, SnapshotPhase};

    assert_phase_contract::<SnapshotPhase>("Quiescing");
    assert_phase_contract::<RestorePhase>("Staging");
    assert_phase_contract::<RepositoryPhase>("Upgrading");
    assert_phase_contract::<RepositoryReplicationPhase>("Verifying");
    assert_phase_contract::<ManualRunPhase>("Queued");
    assert_phase_contract::<ReplicationManualRunPhase>("Queued");
}

#[test]
fn unknown_phase_is_never_terminal() {
    use crate::{RestorePhase, SnapshotPhase};
    // The conservative surface-it policy: a phase this build cannot interpret
    // must never be reported as finished work, or a newer operator's in-flight
    // (or wedged) object goes invisible to an older CLI — exactly the silent
    // green #359 is about.
    assert!(!SnapshotPhase::Unknown("Quiescing".into()).is_terminal());
    assert!(!RestorePhase::Unknown("Staging".into()).is_terminal());
    // Canonical terminals are unaffected.
    assert!(SnapshotPhase::Succeeded.is_terminal());
    assert!(RestorePhase::Completed.is_terminal());
}

#[test]
fn a_snapshot_cr_with_a_future_phase_still_decodes_whole() {
    use crate::testutil::from_yaml;
    use crate::{Snapshot, SnapshotPhase};
    // The regression this exists for: ONE object written by a newer operator
    // must not fail the typed `list()`/watch for every other Snapshot. Parsed
    // the cluster's way (YAML -> serde_json::Value -> typed).
    let s: Snapshot = from_yaml(
        r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: nightly-1
  namespace: apps
spec:
  policyRef:
    name: nightly
status:
  phase: Quiescing
  observedGeneration: 4
"#,
    );
    let phase = s.status.as_ref().unwrap().phase.clone().unwrap();
    assert_eq!(phase, SnapshotPhase::Unknown("Quiescing".into()));
    // The rest of the status survived — the fallback is a decode nicety, not a
    // whole-object bail-out.
    assert_eq!(s.status.as_ref().unwrap().observed_generation, Some(4));
    // Re-serializing the status writes the phase back BYTE-IDENTICAL, so an
    // older operator's read-modify-write cannot downgrade the newer value.
    let round = serde_json::to_value(&s.status).unwrap();
    assert_eq!(round["phase"], "Quiescing");
}

#[test]
fn a_restore_cr_with_a_future_phase_still_decodes_whole() {
    use crate::testutil::from_yaml;
    use crate::{Restore, RestorePhase};
    let r: Restore = from_yaml(
        r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata:
  name: r1
  namespace: apps
spec:
  repository:
    name: nas
  source:
    snapshotRef:
      name: nightly-1
  target:
    pvcRef:
      name: data
status:
  phase: Staging
"#,
    );
    assert_eq!(
        r.status.as_ref().unwrap().phase,
        Some(RestorePhase::Unknown("Staging".into()))
    );
}

#[test]
fn parse_run_requested_at_is_the_one_timestamp_parser() {
    use std::collections::BTreeMap;
    // No annotation at all, and an annotation map without the key: both "no
    // request", never an error.
    assert_eq!(
        parse_run_requested_at(None, "kubectl kopiur replication run"),
        Ok(None)
    );
    let empty = BTreeMap::new();
    assert_eq!(
        parse_run_requested_at(Some(&empty), "kubectl kopiur replication run"),
        Ok(None)
    );

    // A well-formed request parses to the pinned instant, normalized to UTC —
    // an offset timestamp and its UTC equivalent are the SAME request.
    let at = |raw: &str| {
        let mut a = BTreeMap::new();
        a.insert(
            crate::consts::RUN_REQUESTED_ANNOTATION.to_string(),
            raw.to_string(),
        );
        parse_run_requested_at(Some(&a), "kubectl kopiur replication run")
            .unwrap()
            .unwrap()
    };
    assert_eq!(at("2026-06-11T12:00:00Z"), at("2026-06-11T14:00:00+02:00"));

    // Garbage names the annotation, the offending value, the shape, AND the
    // caller's own run command (the fix hint is per-kind for a reason).
    let mut bad = BTreeMap::new();
    bad.insert(
        crate::consts::RUN_REQUESTED_ANNOTATION.to_string(),
        "yesterday".to_string(),
    );
    let err = parse_run_requested_at(Some(&bad), "kubectl kopiur replication run").unwrap_err();
    assert!(
        err.contains(crate::consts::RUN_REQUESTED_ANNOTATION),
        "{err}"
    );
    assert!(err.contains("must be an RFC3339 timestamp"), "{err}");
    assert!(err.contains("yesterday"), "{err}");
    assert!(err.contains("kubectl kopiur replication run"), "{err}");
    // The hint is the CALLER's command, not a hardcoded maintenance one — the
    // whole reason the parameter exists.
    assert!(!err.contains("maintenance"), "{err}");
}

#[test]
fn maintenance_run_annotations_delegate_to_the_shared_parser() {
    use std::collections::BTreeMap;
    // The hoist must be behavior-preserving: `Maintenance`'s parser and the
    // shared one agree on the instant, and maintenance keeps its OWN fix hint.
    let mut a = BTreeMap::new();
    a.insert(
        crate::consts::RUN_REQUESTED_ANNOTATION.to_string(),
        "2026-06-11T12:00:00Z".to_string(),
    );
    let (via_maintenance, _) = crate::maintenance::parse_run_annotations(Some(&a))
        .unwrap()
        .unwrap();
    let shared = parse_run_requested_at(Some(&a), "kubectl kopiur maintenance run")
        .unwrap()
        .unwrap();
    assert_eq!(via_maintenance, shared);

    a.insert(
        crate::consts::RUN_REQUESTED_ANNOTATION.to_string(),
        "not-a-time".to_string(),
    );
    let err = crate::maintenance::parse_run_annotations(Some(&a)).unwrap_err();
    assert!(err.contains("kubectl kopiur maintenance run"), "{err}");
}

#[test]
fn replication_manual_run_phase_answers_only_terminal_outcomes() {
    use ReplicationManualRunPhase as P;
    // The dedupe predicate decides whether a user's run request is DROPPED.
    // Only a terminal outcome answers it.
    assert!(P::Succeeded.answers_request());
    assert!(P::Failed.answers_request());
    assert!(
        !P::Pending.answers_request(),
        "a queued request still owes a run"
    );
    assert!(
        !P::Running.answers_request(),
        "an in-flight run is not an answer"
    );
    assert!(
        !P::Unknown("Queued".into()).answers_request(),
        "a phase this build cannot read is not an answer we can vouch for"
    );
    // Every canonical variant is covered above; `ALL` pins that count so a new
    // variant forces this test to state its dedupe rule.
    assert_eq!(P::ALL.len(), 4);
    assert!(P::Unknown("Queued".into()).is_unknown());
    for p in P::ALL {
        assert!(!p.is_unknown(), "{p:?} is a canonical phase");
    }
}

#[test]
fn replication_manual_run_status_answers_pins_the_exact_request() {
    let status = |requested: &str, phase: ReplicationManualRunPhase| ReplicationManualRunStatus {
        requested_at: Some(requested.to_string()),
        phase: Some(phase),
        completed_at: None,
    };
    let raw = "2026-06-11T12:00:00Z";
    assert!(status(raw, ReplicationManualRunPhase::Succeeded).answers(raw));
    assert!(status(raw, ReplicationManualRunPhase::Failed).answers(raw));
    // A NEW timestamp is a NEW request, even though the old one finished.
    assert!(!status(raw, ReplicationManualRunPhase::Succeeded).answers("2026-06-11T13:00:00Z"));
    // Same request, not yet terminal.
    assert!(!status(raw, ReplicationManualRunPhase::Running).answers(raw));
    assert!(!status(raw, ReplicationManualRunPhase::Pending).answers(raw));
    // Nothing recorded at all.
    assert!(!ReplicationManualRunStatus::default().answers(raw));
}

#[test]
fn replication_manual_run_status_roundtrips_camel_case() {
    let st = ReplicationManualRunStatus {
        requested_at: Some("2026-06-11T12:00:00Z".into()),
        phase: Some(ReplicationManualRunPhase::Succeeded),
        completed_at: Some("2026-06-11T12:01:42Z".into()),
    };
    let json = serde_json::to_value(&st).unwrap();
    assert_eq!(json["requestedAt"], "2026-06-11T12:00:00Z");
    assert_eq!(json["phase"], "Succeeded");
    assert_eq!(json["completedAt"], "2026-06-11T12:01:42Z");
    let back: ReplicationManualRunStatus = serde_json::from_value(json).unwrap();
    assert_eq!(back, st);
    // Absent fields serialize away entirely (no explicit nulls in status).
    let empty = serde_json::to_value(ReplicationManualRunStatus::default()).unwrap();
    assert_eq!(empty, serde_json::json!({}));
}
