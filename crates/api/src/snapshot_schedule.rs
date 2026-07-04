//! The `SnapshotSchedule` CRD — *when* a backup runs. Creates `Snapshot` CRs on a
//! cron schedule in the `SnapshotPolicy`'s namespace. ADR-0001 §3.5, ADR-0003 §4.4.
//!
//! ```
//! use kopiur_api::{SnapshotScheduleSpec, ConcurrencyPolicy};
//!
//! // The cluster path: YAML -> JSON value -> typed (never serde_yaml -> typed).
//! let spec: SnapshotScheduleSpec = serde_json::from_value(serde_json::json!({
//!     "policyRef": { "name": "postgres-data" },
//!     "schedule": { "cron": "H 2 * * *", "jitter": "30m" },
//! }))
//! .unwrap();
//! assert_eq!(spec.policy_ref.as_ref().unwrap().name, "postgres-data");
//! // GitOps-friendly defaults: no immediate fire, not suspended, Forbid overlap.
//! assert!(!spec.schedule.run_on_create);
//! assert!(!spec.schedule.suspend);
//! assert_eq!(spec.schedule.concurrency_policy, ConcurrencyPolicy::Forbid);
//! ```

use crate::common::PolicyRef;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, LabelSelector};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Cron schedule that fires `Snapshot` CRs from a `SnapshotPolicy`.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "SnapshotSchedule",
    namespaced,
    status = "SnapshotScheduleStatus",
    shortname = "kopiasched",
    category = "kopiur",
    printcolumn = r#"{"name":"Config","type":"string","jsonPath":".spec.policyRef.name"}"#,
    printcolumn = r#"{"name":"Schedule","type":"string","jsonPath":".spec.schedule.cron"}"#,
    printcolumn = r#"{"name":"Suspended","type":"boolean","jsonPath":".spec.schedule.suspend"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
// §10/§15: exactly one of policyRef / policySelector (apiserver + CI validation,
// complementing the webhook validator). Both optional at the type level.
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "[has(self.policyRef), has(self.policySelector)].filter(x, x).size() == 1",
    "message": "exactly one of policyRef or policySelector"
}]))]
#[serde(rename_all = "camelCase")]
pub struct SnapshotScheduleSpec {
    /// The single `SnapshotPolicy` this schedule invokes; mutually exclusive with `policySelector`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_ref: Option<PolicyRef>,
    /// Label selector fanning out over `SnapshotPolicy` objects; mutually exclusive with `policyRef`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_selector: Option<LabelSelector>,
    /// Cron, jitter, timezone, and concurrency for the firing cadence.
    pub schedule: ScheduleSpec,
    /// Maximum number of failed `Snapshot` CRs from this schedule to retain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_jobs_history_limit: Option<u32>,
}

/// serde/schemars `default` for [`ScheduleSpec::run_on_create`] — `false`
/// (ADR-0005 §1). A named fn so it backs BOTH `#[serde(default = ...)]` and
/// `#[schemars(default = ...)]`, which is what makes schemars 1 emit the OpenAPI
/// `default:` in the generated CRD schema.
fn default_run_on_create() -> bool {
    false
}

/// serde/schemars `default` for [`ScheduleSpec::concurrency_policy`] — `Forbid`
/// (ADR-0005 §1). Same dual-attribute pattern as [`default_run_on_create`].
fn default_concurrency_policy() -> ConcurrencyPolicy {
    ConcurrencyPolicy::Forbid
}

/// Cron schedule with deterministic jitter, timezone, and concurrency controls.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSpec {
    /// Cron expression with Jenkins-style `H` substitution.
    pub cron: String,
    /// Deterministic jitter (Go-style duration), derived from `(scheduleUID, slot)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter: Option<String>,
    /// IANA timezone the cron is evaluated in; absent uses the controller's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Whether to fire immediately on create (default `false`).
    #[serde(default = "default_run_on_create")]
    #[schemars(default = "default_run_on_create")]
    pub run_on_create: bool,
    /// Skip future firings while true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspend: bool,
    /// How to handle a firing while a prior run is still in flight (default `Forbid`).
    #[serde(default = "default_concurrency_policy")]
    #[schemars(default = "default_concurrency_policy")]
    pub concurrency_policy: ConcurrencyPolicy,
    /// If a slot is missed by more than this many seconds, skip it instead of firing late.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_deadline_seconds: Option<i64>,
}

/// What to do when a previous run is still in flight. Closed enum, default `Forbid`. ADR §4.1 (G5/G18).
///
/// ```
/// use kopiur_api::ConcurrencyPolicy;
///
/// // The safe default: never let runs pile up.
/// assert_eq!(ConcurrencyPolicy::default(), ConcurrencyPolicy::Forbid);
/// // Serializes as the bare PascalCase string the CRD schema expects.
/// assert_eq!(
///     serde_json::to_value(ConcurrencyPolicy::Replace).unwrap(),
///     serde_json::json!("Replace"),
/// );
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum ConcurrencyPolicy {
    /// Skip the new run rather than let runs pile up (default).
    #[default]
    Forbid,
    /// Allow the new run to start alongside the in-flight one.
    Allow,
    /// Cancel the in-flight run and start the new one in its place.
    Replace,
}

/// Observed state of a `SnapshotSchedule`: pinned firing slots and failure run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotScheduleStatus {
    /// The `metadata.generation` this status reflects, for staleness detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Most recent firing (cron + jitter, pinned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_schedule: Option<ScheduleRef>,
    /// The next firing slot the controller has computed (cron + jitter, pinned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_schedule: Option<ScheduleRef>,
    /// The most recent firing whose `Snapshot` succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_schedule: Option<ScheduleRef>,
    /// Count of back-to-back failed runs; resets on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<i64>,
    /// Standard Kubernetes conditions surfacing schedule health.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// A pinned schedule slot and (optionally) the `Snapshot` it created.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRef {
    /// The RFC3339 instant this slot fired (or is scheduled to); also accepts the `scheduledAt` alias.
    #[serde(
        default,
        alias = "scheduledAt",
        skip_serializing_if = "Option::is_none"
    )]
    pub at: Option<String>,
    /// The `Snapshot` CR this slot produced, when one was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_ref: Option<SnapshotReference>,
    /// The IANA timezone the cron was evaluated in when this slot was pinned
    /// (`nextSchedule` only). Recorded so the controller can detect an
    /// effective-timezone change — a `spec.schedule.timezone` edit or a change to
    /// the target repository's `scheduleDefaults.timezone` — and invalidate the
    /// pinned wall-clock slot, recomputing it in the new zone. Absent on legacy
    /// pins written before this field existed (treated as "unchanged").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// A by-name reference to a `Snapshot` CR created by a schedule slot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReference {
    /// The `Snapshot`'s `metadata.name` (same namespace as the schedule).
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn backup_schedule_crd_metadata_is_correct() {
        let crd = SnapshotSchedule::crd();
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
        assert_eq!(crd.spec.names.kind, "SnapshotSchedule");
        assert_eq!(crd.spec.scope, "Namespaced");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
    }

    #[test]
    fn schedule_crd_carries_policy_target_xor_validation() {
        // §10/§15: the spec schema carries the policyRef-XOR-policySelector rule.
        let crd = SnapshotSchedule::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let rules = json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["x-kubernetes-validations"]
            .as_array()
            .expect("spec.x-kubernetes-validations present");
        assert!(rules.iter().any(|r| {
            r["rule"]
                .as_str()
                .is_some_and(|s| s.contains("policySelector"))
        }));
    }

    #[test]
    fn schedule_defaults_carry_static_openapi_defaults_in_crd() {
        // ADR-0005 §1: schedule.runOnCreate (false) and schedule.concurrencyPolicy
        // (Forbid) must carry real schema defaults so they materialize into the
        // stored object / `kubectl explain` and GitOps stops diff-thrashing.
        let crd = SnapshotSchedule::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let schedule = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["schedule"]["properties"];
        assert_eq!(
            schedule["runOnCreate"]["default"], false,
            "runOnCreate must emit `default: false`"
        );
        assert_eq!(
            schedule["concurrencyPolicy"]["default"], "Forbid",
            "concurrencyPolicy must emit `default: Forbid`"
        );
    }

    #[test]
    fn schedule_static_defaults_materialize_and_round_trip() {
        // Both fields parse to their defaults when absent AND serialize (not elided),
        // so the materialized value round-trips.
        let spec: SnapshotScheduleSpec =
            from_yaml("policyRef: { name: pg }\nschedule: { cron: \"H 2 * * *\" }\n");
        assert!(!spec.schedule.run_on_create);
        assert_eq!(spec.schedule.concurrency_policy, ConcurrencyPolicy::Forbid);
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["schedule"]["runOnCreate"], false);
        assert_eq!(json["schedule"]["concurrencyPolicy"], "Forbid");
    }

    #[test]
    fn backup_schedule_roundtrip_matches_adr_shape() {
        // Mirrors ADR-0001 §3.5.
        let yaml = r#"
policyRef:
  name: postgres-data
schedule:
  cron: "H 2 * * *"
  jitter: 30m
  timezone: "America/Los_Angeles"
  runOnCreate: false
  suspend: false
  concurrencyPolicy: Forbid
  startingDeadlineSeconds: 600
failedJobsHistoryLimit: 3
"#;
        let spec: SnapshotScheduleSpec = from_yaml(yaml);
        assert_eq!(spec.policy_ref.as_ref().unwrap().name, "postgres-data");
        assert_eq!(spec.schedule.cron, "H 2 * * *");
        assert_eq!(spec.schedule.jitter.as_deref(), Some("30m"));
        assert_eq!(spec.schedule.concurrency_policy, ConcurrencyPolicy::Forbid);
        assert!(!spec.schedule.run_on_create);
        assert_eq!(spec.failed_jobs_history_limit, Some(3));

        let json = serde_json::to_value(&spec).expect("serialize");
        let reparsed: SnapshotScheduleSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn schedule_defaults_are_gitops_friendly() {
        // Mirrors ADR-0001 §5.1: minimal schedule.
        let spec: SnapshotScheduleSpec = from_yaml(
            "policyRef: { name: postgres-data }\nschedule: { cron: \"H 2 * * *\", jitter: 30m }\n",
        );
        // runOnCreate and suspend default false; concurrency defaults Forbid.
        assert!(!spec.schedule.run_on_create);
        assert!(!spec.schedule.suspend);
        assert_eq!(spec.schedule.concurrency_policy, ConcurrencyPolicy::Forbid);
        // No successfulJobsHistoryLimit exists on the type at all (ADR-0003 §4.4).
    }

    #[test]
    fn concurrency_policy_serializes_to_expected_strings() {
        assert_eq!(
            serde_json::to_value(ConcurrencyPolicy::Forbid).unwrap(),
            "Forbid"
        );
        assert_eq!(
            serde_json::to_value(ConcurrencyPolicy::Allow).unwrap(),
            "Allow"
        );
        assert_eq!(
            serde_json::to_value(ConcurrencyPolicy::Replace).unwrap(),
            "Replace"
        );
        assert_eq!(ConcurrencyPolicy::default(), ConcurrencyPolicy::Forbid);
    }

    #[test]
    fn schedule_status_accepts_both_at_and_scheduled_at() {
        // ADR §3.5 uses `scheduledAt` on lastSchedule and `at` on next/lastSuccessful.
        let status: SnapshotScheduleStatus = from_yaml(
            r#"
lastSchedule:
  scheduledAt: 2026-05-24T02:13:00Z
  snapshotRef: { name: postgres-data-20260524-021300 }
nextSchedule:
  at: 2026-05-25T02:21:00Z
lastSuccessfulSchedule:
  at: 2026-05-24T02:13:00Z
  snapshotRef: { name: postgres-data-20260524-021300 }
consecutiveFailures: 0
"#,
        );
        assert_eq!(
            status.last_schedule.as_ref().unwrap().at.as_deref(),
            Some("2026-05-24T02:13:00Z")
        );
        assert_eq!(
            status.next_schedule.as_ref().unwrap().at.as_deref(),
            Some("2026-05-25T02:21:00Z")
        );
        // Round-trips (serializes back as `at`).
        let json = serde_json::to_value(&status).unwrap();
        let reparsed: SnapshotScheduleStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);
    }

    #[test]
    fn next_schedule_timezone_round_trips() {
        // The pinned-slot timezone (recorded so an effective-timezone change can
        // invalidate the pin) parses from YAML and serializes back unchanged.
        let status: SnapshotScheduleStatus = from_yaml(
            r#"
nextSchedule:
  at: 2026-05-25T09:00:00Z
  timezone: America/Chicago
"#,
        );
        assert_eq!(
            status.next_schedule.as_ref().unwrap().timezone.as_deref(),
            Some("America/Chicago")
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["nextSchedule"]["timezone"], "America/Chicago");
        let reparsed: SnapshotScheduleStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);

        // Absent timezone (legacy pins) stays absent, not `null`.
        let bare: SnapshotScheduleStatus =
            from_yaml("nextSchedule: { at: 2026-05-25T09:00:00Z }\n");
        assert!(bare.next_schedule.as_ref().unwrap().timezone.is_none());
        let bare_json = serde_json::to_value(&bare).unwrap();
        assert!(bare_json["nextSchedule"].get("timezone").is_none());
    }
}
