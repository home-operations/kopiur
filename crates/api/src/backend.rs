//! Storage backends for a kopia repository.
//!
//! ADR-0003 §3.1: `Backend` is a `#[serde(tag = "kind")]` enum. This is the
//! load-bearing example of the ADR's type-safety thesis — a deserialized
//! `Backend` is *always exactly one* variant, so the "exactly one backend block"
//! rule that predecessor drafts enforced with a JSON-schema `oneOf` + webhook
//! check becomes a compile-time invariant. The webhook still validates *content*
//! (bucket names, credential reachability) but cannot receive a multi-variant value.

use crate::common::{SecretRef, TlsConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Credentials for a cloud object-store backend with an IAM plane (S3 / Azure / GCS).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendAuth {
    /// Secret holding the backend's static access credentials (mutually exclusive with `workloadIdentity`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    /// Use a cloud-federated ServiceAccount instead of static keys (mutually exclusive with `secretRef`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_identity: Option<WorkloadIdentity>,
}

/// Cloud workload-identity binding: the mover runs as a federated `ServiceAccount`, not static keys.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadIdentity {
    /// Name of the `ServiceAccount` the mover pod runs as, resolved in the Job's own namespace.
    pub service_account_name: String,
}

/// Credentials for a backend **without** a cloud IAM plane (B2, SFTP, WebDAV): static Secret only.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretAuth {
    /// Secret holding the backend's access credentials, read by well-known keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
}

/// The discriminated backend union (`backend: { s3: {...} }`); exactly one variant by construction.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Backend {
    /// Amazon S3 or any S3-compatible object store (MinIO, RustFS, Ceph RGW, …).
    S3(S3Backend),
    /// Azure Blob Storage.
    Azure(AzureBackend),
    /// Google Cloud Storage.
    Gcs(GcsBackend),
    /// Backblaze B2.
    B2(B2Backend),
    /// A local filesystem path, backed by a PVC the operator mounts into the mover.
    Filesystem(FilesystemBackend),
    /// SFTP server.
    Sftp(SftpBackend),
    /// WebDAV endpoint.
    WebDav(WebDavBackend),
    /// Any rclone remote (kopia shells out to `rclone`), broadening reach to
    /// providers without a native kopia backend.
    Rclone(RcloneBackend),
    /// Google Drive via kopia's native `gdrive` provider (service-account JSON).
    /// kopia marks this provider experimental / not maintained, and a native
    /// gdrive repository is not interchangeable with an rclone-backed Drive remote.
    Gdrive(GdriveBackend),
}

impl Backend {
    /// Stable discriminant string for status/metrics/printcolumns.
    ///
    /// Returns the variant's PascalCase name, independent of the camelCase wire
    /// key (`backend: { s3: ... }` deserializes to [`Backend::S3`], whose
    /// `kind_str()` is `"S3"`).
    ///
    /// ```
    /// use kopiur_api::backend::{Backend, FilesystemBackend};
    ///
    /// let b = Backend::Filesystem(FilesystemBackend {
    ///     path: "/repo".into(),
    ///     volume: None,
    /// });
    /// assert_eq!(b.kind_str(), "Filesystem");
    ///
    /// // The wire key is camelCase, but the discriminant stays PascalCase.
    /// let s3: Backend = serde_json::from_value(serde_json::json!({
    ///     "s3": { "bucket": "my-backups" }
    /// }))
    /// .unwrap();
    /// assert_eq!(s3.kind_str(), "S3");
    /// ```
    pub fn kind_str(&self) -> &'static str {
        match self {
            Backend::S3(_) => "S3",
            Backend::Azure(_) => "Azure",
            Backend::Gcs(_) => "Gcs",
            Backend::B2(_) => "B2",
            Backend::Filesystem(_) => "Filesystem",
            Backend::Sftp(_) => "Sftp",
            Backend::WebDav(_) => "WebDav",
            Backend::Rclone(_) => "Rclone",
            Backend::Gdrive(_) => "Gdrive",
        }
    }
}

/// S3 / S3-compatible object-store backend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct S3Backend {
    /// Bucket holding the kopia repository.
    pub bucket: String,
    /// Key prefix under the bucket, letting several repositories share one bucket
    /// (e.g. `clusters/prod/`). Empty/absent means the bucket root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// S3 endpoint host. Omit for AWS; set it for MinIO/RustFS/other
    /// S3-compatible stores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// S3 region. Required by AWS and some compatible providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Access credentials (Secret ref / workload identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<BackendAuth>,
    /// TLS overrides for self-signed CAs or HTTP-only endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

/// Azure Blob Storage backend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AzureBackend {
    /// Blob container holding the kopia repository.
    pub container: String,
    /// Blob-name prefix within the container; empty/absent means the container root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Storage-account name (when not inferred from credentials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_account: Option<String>,
    /// Access credentials (Secret ref / workload identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<BackendAuth>,
}

/// Google Cloud Storage backend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GcsBackend {
    /// GCS bucket holding the kopia repository.
    pub bucket: String,
    /// Object-name prefix within the bucket; empty/absent means the bucket root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Access credentials (service-account key Secret / workload identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<BackendAuth>,
}

/// Backblaze B2 backend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct B2Backend {
    /// B2 bucket holding the kopia repository.
    pub bucket: String,
    /// Object-name prefix within the bucket; empty/absent means the bucket root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Access credentials (application key ID/key Secret); Secret-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SecretAuth>,
}

/// Local-filesystem backend: kopia writes the repository to a path inside the mover pod.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemBackend {
    /// Mount path inside the mover pod where kopia writes the repository (e.g. `/repo`).
    pub path: String,
    /// What backs `path`: a PVC or an inline NFS export; absent for a path already on the node/image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<RepoVolume>,
}

/// What backs a filesystem repository's mount path (a PVC or an inline NFS export).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RepoVolume {
    /// A `PersistentVolumeClaim` mounted read-write at the repo path.
    Pvc(PvcVolume),
    /// An inline NFS export mounted directly (no PVC).
    Nfs(NfsVolume),
}

impl RepoVolume {
    /// Stable discriminant string for status/metrics.
    pub fn kind_str(&self) -> &'static str {
        match self {
            RepoVolume::Pvc(_) => "Pvc",
            RepoVolume::Nfs(_) => "Nfs",
        }
    }
}

/// A `PersistentVolumeClaim` mounted into the mover pod.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PvcVolume {
    /// Name of the `PersistentVolumeClaim` to mount (in the mover's namespace).
    pub name: String,
}

/// An inline NFS export mounted directly into the mover pod — no PVC, no StorageClass.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NfsVolume {
    /// NFS server hostname or IP (e.g. `nas.lan` or `expanse.internal`).
    pub server: String,
    /// Exported path on the NFS server (e.g. `/export/kopia` or `/mnt/eros/Media`).
    pub path: String,
}

/// SFTP backend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SftpBackend {
    /// SFTP server hostname or IP.
    pub host: String,
    /// Remote path on the server that holds the kopia repository.
    pub path: String,
    /// TCP port; defaults to 22 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// SSH username to connect as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Credentials (SSH private key / known-hosts) sourced from a Secret; Secret-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SecretAuth>,
}

/// WebDAV backend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebDavBackend {
    /// WebDAV collection URL holding the kopia repository.
    pub url: String,
    /// HTTP basic-auth credentials sourced from a Secret; Secret-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SecretAuth>,
}

/// rclone-remote backend; kopia shells out to `rclone` so any rclone-supported provider is reachable.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RcloneBackend {
    /// rclone path in `remote:path` form (the remote name must exist in the
    /// supplied rclone config).
    pub remote_path: String,
    /// Secret holding the `rclone.conf` that defines the remote referenced by
    /// `remote_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_secret_ref: Option<SecretRef>,
    /// How long kopia waits for its embedded `rclone serve` to come up before
    /// failing the connect, as a Go duration (e.g. `2m`). kopia's default is
    /// `15s`; raise it for slow remotes whose repository metadata/indexes load
    /// through the rclone/WebDAV bridge and take longer than the default budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout: Option<String>,
}

/// Google Drive backend using kopia's native `gdrive` provider.
///
/// kopia marks this provider experimental / not maintained, so prefer a native
/// object store where one is available. A native gdrive repository is not
/// interchangeable with an rclone-backed Drive remote — the two lay out data
/// differently and cannot read each other.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GdriveBackend {
    /// Google Drive folder ID that holds the kopia repository.
    pub folder_id: String,
    /// Secret holding the Google service-account JSON used to reach the folder,
    /// read by the well-known key `KOPIA_GDRIVE_CREDENTIALS`. Absent means kopia
    /// falls back to ambient credentials (`GOOGLE_APPLICATION_CREDENTIALS` or
    /// instance metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_secret_ref: Option<SecretRef>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::from_yaml;

    #[test]
    fn gdrive_backend_round_trips() {
        let b: Backend = from_yaml(
            r#"
gdrive:
  folderId: 0ABCDEF
  credentialsSecretRef: { name: gdrive-sa }
"#,
        );
        match &b {
            Backend::Gdrive(g) => {
                assert_eq!(g.folder_id, "0ABCDEF");
                assert_eq!(
                    g.credentials_secret_ref.as_ref().map(|r| r.name.as_str()),
                    Some("gdrive-sa")
                );
            }
            other => panic!("expected gdrive, got {other:?}"),
        }
        assert_eq!(b.kind_str(), "Gdrive");
        // Externally tagged, camelCase — the wire key is the variant, not `kind`.
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["gdrive"]["folderId"], "0ABCDEF");
    }

    #[test]
    fn rclone_startup_timeout_round_trips() {
        let b: Backend = from_yaml(
            r#"
rclone:
  remotePath: "remote:bucket"
  startupTimeout: 2m
"#,
        );
        let Backend::Rclone(r) = &b else {
            panic!("expected rclone, got {b:?}")
        };
        assert_eq!(r.remote_path, "remote:bucket");
        assert_eq!(r.startup_timeout.as_deref(), Some("2m"));

        // Absent startupTimeout stays None and is omitted on the wire.
        let bare: Backend = from_yaml("rclone: { remotePath: \"remote:bucket\" }");
        let Backend::Rclone(r) = &bare else {
            panic!("expected rclone")
        };
        assert!(r.startup_timeout.is_none());
        let json = serde_json::to_value(&bare).unwrap();
        assert!(json["rclone"].get("startupTimeout").is_none());
    }
}
