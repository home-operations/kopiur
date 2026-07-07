//! The `RepositoryReplication` CRD — mirror a repository's blobs to a second
//! backend on a schedule (ADR-0005 §13(d)). The one net-new CRD: it is the "2" in
//! 3-2-1 backup, wrapping `kopia repository sync-to`.
//!
//! It is **namespaced** (it lives alongside its source repository, mirroring
//! `Maintenance`) and references either a namespaced `Repository` or a cluster-scoped
//! `ClusterRepository` via a [`RepositoryRef`]. The controller schedules a per-slot
//! mover Job (croner + deterministic jitter, single-flight, repo-ready gate,
//! transition-guarded status) exactly like `Maintenance`.

use crate::backend::Backend;
use crate::common::{CronSpec, MoverSpec, RepositoryRef};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Mirror a source repository's blobs to a destination backend on a schedule (`kopia repository sync-to`).
///
/// Not `Eq`: `mover` transitively embeds k8s-openapi types.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "RepositoryReplication",
    plural = "repositoryreplications",
    namespaced,
    status = "RepositoryReplicationStatus",
    shortname = "kopiarepl",
    category = "kopiur",
    printcolumn = r#"{"name":"Source","type":"string","jsonPath":".spec.sourceRef.name"}"#,
    printcolumn = r#"{"name":"Destination","type":"string","jsonPath":".status.destinationBackend"}"#,
    printcolumn = r#"{"name":"Schedule","type":"string","jsonPath":".spec.schedule.cron"}"#,
    printcolumn = r#"{"name":"Last","type":"date","jsonPath":".status.lastReplicated"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryReplicationSpec {
    /// Reference to the `Repository` or `ClusterRepository` to mirror from.
    pub source_ref: RepositoryRef,
    /// The backend to mirror to; must differ from the source's backend (webhook-enforced).
    ///
    /// `kopia repository sync-to` is a blob-level copy: the destination inherits the
    /// source repository's format and encryption password verbatim, so there is no
    /// separate destination password to configure. The destination backend's own
    /// access credentials (e.g. S3 keys) ride its `auth.secretRef`, which — like the
    /// source's — must live in this CR's namespace.
    pub destination: Backend,
    /// Cron and deterministic jitter for the replication runs.
    pub schedule: CronSpec,
    /// Mover (Job pod) overrides for the replication run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover: Option<MoverSpec>,
    /// Pause this replication; a suspended replication runs no syncs (default `false`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspend: bool,
}

/// Lifecycle phase of a replication.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum RepositoryReplicationPhase {
    /// Admitted, not yet run (also the default).
    #[default]
    Pending,
    /// A replication mover Job is in flight.
    Replicating,
    /// The most recent replication completed successfully (idle until the next slot).
    Succeeded,
    /// The most recent replication run failed; see conditions.
    Failed,
    /// Suspended via `spec.suspend`.
    Suspended,
}

impl crate::common::PhaseLabel for RepositoryReplicationPhase {
    const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Replicating,
        Self::Succeeded,
        Self::Failed,
        Self::Suspended,
    ];
    fn label(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Replicating => "Replicating",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Suspended => "Suspended",
        }
    }
}

/// Observed state of a `RepositoryReplication`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryReplicationStatus {
    /// Current lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RepositoryReplicationPhase>,
    /// `metadata.generation` last reconciled, for staleness detection / kstatus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// The destination backend kind, for the `DESTINATION` print column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_backend: Option<String>,
    /// RFC3339 timestamp of the most recent successful replication run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replicated: Option<String>,
    /// RFC3339 timestamp of the next scheduled replication run (cron + jitter, pinned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_scheduled_at: Option<String>,
    /// Bytes replicated by the last successful run (best-effort from kopia output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replicated_bytes: Option<i64>,
    /// Blobs replicated by the last successful run (best-effort).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replicated_blobs: Option<i64>,
    /// Standard Kubernetes conditions (`Ready`, `Reconciling`, `Stalled`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::RepositoryKind;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn repository_replication_crd_metadata_is_correct() {
        let crd = RepositoryReplication::crd();
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
        assert_eq!(crd.spec.names.kind, "RepositoryReplication");
        assert_eq!(crd.spec.names.plural, "repositoryreplications");
        assert_eq!(crd.spec.scope, "Namespaced");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
    }

    #[test]
    fn repository_replication_roundtrip() {
        // sourceRef + destination (externally-tagged backend) + schedule.
        let yaml = r#"
sourceRef:
  kind: Repository
  name: nas-primary
destination:
  s3:
    bucket: offsite-mirror
    region: us-east-1
    auth:
      secretRef:
        name: offsite-creds
schedule:
  cron: "0 5 * * *"
  jitter: 1h
suspend: false
"#;
        let spec: RepositoryReplicationSpec = from_yaml(yaml);
        assert_eq!(spec.source_ref.kind, RepositoryKind::Repository);
        assert_eq!(spec.source_ref.name, "nas-primary");
        // Destination is exactly one backend variant (the type guarantees it).
        match &spec.destination {
            Backend::S3(s3) => assert_eq!(s3.bucket, "offsite-mirror"),
            other => panic!("expected S3 destination, got {}", other.kind_str()),
        }
        assert_eq!(spec.schedule.cron, "0 5 * * *");
        assert_eq!(spec.schedule.jitter.as_deref(), Some("1h"));
        assert!(!spec.suspend);

        let json = serde_json::to_value(&spec).expect("serialize");
        // Externally tagged destination backend.
        assert_eq!(json["destination"]["s3"]["bucket"], "offsite-mirror");
        let reparsed: RepositoryReplicationSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn minimal_true_mirror_spec_omits_optionals() {
        // A true mirror reuses the source password (sync-to is a blob copy), so there
        // is no destination-encryption knob to set.
        let yaml = r#"
sourceRef: { name: nas-primary }
destination: { filesystem: { path: /mirror } }
schedule: { cron: "0 6 * * 0" }
"#;
        let spec: RepositoryReplicationSpec = from_yaml(yaml);
        // sourceRef.kind defaults to Repository.
        assert_eq!(spec.source_ref.kind, RepositoryKind::Repository);
        let json = serde_json::to_value(&spec).unwrap();
        assert!(json.get("suspend").is_none());
    }

    #[test]
    fn stored_cr_with_removed_destination_encryption_still_deserializes() {
        // The field was removed (sync-to is a blob copy; it never did anything). A CR
        // stored while the field existed must still round-trip: serde silently drops
        // the now-unknown key (no `deny_unknown_fields`), so existing objects keep
        // reconciling instead of failing to decode.
        let yaml = r#"
sourceRef: { name: nas-primary }
destination: { filesystem: { path: /mirror } }
destinationEncryption:
  passwordSecretRef: { name: legacy-creds, key: KOPIA_PASSWORD }
schedule: { cron: "0 6 * * 0" }
"#;
        let spec: RepositoryReplicationSpec = from_yaml(yaml);
        assert_eq!(spec.source_ref.name, "nas-primary");
        assert_eq!(spec.schedule.cron, "0 6 * * 0");
    }

    #[test]
    fn replication_phase_all_covers_every_variant() {
        use crate::common::PhaseLabel;
        let labels: Vec<&str> = RepositoryReplicationPhase::ALL
            .iter()
            .map(|p| p.label())
            .collect();
        assert_eq!(RepositoryReplicationPhase::ALL.len(), 5);
        assert!(labels.iter().all(|l| !l.is_empty()));
    }

    #[test]
    fn status_roundtrips() {
        let status: RepositoryReplicationStatus = from_yaml(
            "phase: Succeeded\ndestinationBackend: s3\nlastReplicated: 2026-06-09T05:00:00Z\nlastReplicatedBytes: 12345\n",
        );
        assert_eq!(status.phase, Some(RepositoryReplicationPhase::Succeeded));
        assert_eq!(status.destination_backend.as_deref(), Some("s3"));
        assert_eq!(status.last_replicated_bytes, Some(12345));
        let json = serde_json::to_value(&status).unwrap();
        let reparsed: RepositoryReplicationStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);
    }
}
