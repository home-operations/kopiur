//! Tests for generated CRD YAML.
//!
//! Each generated CRD must round-trip back into a `CustomResourceDefinition`
//! (proving the YAML the cluster would receive is well-formed), expose the
//! `kopiur.home-operations.com` group + `v1alpha1` version, and carry the correct scope.

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use xtask::artifact::{Artifact, GEN_HEADER};

/// Fetch the body (header stripped) of a generated per-CRD artifact by plural.
fn crd_yaml(artifacts: &[Artifact], plural: &str) -> String {
    let want = format!("crds/{plural}.yaml");
    let a = artifacts
        .iter()
        .find(|a| a.rel_path == want)
        .unwrap_or_else(|| panic!("missing generated artifact {want}"));
    assert!(
        a.content.starts_with(GEN_HEADER),
        "{want} is missing the generated-file header"
    );
    a.content.strip_prefix(GEN_HEADER).unwrap().to_string()
}

fn parse(yaml: &str) -> CustomResourceDefinition {
    serde_yaml::from_str(yaml).expect("generated CRD YAML must parse as a CustomResourceDefinition")
}

#[test]
fn every_crd_roundtrips_with_expected_group_version_and_scope() {
    let artifacts = xtask::crds::artifacts().expect("generate CRD artifacts");

    // (plural, expected scope)
    let expected = [
        ("repositories", "Namespaced"),
        ("clusterrepositories", "Cluster"),
        ("snapshotpolicies", "Namespaced"),
        ("snapshots", "Namespaced"),
        ("snapshotschedules", "Namespaced"),
        ("restores", "Namespaced"),
        ("maintenances", "Namespaced"),
        ("repositoryreplications", "Namespaced"),
    ];

    for (plural, scope) in expected {
        let crd = parse(&crd_yaml(&artifacts, plural));

        assert_eq!(
            crd.spec.group, "kopiur.home-operations.com",
            "{plural} group"
        );
        assert_eq!(
            crd.spec.names.plural, plural,
            "{plural} metadata plural mismatch"
        );
        assert_eq!(crd.spec.scope, scope, "{plural} scope");

        let versions: Vec<&str> = crd.spec.versions.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            versions,
            vec!["v1alpha1"],
            "{plural} should expose exactly v1alpha1"
        );
    }
}

#[test]
fn bundle_contains_all_eight_crds() {
    let artifacts = xtask::crds::artifacts().expect("generate CRD artifacts");
    let bundle = artifacts
        .iter()
        .find(|a| a.rel_path == "crds/all-crds.yaml")
        .expect("missing all-crds.yaml bundle");

    let docs: Vec<&str> = bundle.content.split("\n---\n").collect();
    assert_eq!(docs.len(), 8, "bundle should hold 8 CRD documents");

    // Every document parses as a CRD.
    for (i, doc) in docs.iter().enumerate() {
        let cleaned = doc.strip_prefix(GEN_HEADER).unwrap_or(doc);
        let crd: CustomResourceDefinition =
            serde_yaml::from_str(cleaned).unwrap_or_else(|e| panic!("bundle doc {i} parse: {e}"));
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
    }
}

/// The hooks `jobSpec` (`RunJobHook`) must render as an opaque
/// `x-kubernetes-preserve-unknown-fields` object, NOT the fully-inlined k8s-openapi
/// `JobSpec`/`PodSpec` structural schema. Inlined, it dragged the whole `PodSpec` into the
/// `SnapshotPolicy` CRD — ~1.2 MB, ~85% of the bundle — which breaks large-CRD apply paths
/// (e.g. client-side apply's 256 KB `last-applied-configuration` annotation limit). The Rust
/// field stays a concrete typed `JobSpec`, and the apiserver validates the actual hook Job at
/// Job-creation time, so only apply-time structural validation of the spec is deferred.
/// Regression guard for `kopiur_api::schema::preserve_unknown_object`.
///
/// Note: the smaller embedded `core/v1` objects (`securityContext`/`podSecurityContext`/
/// `resources`/`affinity` on the mover/server) are deliberately left INLINED so the apiserver
/// keeps validating them at apply time — only the outsized `jobSpec` is pruned.
#[test]
fn hooks_job_spec_renders_as_preserve_unknown_not_inlined() {
    let artifacts = xtask::crds::artifacts().expect("generate CRD artifacts");
    let sp = crd_yaml(&artifacts, "snapshotpolicies");

    // The hooks `jobSpec` is preserve-unknown ...
    assert!(
        sp.contains("x-kubernetes-preserve-unknown-fields: true"),
        "snapshotpolicies hooks `jobSpec` must render as preserve-unknown"
    );
    // ... and the inlined `PodSpec` is gone. "Periodic probe of container" is a container/probe
    // description that ONLY appears when the full PodSpec is inlined (not from a bare
    // securityContext/resources/affinity), so it precisely catches a `jobSpec` re-inlining.
    assert!(
        !sp.contains("Periodic probe of container"),
        "snapshotpolicies must NOT inline the k8s-openapi JobSpec/PodSpec schema; annotate \
         RunJobHook.job_spec with schema_with = preserve_unknown_object"
    );

    // Size ceiling: pruned, the CRD is ~76 KB; with the `JobSpec` re-inlined it is ~1.2 MB.
    // The bound sits well above the current size and far below the inlined size.
    let bytes = sp.len();
    assert!(
        bytes < 300_000,
        "snapshotpolicies.yaml is {bytes} bytes (>= 300_000); did the JobSpec/PodSpec schema \
         get inlined again? See kopiur_api::schema::preserve_unknown_object"
    );
}
