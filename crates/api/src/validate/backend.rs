use super::*;
use crate::error::{ValidationError, ValidationResult};

/// A DNS-1123 subdomain (the shape of every Kubernetes object name): non-empty,
/// ≤253 chars, lowercase alphanumerics / `-` / `.`, starting and ending
/// alphanumeric. The structural schema can't express it, so the webhook does.
/// `field` names where the value appears, for an actionable message.
pub fn validate_dns1123_name(value: &str, field: &str) -> ValidationResult {
    if value.is_empty() {
        return Err(ValidationError::MissingRequiredField {
            field: field.to_string(),
        });
    }
    let valid_len = value.len() <= 253;
    let valid_chars = value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    let valid_edges = value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value.ends_with(|c: char| c.is_ascii_alphanumeric());
    if valid_len && valid_chars && valid_edges {
        Ok(())
    } else {
        Err(ValidationError::InvalidFieldValue {
            field: field.to_string(),
            reason: format!(
                "must be a DNS-1123 subdomain — lowercase alphanumerics, '-' or '.', \
                 starting and ending with an alphanumeric, at most 253 characters \
                 (got {value:?})"
            ),
        })
    }
}

/// A cloud-IAM backend's `auth` block is well-formed: **exactly one** of
/// `secretRef` or `workloadIdentity` when `auth` is present (both are `Option`
/// because the forms share the `auth` key, so it's a webhook check — the same
/// shape as [`validate_source`]). An absent/empty `auth` is legal: the
/// well-known keys may ride the encryption-password Secret, and an empty block
/// means exactly that. A workload-identity `serviceAccountName` must be a valid
/// object name, or the mover Job would be rejected by the API server later with
/// a far less actionable message. `context` names the backend (e.g.
/// `"s3 backend"`) for the message.
pub fn validate_backend_auth(
    auth: &crate::backend::BackendAuth,
    context: &str,
) -> ValidationResult {
    if auth.secret_ref.is_some() && auth.workload_identity.is_some() {
        return Err(ValidationError::MutuallyExclusive {
            a: "auth.secretRef".to_string(),
            b: "auth.workloadIdentity".to_string(),
            context: context.to_string(),
        });
    }
    if let Some(wi) = &auth.workload_identity {
        validate_dns1123_name(
            &wi.service_account_name,
            &format!("{context} auth.workloadIdentity.serviceAccountName"),
        )?;
    }
    Ok(())
}

/// Validate backend *content* the structural schema can't express: the
/// inline-NFS volume on a `Filesystem` backend, the `secretRef` XOR
/// `workloadIdentity` rule on the cloud-IAM backends, and Azure's
/// workload-identity prerequisites. Exhaustive `match` so a new `Backend`
/// variant must be considered here before it compiles.
pub fn validate_backend(backend: &crate::backend::Backend) -> ValidationResult {
    use crate::backend::{Backend, RepoVolume};
    match backend {
        Backend::Filesystem(fs) => match &fs.volume {
            Some(RepoVolume::Nfs(nfs)) => validate_nfs_volume(nfs, "filesystem repo"),
            Some(RepoVolume::Pvc(_)) | None => Ok(()),
        },
        Backend::S3(s) => match &s.auth {
            Some(auth) => validate_backend_auth(auth, "s3 backend"),
            None => Ok(()),
        },
        Backend::Azure(a) => match &a.auth {
            Some(auth) => {
                validate_backend_auth(auth, "azure backend")?;
                // kopia's `--storage-account` is a required flag, and with
                // workload identity there is no Secret to deliver it via the
                // AZURE_STORAGE_ACCOUNT env var (the azure-workload-identity
                // webhook injects only tenant/client/token-file). It must be in
                // the spec, or every mover run fails at kopia flag parsing.
                if auth.workload_identity.is_some() && a.storage_account.is_none() {
                    return Err(ValidationError::InvalidFieldValue {
                        field: "azure backend storageAccount".to_string(),
                        reason: "required with auth.workloadIdentity: the \
                                 azure-workload-identity webhook injects the tenant, \
                                 client id, and federated token, but not the storage \
                                 account — set spec.backend.azure.storageAccount"
                            .to_string(),
                    });
                }
                Ok(())
            }
            None => Ok(()),
        },
        Backend::Gcs(g) => match &g.auth {
            Some(auth) => validate_backend_auth(auth, "gcs backend"),
            None => Ok(()),
        },
        Backend::Rclone(r) => {
            // kopia's `--rclone-startup-timeout` takes a Go duration; reject a
            // malformed value at admission instead of failing every connect.
            if let Some(t) = &r.startup_timeout
                && crate::duration::parse_go_duration(t).is_none()
            {
                return Err(ValidationError::InvalidFieldValue {
                    field: "rclone backend startupTimeout".to_string(),
                    reason: format!("must be a Go duration like \"30s\" or \"2m\" (got {t:?})"),
                });
            }
            Ok(())
        }
        Backend::Gdrive(g) => {
            if g.folder_id.trim().is_empty() {
                return Err(ValidationError::MissingRequiredField {
                    field: "gdrive backend folderId".to_string(),
                });
            }
            Ok(())
        }
        Backend::B2(_) | Backend::Sftp(_) | Backend::WebDav(_) => Ok(()),
    }
}

/// A `RepositoryReplication`'s source/destination auth pair is safe to run in
/// **one** mover pod. The replicate pod's environment carries the static side's
/// credential Secret (`envFrom`); for a same-kind S3 or Azure pair where exactly
/// one side uses workload identity, the workload-identity side's credential
/// chain reads those same env vars (minio-go's `EnvAWS`; kopia's env-bound azure
/// flags) and would silently authenticate as the *other* side — wrong identity,
/// plausibly wrong permissions, no error. Rejected at admission instead. GCS
/// mixed pairs are safe (the static side's key travels as a `--credentials-file`
/// path, not ambient env). Both-workload-identity pairs must name the same
/// ServiceAccount — a pod runs as exactly one.
pub fn validate_replication_auth(
    source: &crate::backend::Backend,
    destination: &crate::backend::Backend,
) -> ValidationResult {
    use crate::creds::{WorkloadIdentityCloud, backend_workload_identity};
    let src_wi = backend_workload_identity(source);
    let dst_wi = backend_workload_identity(destination);
    match (src_wi, dst_wi) {
        (None, None) => Ok(()),
        (Some((a, _)), Some((b, _))) => {
            if a.service_account_name == b.service_account_name {
                Ok(())
            } else {
                Err(ValidationError::InvalidFieldValue {
                    field: "destination auth.workloadIdentity.serviceAccountName".to_string(),
                    reason: format!(
                        "the replication mover is one pod and runs as exactly one \
                         ServiceAccount, but the source repository federates as \
                         {:?} and the destination as {:?} — point both at the same \
                         ServiceAccount (with IAM access to both stores)",
                        a.service_account_name, b.service_account_name
                    ),
                })
            }
        }
        (Some((_, wi_cloud)), None) | (None, Some((_, wi_cloud))) => {
            let static_side = if src_wi.is_some() {
                destination
            } else {
                source
            };
            let conflicts = match wi_cloud {
                WorkloadIdentityCloud::S3 => {
                    matches!(static_side, crate::backend::Backend::S3(_))
                }
                WorkloadIdentityCloud::Azure => {
                    matches!(static_side, crate::backend::Backend::Azure(_))
                }
                // GCS static keys travel as a --credentials-file path, never
                // ambient env, so they cannot leak into the ADC chain.
                WorkloadIdentityCloud::Gcs => false,
            };
            if conflicts {
                Err(ValidationError::InvalidFieldValue {
                    field: "destination backend auth".to_string(),
                    reason: "a same-kind source/destination pair cannot mix \
                             workloadIdentity with a static credential Secret: the \
                             replication mover's environment carries the static \
                             side's keys, and the workload-identity side's ambient \
                             credential chain would silently pick them up and \
                             authenticate as the wrong identity — use \
                             workloadIdentity on both sides (one ServiceAccount \
                             with IAM access to both stores) or static Secrets on \
                             both"
                        .to_string(),
                })
            } else {
                Ok(())
            }
        }
    }
}

/// A `RepositoryReplication`'s **destination** credential Secret is reachable from
/// the mover Job. The replicate Job runs in the CR's own namespace and loads the
/// destination backend's keys via `envFrom`, which is namespace-local — a Secret
/// in another namespace can never be read. `RepositoryReplication` deliberately has
/// no `credentialProjection`, so an out-of-namespace destination `auth.secretRef`
/// is a dead reference the Job would hang on (`CreateContainerConfigError`).
/// Reject it at admission with an actionable message instead. An absent
/// `namespace` means "same namespace as the CR" and is always legal; a workload-
/// identity or filesystem destination carries no auth Secret and is unaffected.
/// `cr_namespace` is the replication CR's own namespace.
pub fn validate_replication_destination_secret_namespace(
    destination: &crate::backend::Backend,
    cr_namespace: &str,
) -> ValidationResult {
    let Some(secret_ref) = crate::creds::backend_auth_secret_ref(destination) else {
        return Ok(());
    };
    match secret_ref.namespace.as_deref() {
        Some(ns) if ns != cr_namespace => Err(ValidationError::InvalidFieldValue {
            field: "destination backend auth.secretRef.namespace".to_string(),
            reason: format!(
                "the replication mover Job runs in namespace {cr_namespace:?} and loads the \
                 destination credentials via envFrom, which is namespace-local, but the Secret \
                 {name:?} is pinned to namespace {ns:?} — the Job could never read it. \
                 RepositoryReplication does not project credentials across namespaces; put the \
                 destination Secret in {cr_namespace:?} (omit `namespace`, or set it to \
                 {cr_namespace:?})",
                name = secret_ref.name,
            ),
        }),
        _ => Ok(()),
    }
}
