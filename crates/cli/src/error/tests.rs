//! Message tests for the CLI-only variants. The variants that moved to
//! `kopiur-ops` (and `classify_kube`) are tested in `kopiur_ops::error`.

use super::*;

fn api_err(code: u16) -> kube::Error {
    kube::Error::Api(
        kube::core::Status::failure("denied", "Forbidden")
            .with_code(code)
            .boxed(),
    )
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
fn all_namespaces_rejection_says_what_to_do_instead() {
    let msg = CliError::AllNamespacesNotApplicable { command: "suspend" }.to_string();
    assert!(msg.contains("suspend targets a single object"), "{msg}");
    assert!(msg.contains("drop -A and pass -n"), "{msg}");
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
fn incomplete_download_is_actionable() {
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
fn an_ops_failure_reaches_the_user_verbatim() {
    // `#[error(transparent)]`: the operations layer's what/why/fix message must
    // arrive unchanged, never wrapped in a second sentence.
    let ops = kopiur_ops::OpsError::PathNotFound {
        path: "sub/missing.txt".into(),
    };
    let want = ops.to_string();
    assert_eq!(CliError::from(ops).to_string(), want);
}
