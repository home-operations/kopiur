//! Validates that every `deploy/examples/*.yaml` manifest deserializes into the
//! real kopiur CRD types. This is the offline equivalent of `kubectl apply
//! --dry-run` against the CRD OpenAPI schemas: if an example uses a wrong field
//! shape (e.g. an internally-tagged `backend: { kind: S3 }`, or `PVCName` instead
//! of the enum `PvcName`), deserialization into the typed struct fails and this
//! test catches it — without ever touching a cluster.
//!
//! Each document is routed by `apiVersion`/`kind`; `kopiur.home-operations.com/v1alpha1` docs have
//! their `.spec` deserialized into the corresponding `*Spec` type via the same
//! YAML -> serde_json::Value -> typed path the cluster uses (see
//! `crates/api/src/lib.rs::testutil` for why serde_yaml-direct is wrong here).

use kopiur_api::{
    ClusterRepositorySpec, MaintenanceSpec, RepositoryReplicationSpec, RepositorySpec, RestoreSpec,
    SnapshotPolicySpec, SnapshotScheduleSpec, SnapshotSpec,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

fn examples_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/api ; examples live at ../../deploy/examples.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/examples")
        .canonicalize()
        .expect("deploy/examples must exist")
}

/// `*.yaml` files directly in `dir` (non-recursive), sorted.
fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("read examples dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "yaml").unwrap_or(false))
        .collect();
    files.sort();
    files
}

/// Deserialize the `.spec` of a kopiur.home-operations.com document into a typed spec, asserting it
/// matches the real CRD field surface.
fn check_spec<T: DeserializeOwned>(kind: &str, doc: &serde_json::Value, file: &str) {
    let spec = doc
        .get("spec")
        .unwrap_or_else(|| panic!("{file}: {kind} has no spec"));
    let typed: Result<T, _> = serde_json::from_value(spec.clone());
    typed.unwrap_or_else(|e| panic!("{file}: {kind}.spec does not match CRD type: {e}"));
}

#[test]
fn all_examples_match_crd_field_shapes() {
    let dir = examples_dir();
    // The numbered tutorial ladder (flat) plus the per-backend reference set
    // under `backends/` — both must deserialize into the real CRD types.
    let ladder = yaml_files(&dir);
    let backends = yaml_files(&dir.join("backends"));
    // Floors, not exact counts — both sets grow as examples/variants are added,
    // and every file found below is validated regardless. The point is to catch
    // an empty/missing dir, not to pin a number.
    assert!(
        ladder.len() >= 9,
        "expected >=9 numbered example files, found {ladder:?}"
    );
    assert!(
        backends.len() >= 8,
        "expected >=8 per-backend example files (one per backend kind), found {backends:?}"
    );
    let files: Vec<PathBuf> = ladder.into_iter().chain(backends).collect();

    let mut kopia_docs = 0usize;
    for path in &files {
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        let content = std::fs::read_to_string(path).expect("read example");
        // serde_yaml splits multi-doc streams on `---`.
        for de in serde_yaml::Deserializer::from_str(&content) {
            let value = serde_yaml::Value::deserialize(de).expect("yaml doc");
            // Convert the per-doc yaml Value into a serde_json::Value (cluster path).
            let json: serde_json::Value =
                serde_json::to_value(&value).expect("yaml value -> json value");
            if json.is_null() {
                continue;
            }
            let api = json
                .get("apiVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let kind = json.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if api != "kopiur.home-operations.com/v1alpha1" {
                // Secrets / PVCs are core types; not our concern here.
                continue;
            }
            kopia_docs += 1;
            match kind {
                "Repository" => check_spec::<RepositorySpec>(kind, &json, &file),
                "ClusterRepository" => check_spec::<ClusterRepositorySpec>(kind, &json, &file),
                "SnapshotPolicy" => check_spec::<SnapshotPolicySpec>(kind, &json, &file),
                "Snapshot" => check_spec::<SnapshotSpec>(kind, &json, &file),
                "SnapshotSchedule" => check_spec::<SnapshotScheduleSpec>(kind, &json, &file),
                "Restore" => check_spec::<RestoreSpec>(kind, &json, &file),
                "Maintenance" => check_spec::<MaintenanceSpec>(kind, &json, &file),
                "RepositoryReplication" => {
                    check_spec::<RepositoryReplicationSpec>(kind, &json, &file)
                }
                other => panic!("{file}: unexpected kopiur.home-operations.com kind {other}"),
            }
        }
    }
    // Sanity: across the ladder + per-backend files we should have validated a
    // healthy number of CRs (8 backend Repositories alone clear this).
    assert!(
        kopia_docs >= 20,
        "expected to validate >=20 kopiur.home-operations.com docs, got {kopia_docs}"
    );
}

/// The new #254/#258 examples do not merely PARSE — their optional keys survive the
/// round trip.
///
/// kopiur's API types deliberately do not set `deny_unknown_fields`, so a misspelled
/// optional key (`advanceOnSizeMB` where the field is `advanceOnSizeMiB`) deserializes
/// perfectly and is silently DROPPED. Every other check in this file would stay green
/// while the example documented a field that does nothing. Re-serializing and looking for
/// the value is the only way to catch that.
#[test]
fn new_optional_example_keys_survive_the_round_trip() {
    let dir = examples_dir();
    let rt = |file: &str, kind: &str| -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        for doc in text.split("\n---") {
            let Ok(v) = serde_yaml::from_str::<serde_json::Value>(doc) else {
                continue;
            };
            if v.get("kind").and_then(|k| k.as_str()) != Some(kind) {
                continue;
            }
            let spec = v.get("spec").expect("doc has a spec").clone();
            return match kind {
                "SnapshotPolicy" => serde_json::to_value(
                    serde_json::from_value::<SnapshotPolicySpec>(spec).unwrap(),
                )
                .unwrap(),
                "Repository" => {
                    serde_json::to_value(serde_json::from_value::<RepositorySpec>(spec).unwrap())
                        .unwrap()
                }
                other => panic!("unhandled kind {other}"),
            };
        }
        panic!("{file}: no {kind} document");
    };

    // #254: readOnly must round-trip as false, not vanish.
    let p = rt("31-source-fsgroup-normalize.yaml", "SnapshotPolicy");
    assert_eq!(
        p["sources"][0]["readOnly"],
        serde_json::json!(false),
        "sources[].readOnly must survive: {p}"
    );
    let p = rt("32-source-writable-direct.yaml", "SnapshotPolicy");
    assert_eq!(p["sources"][0]["readOnly"], serde_json::json!(false));
    assert_eq!(
        p["sources"][0]["acknowledgeLiveMutation"],
        serde_json::json!(true),
        "the acknowledgement must survive, or the example documents a no-op: {p}"
    );

    // #258: the epoch block must survive.
    let r = rt("33-repository-epoch-parameters.yaml", "Repository");
    assert_eq!(
        r["parameters"]["epoch"]["minDuration"],
        serde_json::json!("6h"),
        "spec.parameters.epoch.minDuration must survive: {r}"
    );
}
