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
    /// Tuning knobs for the underlying `kopia repository sync-to` invocation
    /// (issue #216). `None` reproduces today's behavior: sequential copy
    /// (`--parallel` unset), additive sync (no `--delete`), kopia's own
    /// `--must-exist`/`--times`/`--update` defaults, and no throughput cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncOptions>,
}

/// Tuning knobs for `kopia repository sync-to` (issue #216): copy parallelism,
/// destination pruning, and the blob-sync tri-states/throughput caps kopia
/// exposes on the command. Every field's `None`/`false` reproduces kopia's own
/// default, so an absent `sync` block is exactly today's behavior. Pure scalars
/// (no k8s-openapi embeds), so this derives `Eq` unlike its parent spec.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncOptions {
    /// `--parallel`: number of concurrent blob-copy workers (kopia default `1` —
    /// sequential, the root cause of #216's multi-week seed times to R2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--delete`: prune destination-only blobs so the mirror is an exact copy
    /// (kopia default `false` — additive sync, never removes destination
    /// content). Named `deleteExtra`, not kopia's bare `delete`: a `delete: true`
    /// key on backup-adjacent YAML is dangerously ambiguous at a glance.
    ///
    /// CAUTION: with this `true`, blobs present at the destination but absent
    /// from the source are deleted on every run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete_extra: bool,
    /// `--[no-]must-exist`: fail the sync instead of initializing the
    /// destination's repository-format blob (kopia default `false` — sync-to may
    /// create the destination layout on first run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub must_exist: Option<bool>,
    /// `--[no-]times`: synchronize blob modification times to the destination,
    /// when the destination backend supports it (kopia default `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<bool>,
    /// `--[no-]update`: update blobs already present at the destination when the
    /// source copy is newer (kopia default `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<bool>,
    /// `--max-download-speed`: cap read throughput from the source, in
    /// bytes/sec (kopia default: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_download_speed_bytes_per_second: Option<i64>,
    /// `--max-upload-speed`: cap write throughput to the destination, in
    /// bytes/sec (kopia default: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_upload_speed_bytes_per_second: Option<i64>,
}

/// Lifecycle phase of a replication.
///
/// ```
/// use kopiur_api::repository_replication::RepositoryReplicationPhase as P;
///
/// assert_eq!(serde_json::to_value(P::Suspended).unwrap(), "Suspended");
/// // An unrecognized phase from a newer operator decodes instead of erroring.
/// let p: P = serde_json::from_value(serde_json::json!("Verifying")).unwrap();
/// assert_eq!(p, P::Unknown("Verifying".into()));
/// assert_eq!(serde_json::to_value(&p).unwrap(), "Verifying");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Default)]
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
    /// A phase string this build does not recognize (newer operator, or legacy
    /// stored data). Decode-compat only — hidden from the CRD schema, never
    /// produced by this build, never a success.
    Unknown(String),
}

impl RepositoryReplicationPhase {
    /// Whether this phase is the **decode sentinel** — a value the running build
    /// cannot interpret, kept verbatim by [`Unknown`](Self::Unknown) instead of
    /// failing the whole typed `list()`/watch (#359, defect 3).
    ///
    /// Same narrow contract as [`SnapshotPhase::is_unknown`](crate::SnapshotPhase::is_unknown):
    /// `true` means only "this string is not a phase this binary knows", never
    /// "unusual" or "not one I handle". A canonical variant added to this enum
    /// later is by definition **not** the sentinel, which is why the `match` is
    /// written out exhaustively rather than left as a `matches!` — the compiler,
    /// not a reviewer, is what forces the new variant to answer.
    ///
    /// The reconciler uses it for its entry-time version-skew warning
    /// (`io::warn_unreadable_phase`). Unlike the other drivers, it does not
    /// promptly overwrite what it cannot read: no branch reads this phase, and
    /// the terminal stamp comes from the mover at the end of a run, so an
    /// unreadable value simply persists — up to a whole schedule interval. The
    /// warning is therefore repeated every pass rather than emitted once: the
    /// log is where the skew is visible.
    ///
    /// ```
    /// use kopiur_api::RepositoryReplicationPhase;
    ///
    /// assert!(RepositoryReplicationPhase::Unknown("Verifying".into()).is_unknown());
    /// assert!(!RepositoryReplicationPhase::Pending.is_unknown());
    /// assert!(!RepositoryReplicationPhase::Replicating.is_unknown());
    /// assert!(!RepositoryReplicationPhase::Suspended.is_unknown());
    /// ```
    pub fn is_unknown(&self) -> bool {
        match self {
            Self::Unknown(_) => true,
            Self::Pending
            | Self::Replicating
            | Self::Succeeded
            | Self::Failed
            | Self::Suspended => false,
        }
    }
}

crate::common::phase_serde!(
    RepositoryReplicationPhase,
    "Lifecycle phase of a replication."
);

impl crate::common::PhaseLabel for RepositoryReplicationPhase {
    const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Replicating,
        Self::Succeeded,
        Self::Failed,
        Self::Suspended,
    ];
    fn label(&self) -> &str {
        match self {
            Self::Pending => "Pending",
            Self::Replicating => "Replicating",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Suspended => "Suspended",
            Self::Unknown(s) => s,
        }
    }
    fn unknown(raw: String) -> Self {
        Self::Unknown(raw)
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
        // #216: no `sync` block set → the field is entirely absent on the wire,
        // reproducing today's argv exactly (no dormant defaults sneak in).
        assert!(spec.sync.is_none());
        assert!(json.get("sync").is_none());
    }

    #[test]
    fn sync_options_roundtrip_full_block() {
        // #216: every `spec.sync` tuning knob round-trips through the cluster's
        // YAML → serde_json::Value → typed path.
        let yaml = r#"
sourceRef: { name: nas-primary }
destination: { filesystem: { path: /mirror } }
schedule: { cron: "0 5 * * *" }
sync:
  parallel: 8
  deleteExtra: true
  mustExist: false
  times: true
  update: false
  maxDownloadSpeedBytesPerSecond: 1000000
  maxUploadSpeedBytesPerSecond: 500000
"#;
        let spec: RepositoryReplicationSpec = from_yaml(yaml);
        let sync = spec.sync.expect("sync block set");
        assert_eq!(sync.parallel, Some(8));
        assert!(sync.delete_extra);
        assert_eq!(sync.must_exist, Some(false));
        assert_eq!(sync.times, Some(true));
        assert_eq!(sync.update, Some(false));
        assert_eq!(sync.max_download_speed_bytes_per_second, Some(1_000_000));
        assert_eq!(sync.max_upload_speed_bytes_per_second, Some(500_000));

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["sync"]["parallel"], 8);
        assert_eq!(json["sync"]["deleteExtra"], true);
        assert_eq!(json["sync"]["mustExist"], false);
        let reparsed: RepositoryReplicationSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn sync_options_omits_unset_leaf_fields() {
        // A `sync` block that only sets `parallel` must not serialize the other
        // (unset) knobs — `deleteExtra`'s `false` default also skips (Not::not).
        let yaml = r#"
sourceRef: { name: nas-primary }
destination: { filesystem: { path: /mirror } }
schedule: { cron: "0 5 * * *" }
sync:
  parallel: 4
"#;
        let spec: RepositoryReplicationSpec = from_yaml(yaml);
        let json = serde_json::to_value(&spec).unwrap();
        let sync_json = &json["sync"];
        assert_eq!(sync_json["parallel"], 4);
        assert!(sync_json.get("deleteExtra").is_none());
        assert!(sync_json.get("mustExist").is_none());
        assert!(sync_json.get("times").is_none());
        assert!(sync_json.get("update").is_none());
        assert!(sync_json.get("maxDownloadSpeedBytesPerSecond").is_none());
        assert!(sync_json.get("maxUploadSpeedBytesPerSecond").is_none());
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
