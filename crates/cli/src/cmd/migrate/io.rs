//! Offline / GitOps I/O for `migrate volsync`: read VolSync objects and
//! repository Secrets from YAML files (or stdin) instead of the cluster, and
//! write the translated manifests to a directory (one file per source).
//!
//! All YAML parsing goes `yaml → serde_json::Value → typed` (never `serde_yaml`
//! straight into a typed value) — serde_yaml 0.9 mis-encodes externally-tagged
//! enums, which both the VolSync specs and the emitted kopiur types rely on.
//! See `crates/api/src/lib.rs::testutil::from_yaml` for the same rule.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::api::Api;
use serde::Deserialize;

use crate::consts::STDIN_TOKEN;
use crate::error::{CliError, classify_kube};

/// Which VolSync kind a parsed object is. Exhaustive so the orchestration
/// `match`es it and a new kind can't be silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolsyncKind {
    /// `ReplicationSource`.
    Source,
    /// `ReplicationDestination`.
    Destination,
}

/// A VolSync object reduced to what the translator needs, sourced identically
/// whether it came from the cluster or a file — so the reconcile loop never
/// branches on input origin.
#[derive(Debug, Clone)]
pub struct RawVolsync {
    /// Source or Destination.
    pub kind: VolsyncKind,
    /// The object's namespace (metadata, or the fallback for file input).
    pub namespace: String,
    /// The object's name.
    pub name: String,
    /// The raw `spec` object, decoded into a typed spec by the caller.
    pub spec: serde_json::Value,
}

/// How a repository Secret is resolved under `--resolve-secrets`. Exhaustive.
pub enum SecretSource {
    /// Live cluster (`get_opt`) — cluster-input runs and the offline
    /// `--from-cluster-secrets` hybrid.
    Cluster {
        /// The connected client to GET Secrets with.
        client: kube::Client,
    },
    /// Plaintext Secret YAML supplied on disk, keyed `(namespace, name)`.
    Files(BTreeMap<(String, String), Secret>),
    /// `--repository`: Secrets are never read (policies point at an existing
    /// kopiur Repository).
    None,
}

impl SecretSource {
    /// Resolve the Secret named `name` in `namespace`, or `None` if absent.
    pub async fn get(&self, namespace: &str, name: &str) -> Result<Option<Secret>, CliError> {
        match self {
            SecretSource::Cluster { client } => {
                let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
                api.get_opt(name).await.map_err(|e| {
                    classify_kube("get", "Secret", "secrets", Some(namespace), Some(name), e)
                })
            }
            SecretSource::Files(map) => {
                Ok(map.get(&(namespace.to_string(), name.to_string())).cloned())
            }
            // Never reached: constructed only when `--repository` is set, and
            // then resolution is skipped entirely. Typed-error rather than
            // panic, to honor degrade-not-crash.
            SecretSource::None => Err(CliError::MigrationInput {
                what: "internal: secret resolution attempted with --repository set".into(),
                fix: "this is a kubectl-kopiur bug — please report it".into(),
            }),
        }
    }
}

/// Read every YAML document from the given inputs. Each input is a file, a
/// directory (every `*.yaml`/`*.yml` within, sorted by name), or `-` for stdin.
/// Returns `(human-label, document)` pairs; empty documents are skipped.
fn collect_documents(paths: &[PathBuf]) -> Result<Vec<(String, serde_json::Value)>, CliError> {
    let mut out = Vec::new();
    for path in paths {
        let blobs: Vec<(String, String)> = if path.as_os_str() == STDIN_TOKEN {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|source| CliError::LocalIo {
                    what: "reading manifests from stdin".into(),
                    source,
                })?;
            vec![("<stdin>".to_string(), buf)]
        } else if path.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .map_err(|source| CliError::LocalIo {
                    what: format!("reading directory {}", path.display()),
                    source,
                })?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    matches!(
                        p.extension().and_then(|e| e.to_str()),
                        Some("yaml") | Some("yml")
                    )
                })
                .collect();
            entries.sort();
            entries
                .into_iter()
                .map(|p| read_file(&p).map(|c| (p.display().to_string(), c)))
                .collect::<Result<_, _>>()?
        } else {
            vec![(path.display().to_string(), read_file(path)?)]
        };

        for (label, content) in blobs {
            for (i, doc) in serde_yaml::Deserializer::from_str(&content).enumerate() {
                let value =
                    serde_json::Value::deserialize(doc).map_err(|e| CliError::MigrationInput {
                        what: format!("{label} document {i} is not valid YAML/JSON: {e}"),
                        fix: "fix or remove the offending document".into(),
                    })?;
                if value.is_null() {
                    continue;
                }
                // Unwrap a `kind: List` wrapper (what `kubectl get -o yaml`
                // emits) into its items.
                if is_list(&value)
                    && let Some(items) = value.get("items").and_then(|v| v.as_array())
                {
                    for (j, item) in items.iter().enumerate() {
                        out.push((format!("{label} document {i} item {j}"), item.clone()));
                    }
                    continue;
                }
                out.push((format!("{label} document {i}"), value));
            }
        }
    }
    Ok(out)
}

/// Is this document a Kubernetes List wrapper (`kubectl get -o yaml`)? Matches
/// the generic `kind: List` and any `*List` (e.g. `ReplicationSourceList`).
fn is_list(value: &serde_json::Value) -> bool {
    value.get("items").is_some_and(|v| v.is_array())
        && value
            .get("kind")
            .and_then(|v| v.as_str())
            .is_some_and(|k| k == "List" || k.ends_with("List"))
}

fn read_file(path: &Path) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|source| CliError::LocalIo {
        what: format!("reading {}", path.display()),
        source,
    })
}

/// Parse VolSync ReplicationSource/Destination objects from `paths`. Non-VolSync
/// documents (e.g. a whole app manifest) are skipped; `metadata.namespace` wins,
/// falling back to `fallback_ns`.
pub fn parse_volsync(paths: &[PathBuf], fallback_ns: &str) -> Result<Vec<RawVolsync>, CliError> {
    let mut out = Vec::new();
    for (label, value) in collect_documents(paths)? {
        if value.get("apiVersion").and_then(|v| v.as_str()) != Some("volsync.backube/v1alpha1") {
            continue;
        }
        let kind = match value.get("kind").and_then(|v| v.as_str()) {
            Some("ReplicationSource") => VolsyncKind::Source,
            Some("ReplicationDestination") => VolsyncKind::Destination,
            _ => continue,
        };
        let namespace = value
            .pointer("/metadata/namespace")
            .and_then(|v| v.as_str())
            .unwrap_or(fallback_ns)
            .to_string();
        let name = value
            .pointer("/metadata/name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CliError::MigrationInput {
                what: format!("{label} is a VolSync object with no metadata.name"),
                fix: "every object needs a name".into(),
            })?
            .to_string();
        let spec = value
            .get("spec")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        out.push(RawVolsync {
            kind,
            namespace,
            name,
            spec,
        });
    }
    Ok(out)
}

/// Parse plaintext Secret YAML from `paths` into a `(namespace, name) → Secret`
/// map. `stringData` is folded into `data` so the emit-* code (which reads
/// `secret.data`) is untouched.
pub fn parse_secrets(
    paths: &[PathBuf],
    fallback_ns: &str,
) -> Result<BTreeMap<(String, String), Secret>, CliError> {
    let mut map = BTreeMap::new();
    for (label, value) in collect_documents(paths)? {
        if value.get("kind").and_then(|v| v.as_str()) != Some("Secret") {
            continue;
        }
        let mut secret: Secret =
            serde_json::from_value(value).map_err(|e| CliError::MigrationInput {
                what: format!("{label} is not a valid Secret: {e}"),
                fix: "check the Secret's shape".into(),
            })?;
        // Fold stringData into data so downstream `secret.data` reads see it.
        if let Some(string_data) = secret.string_data.take() {
            let data = secret.data.get_or_insert_with(BTreeMap::new);
            for (k, v) in string_data {
                data.entry(k).or_insert_with(|| ByteString(v.into_bytes()));
            }
        }
        let namespace = secret
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| fallback_ns.to_string());
        let name = secret
            .metadata
            .name
            .clone()
            .ok_or_else(|| CliError::MigrationInput {
                what: format!("{label} is a Secret with no metadata.name"),
                fix: "every Secret needs a name".into(),
            })?;
        map.insert((namespace, name), secret);
    }
    Ok(map)
}

/// Write `(filename, content)` pairs into `dir` (created if needed). Each file
/// is written to a sibling `.part` and renamed on success so a failure never
/// leaves a half-written manifest. Without `force`, refuses to overwrite any
/// existing target (protects hand-authored GitOps files).
pub async fn write_files(
    dir: &Path,
    files: &[(String, String)],
    force: bool,
) -> Result<(), CliError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|source| CliError::LocalIo {
            what: format!("creating directory {}", dir.display()),
            source,
        })?;
    if !force {
        let mut existing = Vec::new();
        for (name, _) in files {
            if tokio::fs::try_exists(dir.join(name)).await.unwrap_or(false) {
                existing.push(name.clone());
            }
        }
        if !existing.is_empty() {
            return Err(CliError::MigrationInput {
                what: format!(
                    "--out-dir {} already contains: {}",
                    dir.display(),
                    existing.join(", ")
                ),
                fix: "pass --force to overwrite, or choose an empty directory".into(),
            });
        }
    }
    for (name, content) in files {
        let target = dir.join(name);
        let part = dir.join(format!("{name}.part"));
        tokio::fs::write(&part, content)
            .await
            .map_err(|source| CliError::LocalIo {
                what: format!("writing {}", part.display()),
                source,
            })?;
        tokio::fs::rename(&part, &target)
            .await
            .map_err(|source| CliError::LocalIo {
                what: format!("renaming {} to {}", part.display(), target.display()),
                source,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kopiur-io-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.yaml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn parse_volsync_keeps_only_volsync_and_preserves_namespace() {
        let path = write_temp(
            "mixed",
            "\
apiVersion: apps/v1
kind: Deployment
metadata: { name: ignore-me, namespace: media }
---
apiVersion: volsync.backube/v1alpha1
kind: ReplicationSource
metadata: { name: app, namespace: media }
spec:
  kopia:
    repository: vs-kopia
---
apiVersion: volsync.backube/v1alpha1
kind: ReplicationDestination
metadata: { name: app-dst, namespace: db }
spec:
  restic:
    repository: vs-restic
",
        );
        let parsed = parse_volsync(&[path], "fallback").unwrap();
        assert_eq!(parsed.len(), 2, "{parsed:?}");
        assert_eq!(parsed[0].kind, VolsyncKind::Source);
        assert_eq!(parsed[0].namespace, "media");
        assert_eq!(parsed[0].name, "app");
        // The spec block round-trips through the Value path.
        assert_eq!(parsed[0].spec["kopia"]["repository"], "vs-kopia");
        assert_eq!(parsed[1].kind, VolsyncKind::Destination);
        assert_eq!(parsed[1].namespace, "db");
    }

    #[test]
    fn parse_volsync_unwraps_a_kubectl_get_list() {
        // `kubectl get replicationsource -o yaml` wraps items in a List.
        let path = write_temp(
            "list",
            "\
apiVersion: v1
kind: List
items:
  - apiVersion: volsync.backube/v1alpha1
    kind: ReplicationSource
    metadata: { name: a, namespace: media }
    spec: { kopia: { repository: r1 } }
  - apiVersion: volsync.backube/v1alpha1
    kind: ReplicationSource
    metadata: { name: b, namespace: media }
    spec: { kopia: { repository: r2 } }
",
        );
        let parsed = parse_volsync(&[path], "fallback").unwrap();
        assert_eq!(parsed.len(), 2, "{parsed:?}");
        assert_eq!(parsed[0].name, "a");
        assert_eq!(parsed[1].name, "b");
    }

    #[test]
    fn parse_volsync_falls_back_to_namespace_when_missing() {
        let path = write_temp(
            "nons",
            "\
apiVersion: volsync.backube/v1alpha1
kind: ReplicationSource
metadata: { name: app }
spec: { restic: { repository: r } }
",
        );
        let parsed = parse_volsync(&[path], "the-fallback").unwrap();
        assert_eq!(parsed[0].namespace, "the-fallback");
    }

    #[test]
    fn parse_volsync_reports_malformed_document() {
        let path = write_temp("bad", "this: : : not yaml\n  - [");
        let err = parse_volsync(&[path], "ns").unwrap_err();
        assert!(err.to_string().contains("not valid YAML/JSON"), "{err}");
    }

    #[test]
    fn parse_secrets_folds_stringdata_into_data() {
        let path = write_temp(
            "stringdata",
            "\
apiVersion: v1
kind: Secret
metadata: { name: vs-kopia, namespace: media }
stringData:
  KOPIA_REPOSITORY: s3://bucket/app
  KOPIA_PASSWORD: hunter2
",
        );
        let map = parse_secrets(&[path], "fallback").unwrap();
        let secret = map
            .get(&("media".to_string(), "vs-kopia".to_string()))
            .expect("secret keyed by ns/name");
        let data = secret.data.as_ref().expect("stringData folded into data");
        assert_eq!(
            String::from_utf8(data["KOPIA_REPOSITORY"].0.clone()).unwrap(),
            "s3://bucket/app"
        );
        assert_eq!(
            String::from_utf8(data["KOPIA_PASSWORD"].0.clone()).unwrap(),
            "hunter2"
        );
        assert!(secret.string_data.is_none());
    }

    #[tokio::test]
    async fn write_files_refuses_overwrite_without_force() {
        let dir = std::env::temp_dir().join("kopiur-io-test-overwrite");
        let _ = std::fs::remove_dir_all(&dir);
        let files = vec![("app.yaml".to_string(), "first".to_string())];
        write_files(&dir, &files, false).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("app.yaml")).unwrap(),
            "first"
        );

        // Second write without --force must refuse and leave the file intact.
        let files2 = vec![("app.yaml".to_string(), "second".to_string())];
        let err = write_files(&dir, &files2, false).await.unwrap_err();
        assert!(err.to_string().contains("already contains"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.join("app.yaml")).unwrap(),
            "first"
        );

        // With --force it overwrites, and leaves no .part behind.
        write_files(&dir, &files2, true).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("app.yaml")).unwrap(),
            "second"
        );
        assert!(!dir.join("app.yaml.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
