use super::*;

fn api_err(code: u16) -> kube::Error {
    kube::Error::Api(
        kube::core::Status::failure("denied", "Forbidden")
            .with_code(code)
            .boxed(),
    )
}

#[test]
fn forbidden_message_names_verb_resource_and_fix() {
    let err = classify_kube(
        "patch",
        "SnapshotPolicy",
        "snapshotpolicies",
        Some("media"),
        Some("nightly"),
        api_err(403),
    );
    let msg = err.to_string();
    assert!(
        msg.contains("cannot patch snapshotpolicies in namespace media"),
        "{msg}"
    );
    assert!(msg.contains("grant `patch` on `snapshotpolicies`"), "{msg}");
    assert!(msg.contains("RBAC"), "{msg}");
}

#[test]
fn collection_404_maps_to_missing_crds_with_install_fix() {
    let err = classify_kube(
        "list",
        "Snapshot",
        "snapshots",
        None,
        None,
        kube::Error::Api(
            kube::core::Status::failure(
                "the server could not find the requested resource",
                "NotFound",
            )
            .with_code(404)
            .boxed(),
        ),
    );
    let msg = err.to_string();
    assert!(
        msg.contains("does not know the Snapshot resource type"),
        "{msg}"
    );
    assert!(msg.contains("install kopiur"), "{msg}");
    assert!(msg.contains("deploy/crds"), "{msg}");
}

#[test]
fn not_found_message_offers_a_listing_command() {
    let err = CliError::NotFound {
        kind: "SnapshotSchedule",
        plural: "snapshotschedules",
        name: "nightly".into(),
        scope: scope_suffix(Some("media")),
        scope_flag: " -n media".into(),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("SnapshotSchedule \"nightly\" not found in namespace media"),
        "{msg}"
    );
    assert!(
        msg.contains("kubectl get snapshotschedules -n media"),
        "{msg}"
    );
}

#[test]
fn object_level_404_maps_to_not_found_even_from_patch() {
    // The object vanished between GET and PATCH: the message names the
    // object, so this must be NotFound — never "CRDs not installed".
    let err = classify_kube(
        "patch",
        "SnapshotPolicy",
        "snapshotpolicies",
        Some("media"),
        Some("nightly"),
        kube::Error::Api(
            kube::core::Status::failure(
                "snapshotpolicies.kopiur.home-operations.com \"nightly\" not found",
                "NotFound",
            )
            .with_code(404)
            .boxed(),
        ),
    );
    let msg = err.to_string();
    assert!(
        msg.contains("SnapshotPolicy \"nightly\" not found in namespace media"),
        "{msg}"
    );
    assert!(!msg.contains("does not know"), "{msg}");
}

#[test]
fn unknown_type_404_wins_even_for_object_calls() {
    // A get/patch against an uninstalled CRD also 404s — with the
    // NotFoundHandler message — and must report the missing CRDs.
    let err = classify_kube(
        "get",
        "SnapshotPolicy",
        "snapshotpolicies",
        Some("media"),
        Some("nightly"),
        kube::Error::Api(
            kube::core::Status::failure(
                "the server could not find the requested resource",
                "NotFound",
            )
            .with_code(404)
            .boxed(),
        ),
    );
    assert!(
        err.to_string()
            .contains("does not know the SnapshotPolicy resource type")
    );
}

#[test]
fn admission_denial_beats_the_403_rbac_arm_and_quotes_the_webhook() {
    // A denial often arrives as 403; it must NOT be misread as missing RBAC,
    // and the message must carry the webhook's own (actionable) text.
    let err = classify_kube(
        "create",
        "Snapshot",
        "snapshots",
        Some("media"),
        Some("s"),
        kube::Error::Api(
            kube::core::Status::failure(
                "admission webhook \"vkopiur.kopiur.home-operations.com\" denied the request: deletionPolicy Retain is forced for discovered snapshots",
                "Forbidden",
            )
            .with_code(403)
            .boxed(),
        ),
    );
    let msg = err.to_string();
    assert!(
        msg.contains("an admission webhook rejected this object"),
        "{msg}"
    );
    assert!(msg.contains("deletionPolicy Retain is forced"), "{msg}");
    assert!(!msg.contains("RBAC"), "{msg}");
}

#[test]
fn log_stream_interruption_is_not_reported_as_a_bug() {
    let err = CliError::LogStreamInterrupted {
        source: std::io::Error::other("connection reset").into(),
    };
    let msg = err.to_string();
    assert!(msg.contains("log stream was interrupted"), "{msg}");
    assert!(
        msg.contains("re-run the same `kubectl kopiur logs`"),
        "{msg}"
    );
    assert!(!msg.contains("bug"), "{msg}");
}

#[test]
fn not_a_directory_points_at_cat_download() {
    let msg = CliError::NotADirectory {
        path: "a.txt".into(),
        entry_type: "f".into(),
    }
    .to_string();
    assert!(msg.contains("is not a directory"), "{msg}");
    assert!(msg.contains("cat`/`download"), "{msg}");
}

#[test]
fn mover_image_resolution_failures_carry_the_fix() {
    let msg = CliError::MoverImageUnresolvable {
        why: "2 Deployments match".into(),
        fix: "remove the impostor".into(),
    }
    .to_string();
    assert!(msg.contains("cannot resolve the mover image"), "{msg}");
    assert!(msg.contains("remove the impostor"), "{msg}");
}

#[test]
fn cross_namespace_repo_session_names_the_gc_hazard_and_local_escape() {
    let msg = CliError::RepoOutsideSessionNamespace {
        repo: "Repository/nas".into(),
        repo_namespace: "backups".into(),
        session_namespace: "media".into(),
    }
    .to_string();
    assert!(msg.contains("cross-namespace owners"), "{msg}");
    assert!(msg.contains("--local"), "{msg}");
}

#[test]
fn all_namespaces_rejection_says_what_to_do_instead() {
    let msg = CliError::AllNamespacesNotApplicable { command: "suspend" }.to_string();
    assert!(msg.contains("suspend targets a single object"), "{msg}");
    assert!(msg.contains("drop -A and pass -n"), "{msg}");
}

// --- browse data-plane: every variant says what failed, why, and the fix ---

#[test]
fn snapshot_not_browsable_names_the_status_field_and_fix() {
    let msg = CliError::SnapshotNotBrowsable {
        name: "db-1".into(),
        reason: "phase is Running".into(),
    }
    .to_string();
    assert!(msg.contains("Snapshot \"db-1\" cannot be browsed"), "{msg}");
    assert!(msg.contains("phase is Running"), "{msg}");
    assert!(msg.contains("status.snapshot.kopiaSnapshotID"), "{msg}");
    assert!(msg.contains("kubectl kopiur snapshots list"), "{msg}");
}

#[test]
fn repository_underivable_explains_both_derivation_sources() {
    let msg = CliError::RepositoryUnderivable {
        snapshot: "db-1".into(),
    }
    .to_string();
    assert!(msg.contains("status.resolved.repository"), "{msg}");
    assert!(msg.contains("ownerReference"), "{msg}");
    assert!(msg.contains("kubectl kopiur snapshot now"), "{msg}");
}

#[test]
fn cross_namespace_creds_offer_three_outs() {
    let msg = CliError::CredsOutsideSessionNamespace {
        secret: "s3-creds".into(),
        secret_namespace: "backups".into(),
        session_namespace: "media".into(),
    }
    .to_string();
    assert!(msg.contains("\"s3-creds\""), "{msg}");
    assert!(msg.contains("namespace backups"), "{msg}");
    assert!(
        msg.contains("cannot load a Secret from another namespace"),
        "{msg}"
    );
    assert!(msg.contains("--local"), "{msg}");
}

#[test]
fn cluster_repo_secret_without_namespace_names_the_field() {
    let msg = CliError::ClusterRepoSecretNamespaceMissing {
        secret: "creds".into(),
        repository: "nas".into(),
    }
    .to_string();
    assert!(msg.contains("ClusterRepository \"nas\""), "{msg}");
    assert!(msg.contains("secretRef.namespace"), "{msg}");
}

#[test]
fn operator_namespace_unresolvable_names_the_configmap_and_the_discovery_rule() {
    let msg = CliError::OperatorNamespaceUnresolvable {
        repository: "nas".into(),
        configmap: "internal-ca".into(),
        why: "no Deployment matches the controller labels".into(),
        fix: "is the kopiur operator installed?".into(),
    }
    .to_string();
    assert!(msg.contains("\"nas\""), "{msg}");
    assert!(msg.contains("\"internal-ca\""), "{msg}");
    // The user must learn WHERE a ClusterRepository's bundle lives and HOW the
    // CLI finds that namespace.
    assert!(msg.contains("KOPIUR_NAMESPACE"), "{msg}");
    assert!(msg.contains("controller"), "{msg}");
    assert!(
        msg.contains("Fix: is the kopiur operator installed?"),
        "{msg}"
    );
}

#[test]
fn ca_bundle_unresolvable_names_the_cause_and_the_fix() {
    let msg = CliError::CaBundleUnresolvable {
        repository: "nas".into(),
        detail: "ConfigMap team-a/internal-ca has no key \"ca.crt\"".into(),
        fix: "add the PEM CA bundle under that key".into(),
    }
    .to_string();
    assert!(msg.contains("\"nas\""), "{msg}");
    assert!(msg.contains("team-a/internal-ca"), "{msg}");
    assert!(msg.contains("tls.caBundleRef"), "{msg}");
    assert!(msg.contains("Fix: add the PEM CA bundle"), "{msg}");
}

#[test]
fn session_pod_failure_and_timeout_point_at_pods_and_doctor() {
    let failed = CliError::SessionPodFailed {
        job: "kopiur-browse-nas-abc123".into(),
        namespace: "media".into(),
        detail: "repository not initialized".into(),
    }
    .to_string();
    assert!(failed.contains("kopiur-browse-nas-abc123"), "{failed}");
    assert!(failed.contains("repository not initialized"), "{failed}");
    assert!(failed.contains("kubectl kopiur doctor"), "{failed}");

    let timeout = CliError::SessionNotReady {
        job: "kopiur-browse-nas-abc123".into(),
        after: "2m".into(),
    }
    .to_string();
    assert!(timeout.contains("timed out after 2m"), "{timeout}");
    assert!(
        timeout.contains("batch.kubernetes.io/job-name=kopiur-browse-nas-abc123"),
        "{timeout}"
    );
}

#[test]
fn session_exec_failure_quotes_stderr_and_reassures_read_only() {
    let msg = CliError::SessionExec {
        what: "show kdeadbeef".into(),
        stderr: "object not found".into(),
    }
    .to_string();
    assert!(msg.contains("object not found"), "{msg}");
    assert!(msg.contains("read-only"), "{msg}");
    assert!(msg.contains("session end"), "{msg}");
}

#[test]
fn local_errors_explain_the_local_contract() {
    let missing = CliError::LocalKopiaMissing {
        bin: "kopia".into(),
    }
    .to_string();
    assert!(missing.contains("install kopia"), "{missing}");
    assert!(missing.contains("drop --local"), "{missing}");

    let volume = CliError::LocalRepoVolume {
        repository: "fs-repo".into(),
    }
    .to_string();
    assert!(volume.contains("cluster volume"), "{volume}");
    assert!(volume.contains("in-cluster session"), "{volume}");

    let forbidden = CliError::SecretsForbidden {
        secret: "s3-creds".into(),
        namespace: "media".into(),
        source: Box::new(api_err(403)),
    }
    .to_string();
    assert!(forbidden.contains("`get` on `secrets`"), "{forbidden}");
    assert!(forbidden.contains("or drop --local"), "{forbidden}");
}

#[test]
fn path_errors_teach_the_path_grammar() {
    let invalid = CliError::InvalidPath {
        path: "../etc/passwd".into(),
        reason: "`..` components are not allowed".into(),
    }
    .to_string();
    assert!(
        invalid.contains("relative to the snapshot root"),
        "{invalid}"
    );

    let missing = CliError::PathNotFound {
        path: "sub/missing.txt".into(),
    }
    .to_string();
    assert!(
        missing.contains("does not exist in this snapshot"),
        "{missing}"
    );
    assert!(missing.contains("kubectl kopiur ls"), "{missing}");

    let dir = CliError::IsADirectory { path: "sub".into() }.to_string();
    assert!(dir.contains("is a directory"), "{dir}");
    assert!(dir.contains("ls <snapshot> sub"), "{dir}");

    let link = CliError::NotAFile {
        path: "link".into(),
        entry_type: "s".into(),
    }
    .to_string();
    assert!(link.contains("not a regular file"), "{link}");
}

#[test]
fn missing_catalog_entry_and_incomplete_download_are_actionable() {
    let gone = CliError::SnapshotMissingInRepo { id: "kdead".into() }.to_string();
    assert!(gone.contains("kdead"), "{gone}");
    assert!(gone.contains("expired by retention"), "{gone}");

    let short = CliError::DownloadIncomplete {
        path: "sub/b.txt".into(),
        expected: 12,
        actual: 7,
        dest: "/tmp/b.txt".into(),
    }
    .to_string();
    assert!(short.contains("expected 12 bytes, wrote 7"), "{short}");
    assert!(
        short.contains("partial file at /tmp/b.txt was removed"),
        "{short}"
    );
}

#[test]
fn unexpected_kopia_output_is_reported_as_a_compat_bug() {
    let msg = CliError::UnexpectedKopiaOutput {
        what: "directory manifest kabc".into(),
        detail: "stream marker was \"kopia:other\"".into(),
    }
    .to_string();
    assert!(msg.contains("directory manifest kabc"), "{msg}");
    assert!(msg.contains("version mismatch"), "{msg}");
    assert!(
        msg.contains("github.com/home-operations/kopiur/issues"),
        "{msg}"
    );
}

#[test]
fn generic_api_error_points_at_cluster_health() {
    let err = classify_kube(
        "list",
        "Snapshot",
        "snapshots",
        Some("x"),
        None,
        api_err(500),
    );
    let msg = err.to_string();
    assert!(
        msg.contains("cannot list snapshots in namespace x"),
        "{msg}"
    );
    assert!(msg.contains("kubectl version"), "{msg}");
}
