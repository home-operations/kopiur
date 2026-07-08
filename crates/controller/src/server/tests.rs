use super::*;

/// YAML → JSON value → typed, the cluster's parse path (avoids serde_yaml's
/// broken externally-tagged-enum encoding). See api::testutil.
fn from_yaml<T: serde::de::DeserializeOwned>(yaml: &str) -> T {
    let value: serde_json::Value = serde_yaml::from_str(yaml).expect("yaml -> json value");
    serde_json::from_value(value).expect("json value -> typed")
}

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
        },
        read_only: false,
        port: 51515,
        service_type: "ClusterIP",
        service_annotations: BTreeMap::new(),
        auth,
        creds_secret: "nas-creds",
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
    // Hygiene fields: surface ProgressDeadlineExceeded and cap stale ReplicaSets.
    assert_eq!(spec.progress_deadline_seconds, Some(300));
    assert_eq!(spec.revision_history_limit, Some(2));
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
    // Repo creds via envFrom (KOPIA_PASSWORD + backend creds).
    assert_eq!(
        c.env_from.as_ref().unwrap()[0]
            .secret_ref
            .as_ref()
            .unwrap()
            .name,
        "nas-creds"
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

#[test]
fn classify_returns_none_when_no_status() {
    let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-server
"#;
    let dep: Deployment = from_yaml(yaml);
    assert!(classify_server_deployment(&dep).is_none());
}

#[test]
fn classify_returns_ready_when_available_replicas_gte_1() {
    let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-server
status:
  availableReplicas: 1
  conditions:
    - type: Available
      status: "True"
"#;
    let dep: Deployment = from_yaml(yaml);
    let r = classify_server_deployment(&dep).unwrap();
    assert_eq!(r, ServerReadiness::Ready);
}

#[test]
fn classify_returns_ready_with_multiple_replicas() {
    let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-server
status:
  availableReplicas: 3
"#;
    let dep: Deployment = from_yaml(yaml);
    let r = classify_server_deployment(&dep).unwrap();
    assert_eq!(r, ServerReadiness::Ready);
}

#[test]
fn classify_returns_not_available_when_zero_replicas() {
    let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-server
status:
  availableReplicas: 0
"#;
    let dep: Deployment = from_yaml(yaml);
    let r = classify_server_deployment(&dep).unwrap();
    match r {
        ServerReadiness::NotAvailable { message } => {
            assert!(message.contains("availableReplicas: 0"));
        }
        other => panic!("expected NotAvailable, got {other:?}"),
    }
}

#[test]
fn classify_returns_not_available_when_no_available_replicas_field() {
    let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-server
status:
  conditions:
    - type: Available
      status: "False"
"#;
    let dep: Deployment = from_yaml(yaml);
    let r = classify_server_deployment(&dep).unwrap();
    match r {
        ServerReadiness::NotAvailable { message } => {
            assert!(message.contains("availableReplicas: 0"));
        }
        other => panic!("expected NotAvailable, got {other:?}"),
    }
}

#[test]
fn classify_returns_progress_deadline_exceeded() {
    let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-server
status:
  availableReplicas: 0
  conditions:
    - type: Progressing
      status: "False"
      reason: ProgressDeadlineExceeded
      message: "ReplicaSet has timed out progressing"
"#;
    let dep: Deployment = from_yaml(yaml);
    let r = classify_server_deployment(&dep).unwrap();
    match r {
        ServerReadiness::ProgressDeadlineExceeded { message } => {
            assert!(message.contains("timed out progressing"));
        }
        other => panic!("expected ProgressDeadlineExceeded, got {other:?}"),
    }
}

#[test]
fn classify_progress_deadline_even_with_zero_available_replicas() {
    // Progressing=False wins over available_replicas=0.
    let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-server
status:
  availableReplicas: 0
  conditions:
    - type: Progressing
      status: "False"
      reason: ProgressDeadlineExceeded
      message: "Rollout timed out"
    - type: Available
      status: "False"
"#;
    let dep: Deployment = from_yaml(yaml);
    let r = classify_server_deployment(&dep).unwrap();
    assert!(matches!(
        r,
        ServerReadiness::ProgressDeadlineExceeded { .. }
    ));
}

#[test]
fn classify_condition_input_mapping() {
    let (s, r, m) = ServerReadiness::Ready.to_condition_input();
    assert!(s && r == "ServerReady" && m == "kopia UI server is running");

    let na = ServerReadiness::NotAvailable {
        message: "0 ready".into(),
    };
    let (s, r, m) = na.to_condition_input();
    assert!(!s && r == "ServerNotAvailable" && m == "0 ready");

    let pd = ServerReadiness::ProgressDeadlineExceeded {
        message: "stalled".into(),
    };
    let (s, r, m) = pd.to_condition_input();
    assert!(!s && r == "ServerProgressDeadlineExceeded" && m == "stalled");
}
