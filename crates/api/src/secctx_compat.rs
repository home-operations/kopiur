//! Pure, controller-free reasoning about whether a mover's resolved security context can
//! **read** a backup *source* PVC, and the inverse — whether a future workload can read
//! what a restore mover **writes** to a *target* PVC.
//!
//! ## Why this is deliberately conservative
//!
//! A predicate that reasons *only* from security contexts cannot see file **mode bits**,
//! and world-readable `0644` data is everywhere. So it can almost never be *certain* a
//! backup will fail — the only honest verdicts from the spec alone are:
//!
//! - **`Compatible`** — provably fine: the mover is root, or its UID exactly matches every
//!   writer's UID. (We never claim `Compatible` on a group/`fsGroup` basis — see below.)
//! - **`Unknown`** — we cannot tell from the spec (the common case). The mover-side
//!   readability preflight (in `crates/mover`) is the layer that *certainly* validates this
//!   at runtime, where the files are actually mounted.
//! - **`LikelyIncompatible`** — reserved for near-certainty (the mover shares neither UID
//!   nor any group with the writers). Used only for a best-effort, advisory admission
//!   warning; the reconcile loop maps it to a non-blocking `Unknown` condition (the
//!   certain `False` comes from the mover preflight), so a `0644` tree never produces a
//!   false alarm on a successful backup.
//!
//! ## `fsGroup` is excluded from *backup-source* reasoning
//!
//! A backup mounts the source PVC **read-only** by default, so the kubelet never recursively
//! chgrp's it and `fsGroup` grants nothing for readability there. We therefore never treat an
//! `fsGroup` match as a path to `Compatible` for backups. (`fsGroup` is only counted toward
//! the mover's *process* group set, which can only ever *soften* a mismatch to `Unknown` —
//! the safe direction.) On **restore** the target is a fresh read-write volume where the
//! kubelet *does* apply `fsGroup`, so [`assess_restore_compat`] treats an `fsGroup` match as
//! a positive signal — the predicates are intentionally asymmetric.
//!
//! `Source::readOnly: false` (#254) exists precisely to re-enable that walk on the source, so
//! it partially unwinds this: with a writable source and an `fsGroup`, the kubelet MAY chgrp
//! the tree to the mover's own group, which would make the source readable no matter who wrote
//! it. That is not enough for `Compatible` — whether the walk happens depends on the
//! CSIDriver's `fsGroupPolicy` and on `fsGroupChangePolicy`, neither of which is in the spec —
//! but it IS enough to invalidate every workload-ownership comparison. So
//! [`assess_read_compat`] takes the effective `readOnly` and short-circuits to
//! `Unknown { FsGroupMayApply }`. Without that, the one configuration the flag exists to
//! enable would be reported `LikelyIncompatible` while working fine.

use std::collections::BTreeSet;

use k8s_openapi::api::core::v1::{Pod, PodSecurityContext, SecurityContext};

use crate::common::effective_run_as_user;

/// The mover's effective identity for **read** reasoning (backup source), built from the
/// resolved, post-invariant container + pod security contexts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoverIdentity {
    /// Effective `runAsUser` (`container ?? pod`); `None` when image-determined.
    pub uid: Option<i64>,
    /// Groups the mover *process* holds: container/pod `runAsGroup`, pod
    /// `supplementalGroups`, and pod `fsGroup` (the kubelet adds `fsGroup` to the pod's
    /// supplementary GIDs). Sorted for determinism. Used only to *soften* a UID mismatch.
    pub groups: BTreeSet<i64>,
    /// Effective pod `fsGroup`, kept separately from [`Self::groups`] (which folds it in
    /// among the process's GIDs and so cannot answer *which* one it was). Load-bearing
    /// only on a **read-write** source mount, where the kubelet may recursively chgrp the
    /// tree to it — on the read-only default it grants nothing. Symmetric with
    /// [`MoverWriteIdentity::fs_group`].
    pub fs_group: Option<i64>,
}

/// The mover's effective identity for **write** reasoning (restore target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoverWriteIdentity {
    /// Effective write `runAsUser` (`container ?? pod`); `None` when image-determined.
    /// Non-root kopia cannot `chown`, so restored files end up owned by this UID.
    pub uid: Option<i64>,
    /// Effective `fsGroup` for the target pod — load-bearing on a fresh read-write volume
    /// (the kubelet setgid-owns the restored tree to it). `None` when unset.
    pub fs_group: Option<i64>,
}

/// A workload pod's identity, for comparing against a mover. One per consuming pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadIdentity {
    /// Pod namespace — for deterministic selection + messages.
    pub namespace: String,
    /// Pod name — for deterministic selection + messages.
    pub name: String,
    /// Effective UIDs of every writer (init + main containers, with pod fallback), pinned
    /// values only. A file could be owned by any of these.
    pub writer_uids: BTreeSet<i64>,
    /// True if any container's effective UID is image-determined (unpinned) — then the
    /// true writer set is unknowable and the workload can never be `ExactUidMatch`.
    pub has_unpinned_writer: bool,
    /// Candidate *file* group IDs: the pod `fsGroup` (setgid file group on the workload's
    /// volume), every container/pod `runAsGroup`, and `supplementalGroups`. Sorted.
    pub file_groups: BTreeSet<i64>,
    /// Effective `runAsUser` of the *primary* (first non-init) container, with pod
    /// fallback; `None` when unpinned. Used as the restore "future consumer" UID.
    pub primary_uid: Option<i64>,
    /// The pod's `fsGroup`, if set — used as the restore "future consumer" group.
    pub fs_group: Option<i64>,
}

/// Basis on which a backup mover was found read-compatible. Exhaustive (thesis §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatBasis {
    /// The mover runs as root (UID 0) — reads everything.
    RootMover,
    /// The mover's UID exactly matches every writer's UID — owner-reads everything.
    ExactUidMatch,
}

/// Why a backup read-compatibility verdict is undecidable from the spec alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    /// The mover's UID is image-determined (no `runAsUser`); nothing to compare.
    MoverUidUnpinned,
    /// At least one workload writer's UID is image-determined.
    WorkloadUidUnpinned,
    /// UIDs differ but the mover shares a group with the file group — might read via the
    /// group bit (which we can't see), so we abstain.
    OnlyGroupOverlap,
    /// No pod currently mounts the source PVC (offline workload / mid-rollout).
    NoConsumerPod,
    /// The source is NFS (or has no single PVC) — ownership is NAS-determined and `fsGroup`
    /// is ignored; nothing to assess.
    NfsOrNoPvc,
    /// The source is mounted read-write and the mover declares an `fsGroup`, so the kubelet
    /// **may** recursively chgrp the tree to it before the mover reads — which would make
    /// the source readable regardless of who wrote it. Whether it actually does is not
    /// knowable from the spec (the CSIDriver's `fsGroupPolicy` may be `None`, or the default
    /// `ReadWriteOnceWithFSType`, which skips RWX volumes; and `fsGroupChangePolicy:
    /// OnRootMismatch` skips the walk when the root group already matches), so we abstain.
    FsGroupMayApply,
}

/// Whether a backup mover can read the source PVC's files. See the module docs for why
/// `Compatible` is rare and `LikelyIncompatible` rarer still.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoverReadCompat {
    /// Provably readable.
    Compatible {
        /// Why the mover is read-compatible.
        basis: CompatBasis,
    },
    /// Undecidable from the spec — the mover preflight is the runtime arbiter.
    Unknown {
        /// Why the verdict is undecidable.
        why: UnknownReason,
    },
    /// Near-certain mismatch: the mover shares neither UID nor any group with the
    /// (fully-pinned) writers. Advisory only.
    LikelyIncompatible {
        /// The mover's effective UID.
        mover_uid: i64,
        /// The writer UIDs the mover matches none of (sorted).
        workload_uids: Vec<i64>,
    },
}

/// Basis on which a restore was found write-compatible with the future consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreBasis {
    /// The future workload's UID equals the mover's write UID — it owns the restored files.
    WorkloadOwnsFiles,
    /// The mover's `fsGroup` equals the future workload's `fsGroup`; on a fresh read-write
    /// volume the kubelet setgid-owns the tree to that group, so the workload group-reads it.
    FsGroupMatch,
}

/// Why a restore write-compatibility verdict is undecidable from the spec alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreUnknown {
    /// No pod consumes the target PVC yet (the common case — e.g. a populator target).
    ConsumerAbsent,
    /// The future workload's UID is image-determined.
    WorkloadUidUnpinned,
    /// UID and `fsGroup` both differ, but file modes are unknown so we can't be certain.
    ModeUnknown,
}

/// Whether a future workload can read what a restore mover writes. The restore counterpart
/// of [`MoverReadCompat`] — `fsGroup` IS load-bearing here (fresh read-write volume).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreWriteCompat {
    /// Provably (or near-certainly) readable by the future workload.
    Compatible {
        /// Why the restore is write-compatible.
        basis: RestoreBasis,
    },
    /// Undecidable from the spec.
    Unknown {
        /// Why the verdict is undecidable.
        why: RestoreUnknown,
    },
    /// Near-certain mismatch: the mover writes as a UID and `fsGroup` the future workload
    /// shares neither of. Advisory only.
    LikelyIncompatible {
        /// The mover's write UID, if pinned.
        mover_uid: Option<i64>,
        /// The future workload's UID, if pinned.
        workload_uid: Option<i64>,
    },
}

/// Build the mover's read identity from its resolved container + pod security contexts.
pub fn mover_identity(sc: &SecurityContext, psc: Option<&PodSecurityContext>) -> MoverIdentity {
    let mut groups = BTreeSet::new();
    if let Some(g) = sc.run_as_group.or_else(|| psc.and_then(|p| p.run_as_group)) {
        groups.insert(g);
    }
    if let Some(p) = psc {
        if let Some(fsg) = p.fs_group {
            groups.insert(fsg);
        }
        if let Some(sup) = p.supplemental_groups.as_ref() {
            groups.extend(sup.iter().copied());
        }
    }
    MoverIdentity {
        uid: effective_run_as_user(Some(sc), psc),
        groups,
        fs_group: psc.and_then(|p| p.fs_group),
    }
}

/// Build the mover's *write* identity (restore) from its resolved contexts.
pub fn mover_write_identity(
    sc: &SecurityContext,
    psc: Option<&PodSecurityContext>,
) -> MoverWriteIdentity {
    MoverWriteIdentity {
        uid: effective_run_as_user(Some(sc), psc),
        fs_group: psc.and_then(|p| p.fs_group),
    }
}

/// Extract a [`WorkloadIdentity`] from a live pod: the union of init + main container
/// effective UIDs (pod fallback) as writers, and the file-group candidates.
pub fn workload_identity(pod: &Pod) -> WorkloadIdentity {
    let spec = pod.spec.as_ref();
    let pod_sc = spec.and_then(|s| s.security_context.as_ref());
    let pod_uid = pod_sc.and_then(|p| p.run_as_user);
    let pod_gid = pod_sc.and_then(|p| p.run_as_group);

    let init = spec
        .and_then(|s| s.init_containers.as_deref())
        .unwrap_or(&[]);
    let main = spec.map(|s| s.containers.as_slice()).unwrap_or(&[]);

    let mut writer_uids = BTreeSet::new();
    let mut has_unpinned_writer = false;
    let mut file_groups = BTreeSet::new();
    if let Some(g) = pod_gid {
        file_groups.insert(g);
    }
    if let Some(fsg) = pod_sc.and_then(|p| p.fs_group) {
        file_groups.insert(fsg);
    }
    if let Some(sup) = pod_sc.and_then(|p| p.supplemental_groups.as_ref()) {
        file_groups.extend(sup.iter().copied());
    }
    for c in init.iter().chain(main.iter()) {
        let csc = c.security_context.as_ref();
        match csc.and_then(|s| s.run_as_user).or(pod_uid) {
            Some(u) => {
                writer_uids.insert(u);
            }
            None => has_unpinned_writer = true,
        }
        if let Some(g) = csc.and_then(|s| s.run_as_group) {
            file_groups.insert(g);
        }
    }
    // No containers at all → the writer set is unknowable.
    if init.is_empty() && main.is_empty() {
        has_unpinned_writer = true;
    }
    // The "primary" container is the first non-init container (the long-running app).
    let primary_uid = main
        .first()
        .and_then(|c| c.security_context.as_ref())
        .and_then(|s| s.run_as_user)
        .or(pod_uid);

    WorkloadIdentity {
        namespace: pod.metadata.namespace.clone().unwrap_or_default(),
        name: pod.metadata.name.clone().unwrap_or_default(),
        writer_uids,
        has_unpinned_writer,
        file_groups,
        primary_uid,
        fs_group: pod_sc.and_then(|p| p.fs_group),
    }
}

/// Does this pod mount `claim_name` via a `persistentVolumeClaim` volume? The canonical
/// scanner — the controller's colocation logic reuses this so the two never drift.
pub fn pod_mounts_claim(pod: &Pod, claim_name: &str) -> bool {
    pod.spec
        .as_ref()
        .and_then(|s| s.volumes.as_deref())
        .unwrap_or_default()
        .iter()
        .any(|v| {
            v.persistent_volume_claim
                .as_ref()
                .map(|pvc| pvc.claim_name == claim_name)
                .unwrap_or(false)
        })
}

/// Pods (in stable input order) that mount `claim_name`.
pub fn pods_mounting_pvc<'a>(pods: &'a [Pod], claim_name: &str) -> Vec<&'a Pod> {
    pods.iter()
        .filter(|p| pod_mounts_claim(p, claim_name))
        .collect()
}

/// The **container** within `pod` that mounts `claim_name`: resolve the pod volume(s) backed
/// by the claim, then find the container whose `volumeMounts` reference one.
///
/// [`pod_mounts_claim`] is pod-level and cannot answer "which container is the app?" — a
/// question that matters because `inheritSecurityContextFrom` copies ONE container's
/// `securityContext`. Falling back to the pod's first container picks whatever the manifest or
/// a sidecar injector listed first, which on an istio-injected pod is `istio-proxy` (uid 1337),
/// not the app. The container actually mounting the data is a far better guess at whose
/// identity wrote it.
///
/// Returns `None` when nothing mounts the claim or when **several** containers do — ambiguous
/// is not a guess worth making, and the caller falls back to the first container as before.
/// Init containers are excluded: they are not the long-running writer.
pub fn container_mounting_claim<'a>(
    pod: &'a Pod,
    claim_name: &str,
) -> Option<&'a k8s_openapi::api::core::v1::Container> {
    let spec = pod.spec.as_ref()?;
    // Pod volume names backed by this claim (usually exactly one).
    let volume_names: BTreeSet<&str> = spec
        .volumes
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|v| {
            v.persistent_volume_claim
                .as_ref()
                .is_some_and(|pvc| pvc.claim_name == claim_name)
        })
        .map(|v| v.name.as_str())
        .collect();
    if volume_names.is_empty() {
        return None;
    }
    let mut mounters = spec.containers.iter().filter(|c| {
        c.volume_mounts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|m| volume_names.contains(m.name.as_str()))
    });
    let first = mounters.next()?;
    // More than one container mounts it → ambiguous; let the caller decide.
    if mounters.next().is_some() {
        return None;
    }
    Some(first)
}

/// Whether a pod is a kopiur-managed object (carries `app.kubernetes.io/managed-by=kopiur`).
/// A mover Job's pod mounts the source PVC too, so consumer-discovery and compatibility
/// reasoning must exclude these — otherwise the mover would be compared against (or inherit
/// from) itself. The single definition, shared by the controller and webhook.
pub fn is_managed_by_kopiur(pod: &Pod) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(crate::consts::MANAGED_BY_LABEL))
        .map(|v| v == crate::consts::MANAGED_BY_VALUE)
        .unwrap_or(false)
}

/// Build [`WorkloadIdentity`] for every **workload** pod mounting `claim_name` — i.e. those
/// mounting the claim, minus kopiur-managed (mover) pods. The shared core for backup,
/// restore, and webhook compatibility checks (one definition, no per-caller duplication).
pub fn workload_identities(pods: &[Pod], claim_name: &str) -> Vec<WorkloadIdentity> {
    pods_mounting_pvc(pods, claim_name)
        .into_iter()
        .filter(|p| !is_managed_by_kopiur(p))
        .map(workload_identity)
        .collect()
}

/// Assess whether a backup mover can read the source PVC's files, given the workload pods
/// mounting it. See the module docs for the conservative posture; the result is deterministic
/// (independent of `workloads` ordering).
///
/// `source_read_only` is the source mount's effective `readOnly` (`Source::readOnly`, default
/// `true`). It is load-bearing rather than incidental: the module's whole `fsGroup` posture
/// rests on the mount being read-only, and a writable source invalidates every group
/// comparison below — see [`UnknownReason::FsGroupMayApply`].
pub fn assess_read_compat(
    mover: &MoverIdentity,
    workloads: &[WorkloadIdentity],
    source_read_only: bool,
) -> MoverReadCompat {
    // Root reads everything — independent of any workload (so this holds even with no pod).
    if mover.uid == Some(0) {
        return MoverReadCompat::Compatible {
            basis: CompatBasis::RootMover,
        };
    }
    if workloads.is_empty() {
        return MoverReadCompat::Unknown {
            why: UnknownReason::NoConsumerPod,
        };
    }
    let Some(mover_uid) = mover.uid else {
        return MoverReadCompat::Unknown {
            why: UnknownReason::MoverUidUnpinned,
        };
    };

    // Any workload with an unpinned writer makes the whole assessment undecidable.
    if workloads.iter().any(|w| w.has_unpinned_writer) {
        return MoverReadCompat::Unknown {
            why: UnknownReason::WorkloadUidUnpinned,
        };
    }
    // Compatible only if EVERY writer across all pods is exactly the mover's UID.
    let all_writers: BTreeSet<i64> = workloads
        .iter()
        .flat_map(|w| w.writer_uids.iter().copied())
        .collect();
    if all_writers == BTreeSet::from([mover_uid]) {
        return MoverReadCompat::Compatible {
            basis: CompatBasis::ExactUidMatch,
        };
    }
    // Everything below reasons about the ownership the WORKLOAD left on the volume. A
    // read-write mount plus an `fsGroup` breaks that premise at the root: the kubelet may
    // chgrp the whole tree to the mover's own `fsGroup` and add group-write before the
    // mover ever reads, at which point who wrote the files stops mattering. Bail out here
    // rather than after the group chain — `mover.groups` already contains `fs_group`, so
    // `shares_group` below would otherwise catch this first and blame "a shared group with
    // the workload", which is not what happened. Still `Unknown`, never `Compatible`: the
    // module reserves that for provable verdicts, and whether the kubelet performs the walk
    // is not a property of the spec (see `FsGroupMayApply`).
    if !source_read_only && mover.fs_group.is_some() {
        return MoverReadCompat::Unknown {
            why: UnknownReason::FsGroupMayApply,
        };
    }
    // UIDs differ somewhere. If the mover shares any group with any file group, a group-read
    // (mode we can't see) might let it through — abstain.
    let shares_group = workloads
        .iter()
        .any(|w| w.file_groups.iter().any(|g| mover.groups.contains(g)));
    if shares_group {
        return MoverReadCompat::Unknown {
            why: UnknownReason::OnlyGroupOverlap,
        };
    }
    // No UID match, no group bridge, every writer pinned → near-certain unreadable.
    MoverReadCompat::LikelyIncompatible {
        mover_uid,
        workload_uids: all_writers.into_iter().collect(),
    }
}

/// Assess whether the *future* consumer of a restore target can read what the mover writes.
/// `fsGroup` IS a positive signal here (fresh read-write volume). `future` is `None` when no
/// pod consumes the target yet (the common case → `ConsumerAbsent`).
pub fn assess_restore_compat(
    mover: &MoverWriteIdentity,
    future: Option<&WorkloadIdentity>,
) -> RestoreWriteCompat {
    let Some(w) = future else {
        return RestoreWriteCompat::Unknown {
            why: RestoreUnknown::ConsumerAbsent,
        };
    };
    // UID ownership: the workload runs as the UID that owns the restored files.
    if let (Some(mu), Some(wu)) = (mover.uid, w.primary_uid)
        && mu == wu
    {
        return RestoreWriteCompat::Compatible {
            basis: RestoreBasis::WorkloadOwnsFiles,
        };
    }
    // fsGroup bridge: setgid group ownership on the fresh volume + the workload joins it.
    if let (Some(mg), Some(wg)) = (mover.fs_group, w.fs_group)
        && mg == wg
    {
        return RestoreWriteCompat::Compatible {
            basis: RestoreBasis::FsGroupMatch,
        };
    }
    if w.primary_uid.is_none() {
        return RestoreWriteCompat::Unknown {
            why: RestoreUnknown::WorkloadUidUnpinned,
        };
    }
    // Both UID and fsGroup differ; a pinned mover UID vs a pinned, distinct workload UID with
    // no fsGroup bridge is the near-certain case. Otherwise modes are unknown — abstain.
    match (mover.uid, w.primary_uid) {
        (Some(_), Some(_)) => RestoreWriteCompat::LikelyIncompatible {
            mover_uid: mover.uid,
            workload_uid: w.primary_uid,
        },
        _ => RestoreWriteCompat::Unknown {
            why: RestoreUnknown::ModeUnknown,
        },
    }
}

impl MoverReadCompat {
    /// A stable, deterministic one-line summary for a status condition message / admission
    /// warning. No timestamps / volatile content — sorted UID lists only.
    pub fn summary(&self, mover_uid_render: &str) -> String {
        match self {
            MoverReadCompat::Compatible {
                basis: CompatBasis::RootMover,
            } => format!("mover runs as root ({mover_uid_render}) and can read all source files"),
            MoverReadCompat::Compatible {
                basis: CompatBasis::ExactUidMatch,
            } => format!(
                "mover UID {mover_uid_render} matches the workload's UID; it can read the source"
            ),
            MoverReadCompat::Unknown { why } => format!(
                "cannot determine source readability from securityContext alone ({}); the mover \
                 verifies it at runtime",
                why.as_str()
            ),
            MoverReadCompat::LikelyIncompatible {
                mover_uid,
                workload_uids,
            } => format!(
                "mover UID {mover_uid} shares no UID or group with the workload writer UID(s) {} \
                 — the backup may fail with permission denied or silently skip unreadable files; \
                 set mover.inheritSecurityContextFrom.pvcConsumer, or a matching runAsUser/fsGroup",
                render_uid_list(workload_uids)
            ),
        }
    }
}

impl UnknownReason {
    /// A stable label for the reason (status condition `reason` / messages).
    pub fn as_str(&self) -> &'static str {
        match self {
            UnknownReason::MoverUidUnpinned => "mover UID is image-determined",
            UnknownReason::WorkloadUidUnpinned => "a workload writer UID is image-determined",
            UnknownReason::OnlyGroupOverlap => "UIDs differ but a group is shared",
            UnknownReason::NoConsumerPod => "no pod currently mounts the source PVC",
            UnknownReason::NfsOrNoPvc => "source is NFS or has no single PVC",
            UnknownReason::FsGroupMayApply => {
                "the source is mounted read-write, so the kubelet may apply the mover's fsGroup \
                 to it"
            }
        }
    }
}

/// Render a sorted UID list deterministically, e.g. `[999, 1000]`.
fn render_uid_list(uids: &[i64]) -> String {
    let parts: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
    format!("[{}]", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        Container, PersistentVolumeClaimVolumeSource, PodSpec, Volume,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn sc(uid: Option<i64>, gid: Option<i64>, non_root: Option<bool>) -> SecurityContext {
        SecurityContext {
            run_as_user: uid,
            run_as_group: gid,
            run_as_non_root: non_root,
            ..Default::default()
        }
    }

    fn psc(uid: Option<i64>, fs_group: Option<i64>, supp: Vec<i64>) -> PodSecurityContext {
        PodSecurityContext {
            run_as_user: uid,
            fs_group,
            supplemental_groups: if supp.is_empty() { None } else { Some(supp) },
            ..Default::default()
        }
    }

    /// Build a pod with one main container running as `uid`/`gid`, pod fsGroup `fs_group`,
    /// mounting `claim`.
    fn pod(name: &str, ns: &str, uid: Option<i64>, fs_group: Option<i64>, claim: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(ns.into()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                security_context: Some(PodSecurityContext {
                    fs_group,
                    ..Default::default()
                }),
                containers: vec![Container {
                    name: "app".into(),
                    security_context: Some(sc(uid, None, uid.map(|u| u != 0))),
                    ..Default::default()
                }],
                volumes: Some(vec![Volume {
                    name: "data".into(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: claim.into(),
                        read_only: None,
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn root_mover_is_compatible_even_with_no_consumer() {
        let m = mover_identity(&sc(Some(0), None, Some(false)), None);
        assert!(matches!(
            assess_read_compat(&m, &[], true),
            MoverReadCompat::Compatible {
                basis: CompatBasis::RootMover
            }
        ));
    }

    #[test]
    fn pod_level_root_mover_is_root() {
        // Container UID unset, pod UID 0 → effective root (the precedence bug guard).
        let m = mover_identity(
            &SecurityContext::default(),
            Some(&psc(Some(0), None, vec![])),
        );
        assert_eq!(m.uid, Some(0));
        assert!(matches!(
            assess_read_compat(&m, &[], true),
            MoverReadCompat::Compatible {
                basis: CompatBasis::RootMover
            }
        ));
    }

    /// The exact reported bug, end-to-end through the real merge + assessment: a
    /// `pvcConsumer` mover inheriting from a workload whose container securityContext EXISTS
    /// but pins no `runAsUser` (its UID comes from the image's `USER` line).
    ///
    /// Inherit "succeeds" — the block is non-empty — but contributes no UID, so the merged
    /// mover has none either and runs as the mover image's 65532. The reconciler used to
    /// short-circuit this to `SecurityContextCompatible=True` ("matches the workload by
    /// construction") without ever asking this engine; the backup then failed with
    /// `permission denied`. The engine's answer has always been `Unknown` — it was simply
    /// never consulted on the pvcConsumer path.
    #[test]
    fn inheriting_a_uidless_workload_is_never_compatible() {
        // The workload: hardened container context, no runAsUser at either level.
        let mut w_pod = pod("app-7c9d8f5b6", "app", None, None, "app-data");
        w_pod.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            ..Default::default()
        });
        w_pod.spec.as_mut().unwrap().security_context = None;

        // What `inheritSecurityContextFrom` copies off it, through the real merge ladder.
        let inherited_sc = w_pod.spec.as_ref().unwrap().containers[0]
            .security_context
            .clone();
        let inherited_psc = w_pod.spec.as_ref().unwrap().security_context.clone();
        let resolved = crate::common::resolve_mover(
            None,
            inherited_sc.as_ref(),
            inherited_psc.as_ref(),
            None,
            None,
            None,
        );

        let m = mover_identity(
            &resolved.security_context,
            resolved.pod_security_context.as_ref(),
        );
        assert_eq!(
            m.uid, None,
            "inheriting pinned no UID — the mover silently runs as the image's 65532"
        );

        let ids = workload_identities(std::slice::from_ref(&w_pod), "app-data");
        assert!(
            !matches!(
                assess_read_compat(&m, &ids, true),
                MoverReadCompat::Compatible { .. }
            ),
            "must never be Compatible: nothing here proves the mover can read the source"
        );
    }

    /// The companion: when the workload DOES pin a UID, inheriting it is provably compatible.
    /// Guards against over-correcting the fix above into never confirming anything.
    #[test]
    fn inheriting_a_uid_pinning_workload_is_compatible() {
        let w_pod = pod("app-7c9d8f5b6", "app", Some(1000), None, "app-data");
        let inherited_sc = w_pod.spec.as_ref().unwrap().containers[0]
            .security_context
            .clone();
        let resolved =
            crate::common::resolve_mover(None, inherited_sc.as_ref(), None, None, None, None);
        let m = mover_identity(
            &resolved.security_context,
            resolved.pod_security_context.as_ref(),
        );
        assert_eq!(m.uid, Some(1000));
        assert!(matches!(
            assess_read_compat(
                &m,
                &workload_identities(std::slice::from_ref(&w_pod), "app-data"),
                true
            ),
            MoverReadCompat::Compatible {
                basis: CompatBasis::ExactUidMatch
            }
        ));
    }

    /// Cause (b): a sidecar-injected pod. `containers.first()` may hand the mover the
    /// sidecar's UID while the app writes the files as its own. The whole-namespace writer
    /// set catches the mismatch — "matches by construction" never could.
    #[test]
    fn inheriting_a_sidecars_uid_is_not_compatible_with_the_apps_files() {
        let mut w_pod = pod("app-7c9d8f5b6", "app", Some(1000), None, "app-data");
        // istio-proxy injected ahead of the app container, running as 1337.
        w_pod.spec.as_mut().unwrap().containers.insert(
            0,
            Container {
                name: "istio-proxy".into(),
                security_context: Some(sc(Some(1337), None, Some(true))),
                ..Default::default()
            },
        );
        // The mover inherited the SIDECAR (containers.first()).
        let resolved = crate::common::resolve_mover(
            None,
            Some(&sc(Some(1337), None, Some(true))),
            None,
            None,
            None,
            None,
        );
        let m = mover_identity(
            &resolved.security_context,
            resolved.pod_security_context.as_ref(),
        );
        assert!(
            !matches!(
                assess_read_compat(
                    &m,
                    &workload_identities(std::slice::from_ref(&w_pod), "app-data"),
                    true
                ),
                MoverReadCompat::Compatible { .. }
            ),
            "the app writes as 1000; a 1337 mover is not provably able to read it"
        );
    }

    /// `container_mounting_claim` exists because `containers.first()` is a coin flip on any
    /// pod with an injected sidecar — and inheriting the sidecar's UID yields a mover that
    /// cannot read the app's files.
    #[test]
    fn container_mounting_claim_picks_the_app_over_a_first_listed_sidecar() {
        use k8s_openapi::api::core::v1::VolumeMount;

        let mut p = pod("app-1", "app", Some(1000), None, "app-data");
        // The app container mounts the claim.
        p.spec.as_mut().unwrap().containers[0].volume_mounts = Some(vec![VolumeMount {
            name: "data".into(),
            mount_path: "/data".into(),
            ..Default::default()
        }]);
        // istio-proxy is injected FIRST and mounts nothing of ours.
        p.spec.as_mut().unwrap().containers.insert(
            0,
            Container {
                name: "istio-proxy".into(),
                security_context: Some(sc(Some(1337), None, Some(true))),
                ..Default::default()
            },
        );
        assert_eq!(
            p.spec.as_ref().unwrap().containers.first().unwrap().name,
            "istio-proxy",
            "precondition: the naive pick would take the sidecar"
        );
        let picked = container_mounting_claim(&p, "app-data").expect("the app mounts the claim");
        assert_eq!(picked.name, "app");
        assert_eq!(
            picked.security_context.as_ref().unwrap().run_as_user,
            Some(1000),
            "and it carries the identity that actually wrote the data"
        );
    }

    #[test]
    fn container_mounting_claim_abstains_when_ambiguous_or_absent() {
        use k8s_openapi::api::core::v1::VolumeMount;

        let mount = || {
            Some(vec![VolumeMount {
                name: "data".into(),
                mount_path: "/data".into(),
                ..Default::default()
            }])
        };

        // Nothing mounts the claim -> None (caller keeps its old first-container behavior).
        let p = pod("app-1", "app", Some(1000), None, "app-data");
        assert!(container_mounting_claim(&p, "app-data").is_none());

        // A DIFFERENT claim -> None.
        let mut p = pod("app-1", "app", Some(1000), None, "app-data");
        p.spec.as_mut().unwrap().containers[0].volume_mounts = mount();
        assert!(container_mounting_claim(&p, "other-claim").is_none());

        // TWO containers mount it -> ambiguous, abstain rather than guess.
        let mut p = pod("app-1", "app", Some(1000), None, "app-data");
        p.spec.as_mut().unwrap().containers[0].volume_mounts = mount();
        p.spec.as_mut().unwrap().containers.push(Container {
            name: "sidecar-backup".into(),
            security_context: Some(sc(Some(2000), None, Some(true))),
            volume_mounts: mount(),
            ..Default::default()
        });
        assert!(
            container_mounting_claim(&p, "app-data").is_none(),
            "two mounters: no basis to prefer either"
        );
    }

    #[test]
    fn stock_hardened_mover_is_unknown_never_warns() {
        // Hardened mover: runAsNonRoot:true, no runAsUser → UID unpinned → Unknown.
        let resolved = crate::common::resolve_mover(None, None, None, None, None, None);
        let m = mover_identity(
            &resolved.security_context,
            resolved.pod_security_context.as_ref(),
        );
        let w = workload_identity(&pod("pg-0", "db", Some(999), None, "data"));
        assert!(matches!(
            assess_read_compat(&m, &[w], true),
            MoverReadCompat::Unknown {
                why: UnknownReason::MoverUidUnpinned
            }
        ));
    }

    #[test]
    fn exact_uid_match_is_compatible() {
        let m = mover_identity(&sc(Some(999), None, Some(true)), None);
        let w = workload_identity(&pod("pg-0", "db", Some(999), None, "data"));
        assert!(matches!(
            assess_read_compat(&m, &[w], true),
            MoverReadCompat::Compatible {
                basis: CompatBasis::ExactUidMatch
            }
        ));
    }

    #[test]
    fn disjoint_uid_no_group_is_likely_incompatible() {
        let m = mover_identity(&sc(Some(65532), None, Some(true)), None);
        let w = workload_identity(&pod("pg-0", "db", Some(999), None, "data"));
        assert!(matches!(
            assess_read_compat(&m, &[w], true),
            MoverReadCompat::LikelyIncompatible { .. }
        ));
    }

    #[test]
    fn group_overlap_softens_to_unknown() {
        // Mover non-root 65532 with supplementalGroup 2000; workload writes as 999 with
        // fsGroup 2000 → shared group 2000 → abstain (might group-read).
        let m = mover_identity(
            &sc(Some(65532), None, Some(true)),
            Some(&psc(None, None, vec![2000])),
        );
        let w = workload_identity(&pod("pg-0", "db", Some(999), Some(2000), "data"));
        assert!(matches!(
            assess_read_compat(&m, &[w], true),
            MoverReadCompat::Unknown {
                why: UnknownReason::OnlyGroupOverlap
            }
        ));
    }

    #[test]
    fn root_init_container_writer_is_unknown_not_compatible() {
        // Main container 999, init container root → writer set {0, 999}; mover 999 ≠ {999}
        // exactly, and root-owned files might be 0644 → Unknown (no group bridge), not False.
        let mut p = pod("app-0", "ns", Some(999), None, "data");
        p.spec.as_mut().unwrap().init_containers = Some(vec![Container {
            name: "init".into(),
            security_context: Some(sc(Some(0), None, Some(false))),
            ..Default::default()
        }]);
        let m = mover_identity(&sc(Some(999), None, Some(true)), None);
        let w = workload_identity(&p);
        assert!(w.writer_uids.contains(&0) && w.writer_uids.contains(&999));
        // mover 999 doesn't match the {0,999} set exactly, no shared group → LikelyIncompatible.
        assert!(matches!(
            assess_read_compat(&m, &[w], true),
            MoverReadCompat::LikelyIncompatible { .. }
        ));
    }

    /// #254: the exact configuration `Source::readOnly: false` exists to enable must not
    /// be reported as a near-certain failure.
    #[test]
    fn a_writable_source_with_an_fsgroup_softens_likely_incompatible_to_unknown() {
        // The workload writes as 1000 and shares no group with the mover. Read-only, that
        // is the textbook LikelyIncompatible — and it is the verdict a user gets today.
        let m = mover_identity(
            &sc(Some(65532), None, Some(true)),
            Some(&psc(None, Some(65532), vec![])),
        );
        let w = workload_identity(&pod("app-0", "ns", Some(1000), Some(1000), "data"));
        assert!(
            matches!(
                assess_read_compat(&m, std::slice::from_ref(&w), true),
                MoverReadCompat::LikelyIncompatible { .. }
            ),
            "read-only source: fsGroup grants nothing, so the mismatch stands"
        );
        // Writable, the kubelet MAY chgrp the whole tree to the mover's own fsGroup before
        // it reads, at which point who wrote the files stops mattering. Abstain.
        assert!(
            matches!(
                assess_read_compat(&m, std::slice::from_ref(&w), false),
                MoverReadCompat::Unknown {
                    why: UnknownReason::FsGroupMayApply
                }
            ),
            "a writable source + an fsGroup invalidates the ownership comparison"
        );
    }

    /// The verdict is `Unknown`, never `Compatible` — whether the kubelet performs the walk
    /// is not a property of the spec (CSIDriver `fsGroupPolicy: None`, or the default
    /// `ReadWriteOnceWithFSType` skipping RWX; `fsGroupChangePolicy: OnRootMismatch`).
    #[test]
    fn a_writable_source_never_reaches_compatible_on_an_fsgroup_basis() {
        let m = mover_identity(
            &sc(Some(65532), None, Some(true)),
            Some(&psc(None, Some(65532), vec![])),
        );
        let w = workload_identity(&pod("app-0", "ns", Some(1000), Some(1000), "data"));
        assert!(!matches!(
            assess_read_compat(&m, &[w], false),
            MoverReadCompat::Compatible { .. }
        ));
    }

    /// The softening is keyed on the mover actually HAVING an fsGroup — without one there
    /// is no walk to hope for, and a writable mount changes nothing.
    #[test]
    fn a_writable_source_without_an_fsgroup_still_reports_likely_incompatible() {
        let m = mover_identity(&sc(Some(65532), None, Some(true)), None);
        let w = workload_identity(&pod("app-0", "ns", Some(1000), Some(1000), "data"));
        assert!(matches!(
            assess_read_compat(&m, &[w], false),
            MoverReadCompat::LikelyIncompatible { .. }
        ));
    }

    /// The fsGroup short-circuit sits AFTER the provable verdicts, so it can never mask a
    /// real `Compatible` into an abstention.
    #[test]
    fn a_writable_source_does_not_mask_a_provable_compatible() {
        let root = mover_identity(
            &sc(Some(0), None, Some(false)),
            Some(&psc(None, Some(65532), vec![])),
        );
        let w = workload_identity(&pod("app-0", "ns", Some(1000), Some(1000), "data"));
        assert!(matches!(
            assess_read_compat(&root, std::slice::from_ref(&w), false),
            MoverReadCompat::Compatible {
                basis: CompatBasis::RootMover
            }
        ));
        // ...and an exact UID match likewise survives.
        let exact = mover_identity(
            &sc(Some(1000), None, Some(true)),
            Some(&psc(None, Some(65532), vec![])),
        );
        assert!(matches!(
            assess_read_compat(&exact, &[w], false),
            MoverReadCompat::Compatible {
                basis: CompatBasis::ExactUidMatch
            }
        ));
    }

    #[test]
    fn unpinned_workload_writer_is_unknown() {
        // Container with no runAsUser and no pod-level UID → unpinned writer.
        let mut p = pod("x", "ns", None, None, "data");
        p.spec.as_mut().unwrap().containers[0].security_context = None;
        p.spec.as_mut().unwrap().security_context = None;
        let m = mover_identity(&sc(Some(65532), None, Some(true)), None);
        let w = workload_identity(&p);
        assert!(matches!(
            assess_read_compat(&m, &[w], true),
            MoverReadCompat::Unknown {
                why: UnknownReason::WorkloadUidUnpinned
            }
        ));
    }

    #[test]
    fn verdict_is_independent_of_pod_order() {
        let m = mover_identity(&sc(Some(65532), None, Some(true)), None);
        let a = workload_identity(&pod("a", "ns", Some(999), None, "data"));
        let b = workload_identity(&pod("b", "ns", Some(1000), None, "data"));
        let forward = assess_read_compat(&m, &[a.clone(), b.clone()], true);
        let reversed = assess_read_compat(&m, &[b, a], true);
        assert_eq!(forward, reversed, "verdict must not depend on input order");
    }

    #[test]
    fn pods_mounting_pvc_filters_by_claim() {
        let p1 = pod("p1", "ns", Some(1), None, "wanted");
        let p2 = pod("p2", "ns", Some(1), None, "other");
        let pods = vec![p1, p2];
        let found = pods_mounting_pvc(&pods, "wanted");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].metadata.name.as_deref(), Some("p1"));
    }

    #[test]
    fn workload_identities_excludes_kopiur_movers() {
        // A kopiur mover pod mounts the source PVC too — it must never be treated as a
        // workload (else the mover compares against / inherits from itself).
        let workload = pod("pg-0", "db", Some(999), None, "data");
        let mut mover = pod("mover-x", "db", Some(65532), None, "data");
        mover.metadata.labels = Some(std::collections::BTreeMap::from([(
            crate::consts::MANAGED_BY_LABEL.to_string(),
            crate::consts::MANAGED_BY_VALUE.to_string(),
        )]));
        assert!(is_managed_by_kopiur(&mover) && !is_managed_by_kopiur(&workload));
        let ids = workload_identities(&[mover, workload], "data");
        assert_eq!(ids.len(), 1, "only the non-kopiur workload counts");
        assert_eq!(ids[0].name, "pg-0");
    }

    // --- restore-direction ---

    #[test]
    fn restore_absent_consumer_is_unknown() {
        let m = mover_write_identity(&sc(Some(65532), None, Some(true)), None);
        assert!(matches!(
            assess_restore_compat(&m, None),
            RestoreWriteCompat::Unknown {
                why: RestoreUnknown::ConsumerAbsent
            }
        ));
    }

    #[test]
    fn restore_uid_ownership_is_compatible() {
        let m = mover_write_identity(&sc(Some(2000), None, Some(true)), None);
        let w = workload_identity(&pod("app", "ns", Some(2000), None, "data"));
        assert!(matches!(
            assess_restore_compat(&m, Some(&w)),
            RestoreWriteCompat::Compatible {
                basis: RestoreBasis::WorkloadOwnsFiles
            }
        ));
    }

    #[test]
    fn restore_fsgroup_match_is_compatible() {
        // Mover writes as 65532 with fsGroup 2500; future workload runs as 1000 with fsGroup
        // 2500 → on a fresh RW volume the kubelet setgid-owns to 2500 → group-readable.
        let m = mover_write_identity(
            &sc(Some(65532), None, Some(true)),
            Some(&psc(None, Some(2500), vec![])),
        );
        let w = workload_identity(&pod("app", "ns", Some(1000), Some(2500), "data"));
        assert!(matches!(
            assess_restore_compat(&m, Some(&w)),
            RestoreWriteCompat::Compatible {
                basis: RestoreBasis::FsGroupMatch
            }
        ));
    }

    #[test]
    fn restore_disjoint_uid_and_fsgroup_is_likely_incompatible() {
        let m = mover_write_identity(
            &sc(Some(65532), None, Some(true)),
            Some(&psc(None, Some(65532), vec![])),
        );
        let w = workload_identity(&pod("app", "ns", Some(1000), Some(2500), "data"));
        assert!(matches!(
            assess_restore_compat(&m, Some(&w)),
            RestoreWriteCompat::LikelyIncompatible { .. }
        ));
    }
}
