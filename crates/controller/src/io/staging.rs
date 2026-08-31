//! CSI VolumeSnapshot / clone **staging** for backups (`copyMethod: Snapshot` /
//! `Clone`, ADR §3.3).
//!
//! Instead of mounting the **live** source PVC, a staged backup captures a
//! point-in-time copy and runs the mover against *that*:
//!
//! - **`Snapshot`**: create a CSI `VolumeSnapshot` of the source PVC, wait for it to be
//!   `readyToUse`, then provision a temporary "staged" PVC whose `dataSource` is the
//!   snapshot. The mover mounts the staged PVC.
//! - **`Clone`**: provision the staged PVC directly from the source PVC
//!   (`dataSource: PersistentVolumeClaim`, a CSI volume clone) — no VolumeSnapshot.
//! - **`Direct`** (and any NFS source): no staging — the caller mounts the live source.
//!
//! Staging **decouples the backup from the source node** (the staged PVC is fresh and
//! unheld, so [`super::colocation`] resolves to "no pin") and gives a point-in-time,
//! crash-consistent capture (app-consistency uses `SnapshotPolicy.spec.hooks`).
//!
//! The cluster has no typed Rust binding for `snapshot.storage.k8s.io` (those are CRDs,
//! not core API), so VolumeSnapshots are created/read as [`kube::core::DynamicObject`]s
//! (the same approach the mover uses for status patches). Every **decision** is a pure,
//! unit-tested function ([`decide_class`], [`staged_pvc_size`], [`cleanup_plan`], the
//! builders); the async wrappers do only cluster IO and return a [`StagingOutcome`] the
//! reconciler maps to status/conditions/events — so this module stays free of status
//! side effects (mirroring [`super::colocation`]).

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    PersistentVolume, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod,
    TypedLocalObjectReference, VolumeResourceRequirements,
};
use k8s_openapi::api::storage::v1::StorageClass;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind, ObjectMeta};
use kube::{Api, ResourceExt};

use kopiur_api::{CopyMethod, SnapshotPolicy, StagingSpec};

use super::apply::{FIELD_MANAGER, apply};
use super::child_labels;
use crate::error::{Error, Result};

/// The `snapshot.storage.k8s.io` API group.
pub const SNAPSHOT_GROUP: &str = "snapshot.storage.k8s.io";
/// The well-known annotation marking the default `VolumeSnapshotClass` for a driver.
pub const DEFAULT_CLASS_ANNOTATION: &str = "snapshot.storage.kubernetes.io/is-default-class";

/// `ApiResource` for `VolumeSnapshot` (namespaced).
fn volume_snapshot_ar() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(SNAPSHOT_GROUP, "v1", "VolumeSnapshot"),
        "volumesnapshots",
    )
}

/// `ApiResource` for `VolumeSnapshotClass` (cluster-scoped).
fn volume_snapshot_class_ar() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(SNAPSHOT_GROUP, "v1", "VolumeSnapshotClass"),
        "volumesnapshotclasses",
    )
}

/// `ApiResource` for `VolumeSnapshotContent` (cluster-scoped).
fn volume_snapshot_content_ar() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(SNAPSHOT_GROUP, "v1", "VolumeSnapshotContent"),
        "volumesnapshotcontents",
    )
}

/// [`volume_snapshot_ar`] for the group-staging module.
pub(crate) fn volume_snapshot_ar_pub() -> ApiResource {
    volume_snapshot_ar()
}

/// [`volume_snapshot_content_ar`] for the group-staging module.
pub(crate) fn volume_snapshot_content_ar_pub() -> ApiResource {
    volume_snapshot_content_ar()
}

/// [`fmt_go_duration`] for the group-staging module, which renders the same
/// deadline grammar.
pub(crate) fn fmt_go_duration_pub(d: std::time::Duration) -> String {
    fmt_go_duration(d)
}

fn volume_snapshot_api(client: &kube::Client, ns: &str) -> Api<DynamicObject> {
    Api::namespaced_with(client.clone(), ns, &volume_snapshot_ar())
}

// ---------------------------------------------------------------------------
// Outcome / decision types
// ---------------------------------------------------------------------------

/// The staged source the mover should mount instead of the live PVC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedSource {
    /// Name of the staged PVC (mounted in place of the live source).
    pub pvc_name: String,
    /// Name of the `VolumeSnapshot` backing the stage (`Snapshot` mode; `None` for `Clone`).
    ///
    /// Under `groupBy: VolumeGroupSnapshot` this is the member snapshot the
    /// shared group produced for THIS PVC — resolved by
    /// `group_staging::map_group_members`, never guessed.
    pub volume_snapshot_name: Option<String>,
    /// Name of the shared `VolumeGroupSnapshot` this member staged from, when
    /// the policy asked for a consistency group. Recorded to status so the
    /// group is observable — it has no ownerReferences, so this is how an
    /// operator (and `kubectl kopiur doctor`) can tell which capture a backup
    /// came from.
    pub volume_group_snapshot_name: Option<String>,
    /// The resolved capture method (`Snapshot`/`Clone`) — recorded to status.
    pub copy_method: &'static str,
    /// StorageClass actually written to the staged PVC's spec — the
    /// `spec.staging.storageClassName` override when set, else the source PVC's
    /// class. Recorded to status so a shallow-clone setup is observable.
    pub storage_class_name: Option<String>,
    /// The resolved staging timeout in seconds (`0` = wait indefinitely), pinned
    /// to status so the running-Job staged-PVC bind watchdog never re-resolves a
    /// policy that may have been edited or deleted mid-run.
    pub staging_timeout_seconds: i64,
}

/// What [`resolve_staging`] decided. The reconciler maps this to status/conditions/
/// events — this module performs **no** status side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingOutcome {
    /// No staging — `Direct`, an NFS source, or a multi-PVC selector. Mount the live source.
    NotApplicable,
    /// The stage is provisioned and ready; mount [`StagedSource::pvc_name`].
    Ready(StagedSource),
    /// The stage is not usable yet — requeue (transient). Two waits share this
    /// variant, distinguished by `reason` (both `SourceStaged=False` / `Pending`):
    /// the VolumeSnapshot becoming `readyToUse`
    /// ([`crate::consts::STAGING_WAITING_REASON`]) and the staged PVC binding on an
    /// `Immediate` StorageClass ([`crate::consts::STAGED_PVC_BINDING_REASON`]). A
    /// `status.error` on the VolumeSnapshot lands HERE (not `Failed`) while the
    /// staging deadline has not passed: external-snapshotter sets it transiently
    /// (e.g. a benign 409-conflict retry) and clears it on the next successful
    /// sync, so first sight is never fatal.
    Waiting {
        /// kstatus condition reason (a stable PascalCase token) naming WHICH wait.
        reason: &'static str,
        /// Deterministic-per-object message (embeds the fixed deadline, never
        /// `now()`), so steady-state reconciles don't churn status.
        message: String,
    },
    /// The stage cannot be produced (no snapshot stack / no class / source not
    /// CSI-provisioned / the VolumeSnapshot missed the staging deadline). The reconciler
    /// fails the Snapshot with this `reason` + actionable `message`. **Terminal** for
    /// the Snapshot CR: `Failed` phase → `RunDecision::TerminalFailed` →
    /// `Action::await_change()` — a NEW Snapshot (e.g. the next scheduled run) is how a
    /// retry happens, so the bar for producing this variant is deliberately high.
    Failed {
        /// kstatus condition reason (a stable PascalCase token).
        reason: &'static str,
        /// Actionable what/why/fix message.
        message: String,
    },
}

/// Condition reason: the CSI snapshot stack (snapshot-controller / CRDs) is absent.
pub const REASON_STACK_MISSING: &str = "SnapshotStackMissing";
/// Condition reason: no usable `VolumeSnapshotClass` for the source's driver.
pub const REASON_NO_CLASS: &str = "NoVolumeSnapshotClass";
/// Condition reason: the created `VolumeSnapshot` was still reporting an error when
/// the staging deadline (`spec.staging.timeout`) passed. Never produced on first
/// sight of an error — external-snapshotter errors are transient until proven
/// persistent.
pub const REASON_VS_FAILED: &str = "VolumeSnapshotFailed";
/// Condition reason: the created `VolumeSnapshot` did not become `readyToUse` within
/// the staging deadline (`spec.staging.timeout`) and reported no error — the CSI
/// driver / snapshot-controller is stuck or very slow.
pub const REASON_STAGING_TIMEOUT: &str = "StagingTimedOut";
/// Condition reason: the source PVC isn't CSI-provisioned (no StorageClass), so it
/// can't be snapshotted/cloned.
pub const REASON_SOURCE_NOT_CSI: &str = "SourceNotCSIProvisioned";
/// Condition reason: `spec.staging.storageClassName` names a StorageClass that does
/// not exist.
pub const REASON_STAGED_CLASS_NOT_FOUND: &str = "StagedClassNotFound";
/// Condition reason: `spec.staging.storageClassName` names a StorageClass whose
/// provisioner differs from the source PVC's CSI driver — a VolumeSnapshot restore /
/// volume clone can only be provisioned by the source's own driver, so the staged
/// PVC would never bind. Caught up front instead of as an opaque bind timeout.
pub const REASON_STAGED_CLASS_MISMATCH: &str = "StagedClassMismatch";
/// Condition reason: the staged PVC did not reach `Bound` within the staging
/// deadline (`spec.staging.timeout`) — the CSI restore/clone from the source is
/// still provisioning (e.g. a CephFS full subvolume clone of a small-file-heavy
/// volume) or the class cannot provision it at all. Pre-fix, this window fell
/// under the generic 300 s wedged-pod deadline, whose cleanup deleted the
/// VolumeSnapshot while the restore was still consuming it (the forgejo/CephFS
/// hourly hard-fail).
pub const REASON_STAGED_PVC_BIND_TIMEOUT: &str = "StagedPvcBindTimeout";
/// Condition reason: the staged PVC reports phase `Lost` — its bound
/// PersistentVolume disappeared. Terminal immediately (no point waiting out the
/// bind deadline; the stage can never become usable).
pub const REASON_STAGED_PVC_LOST: &str = "StagedPvcLost";

/// Appended to every `StagingOutcome::Failed` message whose fix is "set copyMethod:
/// Direct" — since `copyMethod` now *defaults* to `Snapshot` (as of this release), a user
/// who never touched the field can land here with no idea a default even applies. Spells
/// out both the fact and the fix so the message is actionable without reading the CRD.
const COPY_METHOD_DEFAULTED_NOTE: &str = "copyMethod defaulted to Snapshot as of this \
    release; set spec.copyMethod: Direct on the SnapshotPolicy to read the live PVC \
    without CSI snapshots";

/// Message for [`REASON_STACK_MISSING`]: no `VolumeSnapshotClass` API at all — the CSI
/// external-snapshotter stack (snapshot-controller + CRDs) isn't installed. This is the
/// failure most likely to hit a user who never set `copyMethod` and has no CSI snapshot
/// support in their cluster, so it carries the full default-changed explanation.
fn stack_missing_message(provisioner: &str) -> String {
    format!(
        "CSI snapshot stack not installed (no VolumeSnapshotClass API), so copyMethod: Snapshot \
         cannot run. Fix: install the external-snapshotter (snapshot-controller + CRDs) and a \
         VolumeSnapshotClass for driver `{provisioner}`, or set copyMethod: Direct. \
         {COPY_METHOD_DEFAULTED_NOTE}."
    )
}

/// Message for a namespaced install (Role-only RBAC), where the cluster-scoped
/// reads/deletes CSI staging needs (StorageClasses, VolumeSnapshotClasses,
/// VolumeSnapshotContents) can never be granted — and `copyMethod` defaults to
/// `Snapshot`, so an untouched policy lands here blindly.
/// The namespaced-install refusal for a GROUP capture.
fn namespaced_install_cannot_group_stage_message() -> String {
    "namespaced install (`installScope: namespaced`): its Role RBAC cannot read the \
     cluster-scoped VolumeGroupSnapshotClass, VolumeSnapshotContent and PersistentVolume objects \
     a `VolumeGroupSnapshot` capture needs to map group members to their PVCs. Fix: set \
     `groupBy: None` with `copyMethod: Direct` (independent reads of the live volumes), or \
     reinstall with installScope=cluster."
        .to_string()
}

fn namespaced_install_cannot_stage_message() -> String {
    format!(
        "namespaced install (installScope: namespaced): its Role RBAC cannot read the \
         cluster-scoped StorageClass/VolumeSnapshotClass objects CSI staging requires. Fix: set \
         copyMethod: Direct, or reinstall with installScope=cluster. \
         {COPY_METHOD_DEFAULTED_NOTE}."
    )
}

/// Message for [`REASON_SOURCE_NOT_CSI`] when the source PVC has no `storageClassName` /
/// CSI provisioner at all (a statically-provisioned or hostPath-backed volume) — the
/// other common "no CSI available" failure a default-`Snapshot` user can hit blindly.
fn source_not_csi_message(ns: &str, source_name: &str) -> String {
    format!(
        "source PVC `{ns}/{source_name}` has no StorageClass / CSI provisioner, so it \
         cannot be CSI-snapshotted; use a CSI StorageClass, or set copyMethod: Direct. \
         {COPY_METHOD_DEFAULTED_NOTE}."
    )
}

/// Message for [`REASON_SOURCE_NOT_CSI`] when the source PVC itself doesn't exist yet.
/// Not strictly a "no CSI stack" failure, but `copyMethod: Direct` is still a valid
/// workaround while the PVC is created, so it carries the same pointer.
fn source_pvc_not_found_message(ns: &str, source_name: &str, copy_method: CopyMethod) -> String {
    format!(
        "source PVC `{ns}/{source_name}` was not found; copyMethod {copy_method:?} needs an \
         existing CSI-provisioned PVC to snapshot — create the PVC, or set copyMethod: \
         Direct. {COPY_METHOD_DEFAULTED_NOTE}."
    )
}

/// Message for [`REASON_NO_CLASS`] / [`ClassDecision::ExplicitNotFound`]: an explicit
/// `volumeSnapshotClassName` names a class that doesn't exist.
fn explicit_class_not_found_message(name: &str, provisioner: &str) -> String {
    format!(
        "volumeSnapshotClassName `{name}` does not exist; create it (driver \
         `{provisioner}`), pick an existing class, or set copyMethod: Direct. \
         {COPY_METHOD_DEFAULTED_NOTE}."
    )
}

/// Message for [`REASON_NO_CLASS`] / [`ClassDecision::NoneForDriver`]: the CSI stack is
/// installed but no `VolumeSnapshotClass` matches the source's driver.
fn no_class_for_driver_message(driver: &str) -> String {
    format!(
        "no VolumeSnapshotClass has driver `{driver}` (the source PVC's provisioner); \
         create a VolumeSnapshotClass for it (optionally annotate it \
         {DEFAULT_CLASS_ANNOTATION}=true), set volumeSnapshotClassName explicitly, or set \
         copyMethod: Direct. {COPY_METHOD_DEFAULTED_NOTE}."
    )
}

/// A `VolumeSnapshotClass` as far as class selection cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    /// `metadata.name`.
    pub name: String,
    /// `.driver` (the CSI driver the class snapshots).
    pub driver: String,
    /// Whether it carries the default-class annotation.
    pub is_default: bool,
}

/// The pure outcome of picking a `VolumeSnapshotClass` for a source provisioner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassDecision {
    /// Set this class name explicitly on the VolumeSnapshot.
    Use(String),
    /// An explicit `volumeSnapshotClassName` was given but no such class exists.
    ExplicitNotFound(String),
    /// No class's driver matches the source provisioner.
    NoneForDriver(String),
    /// Several classes match the driver and none (or more than one) is the default —
    /// the user must name one explicitly.
    Ambiguous {
        /// The source provisioner the candidates matched.
        driver: String,
        /// The candidate class names.
        candidates: Vec<String>,
    },
}

/// Pick the `VolumeSnapshotClass` for a source `provisioner`. **Pure** over the listed
/// classes (the cluster read is the caller's). Precedence: an explicit name wins (if it
/// exists); otherwise the single class whose driver matches; otherwise — among several
/// matches — the unique default-annotated one; otherwise `Ambiguous`/`NoneForDriver`.
///
/// An absent, empty, or whitespace-only `explicit` all mean the same thing: **unset** —
/// fall through to auto-selection. `Option<&str>` alone is a leaky representation here,
/// because `Some("")` is not a nameable Kubernetes object, and GitOps templating produces
/// it routinely: a Flux/Kustomize `volumeSnapshotClassName: ${KOPIUR_SNAPSHOTCLASS}`
/// post-build substitution renders empty whenever the variable is undefined. Normalizing
/// here rather than at the call site makes the invalid state unreachable for every caller,
/// and keeps the behavior provable in unit tests (the one production call site is async and
/// `kube::Client`-bound, so only the e2e tier reaches it).
pub fn decide_class(
    classes: &[ClassInfo],
    provisioner: &str,
    explicit: Option<&str>,
) -> ClassDecision {
    let explicit = explicit.map(str::trim).filter(|s| !s.is_empty());
    if let Some(name) = explicit {
        return if classes.iter().any(|c| c.name == name) {
            ClassDecision::Use(name.to_string())
        } else {
            ClassDecision::ExplicitNotFound(name.to_string())
        };
    }
    let candidates: Vec<&ClassInfo> = classes.iter().filter(|c| c.driver == provisioner).collect();
    match candidates.as_slice() {
        [] => ClassDecision::NoneForDriver(provisioner.to_string()),
        [only] => ClassDecision::Use(only.name.clone()),
        many => {
            let defaults: Vec<&&ClassInfo> = many.iter().filter(|c| c.is_default).collect();
            if let [the_default] = defaults.as_slice() {
                ClassDecision::Use(the_default.name.clone())
            } else {
                ClassDecision::Ambiguous {
                    driver: provisioner.to_string(),
                    candidates: many.iter().map(|c| c.name.clone()).collect(),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure builders
// ---------------------------------------------------------------------------

/// Kubernetes object-name length cap.
const MAX_NAME_LEN: usize = 63;

/// A deterministic child-object name `<base>-<suffix>`, bounded to 63 chars (a short
/// stable hash of the base replaces the overflow so two long Snapshot names never
/// collide). Deterministic so a crashed reconcile re-derives the **same** name and
/// never orphans or double-creates.
pub fn staged_child_name(base: &str, suffix: &str) -> String {
    let full = format!("{base}-{suffix}");
    if full.len() <= MAX_NAME_LEN {
        return full;
    }
    // Stable FNV-1a hash of the base → 8 hex chars; keep the suffix legible.
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in base.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let tag = format!("{:08x}", (hash & 0xffff_ffff) as u32);
    let keep = MAX_NAME_LEN - tag.len() - suffix.len() - 2; // two '-'
    format!("{}-{tag}-{suffix}", &base[..keep.min(base.len())])
}

/// The VolumeSnapshot name for a Snapshot CR.
pub fn volume_snapshot_name(snapshot_cr: &str) -> String {
    staged_child_name(snapshot_cr, "snap")
}

/// The staged-PVC name for a Snapshot CR.
pub fn staged_pvc_name(snapshot_cr: &str) -> String {
    staged_child_name(snapshot_cr, "src")
}

/// Build the `VolumeSnapshot` `DynamicObject` for `source_claim`, owner-referenced so GC
/// reaps it with the Snapshot CR and labelled managed-by. `class` (when `Some`) pins
/// `spec.volumeSnapshotClassName`.
pub fn build_volume_snapshot(
    name: &str,
    ns: &str,
    source_claim: &str,
    class: Option<&str>,
    owner: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> DynamicObject {
    let mut spec = serde_json::json!({
        "source": { "persistentVolumeClaimName": source_claim },
    });
    if let Some(class) = class {
        spec["volumeSnapshotClassName"] = serde_json::Value::String(class.to_string());
    }
    let mut obj = DynamicObject::new(name, &volume_snapshot_ar())
        .within(ns)
        .data(serde_json::json!({ "spec": spec }));
    obj.metadata.owner_references = Some(vec![owner]);
    obj.metadata.labels = Some(child_labels(&[(
        "kopiur.home-operations.com/component",
        "staged-source-snapshot",
    )]));
    obj
}

/// The `dataSource` for the staged PVC: the VolumeSnapshot (`Snapshot`) or the source
/// PVC itself (`Clone`, a CSI volume clone).
pub fn data_source_for(
    copy_method: CopyMethod,
    source_pvc: &str,
    volume_snapshot: &str,
) -> TypedLocalObjectReference {
    match copy_method {
        CopyMethod::Clone => TypedLocalObjectReference {
            api_group: None,
            kind: "PersistentVolumeClaim".to_string(),
            name: source_pvc.to_string(),
        },
        // Snapshot (and any non-Direct default) stages from the VolumeSnapshot.
        _ => TypedLocalObjectReference {
            api_group: Some(SNAPSHOT_GROUP.to_string()),
            kind: "VolumeSnapshot".to_string(),
            name: volume_snapshot.to_string(),
        },
    }
}

/// Parse a `Quantity` like `"10Gi"` to bytes (best-effort; binary + decimal SI). Returns
/// `None` when unparseable, so sizing falls back conservatively.
fn quantity_to_bytes(q: &str) -> Option<i128> {
    let q = q.trim();
    let (num, mult): (&str, i128) = if let Some(p) = q.strip_suffix("Ki") {
        (p, 1 << 10)
    } else if let Some(p) = q.strip_suffix("Mi") {
        (p, 1 << 20)
    } else if let Some(p) = q.strip_suffix("Gi") {
        (p, 1 << 30)
    } else if let Some(p) = q.strip_suffix("Ti") {
        (p, 1i128 << 40)
    } else if let Some(p) = q.strip_suffix("Pi") {
        (p, 1i128 << 50)
    } else if let Some(p) = q.strip_suffix('k') {
        (p, 1_000)
    } else if let Some(p) = q.strip_suffix('M') {
        (p, 1_000_000)
    } else if let Some(p) = q.strip_suffix('G') {
        (p, 1_000_000_000)
    } else if let Some(p) = q.strip_suffix('T') {
        (p, 1_000_000_000_000)
    } else {
        (q, 1)
    };
    num.trim().parse::<i128>().ok().map(|n| n * mult)
}

/// The storage request for the staged PVC: the larger of the snapshot's `restoreSize`
/// (often `0`/absent) and the source PVC's request — the external-provisioner rejects a
/// staged PVC smaller than `restoreSize`. Returns the chosen `Quantity` string, or
/// `None` when neither is known (let the provisioner default).
pub fn staged_pvc_size(restore_size: Option<&str>, source_request: Option<&str>) -> Option<String> {
    let r = restore_size.and_then(quantity_to_bytes).unwrap_or(0);
    let s = source_request.and_then(quantity_to_bytes).unwrap_or(0);
    match (restore_size, source_request) {
        (None, None) => None,
        // Keep the original string of whichever is larger (preserves the unit the user
        // wrote, and avoids re-rendering bytes the provisioner then rounds).
        _ if r >= s => restore_size
            .filter(|_| r > 0)
            .map(str::to_string)
            .or_else(|| source_request.map(str::to_string)),
        _ => source_request.map(str::to_string),
    }
}

/// Build the staged PVC: `accessModes`/`storageClassName` come from the
/// `spec.staging` overrides when set, else are copied from the source PVC;
/// `volumeMode` is always copied (the mover reads files — no override exists).
/// Sets `dataSource`, requests `max(restoreSize, source)` storage, and is
/// owner-referenced + managed-by labelled.
pub fn build_staged_pvc(
    name: &str,
    ns: &str,
    source_pvc: &PersistentVolumeClaim,
    data_source: TypedLocalObjectReference,
    restore_size: Option<&str>,
    staging: Option<&StagingSpec>,
    owner: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> PersistentVolumeClaim {
    let src = source_pvc.spec.clone().unwrap_or_default();
    let source_request = src
        .resources
        .as_ref()
        .and_then(|r| r.requests.as_ref())
        .and_then(|m| m.get("storage"))
        .map(|q| q.0.clone());
    let size = staged_pvc_size(restore_size, source_request.as_deref());
    let resources = size.map(|s| VolumeResourceRequirements {
        requests: Some(BTreeMap::from([("storage".to_string(), Quantity(s))])),
        limits: None,
    });
    let access_modes = staging
        .filter(|s| !s.access_modes.is_empty())
        .map(|s| {
            s.access_modes
                .iter()
                .map(|m| m.mode_str().to_string())
                .collect()
        })
        .or(src.access_modes);
    let storage_class_name = staging
        .and_then(|s| s.storage_class_name.clone())
        .or(src.storage_class_name);
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![owner]),
            labels: Some(child_labels(&[(
                "kopiur.home-operations.com/component",
                "staged-source",
            )])),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes,
            storage_class_name,
            volume_mode: src.volume_mode,
            data_source: Some(data_source),
            resources,
            ..Default::default()
        }),
        status: None,
    }
}

/// Preflight `spec.staging.storageClassName` against the source's CSI driver.
/// **Pure** (the StorageClass read is the caller's): `override_provisioner` is the
/// override class's `provisioner` (`None` = the class does not exist);
/// `source_provisioner` is the source PVC's (`None` = unknown — e.g. a `Clone`
/// source without a class — so the mismatch check is skipped). Without this, a
/// wrong-driver override presents as an opaque bind timeout many minutes later:
/// the override class's provisioner ignores a foreign VolumeSnapshotContent/PVC
/// forever.
pub(crate) fn staged_class_preflight(
    override_class: &str,
    override_provisioner: Option<&str>,
    source_provisioner: Option<&str>,
) -> Option<(&'static str, String)> {
    let Some(op) = override_provisioner else {
        return Some((
            REASON_STAGED_CLASS_NOT_FOUND,
            format!(
                "spec.staging.storageClassName `{override_class}` does not exist. Fix: create \
                 it, point the override at an existing StorageClass of the source's CSI driver, \
                 or remove the override to stage on the source PVC's own class. This Snapshot is \
                 terminal — the next scheduled run (or a new Snapshot) retries"
            ),
        ));
    };
    match source_provisioner {
        Some(sp) if op != sp => Some((
            REASON_STAGED_CLASS_MISMATCH,
            format!(
                "spec.staging.storageClassName `{override_class}` is provisioned by `{op}`, \
                 but the source PVC's CSI driver is `{sp}` — a snapshot restore/clone can only \
                 be provisioned by the source's own driver, so the staged PVC would never bind. \
                 Fix: point the override at a class of driver `{sp}` (e.g. a CephFS \
                 `backingSnapshot: \"true\"` class), or remove the override. This Snapshot is \
                 terminal — the next scheduled run (or a new Snapshot) retries"
            ),
        )),
        _ => None,
    }
}

/// What cleanup must do for a staged PVC: whether its **bound** PV needs a
/// `Retain → Delete` reclaim patch before the PVC is deleted (else a `Retain`
/// StorageClass leaks the PV + backend volume), plus the bound PV name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CleanupPlan {
    /// The bound PV to patch `Retain → Delete` before deleting the PVC (`None` when the
    /// PVC is unbound — nothing to leak).
    pub reclaim_pv: Option<String>,
}

/// Decide the PV-reclaim step for a staged PVC. The PVC is bound iff it has a
/// `spec.volumeName`; only then can a `Retain` PV outlive it.
pub fn cleanup_plan(staged_pvc: &PersistentVolumeClaim) -> CleanupPlan {
    CleanupPlan {
        reclaim_pv: staged_pvc
            .spec
            .as_ref()
            .and_then(|s| s.volume_name.clone())
            .filter(|v| !v.is_empty()),
    }
}

// ---------------------------------------------------------------------------
// Cluster IO
// ---------------------------------------------------------------------------

/// List the cluster's `VolumeSnapshotClass`es, or `None` when the snapshot CRDs are not
/// installed (the API group is absent → the list 404s / "could not find the requested
/// resource"). Distinguishing absence from a transient error lets the caller emit the
/// precise `SnapshotStackMissing` guidance.
pub async fn list_snapshot_classes(client: &kube::Client) -> Result<Option<Vec<ClassInfo>>> {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &volume_snapshot_class_ar());
    match api.list(&ListParams::default()).await {
        Ok(list) => Ok(Some(
            list.items
                .into_iter()
                .map(|o| ClassInfo {
                    driver: o
                        .data
                        .get("driver")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    is_default: o
                        .annotations()
                        .get(DEFAULT_CLASS_ANNOTATION)
                        .map(|v| v == "true")
                        .unwrap_or(false),
                    name: o.name_any(),
                })
                .collect(),
        )),
        // The CRD/API group isn't served → the stack isn't installed.
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// The CSI provisioner backing a PVC: its `StorageClass.provisioner`. `None` when the
/// PVC has no `storageClassName` (a statically-provisioned / hostPath volume that can't
/// be CSI-snapshotted) or the class is missing.
pub async fn source_provisioner(
    client: &kube::Client,
    source_pvc: &PersistentVolumeClaim,
) -> Result<Option<String>> {
    let Some(class) = source_pvc
        .spec
        .as_ref()
        .and_then(|s| s.storage_class_name.clone())
        .filter(|c| !c.is_empty())
    else {
        return Ok(None);
    };
    let api: Api<StorageClass> = Api::all(client.clone());
    Ok(api.get_opt(&class).await?.map(|sc| sc.provisioner))
}

/// One observation of a staged VolumeSnapshot's state, as read from the cluster.
/// Plain data so [`vs_wait_outcome`] — the decision over it — stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VsObservation {
    /// `status.readyToUse` (`false` when absent/unset).
    pub ready: bool,
    /// `status.restoreSize`, normalized to a string quantity.
    pub restore_size: Option<String>,
    /// `status.error.message`, if the last snapshot-controller sync failed. Inherently
    /// transient: set on any failed attempt (including a benign 409-conflict retry)
    /// and cleared on the next successful one, independently of `readyToUse`.
    pub error: Option<String>,
    /// `metadata.creationTimestamp` — the staging deadline anchor. Stable across the
    /// idempotent SSA re-applies each reconcile performs.
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Read a VolumeSnapshot's state; `None` when it doesn't exist.
async fn read_volume_snapshot(
    client: &kube::Client,
    ns: &str,
    name: &str,
) -> Result<Option<VsObservation>> {
    let api = volume_snapshot_api(client, ns);
    let Some(vs) = api.get_opt(name).await? else {
        return Ok(None);
    };
    let status = vs.data.get("status");
    let ready = status
        .and_then(|s| s.get("readyToUse"))
        .and_then(|r| r.as_bool())
        .unwrap_or(false);
    let restore_size = status.and_then(|s| s.get("restoreSize")).and_then(|r| {
        r.as_str()
            .map(str::to_string)
            .or_else(|| r.as_i64().map(|n| n.to_string()))
    });
    let error = status
        .and_then(|s| s.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string);
    // metadata.creationTimestamp is a k8s-openapi `Time` wrapping a jiff
    // `Timestamp`; convert via unix seconds to chrono (matches snapshot_policy).
    let created_at = vs
        .metadata
        .creation_timestamp
        .as_ref()
        .and_then(|t| chrono::DateTime::<chrono::Utc>::from_timestamp(t.0.as_second(), 0));
    Ok(Some(VsObservation {
        ready,
        restore_size,
        error,
        created_at,
    }))
}

/// What [`vs_wait_outcome`] decided about an observed VolumeSnapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VsWait {
    /// `readyToUse` — provision the staged PVC. Overrides any error/deadline: a ready
    /// snapshot is a usable snapshot.
    Ready { restore_size: Option<String> },
    /// Not ready, deadline not passed — hold `Pending` and requeue.
    Waiting(String),
    /// Not ready when the deadline passed — terminal for this Snapshot CR.
    Failed {
        reason: &'static str,
        message: String,
    },
}

/// Render a duration the way the CRD field is written (`1h`/`10m`/`90s`), so the
/// terminal message echoes a value the user can paste into `spec.staging.timeout`.
fn fmt_go_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs > 0 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs > 0 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Decide the wait outcome for one VolumeSnapshot observation. Pure / clock-injected
/// (issue #198): a `status.error` NEVER fails staging on sight — external-snapshotter
/// sets it transiently during benign retry loops (e.g. a 409 finalizer-add conflict)
/// and clears it once a retry succeeds, so the only failure signal is the deadline
/// (`created_at + timeout`, `timeout == None` ⇒ indefinite). Because a `Failed`
/// Snapshot is terminal (one-shot discipline), the bar here is deliberately high.
pub(crate) fn vs_wait_outcome(
    obs: &VsObservation,
    ns: &str,
    vs_name: &str,
    class: &str,
    timeout: Option<std::time::Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> VsWait {
    if obs.ready {
        return VsWait::Ready {
            restore_size: obs.restore_size.clone(),
        };
    }
    // `created_at` missing is a just-created / not-yet-persisted read: not expired.
    // A timeout too large for chrono (absurd but admissible) degrades to indefinite.
    let deadline = match (timeout, obs.created_at) {
        (Some(t), Some(created)) => chrono::Duration::from_std(t).ok().map(|d| created + d),
        _ => None,
    };
    let expired = deadline.is_some_and(|d| now >= d);
    if !expired {
        let mut msg = format!("waiting for VolumeSnapshot `{ns}/{vs_name}` to become readyToUse");
        if let Some(err) = &obs.error {
            // Surface the error as diagnostic context, framed as what it is: a
            // possibly-transient condition the snapshot-controller retries itself.
            msg.push_str(&format!(
                "; it reported a possibly-transient error (the snapshot-controller \
                 retries automatically): {err}"
            ));
        }
        match deadline {
            // Deterministic per-VS (creationTimestamp + timeout are fixed), so the
            // condition message doesn't churn status on every reconcile.
            Some(d) => msg.push_str(&format!(
                "; staging fails at {} if still not ready",
                d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            )),
            None => msg.push_str("; waiting indefinitely (spec.staging.timeout: 0)"),
        }
        return VsWait::Waiting(msg);
    }
    let waited = fmt_go_duration(timeout.expect("expired implies a timeout"));
    match &obs.error {
        Some(err) => VsWait::Failed {
            reason: REASON_VS_FAILED,
            message: format!(
                "VolumeSnapshot `{ns}/{vs_name}` did not become readyToUse within {waited} \
                 (spec.staging.timeout; default 10m) and last reported: {err}. Fix: check the \
                 VolumeSnapshotClass `{class}` / CSI driver, or raise spec.staging.timeout if \
                 the backend is just slow. This Snapshot is terminal — the next scheduled run \
                 (or a new Snapshot) retries"
            ),
        },
        None => VsWait::Failed {
            reason: REASON_STAGING_TIMEOUT,
            message: format!(
                "VolumeSnapshot `{ns}/{vs_name}` did not become readyToUse within {waited} \
                 (spec.staging.timeout; default 10m) and reported no error — the CSI driver or \
                 snapshot-controller is stuck or slow. Fix: check both, or raise \
                 spec.staging.timeout if the backend is just slow. This Snapshot is terminal — \
                 the next scheduled run (or a new Snapshot) retries"
            ),
        },
    }
}

/// `StorageClass.volumeBindingMode` as staging cares about it. Unset ⇒ `Immediate`
/// (the Kubernetes API default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingMode {
    /// The PVC binds (and the CSI restore/clone runs) at provision time — the bind
    /// gate must wait for `Bound` before minting the mover Job.
    Immediate,
    /// Binding happens when the first consuming pod schedules — the pre-Job gate
    /// skips the wait (the running-Job watchdog covers a hung bind instead).
    WaitForFirstConsumer,
}

/// Classify a `StorageClass.volumeBindingMode` string; unset ⇒ `Immediate`.
pub(crate) fn effective_binding_mode(volume_binding_mode: Option<&str>) -> BindingMode {
    match volume_binding_mode {
        Some("WaitForFirstConsumer") => BindingMode::WaitForFirstConsumer,
        _ => BindingMode::Immediate,
    }
}

/// One observation of the staged PVC's state, as read from the cluster. Plain data
/// so [`pvc_bind_outcome`] — the decision over it — stays pure (mirrors
/// [`VsObservation`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedPvcObservation {
    /// `status.phase` (`Pending`/`Bound`/`Lost`).
    pub phase: Option<String>,
    /// `metadata.creationTimestamp` — the bind-deadline anchor. Stable across the
    /// idempotent SSA re-applies each reconcile performs.
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Extract a [`StagedPvcObservation`] from a read PVC (pure — shared by the
/// pre-Job bind gate and the running-Job watchdog in the snapshot reconciler).
pub(crate) fn staged_pvc_observation(pvc: &PersistentVolumeClaim) -> StagedPvcObservation {
    StagedPvcObservation {
        phase: pvc.status.as_ref().and_then(|s| s.phase.clone()),
        created_at: pvc
            .metadata
            .creation_timestamp
            .as_ref()
            .and_then(|t| chrono::DateTime::<chrono::Utc>::from_timestamp(t.0.as_second(), 0)),
    }
}

/// What [`pvc_bind_outcome`] decided about an observed staged PVC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PvcBindWait {
    /// `Bound` — the stage is usable. Overrides the deadline: an operator restart
    /// after a slow-but-successful bind must not terminally fail a good stage.
    Bound,
    /// Still binding, deadline not passed — hold `Pending` and requeue.
    Waiting(String),
    /// Bind deadline passed (or the PVC is `Lost`) — terminal for this Snapshot CR.
    Failed {
        reason: &'static str,
        message: String,
    },
}

/// Render the class portion of a bind message: the known class name, or a neutral
/// phrase when the staged PVC rides the source's/cluster-default class.
fn bind_class_phrase(class: Option<&str>) -> String {
    match class {
        Some(c) => format!("StorageClass `{c}`"),
        None => "its StorageClass".to_string(),
    }
}

/// Decide the bind-wait outcome for one staged-PVC observation. Pure /
/// clock-injected, mirroring [`vs_wait_outcome`] exactly: `Bound` is checked FIRST
/// (ready wins over deadline), a missing `creationTimestamp` is never expired, the
/// deadline is `created_at + timeout` (`timeout == None` ⇒ indefinite), and the
/// Waiting message embeds the fixed RFC3339 deadline — deterministic per PVC, so
/// `patch_status_if_changed` doesn't churn. `Lost` is terminal immediately: the
/// bound PV vanished and no amount of waiting brings the stage back.
pub(crate) fn pvc_bind_outcome(
    obs: &StagedPvcObservation,
    ns: &str,
    pvc_name: &str,
    class: Option<&str>,
    timeout: Option<std::time::Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> PvcBindWait {
    match obs.phase.as_deref() {
        Some("Bound") => return PvcBindWait::Bound,
        Some("Lost") => {
            return PvcBindWait::Failed {
                reason: REASON_STAGED_PVC_LOST,
                message: format!(
                    "staged PVC `{ns}/{pvc_name}` reports phase Lost — its bound \
                     PersistentVolume disappeared, so the stage can never become usable; \
                     check the CSI driver / PV lifecycle. This Snapshot is terminal — the \
                     next scheduled run (or a new Snapshot) retries"
                ),
            };
        }
        _ => {}
    }
    let deadline = match (timeout, obs.created_at) {
        (Some(t), Some(created)) => chrono::Duration::from_std(t).ok().map(|d| created + d),
        _ => None,
    };
    let expired = deadline.is_some_and(|d| now >= d);
    if !expired {
        let mut msg = format!(
            "waiting for staged PVC `{ns}/{pvc_name}` ({}) to bind; the CSI restore/clone \
             from the source is provisioning",
            bind_class_phrase(class)
        );
        match deadline {
            // Deterministic per-PVC (creationTimestamp + timeout are fixed), so the
            // condition message doesn't churn status on every reconcile.
            Some(d) => msg.push_str(&format!(
                "; staging fails at {} if still unbound",
                d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            )),
            None => msg.push_str("; waiting indefinitely (spec.staging.timeout: 0)"),
        }
        return PvcBindWait::Waiting(msg);
    }
    let waited = fmt_go_duration(timeout.expect("expired implies a timeout"));
    PvcBindWait::Failed {
        reason: REASON_STAGED_PVC_BIND_TIMEOUT,
        message: format!(
            "staged PVC `{ns}/{pvc_name}` did not reach Bound within {waited} \
             (spec.staging.timeout; default 10m) — the CSI restore/clone is still \
             provisioning, or {} cannot provision it. Fix: check `kubectl describe pvc -n {ns} \
             {pvc_name}` and the CSI provisioner logs, raise spec.staging.timeout if the copy \
             is slow, or (CephFS) stage on a `backingSnapshot: \"true\"` class via \
             spec.staging.storageClassName for a near-instant shallow mount. This Snapshot is \
             terminal — the next scheduled run (or a new Snapshot) retries",
            bind_class_phrase(class)
        ),
    }
}

/// The staged PVC's *effective* StorageClass name: what its spec pins, else the
/// cluster's default-annotated class (what the PV controller will use), else
/// `None` (nothing to look up — the bind gate then skips, preserving pre-gate
/// behavior for classless setups).
async fn effective_staged_class(
    client: &kube::Client,
    staged: &PersistentVolumeClaim,
) -> Result<Option<String>> {
    if let Some(c) = staged_class_of(staged) {
        return Ok(Some(c));
    }
    let sc_api: Api<StorageClass> = Api::all(client.clone());
    let classes = sc_api.list(&ListParams::default()).await?;
    Ok(classes
        .items
        .into_iter()
        .find(|sc| {
            sc.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get("storageclass.kubernetes.io/is-default-class"))
                .is_some_and(|v| v == "true")
        })
        .and_then(|sc| sc.metadata.name))
}

/// Gate `Ready` on the staged PVC actually binding when its StorageClass binds
/// `Immediate` (the CSI restore/clone runs at provision time — a mover Job minted
/// now would sit Unschedulable on the unbound claim until the wedged-pod deadline
/// killed the run *and the VolumeSnapshot under the in-flight restore*).
/// `WaitForFirstConsumer` classes return immediately: their bind happens at pod
/// schedule, and the running-Job watchdog covers a hang there. Returns `None` when
/// the gate passes (mint the Job) or `Some(outcome)` to surface instead. A class
/// that has vanished mid-flight is treated as `Immediate` (the API default), so a
/// never-binding PVC becomes a bounded, actionable failure rather than a wedge.
async fn staged_pvc_bind_gate(
    client: &kube::Client,
    pvc_api: &Api<PersistentVolumeClaim>,
    ns: &str,
    staged: &PersistentVolumeClaim,
    pvc_name: &str,
    timeout: Option<std::time::Duration>,
) -> Result<Option<StagingOutcome>> {
    let Some(class) = effective_staged_class(client, staged).await? else {
        return Ok(None);
    };
    let sc_api: Api<StorageClass> = Api::all(client.clone());
    let binding = sc_api
        .get_opt(&class)
        .await?
        .map(|sc| effective_binding_mode(sc.volume_binding_mode.as_deref()))
        .unwrap_or(BindingMode::Immediate);
    match binding {
        BindingMode::WaitForFirstConsumer => Ok(None),
        BindingMode::Immediate => {
            let outcome = match pvc_api.get_opt(pvc_name).await? {
                // SSA create still propagating → requeue.
                None => PvcBindWait::Waiting(format!(
                    "waiting for staged PVC `{ns}/{pvc_name}` ({}) to bind; the CSI \
                     restore/clone from the source is provisioning",
                    bind_class_phrase(Some(&class))
                )),
                Some(pvc) => pvc_bind_outcome(
                    &staged_pvc_observation(&pvc),
                    ns,
                    pvc_name,
                    Some(&class),
                    timeout,
                    chrono::Utc::now(),
                ),
            };
            Ok(match outcome {
                PvcBindWait::Bound => None,
                PvcBindWait::Waiting(message) => Some(StagingOutcome::Waiting {
                    reason: crate::consts::STAGED_PVC_BINDING_REASON,
                    message,
                }),
                PvcBindWait::Failed { reason, message } => {
                    Some(StagingOutcome::Failed { reason, message })
                }
            })
        }
    }
}

/// What a source's shape + `copyMethod` + install scope imply for staging,
/// BEFORE any cluster read.
///
/// Pure so the ORDER is pinned by unit tests. The order was wrong: the
/// namespaced-install refusal used to run *before* the source-shape checks, so
/// an NFS-source policy under the defaulted `copyMethod: Snapshot` failed
/// TERMINALLY on a namespaced install — when it should mount the live export
/// and succeed. Nothing about an NFS source needs a cluster-scoped read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StagingApplicability<'a> {
    /// `Direct`, or a non-PVC source (NFS, or a `pvcSelector` that produced no
    /// concrete PVC): mount the live source, stage nothing.
    NotApplicable,
    /// A PVC source that wants staging, on an install whose Role RBAC cannot
    /// read the cluster-scoped objects staging requires.
    ClusterScopeRequired,
    /// Stage this PVC.
    Proceed {
        /// The concrete source PVC name.
        source_name: &'a str,
    },
}

/// Decide [`StagingApplicability`]. See the enum for why the order matters.
pub(crate) fn staging_applicability(
    copy_method: CopyMethod,
    source_pvc: Option<&str>,
    namespaced_install: bool,
) -> StagingApplicability<'_> {
    // 1. Opted out entirely.
    if copy_method == CopyMethod::Direct {
        return StagingApplicability::NotApplicable;
    }
    // 2. Nothing to stage. Checked BEFORE the install-scope gate: every
    //    cluster-scoped read staging performs (StorageClass, VolumeSnapshotClass,
    //    the PersistentVolume reclaim patch) is on the PVC path only.
    let Some(name) = source_pvc else {
        return StagingApplicability::NotApplicable;
    };
    // 3. Now the gate is meaningful: this source really does need the
    //    cluster-scoped reads a namespaced install's Role cannot make.
    if namespaced_install {
        return StagingApplicability::ClusterScopeRequired;
    }
    StagingApplicability::Proceed { source_name: name }
}

/// Resolve staging for a backup. Performs the cluster IO (preflight, create
/// VolumeSnapshot, wait `readyToUse`, create staged PVC) and returns a [`StagingOutcome`]
/// — **no** status side effects (the reconciler maps the outcome). Idempotent: every
/// create is SSA over a deterministic name.
pub async fn resolve_staging(
    client: &kube::Client,
    scope: &crate::config::WatchScope,
    policy: &SnapshotPolicy,
    pin: Option<&kopiur_api::SnapshotSourceRef>,
    ns: &str,
    snapshot_cr: &str,
    owner: &k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> Result<StagingOutcome> {
    let copy_method = policy.spec.copy_method;
    // The source THIS run covers. Without the pin a fanned-out child would
    // stage `sources[0]`, which for a `pvcSelector` policy has no PVC at all —
    // so every child would silently skip staging and read its live volume
    // instead of a point-in-time copy (#346).
    let eff = crate::expand::effective_source(policy, pin)
        .map_err(|e| Error::Validation(e.to_string()))?;
    let pinned_pvc = eff.pvc.as_ref().map(|p| p.name.clone());
    // Decide, before touching the cluster, whether staging applies at all and
    // whether this install can do it. Pure + unit-tested, because the ORDER is
    // the load-bearing part and `resolve_staging` itself is async and
    // Client-bound (so only the e2e tier reaches it).
    let source_name = match staging_applicability(
        copy_method,
        pinned_pvc.as_deref(),
        matches!(scope, crate::config::WatchScope::Namespaced(_)),
    ) {
        StagingApplicability::NotApplicable => return Ok(StagingOutcome::NotApplicable),
        StagingApplicability::ClusterScopeRequired => {
            // Group staging is STRICTLY more cluster-scoped than single-volume
            // staging — on top of StorageClasses it needs cluster-scoped
            // VolumeGroupSnapshotClasses, VolumeSnapshotContents and
            // PersistentVolumes (the three-hop member→PVC reconstruction) — so
            // it gets its own message rather than one that only mentions
            // `copyMethod: Direct`.
            let message = if pin.and_then(|p| p.group.as_ref()).is_some() {
                namespaced_install_cannot_group_stage_message()
            } else {
                namespaced_install_cannot_stage_message()
            };
            return Ok(StagingOutcome::Failed {
                reason: REASON_SOURCE_NOT_CSI,
                message,
            });
        }
        StagingApplicability::Proceed { source_name } => source_name.to_string(),
    };
    let source_name = &source_name;

    // Read the source PVC (needed for sizing + provisioner + shape).
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    let Some(source_pvc) = pvc_api.get_opt(source_name).await? else {
        return Ok(StagingOutcome::Failed {
            reason: REASON_SOURCE_NOT_CSI,
            message: source_pvc_not_found_message(ns, source_name, copy_method),
        });
    };

    let staged_pvc = staged_pvc_name(snapshot_cr);
    let vs_name = volume_snapshot_name(snapshot_cr);
    let staging = policy.spec.staging.as_ref();

    // The staging deadline budget — bounds the VolumeSnapshot readyToUse wait and
    // the staged-PVC bind wait (each phase gets a fresh budget anchored at its
    // object's creation), and is pinned to status for the running-Job watchdog.
    let timeout = kopiur_api::resolve_timeout(
        staging.and_then(|s| s.timeout.as_deref()),
        crate::consts::DEFAULT_STAGING_TIMEOUT,
    );
    let staging_timeout_seconds = timeout.map(|d| d.as_secs() as i64).unwrap_or(0);

    // Source provisioner (CSI driver): required for `Snapshot` class selection, and
    // the reference for the staged-class preflight (both methods).
    let provisioner = source_provisioner(client, &source_pvc).await?;

    // Fail fast on a bad `spec.staging.storageClassName` — missing, or on a
    // different driver than the source — instead of letting the staged PVC sit
    // Pending until the bind deadline with no explanation.
    if let Some(override_class) = staging.and_then(|s| s.storage_class_name.as_deref()) {
        let sc_api: Api<StorageClass> = Api::all(client.clone());
        let override_provisioner = sc_api
            .get_opt(override_class)
            .await?
            .map(|sc| sc.provisioner);
        if let Some((reason, message)) = staged_class_preflight(
            override_class,
            override_provisioner.as_deref(),
            provisioner.as_deref(),
        ) {
            return Ok(StagingOutcome::Failed { reason, message });
        }
    }

    // `groupBy: VolumeGroupSnapshot`: the capture is ONE shared object across
    // every member of the expansion, so the create/wait/pick-my-member half is
    // replaced — but the staged-PVC build and bind gate below are reused
    // verbatim, because each member still restores into its own private
    // `<cr>-src` PVC from its own private member snapshot.
    if let Some(group) = pin.and_then(|p| p.group.as_ref())
        && copy_method != CopyMethod::Clone
    {
        let Some(pvc) = eff.pvc.as_ref() else {
            return Ok(StagingOutcome::NotApplicable);
        };
        match super::group_staging::resolve_group_stage(
            client,
            policy,
            group,
            eff.index,
            &pvc.namespace,
            &pvc.name,
            provisioner.as_deref(),
            timeout,
        )
        .await?
        {
            super::group_staging::GroupStage::Ready { member } => {
                let data_source =
                    data_source_for(copy_method, source_name, &member.volume_snapshot_name);
                let staged = build_staged_pvc(
                    &staged_pvc,
                    ns,
                    &source_pvc,
                    data_source,
                    member.restore_size.as_ref().map(|q| q.0.as_str()),
                    staging,
                    owner.clone(),
                );
                apply(&pvc_api, &staged_pvc, &staged).await?;
                if let Some(outcome) =
                    staged_pvc_bind_gate(client, &pvc_api, ns, &staged, &staged_pvc, timeout)
                        .await?
                {
                    return Ok(outcome);
                }
                return Ok(StagingOutcome::Ready(StagedSource {
                    storage_class_name: staged_class_of(&staged),
                    pvc_name: staged_pvc,
                    volume_snapshot_name: Some(member.volume_snapshot_name),
                    volume_group_snapshot_name: Some(group.volume_group_snapshot_name.clone()),
                    copy_method: "Snapshot",
                    staging_timeout_seconds,
                }));
            }
            super::group_staging::GroupStage::Waiting { reason, message } => {
                return Ok(StagingOutcome::Waiting { reason, message });
            }
            super::group_staging::GroupStage::Failed { reason, message } => {
                return Ok(StagingOutcome::Failed { reason, message });
            }
        }
    }

    // `Snapshot` needs a VolumeSnapshotClass + a ready VolumeSnapshot; `Clone` stages
    // straight from the source PVC.
    if copy_method != CopyMethod::Clone {
        // Provisioner (CSI driver) of the source — required to match a class.
        let Some(provisioner) = provisioner else {
            return Ok(StagingOutcome::Failed {
                reason: REASON_SOURCE_NOT_CSI,
                message: source_not_csi_message(ns, source_name),
            });
        };
        // Preflight the snapshot stack + pick the class.
        let Some(classes) = list_snapshot_classes(client).await? else {
            return Ok(StagingOutcome::Failed {
                reason: REASON_STACK_MISSING,
                message: stack_missing_message(&provisioner),
            });
        };
        let class = match decide_class(
            &classes,
            &provisioner,
            policy.spec.volume_snapshot_class_name.as_deref(),
        ) {
            ClassDecision::Use(name) => name,
            ClassDecision::ExplicitNotFound(name) => {
                return Ok(StagingOutcome::Failed {
                    reason: REASON_NO_CLASS,
                    message: explicit_class_not_found_message(&name, &provisioner),
                });
            }
            ClassDecision::NoneForDriver(driver) => {
                return Ok(StagingOutcome::Failed {
                    reason: REASON_NO_CLASS,
                    message: no_class_for_driver_message(&driver),
                });
            }
            ClassDecision::Ambiguous { driver, candidates } => {
                return Ok(StagingOutcome::Failed {
                    reason: REASON_NO_CLASS,
                    message: format!(
                        "multiple VolumeSnapshotClasses match driver `{driver}` ({}) and none is \
                         the unique default; set volumeSnapshotClassName explicitly to choose one",
                        candidates.join(", ")
                    ),
                });
            }
        };

        // Create (SSA, idempotent) the VolumeSnapshot and wait for readyToUse.
        let vs = build_volume_snapshot(&vs_name, ns, source_name, Some(&class), owner.clone());
        apply(&volume_snapshot_api(client, ns), &vs_name, &vs).await?;

        // Wait for readyToUse, bounded by the staging deadline (issue #198): a
        // `status.error` alone is never fatal — external-snapshotter sets it during
        // benign retry loops — only the deadline fails staging.
        let restore_size = match read_volume_snapshot(client, ns, &vs_name).await? {
            // Not visible yet (SSA create still propagating) → requeue.
            None => {
                return Ok(StagingOutcome::Waiting {
                    reason: crate::consts::STAGING_WAITING_REASON,
                    message: format!(
                        "waiting for VolumeSnapshot `{ns}/{vs_name}` to become readyToUse"
                    ),
                });
            }
            Some(obs) => {
                match vs_wait_outcome(&obs, ns, &vs_name, &class, timeout, chrono::Utc::now()) {
                    VsWait::Ready { restore_size } => restore_size,
                    VsWait::Waiting(message) => {
                        return Ok(StagingOutcome::Waiting {
                            reason: crate::consts::STAGING_WAITING_REASON,
                            message,
                        });
                    }
                    VsWait::Failed { reason, message } => {
                        return Ok(StagingOutcome::Failed { reason, message });
                    }
                }
            }
        };

        let data_source = data_source_for(copy_method, source_name, &vs_name);
        let staged = build_staged_pvc(
            &staged_pvc,
            ns,
            &source_pvc,
            data_source,
            restore_size.as_deref(),
            staging,
            owner.clone(),
        );
        apply(&pvc_api, &staged_pvc, &staged).await?;
        // Gate on the bind for Immediate classes: minting the Job while the CSI
        // restore is in flight is exactly the wedge-then-delete-the-VS race.
        if let Some(outcome) =
            staged_pvc_bind_gate(client, &pvc_api, ns, &staged, &staged_pvc, timeout).await?
        {
            return Ok(outcome);
        }
        return Ok(StagingOutcome::Ready(StagedSource {
            storage_class_name: staged_class_of(&staged),
            pvc_name: staged_pvc,
            volume_snapshot_name: Some(vs_name),
            volume_group_snapshot_name: None,
            copy_method: "Snapshot",
            staging_timeout_seconds,
        }));
    }

    // Clone: stage straight from the source PVC (CSI volume clone). No VolumeSnapshot.
    let data_source = data_source_for(CopyMethod::Clone, source_name, &vs_name);
    let staged = build_staged_pvc(
        &staged_pvc,
        ns,
        &source_pvc,
        data_source,
        None,
        staging,
        owner.clone(),
    );
    apply(&pvc_api, &staged_pvc, &staged).await?;
    // Same bind gate as the Snapshot tail — a CSI full clone is the SLOWEST staging
    // method there is, so skipping the gate here would reproduce the original bug
    // verbatim for `copyMethod: Clone`.
    if let Some(outcome) =
        staged_pvc_bind_gate(client, &pvc_api, ns, &staged, &staged_pvc, timeout).await?
    {
        return Ok(outcome);
    }
    Ok(StagingOutcome::Ready(StagedSource {
        storage_class_name: staged_class_of(&staged),
        pvc_name: staged_pvc,
        volume_snapshot_name: None,
        volume_group_snapshot_name: None,
        copy_method: "Clone",
        staging_timeout_seconds,
    }))
}

/// The `storageClassName` actually written to a built staged PVC's spec — the single
/// source for what `status.staged.storageClassName` records.
fn staged_class_of(staged: &PersistentVolumeClaim) -> Option<String> {
    staged
        .spec
        .as_ref()
        .and_then(|s| s.storage_class_name.clone())
}

/// Reap the staged objects a backup created (idempotent; 404-tolerant). Deletes the
/// finished mover **pod** first (a completed Job pod still references the staged PVC via
/// `spec.volumes`, so `kubernetes.io/pvc-protection` blocks the PVC's deletion until the
/// pod is gone — but the Job object is left intact so the reconcile's terminal-state
/// detection still sees a completed Job and does NOT re-run the backup). Then patches a
/// bound staged PV `Retain → Delete` (so a `Retain` StorageClass doesn't leak the PV +
/// backend volume), and deletes the staged PVC and the VolumeSnapshot. Safe to call on
/// every terminal reconcile — once the objects are gone it is a no-op.
pub async fn cleanup_staged_source(
    client: &kube::Client,
    ns: &str,
    snapshot_cr: &str,
    group: Option<&kopiur_api::SnapshotSourceGroup>,
) -> Result<()> {
    let staged_pvc = staged_pvc_name(snapshot_cr);
    let vs_name = volume_snapshot_name(snapshot_cr);

    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    if let Some(pvc) = pvc_api.get_opt(&staged_pvc).await? {
        // Release pvc-protection: delete the finished mover pod(s) (the backup Job is
        // named after the Snapshot CR). The Job object itself is preserved. Use
        // list + delete-by-name (not delete_collection), since the controller is granted
        // the `delete`/`list` verbs but NOT the distinct `deletecollection` verb.
        let pod_api: Api<Pod> = Api::namespaced(client.clone(), ns);
        let mover_pods = pod_api
            .list(
                &ListParams::default()
                    .labels(&format!("batch.kubernetes.io/job-name={snapshot_cr}")),
            )
            .await
            .map(|l| l.items)
            .unwrap_or_default();
        for pod in mover_pods {
            if let Some(pod_name) = pod.metadata.name {
                delete_ignore_404(&pod_api, &pod_name).await?;
            }
        }
        // Patch the bound PV to Delete so the underlying volume is reclaimed even on a
        // Retain StorageClass (the #82 OpenEBS-ZFS leak).
        if let Some(pv_name) = cleanup_plan(&pvc).reclaim_pv {
            let pv_api: Api<PersistentVolume> = Api::all(client.clone());
            let patch =
                serde_json::json!({ "spec": { "persistentVolumeReclaimPolicy": "Delete" } });
            match pv_api
                .patch(
                    &pv_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch),
                )
                .await
            {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => return Err(Error::Kube(e)),
            }
        }
        delete_ignore_404(&pvc_api, &staged_pvc).await?;
    }

    // Under `groupBy: VolumeGroupSnapshot` the capture is SHARED, so this
    // member owns no `<cr>-snap` of its own and must not delete the member
    // snapshot directly — the group cascades onto it, and deleting it here
    // would race the as-source-protection ordering the block above exists for.
    // Instead try to reap the group itself; that is a no-op unless every
    // sibling is done, and fails closed if the sibling list cannot be read.
    if let Some(group) = group {
        super::group_staging::cleanup_group_if_unused(
            client,
            &group.namespace,
            &group.volume_group_snapshot_name,
        )
        .await?;
        return Ok(());
    }

    let vs_api = volume_snapshot_api(client, ns);
    if vs_api.get_opt(&vs_name).await?.is_some() {
        delete_ignore_404(&vs_api, &vs_name).await?;
    }
    Ok(())
}

async fn delete_ignore_404<K>(api: &Api<K>, name: &str) -> Result<()>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.delete(name, &DeleteParams::background()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(Error::Kube(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(name: &str, driver: &str, default: bool) -> ClassInfo {
        ClassInfo {
            name: name.into(),
            driver: driver.into(),
            is_default: default,
        }
    }

    // --- decide_class ---

    #[test]
    fn decide_class_explicit_wins_when_present() {
        let classes = [class("csi-a", "ebs.csi.aws.com", false)];
        assert_eq!(
            decide_class(&classes, "ebs.csi.aws.com", Some("csi-a")),
            ClassDecision::Use("csi-a".into())
        );
        assert_eq!(
            decide_class(&classes, "ebs.csi.aws.com", Some("nope")),
            ClassDecision::ExplicitNotFound("nope".into())
        );
    }

    #[test]
    fn decide_class_treats_a_blank_explicit_name_as_unset() {
        let driver = "ebs.csi.aws.com";
        let classes = [class("csi-a", driver, false)];

        // A GitOps-templated `volumeSnapshotClassName: ${VAR}` renders empty when the
        // variable is undefined. No VolumeSnapshotClass can be named "", so an empty or
        // whitespace-only value is "unset", not "an explicit class that is missing" —
        // otherwise the user gets the nonsense message "volumeSnapshotClassName `` does
        // not exist" instead of the auto-selection they asked for.
        for blank in ["", "   ", "\n\t "] {
            assert_eq!(
                decide_class(&classes, driver, Some(blank)),
                ClassDecision::Use("csi-a".into()),
                "blank name {blank:?} should auto-select, not report a missing class"
            );
        }

        // Blank normalizes *before* the explicit branch, so the genuinely-useful
        // "name one explicitly" diagnostic still wins over ExplicitNotFound("").
        let ambiguous = [class("a", driver, false), class("b", driver, false)];
        assert!(matches!(
            decide_class(&ambiguous, driver, Some("")),
            ClassDecision::Ambiguous { .. }
        ));

        // A padded name still resolves, and the padding never reaches the
        // VolumeSnapshot's spec.volumeSnapshotClassName.
        assert_eq!(
            decide_class(&classes, driver, Some("  csi-a  ")),
            ClassDecision::Use("csi-a".into())
        );

        // Regression guard: a real, genuinely-absent name is still reported as missing.
        assert_eq!(
            decide_class(&classes, driver, Some("nope")),
            ClassDecision::ExplicitNotFound("nope".into())
        );
    }

    #[test]
    fn decide_class_single_match_is_used() {
        let classes = [
            class("csi-a", "ebs.csi.aws.com", false),
            class("other", "pd.csi.storage.gke.io", false),
        ];
        assert_eq!(
            decide_class(&classes, "ebs.csi.aws.com", None),
            ClassDecision::Use("csi-a".into())
        );
    }

    #[test]
    fn decide_class_no_match_for_driver() {
        let classes = [class("csi-a", "ebs.csi.aws.com", false)];
        assert_eq!(
            decide_class(&classes, "zfs.csi.openebs.io", None),
            ClassDecision::NoneForDriver("zfs.csi.openebs.io".into())
        );
        // Empty cluster.
        assert_eq!(
            decide_class(&[], "any", None),
            ClassDecision::NoneForDriver("any".into())
        );
    }

    #[test]
    fn decide_class_many_picks_unique_default_else_ambiguous() {
        let driver = "rbd.csi.ceph.com";
        let many = [
            class("a", driver, false),
            class("b", driver, true),
            class("c", driver, false),
        ];
        assert_eq!(
            decide_class(&many, driver, None),
            ClassDecision::Use("b".into())
        );
        // No default among several → ambiguous.
        let no_default = [class("a", driver, false), class("b", driver, false)];
        match decide_class(&no_default, driver, None) {
            ClassDecision::Ambiguous { candidates, .. } => {
                assert_eq!(candidates, vec!["a".to_string(), "b".to_string()])
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        // Two defaults → also ambiguous (no unique default).
        let two_defaults = [class("a", driver, true), class("b", driver, true)];
        assert!(matches!(
            decide_class(&two_defaults, driver, None),
            ClassDecision::Ambiguous { .. }
        ));
    }

    // --- staged_child_name ---

    #[test]
    fn staged_names_are_deterministic_and_bounded() {
        assert_eq!(volume_snapshot_name("db"), "db-snap");
        assert_eq!(staged_pvc_name("db"), "db-src");
        let long = "a".repeat(80);
        let n = staged_pvc_name(&long);
        assert!(n.len() <= MAX_NAME_LEN, "bounded to 63: {} ", n.len());
        assert!(n.ends_with("-src"));
        // Deterministic.
        assert_eq!(staged_pvc_name(&long), n);
    }

    // --- data_source_for ---

    #[test]
    fn data_source_snapshot_vs_clone() {
        let snap = data_source_for(CopyMethod::Snapshot, "src", "vs");
        assert_eq!(snap.api_group.as_deref(), Some(SNAPSHOT_GROUP));
        assert_eq!(snap.kind, "VolumeSnapshot");
        assert_eq!(snap.name, "vs");
        let clone = data_source_for(CopyMethod::Clone, "src", "vs");
        assert_eq!(clone.api_group, None);
        assert_eq!(clone.kind, "PersistentVolumeClaim");
        assert_eq!(clone.name, "src");
    }

    // --- staged_pvc_size: max(restoreSize, source) with restoreSize 0/absent ---

    #[test]
    fn staged_size_takes_the_larger() {
        assert_eq!(
            staged_pvc_size(Some("20Gi"), Some("10Gi")).as_deref(),
            Some("20Gi")
        );
        assert_eq!(
            staged_pvc_size(Some("5Gi"), Some("10Gi")).as_deref(),
            Some("10Gi")
        );
        // restoreSize 0/absent → fall back to source.
        assert_eq!(
            staged_pvc_size(Some("0"), Some("10Gi")).as_deref(),
            Some("10Gi")
        );
        assert_eq!(staged_pvc_size(None, Some("10Gi")).as_deref(), Some("10Gi"));
        // Neither known → let the provisioner default.
        assert_eq!(staged_pvc_size(None, None), None);
    }

    // --- build_staged_pvc copies source shape + dataSource ---

    #[test]
    fn staged_pvc_copies_source_and_sets_data_source() {
        let source = PersistentVolumeClaim {
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".into()]),
                storage_class_name: Some("fast".into()),
                volume_mode: Some("Filesystem".into()),
                resources: Some(VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "storage".into(),
                        Quantity("10Gi".into()),
                    )])),
                    limits: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ds = data_source_for(CopyMethod::Snapshot, "src", "vs");
        let owner = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference::default();
        // Regression pin: no staging overrides ⇒ the source's shape is copied verbatim.
        let staged = build_staged_pvc("db-src", "ns", &source, ds, Some("0"), None, owner);
        let spec = staged.spec.unwrap();
        assert_eq!(
            spec.access_modes.as_deref(),
            Some(["ReadWriteOnce".to_string()].as_slice())
        );
        assert_eq!(spec.storage_class_name.as_deref(), Some("fast"));
        assert_eq!(spec.volume_mode.as_deref(), Some("Filesystem"));
        // restoreSize 0 → falls back to the source's 10Gi request.
        assert_eq!(
            spec.resources
                .unwrap()
                .requests
                .unwrap()
                .get("storage")
                .unwrap()
                .0,
            "10Gi"
        );
        let ds = spec.data_source.unwrap();
        assert_eq!(ds.kind, "VolumeSnapshot");
        assert_eq!(ds.name, "vs");
    }

    /// A source PVC with the common `fast`/RWO/Filesystem shape, for the override tests.
    fn override_test_source() -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".into()]),
                storage_class_name: Some("fast".into()),
                volume_mode: Some("Filesystem".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    // --- staging applicability ordering ------------------------------------

    #[test]
    fn nfs_source_is_not_applicable_even_on_a_namespaced_install() {
        // The bug this reorder fixes. The namespaced-install refusal used to run
        // BEFORE the source-shape checks, so an NFS-source policy under the
        // DEFAULTED `copyMethod: Snapshot` failed terminally on a namespaced
        // install — for a source that never needed a cluster-scoped read.
        assert_eq!(
            staging_applicability(CopyMethod::Snapshot, None, true),
            StagingApplicability::NotApplicable
        );
        assert_eq!(
            staging_applicability(CopyMethod::Clone, None, true),
            StagingApplicability::NotApplicable
        );
    }

    #[test]
    fn direct_is_not_applicable_regardless_of_scope_or_source() {
        for namespaced in [true, false] {
            assert_eq!(
                staging_applicability(CopyMethod::Direct, Some("data"), namespaced),
                StagingApplicability::NotApplicable
            );
            assert_eq!(
                staging_applicability(CopyMethod::Direct, None, namespaced),
                StagingApplicability::NotApplicable
            );
        }
    }

    #[test]
    fn a_pvc_source_on_a_namespaced_install_still_needs_cluster_scope() {
        // The gate is not removed, only moved: a source that genuinely needs the
        // cluster-scoped StorageClass / VolumeSnapshotClass / PersistentVolume
        // reads must still be refused with the actionable message rather than
        // wedging the reconcile on retried 403s.
        assert_eq!(
            staging_applicability(CopyMethod::Snapshot, Some("data"), true),
            StagingApplicability::ClusterScopeRequired
        );
    }

    #[test]
    fn a_pvc_source_on_a_cluster_install_proceeds() {
        assert_eq!(
            staging_applicability(CopyMethod::Snapshot, Some("data"), false),
            StagingApplicability::Proceed {
                source_name: "data"
            }
        );
    }

    #[test]
    fn staged_pvc_honors_staging_overrides() {
        use kopiur_api::common::PvcAccessMode;
        let owner = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference::default();
        let source = override_test_source();

        // Both overrides set — the CephFS shallow-clone recipe.
        let staging = StagingSpec {
            storage_class_name: Some("cephfs-shallow".into()),
            access_modes: vec![PvcAccessMode::ReadOnlyMany],
            ..Default::default()
        };
        let ds = data_source_for(CopyMethod::Snapshot, "src", "vs");
        let staged = build_staged_pvc("db-src", "ns", &source, ds, None, Some(&staging), owner);
        let spec = staged.spec.as_ref().unwrap();
        assert_eq!(spec.storage_class_name.as_deref(), Some("cephfs-shallow"));
        assert_eq!(
            spec.access_modes.as_deref(),
            Some(["ReadOnlyMany".to_string()].as_slice())
        );
        // volumeMode has no override — always copied from the source.
        assert_eq!(spec.volume_mode.as_deref(), Some("Filesystem"));
        // `staged_class_of` (the status.staged.storageClassName source) sees the override.
        assert_eq!(staged_class_of(&staged).as_deref(), Some("cephfs-shallow"));
    }

    #[test]
    fn staged_pvc_overrides_are_independent_and_empty_modes_inherit() {
        use kopiur_api::common::PvcAccessMode;
        let owner = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference::default();
        let source = override_test_source();

        // Class-only override: modes still inherited.
        let staging = StagingSpec {
            storage_class_name: Some("cephfs-shallow".into()),
            ..Default::default()
        };
        let ds = data_source_for(CopyMethod::Clone, "src", "vs");
        let staged = build_staged_pvc(
            "db-src",
            "ns",
            &source,
            ds,
            None,
            Some(&staging),
            owner.clone(),
        );
        let spec = staged.spec.as_ref().unwrap();
        assert_eq!(spec.storage_class_name.as_deref(), Some("cephfs-shallow"));
        assert_eq!(
            spec.access_modes.as_deref(),
            Some(["ReadWriteOnce".to_string()].as_slice()),
            "unset accessModes must inherit the source's"
        );

        // Modes-only override: class still inherited. An empty modes vec = no override.
        let staging = StagingSpec {
            access_modes: vec![PvcAccessMode::ReadOnlyMany],
            ..Default::default()
        };
        let ds = data_source_for(CopyMethod::Clone, "src", "vs");
        let staged = build_staged_pvc("db-src", "ns", &source, ds, None, Some(&staging), owner);
        let spec = staged.spec.as_ref().unwrap();
        assert_eq!(
            spec.storage_class_name.as_deref(),
            Some("fast"),
            "unset storageClassName must inherit the source's"
        );
        assert_eq!(
            spec.access_modes.as_deref(),
            Some(["ReadOnlyMany".to_string()].as_slice())
        );
    }

    // --- staged_class_preflight: fail fast on a missing / wrong-driver override ---

    #[test]
    fn staged_class_preflight_passes_matching_and_unknown_source_driver() {
        // Same driver → no complaint.
        assert_eq!(
            staged_class_preflight(
                "cephfs-shallow",
                Some("cephfs.csi.ceph.com"),
                Some("cephfs.csi.ceph.com")
            ),
            None
        );
        // Unknown source driver (e.g. Clone from a classless PVC) → can't compare,
        // don't block; a real mismatch then surfaces at the bind deadline.
        assert_eq!(
            staged_class_preflight("cephfs-shallow", Some("cephfs.csi.ceph.com"), None),
            None
        );
    }

    #[test]
    fn staged_class_preflight_rejects_missing_class_and_wrong_driver() {
        let (reason, msg) =
            staged_class_preflight("nope", None, Some("cephfs.csi.ceph.com")).unwrap();
        assert_eq!(reason, REASON_STAGED_CLASS_NOT_FOUND);
        assert!(msg.contains("`nope`"), "{msg}");
        assert!(msg.contains("does not exist"), "{msg}");

        let (reason, msg) = staged_class_preflight(
            "ebs-class",
            Some("ebs.csi.aws.com"),
            Some("cephfs.csi.ceph.com"),
        )
        .unwrap();
        assert_eq!(reason, REASON_STAGED_CLASS_MISMATCH);
        assert!(msg.contains("ebs.csi.aws.com"), "{msg}");
        assert!(msg.contains("cephfs.csi.ceph.com"), "{msg}");
        assert!(
            msg.contains("would never bind"),
            "the why must be spelled out: {msg}"
        );
    }

    // --- cleanup_plan ---

    #[test]
    fn cleanup_plan_only_reclaims_a_bound_pv() {
        let mut pvc = PersistentVolumeClaim {
            spec: Some(PersistentVolumeClaimSpec {
                volume_name: Some("pv-123".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(cleanup_plan(&pvc).reclaim_pv.as_deref(), Some("pv-123"));
        // Unbound (no volumeName) → nothing to reclaim.
        pvc.spec.as_mut().unwrap().volume_name = None;
        assert_eq!(cleanup_plan(&pvc).reclaim_pv, None);
    }

    // --- Failed message text: what/why/fix + the copyMethod-defaulted pointer ---

    #[test]
    fn stack_missing_message_names_driver_and_default_pointer() {
        let msg = stack_missing_message("ebs.csi.aws.com");
        assert!(
            msg.contains("VolumeSnapshotClass API"),
            "should say what's missing: {msg}"
        );
        assert!(
            msg.contains("external-snapshotter"),
            "should say the fix (install the stack): {msg}"
        );
        assert!(
            msg.contains("driver `ebs.csi.aws.com`"),
            "should name the source's driver: {msg}"
        );
        assert!(
            msg.contains("copyMethod defaulted to Snapshot as of this release"),
            "should explain the new default: {msg}"
        );
        assert!(
            msg.contains("set spec.copyMethod: Direct on the SnapshotPolicy"),
            "should give the exact fix: {msg}"
        );
    }

    #[test]
    fn source_not_csi_message_has_default_pointer() {
        let msg = source_not_csi_message("backups", "app-data");
        assert!(msg.contains("backups/app-data"));
        assert!(msg.contains("no StorageClass / CSI provisioner"));
        assert!(msg.contains("copyMethod defaulted to Snapshot as of this release"));
    }

    #[test]
    fn source_pvc_not_found_message_has_default_pointer() {
        let msg = source_pvc_not_found_message("backups", "app-data", CopyMethod::Snapshot);
        assert!(msg.contains("backups/app-data"));
        assert!(msg.contains("was not found"));
        assert!(msg.contains("copyMethod defaulted to Snapshot as of this release"));
    }

    #[test]
    fn explicit_class_not_found_message_has_default_pointer() {
        let msg = explicit_class_not_found_message("csi-missing", "ebs.csi.aws.com");
        assert!(msg.contains("volumeSnapshotClassName `csi-missing` does not exist"));
        assert!(msg.contains("driver `ebs.csi.aws.com`"));
        assert!(msg.contains("copyMethod defaulted to Snapshot as of this release"));
    }

    #[test]
    fn no_class_for_driver_message_has_default_pointer() {
        let msg = no_class_for_driver_message("zfs.csi.openebs.io");
        assert!(msg.contains("no VolumeSnapshotClass has driver `zfs.csi.openebs.io`"));
        assert!(msg.contains(DEFAULT_CLASS_ANNOTATION));
        assert!(msg.contains("copyMethod defaulted to Snapshot as of this release"));
    }

    // --- vs_wait_outcome: the issue-#198 decision (error ≠ failure; deadline is) ---

    fn t0() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-04T18:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn obs(ready: bool, error: Option<&str>, age: Option<std::time::Duration>) -> VsObservation {
        VsObservation {
            ready,
            restore_size: Some("5Gi".into()),
            error: error.map(str::to_string),
            created_at: age.map(|a| t0() - chrono::Duration::from_std(a).unwrap()),
        }
    }

    const TEN_MIN: Option<std::time::Duration> = Some(std::time::Duration::from_secs(600));
    const CONFLICT_409: &str = "the object has been modified; please apply your changes \
        to the latest version and try again";

    fn decide(o: &VsObservation, timeout: Option<std::time::Duration>) -> VsWait {
        vs_wait_outcome(
            o,
            "selfhosted",
            "karakeep-snap",
            "openebs-snapshots",
            timeout,
            t0(),
        )
    }

    #[test]
    fn vs_error_on_first_sight_is_waiting_not_failed() {
        // THE #198 regression: a transient snapshot-controller 409 observed while the
        // VolumeSnapshot is seconds old must wait, never terminally fail the backup.
        let o = obs(
            false,
            Some(CONFLICT_409),
            Some(std::time::Duration::from_secs(1)),
        );
        match decide(&o, TEN_MIN) {
            VsWait::Waiting(msg) => {
                assert!(
                    msg.contains(CONFLICT_409),
                    "error surfaced for diagnosis: {msg}"
                );
                assert!(
                    msg.contains("possibly-transient") && msg.contains("retries automatically"),
                    "framed as transient, not fatal: {msg}"
                );
                assert!(
                    msg.contains("staging fails at 2026-07-04T18:39:59Z"),
                    "names the (deterministic) deadline: {msg}"
                );
            }
            other => panic!("a fresh VS error must be Waiting, got {other:?}"),
        }
    }

    #[test]
    fn ready_wins_over_error_and_deadline() {
        // readyToUse: true is a usable snapshot even if a stale error rides along or
        // the deadline has passed (e.g. the operator was down while it recovered).
        let o = obs(
            true,
            Some(CONFLICT_409),
            Some(std::time::Duration::from_secs(3600)),
        );
        assert_eq!(
            decide(&o, TEN_MIN),
            VsWait::Ready {
                restore_size: Some("5Gi".into())
            }
        );
    }

    #[test]
    fn persistent_error_fails_only_at_the_deadline() {
        let age = |s| Some(std::time::Duration::from_secs(s));
        // One second before the deadline: still waiting.
        assert!(matches!(
            decide(&obs(false, Some(CONFLICT_409), age(599)), TEN_MIN),
            VsWait::Waiting(_)
        ));
        // At/after the deadline: terminal, with the error + the knob in the message.
        match decide(&obs(false, Some(CONFLICT_409), age(600)), TEN_MIN) {
            VsWait::Failed { reason, message } => {
                assert_eq!(reason, REASON_VS_FAILED);
                assert!(message.contains(CONFLICT_409), "{message}");
                assert!(message.contains("within 10m"), "{message}");
                assert!(message.contains("spec.staging.timeout"), "{message}");
                assert!(
                    message.contains("VolumeSnapshotClass `openebs-snapshots`"),
                    "{message}"
                );
                assert!(
                    message.contains("next scheduled run (or a new Snapshot) retries"),
                    "terminal semantics must be honest: {message}"
                );
            }
            other => panic!("expected Failed at the deadline, got {other:?}"),
        }
    }

    #[test]
    fn silent_not_ready_times_out_with_distinct_reason() {
        // No error, never ready → StagingTimedOut (stuck driver), not VolumeSnapshotFailed.
        match decide(
            &obs(false, None, Some(std::time::Duration::from_secs(601))),
            TEN_MIN,
        ) {
            VsWait::Failed { reason, message } => {
                assert_eq!(reason, REASON_STAGING_TIMEOUT);
                assert!(message.contains("reported no error"), "{message}");
                assert!(message.contains("spec.staging.timeout"), "{message}");
            }
            other => panic!("expected StagingTimedOut, got {other:?}"),
        }
    }

    #[test]
    fn zero_timeout_waits_indefinitely() {
        // spec.staging.timeout: "0" resolves to None → never expires, even with a
        // persistent error and an ancient VS.
        let o = obs(
            false,
            Some(CONFLICT_409),
            Some(std::time::Duration::from_secs(864000)),
        );
        match decide(&o, None) {
            VsWait::Waiting(msg) => {
                assert!(msg.contains("waiting indefinitely"), "{msg}")
            }
            other => panic!("indefinite timeout must never fail, got {other:?}"),
        }
    }

    #[test]
    fn missing_creation_timestamp_is_not_expired() {
        // Defensive: a read racing the SSA create (no persisted creationTimestamp yet)
        // must count as "just started", not instantly expired.
        assert!(matches!(
            decide(&obs(false, Some(CONFLICT_409), None), TEN_MIN),
            VsWait::Waiting(_)
        ));
    }

    #[test]
    fn fmt_go_duration_echoes_crd_grammar() {
        let d = std::time::Duration::from_secs;
        assert_eq!(fmt_go_duration(d(600)), "10m");
        assert_eq!(fmt_go_duration(d(3600)), "1h");
        assert_eq!(fmt_go_duration(d(90)), "90s");
        assert_eq!(fmt_go_duration(d(0)), "0s");
    }

    // --- build_volume_snapshot wire shape ---

    #[test]
    fn volume_snapshot_has_source_and_class() {
        let owner = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference::default();
        let vs = build_volume_snapshot("db-snap", "ns", "db", Some("csi-class"), owner);
        assert_eq!(vs.data["spec"]["source"]["persistentVolumeClaimName"], "db");
        assert_eq!(vs.data["spec"]["volumeSnapshotClassName"], "csi-class");
        // Omitting the class leaves it unset (controller auto-selects the default).
        let owner = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference::default();
        let vs2 = build_volume_snapshot("db-snap", "ns", "db", None, owner);
        assert!(vs2.data["spec"].get("volumeSnapshotClassName").is_none());
    }

    // --- effective_binding_mode: unset ⇒ Immediate (the k8s API default) ---

    #[test]
    fn binding_mode_defaults_to_immediate() {
        assert_eq!(effective_binding_mode(None), BindingMode::Immediate);
        assert_eq!(
            effective_binding_mode(Some("Immediate")),
            BindingMode::Immediate
        );
        assert_eq!(
            effective_binding_mode(Some("WaitForFirstConsumer")),
            BindingMode::WaitForFirstConsumer
        );
    }

    // --- pvc_bind_outcome: mirrors the vs_wait_outcome decision matrix ---

    fn pvc_obs(phase: Option<&str>, age: Option<std::time::Duration>) -> StagedPvcObservation {
        StagedPvcObservation {
            phase: phase.map(str::to_string),
            created_at: age.map(|a| t0() - chrono::Duration::from_std(a).unwrap()),
        }
    }

    fn bind_decide(o: &StagedPvcObservation, timeout: Option<std::time::Duration>) -> PvcBindWait {
        pvc_bind_outcome(
            o,
            "selfhosted",
            "forgejo-src",
            Some("cephfs"),
            timeout,
            t0(),
        )
    }

    #[test]
    fn bound_wins_even_past_the_deadline() {
        // Operator down while a slow bind completed: a Bound PVC is a usable stage,
        // never a terminal failure (mirrors vs_wait_outcome's ready-first rule).
        let o = pvc_obs(Some("Bound"), Some(std::time::Duration::from_secs(3600)));
        assert_eq!(bind_decide(&o, TEN_MIN), PvcBindWait::Bound);
    }

    #[test]
    fn pending_within_deadline_waits_with_the_fixed_deadline_named() {
        let o = pvc_obs(Some("Pending"), Some(std::time::Duration::from_secs(1)));
        match bind_decide(&o, TEN_MIN) {
            PvcBindWait::Waiting(msg) => {
                assert!(msg.contains("selfhosted/forgejo-src"), "{msg}");
                assert!(msg.contains("StorageClass `cephfs`"), "{msg}");
                assert!(
                    msg.contains("staging fails at 2026-07-04T18:39:59Z"),
                    "deterministic deadline (no now()) so status doesn't churn: {msg}"
                );
            }
            other => panic!("a fresh Pending PVC must be Waiting, got {other:?}"),
        }
    }

    #[test]
    fn pending_past_deadline_fails_with_the_knob_and_the_shallow_clone_fix() {
        // THE forgejo/CephFS scenario: an 8.5-minute full subvolume clone with a
        // bounded budget must become an actionable terminal failure — not a
        // MoverPodWedged that deletes the VolumeSnapshot mid-restore.
        let o = pvc_obs(Some("Pending"), Some(std::time::Duration::from_secs(600)));
        match bind_decide(&o, TEN_MIN) {
            PvcBindWait::Failed { reason, message } => {
                assert_eq!(reason, REASON_STAGED_PVC_BIND_TIMEOUT);
                assert!(message.contains("within 10m"), "{message}");
                assert!(message.contains("spec.staging.timeout"), "{message}");
                assert!(
                    message.contains("backingSnapshot") && message.contains("storageClassName"),
                    "the CephFS shallow-clone fix must be named: {message}"
                );
                assert!(
                    message.contains("next scheduled run (or a new Snapshot) retries"),
                    "{message}"
                );
            }
            other => panic!("expected Failed at the deadline, got {other:?}"),
        }
        // One second earlier: still waiting.
        let o = pvc_obs(Some("Pending"), Some(std::time::Duration::from_secs(599)));
        assert!(matches!(bind_decide(&o, TEN_MIN), PvcBindWait::Waiting(_)));
    }

    #[test]
    fn lost_pvc_is_terminal_immediately() {
        // No point waiting out the deadline: the bound PV vanished, the stage can
        // never recover.
        let o = pvc_obs(Some("Lost"), Some(std::time::Duration::from_secs(1)));
        match bind_decide(&o, TEN_MIN) {
            PvcBindWait::Failed { reason, message } => {
                assert_eq!(reason, REASON_STAGED_PVC_LOST);
                assert!(message.contains("Lost"), "{message}");
            }
            other => panic!("Lost must be terminal on sight, got {other:?}"),
        }
    }

    #[test]
    fn missing_creation_timestamp_and_zero_timeout_never_expire() {
        // Just-created / not-yet-persisted read: no anchor, not expired.
        let o = pvc_obs(Some("Pending"), None);
        assert!(matches!(bind_decide(&o, TEN_MIN), PvcBindWait::Waiting(_)));
        // spec.staging.timeout: 0 → indefinite, message says so.
        let o = pvc_obs(
            Some("Pending"),
            Some(std::time::Duration::from_secs(86_400)),
        );
        match bind_decide(&o, None) {
            PvcBindWait::Waiting(msg) => {
                assert!(msg.contains("waiting indefinitely"), "{msg}");
            }
            other => panic!("no timeout must wait forever, got {other:?}"),
        }
    }

    #[test]
    fn bind_message_without_a_known_class_stays_readable() {
        let o = pvc_obs(Some("Pending"), Some(std::time::Duration::from_secs(1)));
        match pvc_bind_outcome(&o, "ns", "db-src", None, TEN_MIN, t0()) {
            PvcBindWait::Waiting(msg) => {
                assert!(msg.contains("its StorageClass"), "{msg}");
            }
            other => panic!("expected Waiting, got {other:?}"),
        }
    }

    // --- staged_pvc_observation extraction ---

    #[test]
    fn staged_pvc_observation_reads_phase_and_creation() {
        use k8s_openapi::api::core::v1::PersistentVolumeClaimStatus;
        let pvc = PersistentVolumeClaim {
            status: Some(PersistentVolumeClaimStatus {
                phase: Some("Pending".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let obs = staged_pvc_observation(&pvc);
        assert_eq!(obs.phase.as_deref(), Some("Pending"));
        // No creationTimestamp on a fresh object → no deadline anchor.
        assert_eq!(obs.created_at, None);
    }
}
