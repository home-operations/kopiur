//! CSI **VolumeGroupSnapshot** staging: one crash-consistent capture shared by
//! every member of a `pvcSelector` expansion (`groupBy: VolumeGroupSnapshot`).
//!
//! # What is shared and what is not
//!
//! Grouping replaces only the **capture** half of staging. Single-PVC staging
//! does: pick a class → create a `VolumeSnapshot` → wait `readyToUse` → build a
//! staged PVC from it → gate on bind. Under grouping the first three become
//! "create/wait ONE shared `VolumeGroupSnapshot`, then find *my* member
//! `VolumeSnapshot` within it", and the last two are reused byte-for-byte —
//! every member still gets its own private `<cr>-src` PVC restored from its own
//! private member snapshot.
//!
//! That is what makes this tractable. Sharing the staged PVC instead would
//! re-open #103 (reaping `-src` under an Active Job strands an unschedulable
//! replacement pod), #82 (the `Retain` PV reclaim leak), #198 (a transient
//! `status.error` must never be fatal on sight) and the CephFS bind-timeout
//! handling all at once.
//!
//! # The VolumeGroupSnapshot has no ownerReferences
//!
//! Deliberately, after rejecting every owner:
//!
//! * **The `SnapshotSchedule`** — a `kubectl kopiur snapshot now` run has none;
//!   it is long-lived so ownerRef GC would never fire and bounds nothing; and it
//!   is not in the staging path (staging must run *after* `beforeSnapshot`
//!   hooks, which is the entire point of quiescing before a capture).
//! * **A "leader" member `Snapshot`** — actively dangerous. Members are deleted
//!   on schedules the group knows nothing about (GFS prune, history prune,
//!   `kubectl delete`). Cascading GC from the leader would delete the member
//!   `VolumeSnapshot`s while siblings are still mid-CSI-restore off them.
//! * **All N as non-controller ownerRefs** — semantically ideal, but
//!   `io::apply` uses one shared field manager with `.force()`, so member B's
//!   apply strips A's ownerRef.
//! * **A new internal CR** — a ninth CRD, reconciler, finalizer and RBAC
//!   surface for a refcount already obtainable from a label LIST.
//!
//! So lifetime is governed explicitly: any member SSA-creates the group over a
//! name pinned in its own `spec.source.group` (convergent, no leader);
//! [`GROUP_LABEL`](kopiur_api::consts::GROUP_LABEL) joins members to it; and
//! [`cleanup_group_if_unused`] reaps it only when no sibling still needs it,
//! **failing closed** on a bad read. `sweep` carries the leak backstop.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::PersistentVolume;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{DeleteParams, ListParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, ResourceExt};

use kopiur_api::consts::GROUP_LABEL;
use kopiur_api::{Snapshot, SnapshotPhase};

use super::apply::apply;
use super::child_labels;
use crate::error::Result;

/// The `groupsnapshot.storage.k8s.io` API group.
pub const GROUP_SNAPSHOT_GROUP: &str = "groupsnapshot.storage.k8s.io";

/// The served version kopiur targets.
///
/// `v1beta1` is what external-snapshotter 8.2 (the release that made group
/// snapshots **Beta**, alongside Kubernetes 1.32) serves.
pub const GROUP_SNAPSHOT_VERSION: &str = "v1beta1";

/// `app.kubernetes.io/component` value on a shared group object.
pub const GROUP_COMPONENT: &str = "staged-source-group";

/// `ApiResource` for `VolumeGroupSnapshot` (namespaced).
pub fn volume_group_snapshot_ar() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(
            GROUP_SNAPSHOT_GROUP,
            GROUP_SNAPSHOT_VERSION,
            "VolumeGroupSnapshot",
        ),
        "volumegroupsnapshots",
    )
}

/// `ApiResource` for `VolumeGroupSnapshotClass` (cluster-scoped).
pub fn volume_group_snapshot_class_ar() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(
            GROUP_SNAPSHOT_GROUP,
            GROUP_SNAPSHOT_VERSION,
            "VolumeGroupSnapshotClass",
        ),
        "volumegroupsnapshotclasses",
    )
}

fn vgs_api(client: &kube::Client, ns: &str) -> Api<DynamicObject> {
    Api::namespaced_with(client.clone(), ns, &volume_group_snapshot_ar())
}

// ---------------------------------------------------------------------------
// Pure builders
// ---------------------------------------------------------------------------

/// The shared `VolumeGroupSnapshot` name for one expansion.
///
/// Derived from the per-invocation base name (the slot stamp), NOT from any one
/// member: every member must compute the identical name without talking to its
/// siblings, which is what makes the racing creates converge.
pub fn group_name(base: &str) -> String {
    super::staging::staged_child_name(base, "grp")
}

/// Build the shared `VolumeGroupSnapshot`.
///
/// `selector` is the policy's own `pvcSelector` label selector, passed through
/// verbatim. Note the snapshot-controller re-evaluates it **at create time**, so
/// the captured set can differ from what the expander matched moments earlier —
/// [`map_group_members`] is where that shows up, as a named per-member failure
/// rather than a silent omission.
pub fn build_volume_group_snapshot(
    name: &str,
    ns: &str,
    selector: &k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector,
    class: &str,
    group: &str,
) -> DynamicObject {
    // The GROUP_LABEL is the join key. Without it the sibling-liveness reaper
    // and the sweep backstop have nothing to select on, and the group leaks
    // forever — this object has no ownerReferences to fall back on.
    let labels = child_labels(&[
        ("app.kubernetes.io/component", GROUP_COMPONENT),
        (GROUP_LABEL, group),
    ]);
    // `DynamicObject::new` (NOT a struct literal) so `types` carries the
    // TypeMeta. A DynamicObject with `types: None` serializes a body with no
    // apiVersion/kind, and a server-side apply of that is rejected with
    // `400 invalid object type: /, Kind=` — a message that names neither the
    // object nor the missing field, and which the reconciler classifies as
    // TRANSIENT, so the member requeues forever instead of failing. Same
    // construction as `staging::build_volume_snapshot`.
    let mut obj = DynamicObject::new(name, &volume_group_snapshot_ar())
        .within(ns)
        .data(serde_json::json!({
            "spec": {
                "volumeGroupSnapshotClassName": class,
                "source": { "selector": selector },
            }
        }));
    // No ownerReferences — see the module docs.
    obj.metadata.labels = Some(labels);
    obj
}

// ---------------------------------------------------------------------------
// Readiness
// ---------------------------------------------------------------------------

/// The fields of a `VolumeGroupSnapshot` this module reasons about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VgsObservation {
    /// `status.readyToUse`.
    pub ready: bool,
    /// `status.error.message`, if any.
    pub error: Option<String>,
    /// `metadata.creationTimestamp`, the deadline anchor.
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The outcome of one group-readiness observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VgsWait {
    /// The group is `readyToUse`; resolve members and stage.
    Ready,
    /// Not ready and not past the deadline — requeue.
    Waiting(String),
    /// Terminal.
    Failed {
        /// kstatus condition reason.
        reason: &'static str,
        /// Actionable what/why/fix message.
        message: String,
    },
}

/// Decide the wait outcome for one `VolumeGroupSnapshot` observation.
///
/// A structural clone of [`super::staging::vs_wait_outcome`], preserving all
/// three of its hard-won rules:
///
/// 1. `readyToUse` wins first, over both the error and the deadline.
/// 2. A `status.error` is **never** fatal on sight (#198) — the
///    snapshot-controller sets it transiently during benign retries and clears
///    it. It is surfaced as diagnostic context inside the `Waiting` message.
/// 3. Only `created_at + timeout` fails, and a missing `created_at` is a
///    just-created read, never expired.
///
/// The deadline anchors on the **group's own** `creationTimestamp`, so all N
/// members compute the identical deadline and emit a byte-identical message
/// with no coordination — which is what makes their failure consistent and
/// keeps `patch_status_if_changed` from churning.
pub fn vgs_wait_outcome(
    obs: &VgsObservation,
    ns: &str,
    name: &str,
    class: &str,
    timeout: Option<std::time::Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> VgsWait {
    if obs.ready {
        return VgsWait::Ready;
    }
    let deadline = match (timeout, obs.created_at) {
        (Some(t), Some(created)) => chrono::Duration::from_std(t).ok().map(|d| created + d),
        _ => None,
    };
    if !deadline.is_some_and(|d| now >= d) {
        let mut msg = format!("waiting for VolumeGroupSnapshot `{ns}/{name}` to become readyToUse");
        if let Some(err) = &obs.error {
            msg.push_str(&format!(
                "; it reported a possibly-transient error (the snapshot-controller \
                 retries automatically): {err}"
            ));
        }
        match deadline {
            Some(d) => msg.push_str(&format!(
                "; staging fails at {} if still not ready",
                d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            )),
            None => msg.push_str("; waiting indefinitely (spec.staging.timeout: 0)"),
        }
        return VgsWait::Waiting(msg);
    }
    let waited = super::staging::fmt_go_duration_pub(timeout.expect("expired implies a timeout"));
    match &obs.error {
        Some(err) => VgsWait::Failed {
            reason: crate::consts::REASON_VGS_FAILED,
            message: format!(
                "VolumeGroupSnapshot `{ns}/{name}` did not become readyToUse within {waited} \
                 (spec.staging.timeout; default 10m) and last reported: {err}; check the \
                 VolumeGroupSnapshotClass `{class}` / CSI driver, or set `groupBy: None` to \
                 capture each PVC independently. Every member Snapshot of this group is \
                 terminal — the next scheduled run retries"
            ),
        },
        None => VgsWait::Failed {
            reason: crate::consts::REASON_GROUP_STAGING_TIMEOUT,
            message: format!(
                "VolumeGroupSnapshot `{ns}/{name}` did not become readyToUse within {waited} \
                 (spec.staging.timeout; default 10m) and reported no error — the CSI driver or \
                 snapshot-controller is stuck or very slow. Check both, raise \
                 spec.staging.timeout, or set `groupBy: None` to capture each PVC \
                 independently. Every member Snapshot of this group is terminal — the next \
                 scheduled run retries"
            ),
        },
    }
}

/// Read the group's observable state. `None` when the object is not there yet
/// (an SSA still propagating), which the caller treats as "keep waiting".
pub async fn read_group(
    client: &kube::Client,
    ns: &str,
    name: &str,
) -> Result<Option<VgsObservation>> {
    let Some(obj) = vgs_api(client, ns).get_opt(name).await? else {
        return Ok(None);
    };
    let status = obj.data.get("status");
    Ok(Some(VgsObservation {
        ready: status
            .and_then(|s| s.get("readyToUse"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        error: status
            .and_then(|s| s.get("error"))
            .and_then(|e| e.get("message"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        created_at: obj
            .metadata
            .creation_timestamp
            .as_ref()
            .and_then(|t| chrono::DateTime::from_timestamp(t.0.as_second(), 0)),
    }))
}

// ---------------------------------------------------------------------------
// Member → PVC mapping
// ---------------------------------------------------------------------------

/// One member `VolumeSnapshot` of a group, as far as the mapping cares.
// No `Eq`: `Quantity` is `PartialEq` only (the k8s-openapi rule in CLAUDE.md).
#[derive(Debug, Clone, PartialEq)]
pub struct MemberVs {
    /// The member's `metadata.name`.
    pub name: String,
    /// `status.volumeGroupSnapshotName` — the back-pointer to its group.
    pub group: Option<String>,
    /// `status.boundVolumeSnapshotContentName`.
    pub content: Option<String>,
    /// `status.restoreSize`, used to size the staged PVC.
    pub restore_size: Option<Quantity>,
}

/// A `VolumeSnapshotContent`, reduced to the one field that identifies a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentInfo {
    /// `metadata.name`.
    pub name: String,
    /// `spec.source.volumeHandle` — the CSI `volume_id` the snapshot came from.
    pub volume_handle: Option<String>,
}

/// A `PersistentVolume`, reduced to the CSI handle and the claim it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvInfo {
    /// `spec.csi.volumeHandle`.
    pub volume_handle: String,
    /// `spec.claimRef.namespace`.
    pub claim_namespace: String,
    /// `spec.claimRef.name`.
    pub claim_name: String,
}

/// The member snapshot that captured one PVC.
// No `Eq`: see `MemberVs`.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupMember {
    /// The member `VolumeSnapshot`'s name — what the staged PVC restores from.
    pub volume_snapshot_name: String,
    /// Its `restoreSize`, if reported.
    pub restore_size: Option<Quantity>,
}

/// **Pure.** Map each member `VolumeSnapshot` of `group` to the PVC it captured.
///
/// `v1beta1`'s `VolumeGroupSnapshotStatus` carries only
/// `boundVolumeGroupSnapshotContentName`, `creationTime`, `readyToUse` and
/// `error` — there is **no member list** (and `v1beta2` does not add one; it
/// moves the information onto the *content*). So the mapping is reconstructed
/// in three hops:
///
/// 1. member `VolumeSnapshot`s, by `status.volumeGroupSnapshotName`;
/// 2. each one's bound `VolumeSnapshotContent` → `spec.source.volumeHandle`
///    (**required**, not optional: a group-created member is content-sourced,
///    so its own `spec.source.persistentVolumeClaimName` is empty);
/// 3. the `PersistentVolume` with that CSI handle → `spec.claimRef`.
///
/// A member whose content or PV cannot be resolved is **omitted, not guessed**.
/// The caller turns a missing self-entry into a named terminal failure, which is
/// also the correct handling for the inherent race that the snapshot-controller
/// re-evaluates the group's selector at create time.
pub fn map_group_members(
    members: &[MemberVs],
    contents: &[ContentInfo],
    pvs: &[PvInfo],
    group: &str,
) -> BTreeMap<(String, String), GroupMember> {
    let handle_by_content: BTreeMap<&str, &str> = contents
        .iter()
        .filter_map(|c| c.volume_handle.as_deref().map(|h| (c.name.as_str(), h)))
        .collect();
    let claim_by_handle: BTreeMap<&str, (&str, &str)> = pvs
        .iter()
        .map(|p| {
            (
                p.volume_handle.as_str(),
                (p.claim_namespace.as_str(), p.claim_name.as_str()),
            )
        })
        .collect();

    let mut out = BTreeMap::new();
    for m in members {
        if m.group.as_deref() != Some(group) {
            continue;
        }
        let Some(content) = m.content.as_deref() else {
            continue;
        };
        let Some(handle) = handle_by_content.get(content) else {
            continue;
        };
        let Some((ns, name)) = claim_by_handle.get(handle) else {
            continue;
        };
        out.insert(
            ((*ns).to_string(), (*name).to_string()),
            GroupMember {
                volume_snapshot_name: m.name.clone(),
                restore_size: m.restore_size.clone(),
            },
        );
    }
    out
}

/// Read everything [`map_group_members`] needs and run it.
pub async fn resolve_group_members(
    client: &kube::Client,
    ns: &str,
    group: &str,
) -> Result<BTreeMap<(String, String), GroupMember>> {
    let vs_api: Api<DynamicObject> = Api::namespaced_with(
        client.clone(),
        ns,
        &super::staging::volume_snapshot_ar_pub(),
    );
    let members: Vec<MemberVs> = vs_api
        .list(&ListParams::default())
        .await?
        .items
        .iter()
        .map(|o| {
            let status = o.data.get("status");
            MemberVs {
                name: o.name_any(),
                group: status
                    .and_then(|s| s.get("volumeGroupSnapshotName"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                content: status
                    .and_then(|s| s.get("boundVolumeSnapshotContentName"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                restore_size: status
                    .and_then(|s| s.get("restoreSize"))
                    .and_then(serde_json::Value::as_str)
                    .map(|s| Quantity(s.to_string())),
            }
        })
        .collect();
    if members.iter().all(|m| m.group.as_deref() != Some(group)) {
        return Ok(BTreeMap::new());
    }

    let content_api: Api<DynamicObject> = Api::all_with(
        client.clone(),
        &super::staging::volume_snapshot_content_ar_pub(),
    );
    let contents: Vec<ContentInfo> = content_api
        .list(&ListParams::default())
        .await?
        .items
        .iter()
        .map(|o| ContentInfo {
            name: o.name_any(),
            volume_handle: o
                .data
                .get("spec")
                .and_then(|s| s.get("source"))
                .and_then(|s| s.get("volumeHandle"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        })
        .collect();

    let pv_api: Api<PersistentVolume> = Api::all(client.clone());
    let pvs: Vec<PvInfo> = pv_api
        .list(&ListParams::default())
        .await?
        .items
        .iter()
        .filter_map(|pv| {
            let spec = pv.spec.as_ref()?;
            let handle = spec.csi.as_ref()?.volume_handle.clone();
            let claim = spec.claim_ref.as_ref()?;
            Some(PvInfo {
                volume_handle: handle,
                claim_namespace: claim.namespace.clone()?,
                claim_name: claim.name.clone()?,
            })
        })
        .collect();

    Ok(map_group_members(&members, &contents, &pvs, group))
}

// ---------------------------------------------------------------------------
// Lifetime
// ---------------------------------------------------------------------------

/// One sibling's state, as the reap gate sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiblingState {
    /// Whether the member reached a terminal phase (no further Job can launch).
    pub terminal: bool,
    /// Whether its staged `-src` PVC still EXISTS in the cluster.
    ///
    /// Observed live, deliberately NOT read from `status.staged.ready`. That
    /// field is set when staging succeeds and is never cleared on the success
    /// path, so trusting it would make every member that ever staged look
    /// permanently un-reapable — the group would then leak until the sweep's
    /// min-age floor expired, on every single grouped run.
    pub staged_pvc_present: bool,
}

/// **Pure.** Whether the shared group may be reaped, given its members.
///
/// Reapable only when EVERY sibling is terminal **and** none still holds a
/// staged PVC restoring from a member snapshot. Deleting it earlier cascades
/// onto the member `VolumeSnapshot`s and pulls the rug from under an in-flight
/// CSI restore.
///
/// Fails **closed**: the caller passes `None` when it could not enumerate
/// siblings, and a read it could not complete must never look like "nobody
/// needs it".
pub fn group_reapable(siblings: Option<&[SiblingState]>) -> bool {
    let Some(siblings) = siblings else {
        return false;
    };
    siblings.iter().all(|s| s.terminal && !s.staged_pvc_present)
}

/// Observe each member's terminal-ness and whether its staged PVC still exists.
///
/// `None` means a read failed — the caller must then leave the group alone.
/// Split out of [`cleanup_group_if_unused`] so the reap path stays a short,
/// readable sequence of gates.
async fn sibling_states(
    client: &kube::Client,
    ns: &str,
    group: &str,
    members: &[&Snapshot],
) -> Option<Vec<SiblingState>> {
    let pvc_api: Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
        Api::namespaced(client.clone(), ns);
    let mut states = Vec::with_capacity(members.len());
    for m in members {
        let staged = super::staging::staged_pvc_name(&m.name_any());
        let staged_pvc_present = match pvc_api.get_opt(&staged).await {
            Ok(p) => p.is_some(),
            // Same fail-closed rule as the LIST: an unreadable PVC must never
            // read as "gone", or the group is deleted under a live restore.
            Err(e) => {
                tracing::warn!(group = %group, snapshot = %m.name_any(), error = %e, "could not check a group member's staged PVC; leaving the VolumeGroupSnapshot in place");
                return None;
            }
        };
        states.push(SiblingState {
            terminal: matches!(
                m.status.as_ref().and_then(|st| st.phase),
                Some(SnapshotPhase::Succeeded)
                    | Some(SnapshotPhase::Failed)
                    | Some(SnapshotPhase::Unchanged)
            ),
            staged_pvc_present,
        });
    }
    Some(states)
}

/// Reap the shared `VolumeGroupSnapshot` if no sibling still needs it.
///
/// Deleting the group cascades onto its member `VolumeSnapshot`s (they carry an
/// ownerReference to it), which is exactly why the members are never deleted
/// directly here — that would race the as-source-protection ordering
/// `cleanup_staged_source` was written for.
pub async fn cleanup_group_if_unused(client: &kube::Client, ns: &str, group: &str) -> Result<bool> {
    let snaps: Api<Snapshot> = Api::namespaced(client.clone(), ns);
    // Siblings are found by the SPEC field that pins the group, NOT by a label.
    //
    // A label would have to be stamped by three independent minting paths (the
    // schedule, `kubectl kopiur snapshot now`, and a hand-applied CR), and
    // missing it in any one is catastrophic rather than cosmetic: the list comes
    // back empty, `group_reapable` is vacuously true, and the FIRST member to
    // finish deletes the shared capture out from under its siblings — cascading
    // onto their member VolumeSnapshots mid-restore. Reading `spec.source.group`
    // cannot drift: it is the same field that told this member which group to
    // wait on in the first place.
    //
    // Fail closed on a list error: pretending "nobody needs it" would delete a
    // capture N live backups are restoring from.
    let all = match snaps.list(&ListParams::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(group = %group, error = %e, "could not list group members; leaving the VolumeGroupSnapshot in place (the sweep is the backstop)");
            return Ok(false);
        }
    };
    let members: Vec<&Snapshot> = all
        .iter()
        .filter(|s| {
            s.spec
                .source
                .as_ref()
                .and_then(|src| src.group.as_ref())
                .is_some_and(|g| g.volume_group_snapshot_name == group)
        })
        .collect();

    // `None` = a read failed; the reap then fails closed.
    let Some(states) = sibling_states(client, ns, group, &members).await else {
        return Ok(false);
    };
    if !group_reapable(Some(&states)) {
        return Ok(false);
    }
    match vgs_api(client, ns)
        .delete(group, &DeleteParams::background())
        .await
    {
        Ok(_) => {
            tracing::info!(group = %group, namespace = %ns, "reaped shared VolumeGroupSnapshot (all members terminal)");
            Ok(true)
        }
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Create (or converge on) the shared group. Idempotent: N racing members
/// server-side-apply the same object over the same deterministic name.
pub async fn ensure_group(
    client: &kube::Client,
    ns: &str,
    name: &str,
    selector: &k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector,
    class: &str,
) -> Result<()> {
    let obj = build_volume_group_snapshot(name, ns, selector, class, name);
    apply(&vgs_api(client, ns), name, &obj).await?;
    Ok(())
}

/// The group-capture half's outcome, mirroring [`super::staging::StagingOutcome`]
/// but scoped to "is my member snapshot ready".
#[derive(Debug, Clone, PartialEq)]
pub enum GroupStage {
    /// This PVC's member snapshot exists and is usable.
    Ready {
        /// The member snapshot to restore the staged PVC from.
        member: GroupMember,
    },
    /// Not ready yet — requeue.
    Waiting {
        /// kstatus condition reason.
        reason: &'static str,
        /// Deterministic message (no `now()`).
        message: String,
    },
    /// Terminal for this member.
    Failed {
        /// kstatus condition reason.
        reason: &'static str,
        /// Actionable what/why/fix message.
        message: String,
    },
}

/// The annotation marking the default `VolumeGroupSnapshotClass` for a driver.
///
/// **Not** [`super::staging::DEFAULT_CLASS_ANNOTATION`], and not derivable from
/// the API group either: external-snapshotter uses `groupsnapshot.storage.`
/// **`kubernetes`**`.io/is-default-class` (`pkg/utils/util.go`
/// `IsDefaultGroupSnapshotClassAnnotation`), while the API group is
/// `groupsnapshot.storage.`**`k8s`**`.io`. Reading the per-volume annotation
/// here silently marks every group class non-default, so a cluster with two
/// classes for one driver fails `AmbiguousClass` even though it annotated one
/// exactly as the CSI docs say to.
pub const DEFAULT_GROUP_CLASS_ANNOTATION: &str =
    "groupsnapshot.storage.kubernetes.io/is-default-class";

/// List `VolumeGroupSnapshotClass`es. `None` when the API group is not served
/// (the group-snapshot stack is not installed), which is a distinct and much
/// more actionable answer than "no class matched".
pub async fn list_group_classes(
    client: &kube::Client,
) -> Result<Option<Vec<super::staging::ClassInfo>>> {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &volume_group_snapshot_class_ar());
    match api.list(&ListParams::default()).await {
        Ok(list) => Ok(Some(
            list.items
                .into_iter()
                .map(|o| super::staging::ClassInfo {
                    driver: o
                        .data
                        .get("driver")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    is_default: o
                        .annotations()
                        .get(DEFAULT_GROUP_CLASS_ANNOTATION)
                        .map(|v| v == "true")
                        .unwrap_or(false),
                    name: o.name_any(),
                })
                .collect(),
        )),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Drive the shared capture for one member: resolve the class, converge the
/// group, wait for it, then find THIS PVC's member snapshot.
///
/// Every member runs this independently against the same object, which is what
/// makes their outcome consistent without any coordination.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_group_stage(
    client: &kube::Client,
    policy: &kopiur_api::SnapshotPolicy,
    group: &kopiur_api::SnapshotSourceGroup,
    source_index: usize,
    pvc_namespace: &str,
    pvc_name: &str,
    provisioner: Option<&str>,
    timeout: Option<std::time::Duration>,
) -> Result<GroupStage> {
    let ns = group.namespace.as_str();
    let name = group.volume_group_snapshot_name.as_str();

    let Some(selector) = policy
        .spec
        .sources
        .get(source_index)
        .and_then(|s| s.pvc_selector.as_ref())
        .and_then(|s| s.label_selector.as_ref())
    else {
        return Ok(GroupStage::Failed {
            reason: crate::consts::REASON_GROUP_MEMBER_MISSING,
            message: format!(
                "this Snapshot pins the VolumeGroupSnapshot `{ns}/{name}`, but SnapshotPolicy                  `{}` source #{source_index} no longer has a pvcSelector with a labelSelector —                  the recipe was edited mid-run. Delete this Snapshot and let the schedule                  re-fire.",
                policy.name_any(),
            ),
        });
    };

    // Class resolution mirrors the per-volume path, including the
    // stack-not-installed distinction: `groupsnapshot.storage.k8s.io` is a
    // separate, Beta API group that many clusters do not serve at all.
    let Some(provisioner) = provisioner else {
        return Ok(GroupStage::Failed {
            reason: crate::consts::REASON_NO_GROUP_CLASS,
            message: format!(
                "PersistentVolumeClaim `{pvc_namespace}/{pvc_name}` has no StorageClass, so its                  CSI driver is unknown and no VolumeGroupSnapshotClass can be matched. Set                  `copyMethod: Direct`, or `groupBy: None`."
            ),
        });
    };
    let Some(classes) = list_group_classes(client).await? else {
        return Ok(GroupStage::Failed {
            reason: crate::consts::REASON_NO_GROUP_CLASS,
            message: format!(
                "`groupBy: VolumeGroupSnapshot` needs the {GROUP_SNAPSHOT_GROUP} API group, which                  this cluster does not serve. Install external-snapshotter 8.2+ (group snapshots                  are Beta from Kubernetes 1.32), or set `groupBy: None` to capture each PVC                  independently."
            ),
        });
    };
    let class = match super::staging::decide_class(&classes, provisioner, None) {
        super::staging::ClassDecision::Use(name) => name,
        super::staging::ClassDecision::NoneForDriver(driver)
        | super::staging::ClassDecision::ExplicitNotFound(driver) => {
            return Ok(GroupStage::Failed {
                reason: crate::consts::REASON_NO_GROUP_CLASS,
                message: format!(
                    "no VolumeGroupSnapshotClass exists for CSI driver `{driver}`. Create one, or                      set `groupBy: None` to capture each PVC independently (many drivers support                      per-volume snapshots but not group snapshots)."
                ),
            });
        }
        super::staging::ClassDecision::Ambiguous { driver, candidates } => {
            return Ok(GroupStage::Failed {
                reason: crate::consts::REASON_NO_GROUP_CLASS,
                message: format!(
                    "multiple VolumeGroupSnapshotClasses match driver `{driver}` ({}) and none is                      the unique default; annotate one as the default class",
                    candidates.join(", ")
                ),
            });
        }
    };

    // Converge the shared object. N members race here by design.
    ensure_group(client, ns, name, selector, &class).await?;

    match read_group(client, ns, name).await? {
        None => Ok(GroupStage::Waiting {
            reason: crate::consts::GROUP_STAGING_WAITING_REASON,
            message: format!("waiting for VolumeGroupSnapshot `{ns}/{name}` to become readyToUse"),
        }),
        Some(obs) => match vgs_wait_outcome(&obs, ns, name, &class, timeout, chrono::Utc::now()) {
            VgsWait::Waiting(message) => Ok(GroupStage::Waiting {
                reason: crate::consts::GROUP_STAGING_WAITING_REASON,
                message,
            }),
            VgsWait::Failed { reason, message } => Ok(GroupStage::Failed { reason, message }),
            VgsWait::Ready => {
                let members = resolve_group_members(client, ns, name).await?;
                match members.get(&(pvc_namespace.to_string(), pvc_name.to_string())) {
                    Some(member) => Ok(GroupStage::Ready {
                        member: member.clone(),
                    }),
                    // The group re-evaluates its selector at CREATE time, so the
                    // captured set can differ from what the expander matched.
                    // Name that rather than staging from a neighbour's snapshot.
                    None => Ok(GroupStage::Failed {
                        reason: crate::consts::REASON_GROUP_MEMBER_MISSING,
                        message: format!(
                            "VolumeGroupSnapshot `{ns}/{name}` became readyToUse but captured no                              member for PersistentVolumeClaim `{pvc_namespace}/{pvc_name}`. Its                              labels most likely stopped matching the pvcSelector between                              expansion and capture (the snapshot-controller re-evaluates the                              selector when it creates the group), or the CSI driver excluded it.                              Check `kubectl -n {ns} get volumegroupsnapshot {name} -o yaml`, or                              set `groupBy: None`. This Snapshot is terminal — the next scheduled                              run retries."
                        ),
                    }),
                }
            }
        },
    }
}

#[cfg(test)]
mod tests;
