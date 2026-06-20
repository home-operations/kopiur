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

/// Large embedded `core/v1` objects — the mover `securityContext`/`podSecurityContext`/
/// `resources`/`affinity`, the server security context, and the hooks `jobSpec` — must
/// render as opaque `x-kubernetes-preserve-unknown-fields` objects, NOT the fully-inlined
/// k8s-openapi structural schema. Inlining the `JobSpec`/`PodSpec` schema bloated a single
/// CRD past 1 MB (~85% of the bundle) and breaks large-CRD apply paths (e.g. client-side
/// apply's 256 KB `last-applied-configuration` annotation limit). The Rust fields stay
/// their concrete typed `k8s-openapi` types — only the apiserver's structural validation of
/// the internals is relaxed. Regression guard for `kopiur_api::schema::preserve_unknown_object`.
#[test]
fn embedded_core_v1_objects_render_as_preserve_unknown_not_inlined() {
    let artifacts = xtask::crds::artifacts().expect("generate CRD artifacts");

    // The SnapshotPolicy hooks `jobSpec` was the worst offender (~1.2 MB inlined).
    let sp = crd_yaml(&artifacts, "snapshotpolicies");
    assert!(
        sp.contains("x-kubernetes-preserve-unknown-fields: true"),
        "snapshotpolicies must render embedded core/v1 objects as preserve-unknown"
    );
    // This description only appears when the full PodSpec `SecurityContext` is inlined.
    assert!(
        !sp.contains("AllowPrivilegeEscalation controls"),
        "snapshotpolicies must NOT inline the k8s-openapi PodSpec/SecurityContext schema; \
         annotate the embedded field with schema_with = preserve_unknown_object"
    );

    // Size ceilings: with the embedded schemas pruned every CRD is small. Guard against a
    // regression that re-inlines a PodSpec/JobSpec (which would ~10x the file). These bounds
    // sit well above the current sizes (snapshotpolicies ~47 KB, repositories ~57 KB) but far
    // below the inlined sizes (~1.2 MB / ~210 KB).
    for (plural, max) in [("snapshotpolicies", 150_000), ("repositories", 100_000)] {
        let bytes = crd_yaml(&artifacts, plural).len();
        assert!(
            bytes < max,
            "{plural}.yaml is {bytes} bytes (>= {max}); did an embedded core/v1 schema get \
             inlined again? See kopiur_api::schema::preserve_unknown_object"
        );
    }
}
