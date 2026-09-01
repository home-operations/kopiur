use super::*;
use crate::jobs::CredsEnvFrom;
use kopiur_api::creds::CredsSecretRef;

fn api_error(code: u16) -> crate::error::Error {
    use kube::core::Status;
    crate::error::Error::Kube(kube::Error::Api(Box::new(Status {
        code,
        message: "boom".into(),
        reason: "Forbidden".into(),
        ..Default::default()
    })))
}

#[test]
fn server_secret_403_maps_to_rbac_toggle_hint() {
    let mapped = map_server_secret_error(api_error(403), "nas-kopia-ui-auth", "backups");
    match mapped {
        crate::error::Error::MissingDependency(m) => {
            assert!(m.contains("HTTP 403"), "message: {m}");
            assert!(m.contains("features.kopiaUi.enabled: true"), "message: {m}");
            assert!(m.contains("`nas-kopia-ui-auth`"), "message: {m}");
            assert!(m.contains("`backups`"), "message: {m}");
        }
        other => panic!("expected MissingDependency, got {other:?}"),
    }
}

#[test]
fn server_secret_non_403_passes_through_unchanged() {
    assert!(matches!(
        map_server_secret_error(api_error(500), "nas-kopia-ui-auth", "backups"),
        crate::error::Error::Kube(_)
    ));
}

fn inputs<'a>(ns: &'a str, auth: ResolvedAuth) -> ServerBuildInputs<'a> {
    ServerBuildInputs {
        instance: "nas",
        namespace: ns,
        owner: None,
        extra_labels: BTreeMap::new(),
        image: "ghcr.io/home-operations/kopiur-mover:test",
        image_pull_policy: Some("IfNotPresent"),
        service_account: Some("kopiur-controller"),
        repository: RepositoryConnect::S3 {
            bucket: "b".into(),
            endpoint: Some("https://minio".into()),
            prefix: None,
            region: None,
            disable_tls: false,
            disable_tls_verification: false,
            ambient_credentials: false,
            ca_bundle_pem: None,
        },
        read_only: false,
        port: 51515,
        service_type: "ClusterIP",
        service_annotations: BTreeMap::new(),
        auth,
        creds_secrets: vec![CredsEnvFrom::plain("nas-creds")],
        azure_workload_identity: false,
        repo_volume: None,
        resources: None,
        security_context: None,
        pod_security_context: None,
    }
}

/// The pod-level securityContext of the server Deployment, for assertions.
fn pod_sec_ctx(dep: &Deployment) -> PodSecurityContext {
    dep.spec
        .as_ref()
        .unwrap()
        .template
        .spec
        .as_ref()
        .unwrap()
        .security_context
        .clone()
        .expect("server pod carries a pod-level securityContext")
}

fn gen_auth() -> ResolvedAuth {
    ResolvedAuth::Password {
        username: "kopia".into(),
        password_secret: "nas-kopia-ui-auth".into(),
        password_key: "password".into(),
    }
}

#[test]
fn object_names_are_derived_from_instance() {
    assert_eq!(server_object_name("nas"), "nas-kopia-ui");
    assert_eq!(generated_secret_name("nas"), "nas-kopia-ui-auth");
}

/// The pod-template spec-hash annotation, for assertions.
fn spec_hash_annotation(dep: &Deployment) -> String {
    dep.spec
        .as_ref()
        .unwrap()
        .template
        .metadata
        .as_ref()
        .expect("pod template carries metadata")
        .annotations
        .as_ref()
        .expect("pod template carries annotations")
        .get(crate::consts::SERVER_SPEC_HASH_ANNOTATION)
        .expect("pod template carries the server-spec hash")
        .clone()
}

#[test]
fn deployment_pod_template_hash_rolls_on_spec_change_and_is_stable_otherwise() {
    // Regression: the server reads its work spec from a mounted ConfigMap, and
    // a ConfigMap content change alone never restarts a running pod — so a CA
    // rotation (or any server-spec change) used to leave a stale server running
    // until something else touched the pod template. The template must pin a
    // hash of the spec.
    let base = inputs("ns", gen_auth());
    let dep_a = build_server_deployment(&base);
    let dep_b = build_server_deployment(&inputs("ns", gen_auth()));
    // Same spec ⇒ same annotation (the SSA apply stays a no-op — no restart churn).
    assert_eq!(spec_hash_annotation(&dep_a), spec_hash_annotation(&dep_b));

    // A changed CA bundle is a server-spec change ⇒ different annotation ⇒ the
    // pod template differs ⇒ the Deployment rolls.
    let mut with_ca = inputs("ns", gen_auth());
    with_ca.repository = RepositoryConnect::S3 {
        bucket: "b".into(),
        endpoint: Some("https://minio".into()),
        prefix: None,
        region: None,
        disable_tls: false,
        disable_tls_verification: false,
        ambient_credentials: false,
        ca_bundle_pem: Some(
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into(),
        ),
    };
    assert_ne!(
        spec_hash_annotation(&dep_a),
        spec_hash_annotation(&build_server_deployment(&with_ca))
    );

    // Any other spec knob rolls too (port change as a representative).
    let mut with_port = inputs("ns", gen_auth());
    with_port.port = 51516;
    assert_ne!(
        spec_hash_annotation(&dep_a),
        spec_hash_annotation(&build_server_deployment(&with_port))
    );
}

#[test]
fn work_spec_maps_password_auth_and_port() {
    let ws = build_server_work_spec(&inputs("ns", gen_auth()));
    assert_eq!(ws.listen_port, 51515);
    assert_eq!(
        ws.auth,
        ServerAuthSpec::Password {
            username: "kopia".into()
        }
    );
    assert_eq!(ws.to_start_spec().address, "0.0.0.0:51515");
}

#[test]
fn work_spec_maps_no_auth() {
    let ws = build_server_work_spec(&inputs("ns", ResolvedAuth::None));
    assert_eq!(ws.auth, ServerAuthSpec::None {});
}

#[test]
fn work_spec_maps_read_only() {
    // Default fixture is read-write; the work spec must not connect read-only.
    let rw = build_server_work_spec(&inputs("ns", gen_auth()));
    assert!(!rw.read_only);
    // Flip the resolved flag → the mover connects read-only.
    let mut ro_inputs = inputs("ns", gen_auth());
    ro_inputs.read_only = true;
    assert!(build_server_work_spec(&ro_inputs).read_only);
}

#[test]
fn status_json_emits_effective_read_only() {
    let pin = ServerStatusPin {
        endpoint: "nas-kopia-ui.ns.svc:51515".into(),
        namespace: "ns".into(),
        auth_mode: "Generate".into(),
        read_only: true,
        generated_secret_ref: Some("nas-kopia-ui-auth".into()),
    };
    let v = server_status_json(&ServerOutcome::Active(pin)).unwrap();
    assert_eq!(v["server"]["readOnly"], serde_json::json!(true));
}

#[test]
fn deployment_is_single_replica_recreate_with_probe() {
    let dep = build_server_deployment(&inputs("ns", gen_auth()));
    let spec = dep.spec.unwrap();
    assert_eq!(spec.replicas, Some(1));
    assert_eq!(spec.strategy.unwrap().type_.as_deref(), Some("Recreate"));
    // The selector must be a SUBSET of the pod template labels (or the Service
    // can't route / the Deployment is rejected). The template additionally
    // carries the `managed-by` label so the controller's scoped watches see it.
    let selector = spec.selector.match_labels.clone().unwrap();
    let template_labels = spec
        .template
        .metadata
        .as_ref()
        .unwrap()
        .labels
        .clone()
        .unwrap();
    for (k, v) in &selector {
        assert_eq!(
            template_labels.get(k),
            Some(v),
            "selector label {k} missing/mismatched in template"
        );
    }
    assert!(
        template_labels.contains_key("app.kubernetes.io/managed-by"),
        "pod template should carry managed-by for scoped watches"
    );
    let c = &spec.template.spec.as_ref().unwrap().containers[0];
    assert_eq!(c.args.as_ref().unwrap(), &vec!["serve".to_string()]);
    let probe = c.readiness_probe.as_ref().unwrap();
    assert_eq!(
        probe.tcp_socket.as_ref().unwrap().port,
        IntOrString::Int(51515)
    );
}

#[test]
fn deployment_password_auth_injects_server_password_env_from_secret() {
    let dep = build_server_deployment(&inputs("ns", gen_auth()));
    let c = &dep.spec.unwrap().template.spec.unwrap().containers[0];
    let env = c.env.as_ref().unwrap();
    let pw = env
        .iter()
        .find(|e| e.name == "KOPIA_SERVER_PASSWORD")
        .expect("KOPIA_SERVER_PASSWORD env");
    // The password is a secretKeyRef, never an inline value.
    assert!(pw.value.is_none());
    let sk = pw
        .value_from
        .as_ref()
        .unwrap()
        .secret_key_ref
        .as_ref()
        .unwrap();
    assert_eq!(sk.name, "nas-kopia-ui-auth");
    assert_eq!(sk.key, "password");
    // Repo creds via envFrom. The common single-Secret layout emits exactly ONE
    // entry — pinned so the #416 fix cannot churn existing single-secret
    // Deployments on upgrade (identical envFrom ⇒ SSA no-op ⇒ no pod roll).
    let env_from = c.env_from.as_ref().unwrap();
    assert_eq!(env_from.len(), 1);
    assert_eq!(env_from[0].secret_ref.as_ref().unwrap().name, "nas-creds");
}

#[test]
fn deployment_env_from_includes_every_creds_secret_in_order() {
    // Regression (#416): the server used to env-inject only the encryption
    // Secret, so a repo whose password and backend keys live in SEPARATE
    // Secrets crashlooped on `repository connect` (missing AWS_* keys). The
    // server now follows the mover contract: one envFrom per distinct Secret,
    // password first (crates/mover/src/jobs.rs).
    let mut i = inputs("ns", gen_auth());
    i.creds_secrets = vec![
        CredsEnvFrom::plain("nas-password"),
        CredsEnvFrom::plain("nas-s3-keys"),
    ];
    let dep = build_server_deployment(&i);
    let c = &dep.spec.unwrap().template.spec.unwrap().containers[0];
    let env_from = c.env_from.as_ref().unwrap();
    assert_eq!(env_from.len(), 2);
    let names: Vec<&str> = env_from
        .iter()
        .map(|e| e.secret_ref.as_ref().unwrap().name.as_str())
        .collect();
    assert_eq!(names, vec!["nas-password", "nas-s3-keys"]);
    for e in env_from {
        assert_eq!(e.secret_ref.as_ref().unwrap().optional, Some(false));
        assert!(e.prefix.is_none(), "server creds are always unprefixed");
    }
}

#[test]
fn deployment_sets_azure_workload_identity_label_only_when_flagged() {
    // Azure workload identity injects credentials via a mutating webhook that
    // only acts on pods carrying the opt-in label — same contract as movers
    // (`MoverRunIdentity::decorate_labels`). Non-Azure servers must NOT carry
    // it (a label change would roll every existing Deployment on upgrade).
    let plain = build_server_deployment(&inputs("ns", gen_auth()));
    let plain_labels = plain
        .spec
        .unwrap()
        .template
        .metadata
        .unwrap()
        .labels
        .unwrap();
    assert!(!plain_labels.contains_key(kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL));

    let mut i = inputs("ns", gen_auth());
    i.azure_workload_identity = true;
    let dep = build_server_deployment(&i);
    let spec = dep.spec.unwrap();
    let labels = spec
        .template
        .metadata
        .as_ref()
        .unwrap()
        .labels
        .clone()
        .unwrap();
    assert_eq!(
        labels.get(kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL),
        Some(&kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL_VALUE.to_string())
    );
    // The selector must never grow the label (selectors are immutable in-place).
    assert!(
        !spec
            .selector
            .match_labels
            .unwrap()
            .contains_key(kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL)
    );
}

#[test]
fn deployment_no_auth_omits_server_password_env() {
    let dep = build_server_deployment(&inputs("ns", ResolvedAuth::None));
    let c = &dep.spec.unwrap().template.spec.unwrap().containers[0];
    assert!(
        !c.env
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| e.name == "KOPIA_SERVER_PASSWORD")
    );
}

#[test]
fn deployment_mounts_repo_pvc_for_filesystem() {
    let mut i = inputs("ns", gen_auth());
    i.repo_volume = Some(ServerRepoVolume::Pvc(PvcMount {
        claim_name: "repo-rwx".into(),
        mount_path: "/repo".into(),
        read_only: false,
    }));
    let dep = build_server_deployment(&i);
    let pod = dep.spec.unwrap().template.spec.unwrap();
    let repo = pod
        .volumes
        .unwrap()
        .into_iter()
        .find(|v| v.name == "repo")
        .unwrap();
    assert_eq!(repo.persistent_volume_claim.unwrap().claim_name, "repo-rwx");
}

#[test]
fn deployment_mounts_nfs_export_for_filesystem() {
    let mut i = inputs("ns", gen_auth());
    i.repo_volume = Some(ServerRepoVolume::Nfs {
        server: "nas.lan".into(),
        path: "/export/kopia".into(),
        mount_path: "/repo".into(),
    });
    let dep = build_server_deployment(&i);
    let pod = dep.spec.unwrap().template.spec.unwrap();
    let repo = pod
        .volumes
        .unwrap()
        .into_iter()
        .find(|v| v.name == "repo")
        .unwrap();
    let nfs = repo.nfs.unwrap();
    assert_eq!(nfs.server, "nas.lan");
    assert_eq!(nfs.path, "/export/kopia");
}

#[test]
fn server_pod_carries_hardened_fsgroup_by_default() {
    // Regression: the server pod used to have NO pod-level securityContext, so
    // it could carry neither fsGroup nor supplementalGroups. It now matches the
    // mover's hardened pod base out of the box.
    let dep = build_server_deployment(&inputs("ns", gen_auth()));
    let psc = pod_sec_ctx(&dep);
    assert_eq!(psc.fs_group, Some(kopiur_api::common::MOVER_NONROOT_ID));
    assert_eq!(
        psc.fs_group_change_policy.as_deref(),
        Some("OnRootMismatch")
    );
    assert!(
        psc.supplemental_groups.is_none(),
        "no supplemental groups unless configured"
    );
}

#[test]
fn server_pod_security_context_supplemental_groups_merge_over_hardened() {
    // The NFS-shared-group path: an explicit supplementalGroups is overlaid on
    // the hardened base, and the hardened fsGroup survives (field-wise merge).
    let mut i = inputs("ns", gen_auth());
    i.pod_security_context = Some(PodSecurityContext {
        supplemental_groups: Some(vec![3001]),
        ..Default::default()
    });
    let dep = build_server_deployment(&i);
    let psc = pod_sec_ctx(&dep);
    assert_eq!(psc.supplemental_groups.as_deref(), Some(&[3001i64][..]));
    // Hardened base is preserved through the merge.
    assert_eq!(psc.fs_group, Some(kopiur_api::common::MOVER_NONROOT_ID));
    assert_eq!(
        psc.fs_group_change_policy.as_deref(),
        Some("OnRootMismatch")
    );
}

#[test]
fn server_pod_security_context_override_wins_over_hardened_fsgroup() {
    // An explicit fsGroup/runAsUser in the override beats the hardened default,
    // while the container securityContext stays independent (its own resolution).
    let mut i = inputs("ns", gen_auth());
    i.pod_security_context = Some(PodSecurityContext {
        fs_group: Some(3001),
        run_as_user: Some(3001),
        ..Default::default()
    });
    let dep = build_server_deployment(&i);
    let psc = pod_sec_ctx(&dep);
    assert_eq!(psc.fs_group, Some(3001));
    assert_eq!(psc.run_as_user, Some(3001));
    // Container securityContext is still the hardened default (not the pod one).
    let container = &dep.spec.unwrap().template.spec.unwrap().containers[0];
    let csc = container.security_context.as_ref().unwrap();
    assert_eq!(csc.run_as_non_root, Some(true));
    assert_eq!(csc.run_as_user, None);
}

#[test]
fn service_selector_matches_deployment_and_carries_type_and_annotations() {
    let mut i = inputs("ns", gen_auth());
    i.service_type = "LoadBalancer";
    i.service_annotations =
        BTreeMap::from([("io.cilium/lb-ipam-ips".to_string(), "10.0.0.5".to_string())]);
    let svc = build_server_service(&i);
    let spec = svc.spec.unwrap();
    assert_eq!(spec.type_.as_deref(), Some("LoadBalancer"));
    assert_eq!(spec.selector, Some(selector_labels("nas")));
    assert_eq!(spec.ports.unwrap()[0].port, 51515);
    assert_eq!(
        svc.metadata.annotations.unwrap()["io.cilium/lb-ipam-ips"],
        "10.0.0.5"
    );
}

#[test]
fn generated_secret_holds_credentials_once() {
    let s = build_generated_secret(&inputs("ns", gen_auth()), "kopia", "s3cret");
    let data = s.string_data.unwrap();
    assert_eq!(data["username"], "kopia");
    assert_eq!(data["password"], "s3cret");
    assert_eq!(s.metadata.name.as_deref(), Some("nas-kopia-ui-auth"));
}

#[test]
fn config_map_round_trips_the_work_spec() {
    let cm = build_server_config_map(&inputs("ns", gen_auth())).unwrap();
    let body = &cm.data.unwrap()[SERVER_SPEC_FILE];
    let parsed: ServerWorkSpec = serde_json::from_str(body).unwrap();
    assert_eq!(parsed.listen_port, 51515);
}

// --- plan_server ---

#[test]
fn plan_ensure_when_desired_and_none_observed() {
    assert_eq!(
        plan_server(Some("ns"), None),
        ServerAction::Ensure {
            namespace: "ns".into()
        }
    );
}

#[test]
fn plan_ensure_when_namespace_unchanged() {
    assert_eq!(
        plan_server(Some("ns"), Some("ns")),
        ServerAction::Ensure {
            namespace: "ns".into()
        }
    );
}

#[test]
fn plan_migrate_when_namespace_changed() {
    assert_eq!(
        plan_server(Some("new"), Some("old")),
        ServerAction::Migrate {
            from: "old".into(),
            to: "new".into()
        }
    );
}

#[test]
fn plan_teardown_when_disabled_but_observed() {
    assert_eq!(
        plan_server(None, Some("old")),
        ServerAction::Teardown {
            namespace: "old".into()
        }
    );
}

#[test]
fn plan_noop_when_nothing_desired_or_observed() {
    assert_eq!(plan_server(None, None), ServerAction::Noop);
}

// --- plan_server_creds ---

fn cref(name: &str, ns: Option<&str>) -> CredsSecretRef {
    CredsSecretRef {
        name: name.into(),
        namespace: ns.map(str::to_string),
    }
}

#[test]
fn mirror_name_idx0_is_the_exact_legacy_name() {
    // ON-CLUSTER NAME. idx 0 must stay byte-identical to the pre-#416 single
    // mirror name: a rename would orphan the copy on every deployed
    // ClusterRepository server and roll its pod for nothing.
    assert_eq!(
        mirrored_creds_secret_name("nas", 0),
        "nas-kopia-ui-repo-creds"
    );
    assert_eq!(
        mirrored_creds_secret_name("nas", 1),
        "nas-kopia-ui-repo-creds-1"
    );
}

#[test]
fn creds_plan_namespaced_is_always_direct_with_no_reap() {
    // A namespaced Repository's secrets are same-namespace by construction
    // (movers likewise require same-ns without projection); mirrors never
    // existed for namespaced repos, so nothing to reap.
    let plan = plan_server_creds(
        "nas",
        "backups",
        &[cref("nas-password", None), cref("nas-s3-keys", None)],
        false,
        None,
    );
    assert_eq!(
        plan.sources,
        vec![
            ServerCredsSource::Direct {
                name: "nas-password".into()
            },
            ServerCredsSource::Direct {
                name: "nas-s3-keys".into()
            },
        ]
    );
    assert!(plan.reap.is_empty());
}

#[test]
fn creds_plan_cluster_split_cross_ns_mirrors_both() {
    // The #416 shape on a ClusterRepository: password and backend keys in two
    // Secrets, both defaulting to the operator namespace, server elsewhere —
    // BOTH must be mirrored (the second used to be dropped entirely).
    let plan = plan_server_creds(
        "nas",
        "storage",
        &[cref("repo-password", None), cref("backend-creds", None)],
        true,
        Some("kopiur-system"),
    );
    assert_eq!(
        plan.sources,
        vec![
            ServerCredsSource::Mirrored {
                src_namespace: "kopiur-system".into(),
                src_name: "repo-password".into(),
                mirror_name: "nas-kopia-ui-repo-creds".into(),
            },
            ServerCredsSource::Mirrored {
                src_namespace: "kopiur-system".into(),
                src_name: "backend-creds".into(),
                mirror_name: "nas-kopia-ui-repo-creds-1".into(),
            },
        ]
    );
    assert!(plan.reap.is_empty());
}

#[test]
fn creds_plan_cluster_single_cross_ns_keeps_legacy_name_and_reaps_second_slot() {
    // Migration: an existing single-secret cluster server keeps its exact
    // mirror name (SSA no-op, no orphan), and the unused second slot is reaped.
    let plan = plan_server_creds(
        "nas",
        "storage",
        &[cref("repo-creds", Some("infra"))],
        true,
        Some("kopiur-system"),
    );
    assert_eq!(
        plan.sources,
        vec![ServerCredsSource::Mirrored {
            src_namespace: "infra".into(),
            src_name: "repo-creds".into(),
            mirror_name: "nas-kopia-ui-repo-creds".into(),
        }]
    );
    assert_eq!(plan.reap, vec!["nas-kopia-ui-repo-creds-1".to_string()]);
}

#[test]
fn creds_plan_cluster_same_ns_is_direct_and_reaps_both_slots() {
    // Secrets already living in the server namespace need no copy; stale
    // mirrors from a previous cross-ns topology must not outlive the move.
    let plan = plan_server_creds(
        "nas",
        "storage",
        &[
            cref("repo-password", Some("storage")),
            cref("backend-creds", Some("storage")),
        ],
        true,
        Some("kopiur-system"),
    );
    assert_eq!(
        plan.sources,
        vec![
            ServerCredsSource::Direct {
                name: "repo-password".into()
            },
            ServerCredsSource::Direct {
                name: "backend-creds".into()
            },
        ]
    );
    assert_eq!(
        plan.reap,
        vec![
            "nas-kopia-ui-repo-creds".to_string(),
            "nas-kopia-ui-repo-creds-1".to_string(),
        ]
    );
}

#[test]
fn creds_plan_cluster_mixed_direct_and_mirrored() {
    // ref0 already in the server namespace (Direct), ref1 defaulting to the
    // operator namespace (Mirrored under its OWN index name); the unused idx-0
    // slot is reaped.
    let plan = plan_server_creds(
        "nas",
        "storage",
        &[
            cref("repo-password", Some("storage")),
            cref("backend-creds", None),
        ],
        true,
        Some("kopiur-system"),
    );
    assert_eq!(
        plan.sources,
        vec![
            ServerCredsSource::Direct {
                name: "repo-password".into()
            },
            ServerCredsSource::Mirrored {
                src_namespace: "kopiur-system".into(),
                src_name: "backend-creds".into(),
                mirror_name: "nas-kopia-ui-repo-creds-1".into(),
            },
        ]
    );
    assert_eq!(plan.reap, vec!["nas-kopia-ui-repo-creds".to_string()]);
}

#[test]
fn creds_plan_reap_never_names_a_live_source_secret() {
    // Collision guard: a cluster repo whose ACTUAL same-ns credentials Secret
    // happens to be named like a mirror slot must not have it reaped — that
    // would delete the user's live Secret on every reconcile.
    let plan = plan_server_creds(
        "nas",
        "storage",
        &[cref("nas-kopia-ui-repo-creds", Some("storage"))],
        true,
        Some("kopiur-system"),
    );
    assert_eq!(
        plan.sources,
        vec![ServerCredsSource::Direct {
            name: "nas-kopia-ui-repo-creds".into()
        }]
    );
    assert_eq!(
        plan.reap,
        vec!["nas-kopia-ui-repo-creds-1".to_string()],
        "the colliding slot name must be excluded; the other slot still reaps"
    );
}

#[test]
fn creds_plan_cluster_falls_back_to_server_ns_last() {
    // No explicit ref namespace and no operator namespace: the server
    // namespace is the documented last resort — which makes the ref Direct.
    let plan = plan_server_creds("nas", "storage", &[cref("repo-creds", None)], true, None);
    assert_eq!(
        plan.sources,
        vec![ServerCredsSource::Direct {
            name: "repo-creds".into()
        }]
    );
}
