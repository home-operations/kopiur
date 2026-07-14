//! The `ClusterRepository` CRD — a cluster-scoped, shared kopia repository
//! operated by a platform team. ADR-0001 §3.2, ADR-0003 §3.2.
//!
//! Same spec surface as `Repository` (backend/encryption/create/moverDefaults/
//! catalog), plus a tenancy gate (`allowedNamespaces`) and per-namespace identity
//! expressions (`identityDefaults`).

use crate::backend::Backend;
use crate::common::{
    CatalogBounds, CreateBehavior, Encryption, IdentityDefaults, MoverDefaults,
    NamespaceDeletePolicy, RepositoryMode, ScheduleDefaults, default_namespace_delete_policy,
    default_repository_mode,
};
use crate::maintenance::RepositoryMaintenanceSpec;
use crate::repository::{
    BootstrapSpec, CatalogStatus, RepositoryHealthSpec, RepositoryHealthStatus, RepositoryPhase,
    StorageStats,
};
use crate::server::{ClusterServerSpec, ServerStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, LabelSelector};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A cluster-scoped kopia repository referenceable from allow-listed namespaces.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "ClusterRepository",
    status = "ClusterRepositoryStatus",
    shortname = "kopiacrepo",
    category = "kopiur",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Backend","type":"string","jsonPath":".status.backend"}"#,
    printcolumn = r#"{"name":"Namespaces","type":"integer","jsonPath":".status.allowedNamespaceCount"}"#,
    printcolumn = r#"{"name":"Server","type":"string","jsonPath":".status.server.endpoint"}"#,
    printcolumn = r#"{"name":"IndexBlobs","type":"integer","jsonPath":".status.storageStats.indexBlobCount","priority":1}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
// §7/§15: create-time-immutability transition rules (apiserver + CI), same set as
// the namespaced Repository — and like it, `encryption` (the password Secret reference)
// is deliberately NOT locked (kopia fixes only the resolved value; a rename with identical
// content must pass). Each `create.*` leaf is `has()`-guarded: CEL field access on an
// absent optional key raises a "no such key" error that fails the whole rule (→ 422 on
// *every* update, wedging the controller's finalizer/status writes), so we compare
// presence first and only dereference when set — see the namespaced `Repository` for the
// full rationale.
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "!has(self.create) || !has(oldSelf.create) || (has(self.create.splitter) == has(oldSelf.create.splitter) && (!has(self.create.splitter) || self.create.splitter == oldSelf.create.splitter))", "message": "create.splitter is immutable after creation"},
    {"rule": "!has(self.create) || !has(oldSelf.create) || (has(self.create.hash) == has(oldSelf.create.hash) && (!has(self.create.hash) || self.create.hash == oldSelf.create.hash))", "message": "create.hash is immutable after creation"},
    {"rule": "!has(self.create) || !has(oldSelf.create) || (has(self.create.encryption) == has(oldSelf.create.encryption) && (!has(self.create.encryption) || self.create.encryption == oldSelf.create.encryption))", "message": "create.encryption is immutable after creation"},
    {"rule": "!has(self.create) || !has(oldSelf.create) || (has(self.create.ecc) == has(oldSelf.create.ecc) && (!has(self.create.ecc) || self.create.ecc == oldSelf.create.ecc))", "message": "create.ecc is immutable after creation"}
]))]
#[serde(rename_all = "camelCase")]
pub struct ClusterRepositorySpec {
    /// Exactly one storage backend.
    pub backend: Backend,
    /// Repository password (a Secret reference that must carry an explicit `namespace`).
    pub encryption: Encryption,
    /// What to do when the repository does not yet exist (absent means it must already exist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<CreateBehavior>,
    /// Tuning for the one-shot bootstrap Job that connects/creates an object-store
    /// repository the operator cannot reach in-process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<BootstrapSpec>,
    /// Base mover configuration inherited by every mover this repository spawns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover_defaults: Option<MoverDefaults>,
    /// Scheduling defaults (e.g. `timezone`) inherited by consumers that don't set
    /// their own equivalent field — verification, replication, and maintenance
    /// schedules today; set once here instead of repeating it on every cron.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_defaults: Option<ScheduleDefaults>,
    /// Bounds materialization of `origin: discovered` `Snapshot` CRs from the kopia catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogBounds>,
    /// Which namespaces are permitted to reference this repository.
    pub allowed_namespaces: AllowedNamespaces,
    /// Identity defaults (CEL `*Expr`) applied when consumers don't override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_defaults: Option<IdentityDefaults>,
    /// Optional kopia web-UI server (the target `namespace` is required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ClusterServerSpec>,
    /// Maintenance control; `maintenance.namespace` selects where the owned `Maintenance` CR lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<RepositoryMaintenanceSpec>,
    /// What happens to this repository's snapshots when a consuming namespace is deleted.
    #[serde(default = "default_namespace_delete_policy")]
    #[schemars(default = "default_namespace_delete_policy")]
    pub on_namespace_delete: NamespaceDeletePolicy,
    /// Repository-owner gate for projecting credential Secrets into a foreign consumer namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_projection: Option<ClusterRepoCredentialProjection>,
    /// Access mode: `ReadWrite` (default) or `ReadOnly` (serves restores only).
    #[serde(default = "default_repository_mode")]
    #[schemars(default = "default_repository_mode")]
    pub mode: RepositoryMode,
    /// Pause this cluster repository: skip connect/bootstrap and maintenance projection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspend: bool,
    /// Repository health thresholds (tunes the index-blob-count warning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<RepositoryHealthSpec>,
}

/// The repository-owner side of credential projection on a `ClusterRepository`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRepoCredentialProjection {
    /// When `true`, the owner permits projecting this repository's credential Secret(s) into a consumer namespace.
    #[serde(default)]
    pub allowed: bool,
}

/// The set of namespaces permitted to reference this `ClusterRepository` (exactly one of).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AllowedNamespaces {
    /// Explicit namespace names.
    List(Vec<String>),
    /// Match namespaces by label.
    Selector(LabelSelector),
    /// Allow all namespaces (must be `true`).
    All(bool),
}

impl AllowedNamespaces {
    /// Stable discriminant string for status/metrics.
    ///
    /// ```
    /// use kopiur_api::cluster_repository::AllowedNamespaces;
    ///
    /// let ns = AllowedNamespaces::List(vec!["production".into(), "staging".into()]);
    /// assert_eq!(ns.kind_str(), "List");
    /// assert_eq!(AllowedNamespaces::All(true).kind_str(), "All");
    /// ```
    pub fn kind_str(&self) -> &'static str {
        match self {
            AllowedNamespaces::List(_) => "List",
            AllowedNamespaces::Selector(_) => "Selector",
            AllowedNamespaces::All(_) => "All",
        }
    }
}

/// Observed state of a `ClusterRepository`; mirrors `RepositoryStatus` plus `allowedNamespaceCount`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRepositoryStatus {
    /// Current lifecycle phase (shared with `Repository`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RepositoryPhase>,
    /// `metadata.generation` of the `spec` last reconciled; drives staleness detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// `resourceVersion` of the password Secret observed at the last connect attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_credential_version: Option<String>,
    /// Kopia repository unique ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_id: Option<String>,
    /// Mirror of `spec.backend` discriminant for the print column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Number of namespaces currently resolved by `spec.allowedNamespaces`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_namespace_count: Option<i64>,
    /// Repository size and snapshot counts from the last catalog scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_stats: Option<StorageStats>,
    /// Catalog-materialization status (discovered-backup count, last refresh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogStatus>,
    /// Resolved kopia server endpoint/auth, pinned by the reconciler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerStatus>,
    /// Last reverify-request token honored from a `Snapshot`'s re-probe nudge
    /// (RFC3339); the loop guard that keeps each request a one-shot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reverify_at: Option<String>,
    /// Backend health-probe state (`spec.health.probe`), when enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<RepositoryHealthStatus>,
    /// Standard Kubernetes conditions (e.g. `Connected`, `MaintenanceOwned`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn cluster_repository_crd_metadata_is_correct() {
        // `crd()` exercises schema generation; mis-encoded enums panic here.
        let crd = ClusterRepository::crd();
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
        assert_eq!(crd.spec.names.kind, "ClusterRepository");
        // Cluster-scoped: this is the load-bearing assertion vs. namespaced CRDs.
        assert_eq!(crd.spec.scope, "Cluster");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
    }

    #[test]
    fn cluster_repository_roundtrip_matches_adr_shape() {
        // Mirrors ADR-0001 §3.2 / §5.2.
        let yaml = r#"
backend:
  s3:
    bucket: org-kopia-repo
    prefix: ""
    endpoint: s3.us-east-1.amazonaws.com
    region: us-east-1
    auth:
      secretRef:
        name: kopia-platform-creds
        namespace: kopia-system
encryption:
  passwordSecretRef:
    name: kopia-platform-creds
    namespace: kopia-system
    key: KOPIA_PASSWORD
create:
  enabled: true
  encryption: AES256-GCM-HMAC-SHA256
allowedNamespaces:
  list: [production, staging, billing]
identityDefaults:
  hostnameExpr: "namespace"
  usernameExpr: "namespace + '-' + policyName"
catalog:
  retain:
    perIdentity: 50
    maxAgeDays: 60
  refreshInterval: 5m
  fallbackNamespace: kopia-system
"#;
        let spec: ClusterRepositorySpec = from_yaml(yaml);
        match &spec.backend {
            Backend::S3(s3) => assert_eq!(s3.bucket, "org-kopia-repo"),
            other => panic!("expected S3 backend, got {}", other.kind_str()),
        }
        match &spec.allowed_namespaces {
            AllowedNamespaces::List(ns) => {
                assert_eq!(ns, &["production", "staging", "billing"]);
            }
            other => panic!("expected List, got {}", other.kind_str()),
        }
        let id = spec.identity_defaults.as_ref().expect("identityDefaults");
        assert_eq!(id.hostname_expr.as_deref(), Some("namespace"));
        assert_eq!(
            id.username_expr.as_deref(),
            Some("namespace + '-' + policyName")
        );
        assert_eq!(
            spec.catalog.as_ref().unwrap().fallback_namespace.as_deref(),
            Some("kopia-system")
        );

        let json = serde_json::to_value(&spec).expect("serialize");
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn allowed_namespaces_selector_variant() {
        let v: AllowedNamespaces = from_yaml(
            "selector:\n  matchLabels: { kopiur.home-operations.com/tier: enterprise }\n",
        );
        assert_eq!(v.kind_str(), "Selector");
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(
            json["selector"]["matchLabels"]["kopiur.home-operations.com/tier"],
            "enterprise"
        );
    }

    #[test]
    fn allowed_namespaces_all_variant() {
        let v: AllowedNamespaces = from_yaml("all: true\n");
        assert_eq!(v.kind_str(), "All");
        assert_eq!(serde_json::to_value(&v).unwrap()["all"], true);
    }

    #[test]
    fn allowed_namespaces_unknown_variant_is_rejected() {
        let value: serde_json::Value = serde_yaml::from_str("everyone: true\n").unwrap();
        assert!(serde_json::from_value::<AllowedNamespaces>(value).is_err());
    }

    #[test]
    fn schedule_defaults_timezone_round_trips() {
        let yaml = r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }
allowedNamespaces: { all: true }
scheduleDefaults:
  timezone: America/New_York
"#;
        let spec: ClusterRepositorySpec = from_yaml(yaml);
        assert_eq!(
            spec.schedule_defaults
                .as_ref()
                .and_then(|d| d.timezone.as_deref()),
            Some("America/New_York")
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["scheduleDefaults"]["timezone"], "America/New_York");
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent scheduleDefaults stays None and is elided (no stored-object churn).
        let bare: ClusterRepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }\n\
             allowedNamespaces: { all: true }\n",
        );
        assert!(bare.schedule_defaults.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("scheduleDefaults")
                .is_none(),
            "absent scheduleDefaults must be elided"
        );
    }

    #[test]
    fn identity_defaults_cluster_round_trips() {
        // `identityDefaults.cluster` is the multi-cluster shared-repo identity
        // suffix (M1): present, it round-trips through serde like any other field.
        let yaml = r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }
allowedNamespaces: { all: true }
identityDefaults:
  cluster: east
"#;
        let spec: ClusterRepositorySpec = from_yaml(yaml);
        let id = spec.identity_defaults.as_ref().expect("identityDefaults");
        assert_eq!(id.cluster.as_deref(), Some("east"));
        assert!(id.hostname_expr.is_none());
        assert!(id.username_expr.is_none());

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["identityDefaults"]["cluster"], "east");
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent `cluster` stays None and is elided (no stored-object churn) —
        // exercised independently of the existing `identityDefaults` back-compat
        // fixture in `cluster_repository_roundtrip_matches_adr_shape`, which is
        // left untouched.
        let bare: ClusterRepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }\n\
             allowedNamespaces: { all: true }\n\
             identityDefaults:\n  hostnameExpr: namespace\n",
        );
        let id = bare.identity_defaults.as_ref().expect("identityDefaults");
        assert!(id.cluster.is_none());
        assert!(
            serde_json::to_value(&bare).unwrap()["identityDefaults"]
                .get("cluster")
                .is_none(),
            "absent identityDefaults.cluster must be elided"
        );
    }

    #[test]
    fn catalog_foreign_snapshots_round_trips_on_cluster_repository() {
        use crate::common::ForeignSnapshots;

        let yaml = r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }
allowedNamespaces: { all: true }
identityDefaults:
  cluster: east
catalog:
  fallbackNamespace: kopia-system
  foreignSnapshots: Fallback
"#;
        let spec: ClusterRepositorySpec = from_yaml(yaml);
        assert_eq!(
            spec.catalog.as_ref().and_then(|c| c.foreign_snapshots),
            Some(ForeignSnapshots::Fallback)
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["catalog"]["foreignSnapshots"], "Fallback");
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        let yaml_ignore = r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }
allowedNamespaces: { all: true }
identityDefaults:
  cluster: east
catalog:
  foreignSnapshots: Ignore
"#;
        let spec: ClusterRepositorySpec = from_yaml(yaml_ignore);
        assert_eq!(
            spec.catalog.as_ref().and_then(|c| c.foreign_snapshots),
            Some(ForeignSnapshots::Ignore)
        );

        // Absent stays None and is elided.
        let bare: ClusterRepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }\n\
             allowedNamespaces: { all: true }\n\
             catalog: {}\n",
        );
        assert!(bare.catalog.as_ref().unwrap().foreign_snapshots.is_none());
        assert!(
            serde_json::to_value(&bare).unwrap()["catalog"]
                .get("foreignSnapshots")
                .is_none(),
            "absent catalog.foreignSnapshots must be elided"
        );
    }

    #[test]
    fn catalog_foreign_snapshots_unknown_variant_is_rejected() {
        let value: serde_json::Value = serde_yaml::from_str("foreignSnapshots: Delete\n").unwrap();
        assert!(serde_json::from_value::<crate::common::CatalogBounds>(value).is_err());
    }

    #[test]
    fn catalog_foreign_snapshots_schema_carries_no_default() {
        // Per the conventions doc (§4a): the effective default (`Ignore`) is
        // context-dependent (coupled to identityDefaults.cluster), so no
        // schemars `default` is emitted — the field must stay `—` in the
        // generated field reference, not silently materialize `Ignore` for
        // every repository regardless of whether it has a cluster identity.
        let crd = ClusterRepository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let prop = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["catalog"]["properties"]["foreignSnapshots"];
        assert!(
            prop.get("default").is_none(),
            "catalog.foreignSnapshots must NOT carry a schema default: {prop}"
        );
        // Sanity: the property itself is present, with the expected enum values.
        assert_eq!(prop["enum"].as_array().map(|a| a.len()), Some(2), "{prop}");
    }
}
