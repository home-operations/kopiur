//! The `ClusterRepository` CRD — a cluster-scoped, shared kopia repository
//! operated by a platform team. ADR-0001 §3.2, ADR-0003 §3.2.
//!
//! Same spec surface as `Repository` (backend/encryption/create/moverDefaults/
//! catalog), plus a tenancy gate (`allowedNamespaces`) and per-namespace identity
//! expressions (`identityDefaults`).

use crate::backend::Backend;
use crate::common::{
    CatalogBounds, ConcurrencySpec, CreateBehavior, DeletionProtectionSpec, Encryption,
    IdentityDefaults, MoverDefaults, NamespaceDeletePolicy, RepositoryMode, ScheduleDefaults,
    default_namespace_delete_policy, default_repository_mode,
};
use crate::maintenance::RepositoryMaintenanceSpec;
use crate::repository::{
    BootstrapSpec, CatalogStatus, ObservedRepositoryParameters, RepositoryHealthSpec,
    RepositoryHealthStatus, RepositoryParameters, RepositoryPhase, StorageStats,
};
use crate::seed::{SeedSpec, SeedStatus};
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
    /// Initialize this repository from an existing replica on its FIRST
    /// bootstrap (issue #380) — a disaster-recovery entry point.
    ///
    /// Armed only while the repository has never been initialized
    /// (`status.uniqueId` unset) **and** the mover's connect reports the backend
    /// uninitialized; on an already-initialized repository it is a documented
    /// no-op (`Seeded=True`, reason `AlreadyInitialized`), so it is safe to
    /// leave standing in a GitOps manifest. When armed it also replaces
    /// `spec.create`'s fallback: the repository is seeded or the bootstrap
    /// fails, never silently created empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<SeedSpec>,
    /// Tuning for the bootstrap/discovery mover Job (`<name>-discovery`) that
    /// connects/creates an object-store repository the operator cannot reach
    /// in-process (and re-runs for catalog re-scans).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<BootstrapSpec>,
    /// Base mover configuration inherited by every mover this repository spawns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover_defaults: Option<MoverDefaults>,
    /// Scheduling defaults (`timezone`, `jitter`) inherited by consumers that don't
    /// set their own equivalent field — backup, verification, replication, and
    /// maintenance schedules; set once here instead of repeating it on every cron.
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
    /// Mass-deletion circuit breaker for this repository's Snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_protection: Option<DeletionProtectionSpec>,
    /// Concurrency limits for mover Jobs against this repository (absent = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<ConcurrencySpec>,
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
    /// Mutable kopia repository parameters, re-applied on bootstrap whenever they drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<RepositoryParameters>,
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
    /// What the last seed attempt did (`spec.seed`); absent on a repository that
    /// was never seeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<SeedStatus>,
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
    /// The kopia repository parameters actually observed at the last bootstrap. Compare
    /// against `spec.parameters` to see whether a declared value landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<ObservedRepositoryParameters>,
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
    fn deletion_protection_threshold_schema_default_matches_the_constant() {
        let crd = ClusterRepository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let spec = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
        assert_eq!(
            spec["properties"]["deletionProtection"]["properties"]["threshold"]["default"],
            serde_json::json!(crate::consts::DEFAULT_MASS_DELETION_THRESHOLD)
        );
        assert_eq!(
            crate::consts::effective_mass_deletion_threshold(None),
            crate::consts::DEFAULT_MASS_DELETION_THRESHOLD
        );
    }

    #[test]
    fn health_probe_schema_defaults_mirror_the_repository_twin() {
        // #345: the ClusterRepository CRD must carry the same context-free
        // probe defaults as the namespaced Repository — default-ON and the
        // breaker (`Degrade`) as the onFailure default. The resolvers
        // (`RepositoryHealthProbeSpec::enabled` / `effective_on_failure`) are
        // shared, so only the schema emission needs its own guard here.
        let crd = ClusterRepository::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let probe = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["health"]["properties"]["probe"]["properties"];
        assert_eq!(
            probe["enabled"]["default"],
            serde_json::json!(crate::consts::DEFAULT_HEALTH_PROBE_ENABLED)
        );
        assert_eq!(probe["onFailure"]["default"], serde_json::json!("Degrade"));
        assert_eq!(probe["interval"]["default"], serde_json::json!("30m"));
        assert_eq!(probe["failureThreshold"]["default"], serde_json::json!(3));
    }

    #[test]
    fn deletion_protection_round_trips_on_cluster_repository() {
        let yaml = r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }
allowedNamespaces: { all: true }
deletionProtection:
  threshold: 0
"#;
        let spec: ClusterRepositorySpec = from_yaml(yaml);
        assert_eq!(
            spec.deletion_protection.as_ref().and_then(|d| d.threshold),
            Some(0)
        );
        assert_eq!(
            crate::consts::effective_mass_deletion_threshold(spec.deletion_protection.as_ref()),
            0,
            "Some(0) must pass through as the disable sentinel"
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["deletionProtection"]["threshold"], 0);
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent stays None and is elided.
        let bare: ClusterRepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }\n\
             allowedNamespaces: { all: true }\n",
        );
        assert!(bare.deletion_protection.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("deletionProtection")
                .is_none(),
            "absent deletionProtection must be elided"
        );
    }

    #[test]
    fn concurrency_max_concurrent_jobs_emits_no_schema_default() {
        // The `ClusterRepository` mirror of the `Repository` guard. Both kinds
        // embed the SAME `ConcurrencySpec`, but each generates its own CRD schema,
        // so a schemars `default` added to the shared struct would materialize on
        // both — and a guard that only watched one kind would let it through on the
        // other. §4a: absent ≡ 0 ≡ unlimited, so a server-side default would stamp
        // `{maxConcurrentJobs: 0}` onto every stored cluster repository for no
        // behavior change at all.
        let json = serde_json::to_value(ClusterRepository::crd()).unwrap();
        let spec = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
        let field = &spec["properties"]["concurrency"]["properties"]["maxConcurrentJobs"];
        assert!(
            !field.is_null(),
            "the field itself must exist in the schema: {spec}"
        );
        assert!(
            field.get("default").is_none(),
            "maxConcurrentJobs must NOT carry a schema default: {field}"
        );
        assert_eq!(crate::consts::effective_max_concurrent_jobs(None), None);
    }

    #[test]
    fn concurrency_round_trips_on_cluster_repository() {
        use crate::common::ConcurrencySpec;
        use crate::consts::effective_max_concurrent_jobs;

        let head = "backend: { filesystem: { path: /repo } }\n\
                    encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }\n\
                    allowedNamespaces: { all: true }\n";

        let spec: ClusterRepositorySpec =
            from_yaml(&format!("{head}concurrency:\n  maxConcurrentJobs: 4\n"));
        assert_eq!(
            spec.concurrency,
            Some(ConcurrencySpec {
                max_concurrent_jobs: Some(4)
            })
        );
        assert_eq!(
            effective_max_concurrent_jobs(spec.concurrency.as_ref()).map(|n| n.get()),
            Some(4)
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["concurrency"]["maxConcurrentJobs"], 4);
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // `0` is the explicit "unlimited" spelling; it round-trips as itself
        // rather than being normalized away, and resolves to uncapped.
        let zero: ClusterRepositorySpec =
            from_yaml(&format!("{head}concurrency:\n  maxConcurrentJobs: 0\n"));
        assert_eq!(
            zero.concurrency.and_then(|c| c.max_concurrent_jobs),
            Some(0)
        );
        assert_eq!(
            effective_max_concurrent_jobs(zero.concurrency.as_ref()),
            None
        );

        // Absent stays None and is elided (no stored-object churn).
        let bare: ClusterRepositorySpec = from_yaml(head);
        assert!(bare.concurrency.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("concurrency")
                .is_none(),
            "absent concurrency must be elided"
        );
    }

    #[test]
    fn schedule_defaults_jitter_round_trips_on_cluster_repository() {
        let head = "backend: { filesystem: { path: /repo } }\n\
                    encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }\n\
                    allowedNamespaces: { all: true }\n";
        let spec: ClusterRepositorySpec = from_yaml(&format!(
            "{head}scheduleDefaults:\n  timezone: America/New_York\n  jitter: 10m\n"
        ));
        let sd = spec.schedule_defaults.as_ref().expect("scheduleDefaults");
        assert_eq!(sd.jitter.as_deref(), Some("10m"));
        assert_eq!(sd.timezone.as_deref(), Some("America/New_York"));
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["scheduleDefaults"]["jitter"], "10m");
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // A scheduleDefaults with only a timezone elides jitter entirely.
        let tz_only: ClusterRepositorySpec = from_yaml(&format!(
            "{head}scheduleDefaults:\n  timezone: America/New_York\n"
        ));
        assert!(
            tz_only
                .schedule_defaults
                .as_ref()
                .and_then(|d| d.jitter.as_ref())
                .is_none()
        );
        assert!(
            serde_json::to_value(&tz_only).unwrap()["scheduleDefaults"]
                .get("jitter")
                .is_none(),
            "absent jitter must be elided"
        );
    }

    #[test]
    fn mover_defaults_pod_metadata_round_trips_on_cluster_repository() {
        let spec: ClusterRepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }\n\
             allowedNamespaces: { all: true }\n\
             moverDefaults:\n\
             \x20 podLabels: { kueue.x-k8s.io/queue-name: backups }\n\
             \x20 podAnnotations: { sidecar.istio.io/inject: \"false\" }\n",
        );
        let md = spec.mover_defaults.as_ref().expect("moverDefaults");
        assert_eq!(
            md.pod_labels
                .as_ref()
                .and_then(|m| m.get("kueue.x-k8s.io/queue-name"))
                .map(String::as_str),
            Some("backups")
        );
        assert_eq!(
            md.pod_annotations
                .as_ref()
                .and_then(|m| m.get("sidecar.istio.io/inject"))
                .map(String::as_str),
            Some("false")
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
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
    fn catalog_adoption_round_trips_on_cluster_repository() {
        use crate::common::SnapshotAdoption;

        let yaml = r#"
backend: { filesystem: { path: /repo } }
encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }
allowedNamespaces: { all: true }
catalog:
  adoption: Ignore
"#;
        let spec: ClusterRepositorySpec = from_yaml(yaml);
        assert_eq!(
            spec.catalog.as_ref().and_then(|c| c.adoption),
            Some(SnapshotAdoption::Ignore)
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["catalog"]["adoption"], "Ignore");
        let reparsed: ClusterRepositorySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent stays None and is elided.
        let bare: ClusterRepositorySpec = from_yaml(
            "backend: { filesystem: { path: /repo } }\n\
             encryption: { passwordSecretRef: { name: s, namespace: kopia-system } }\n\
             allowedNamespaces: { all: true }\n\
             catalog: {}\n",
        );
        assert!(bare.catalog.as_ref().unwrap().adoption.is_none());
        assert!(
            serde_json::to_value(&bare).unwrap()["catalog"]
                .get("adoption")
                .is_none(),
            "absent catalog.adoption must be elided"
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
