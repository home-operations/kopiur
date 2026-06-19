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
//! A backup mounts the source PVC **read-only**, so the kubelet never recursively chgrp's
//! it and `fsGroup` grants nothing for readability there. We therefore never treat an
//! `fsGroup` match as a path to `Compatible` for backups. (`fsGroup` is only counted toward
//! the mover's *process* group set, which can only ever *soften* a mismatch to `Unknown` —
//! the safe direction.) On **restore** the target is a fresh read-write volume where the
//! kubelet *does* apply `fsGroup`, so [`assess_restore_compat`] treats an `fsGroup` match as
//! a positive signal — the predicates are intentionally asymmetric.

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

/// Assess whether a backup mover can read the source PVC's files, given the workload pods
/// mounting it. See the module docs for the conservative posture; the result is deterministic
/// (independent of `workloads` ordering).
pub fn assess_read_compat(
    mover: &MoverIdentity,
    workloads: &[WorkloadIdentity],
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
            assess_read_compat(&m, &[]),
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
            assess_read_compat(&m, &[]),
            MoverReadCompat::Compatible {
                basis: CompatBasis::RootMover
            }
        ));
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
            assess_read_compat(&m, &[w]),
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
            assess_read_compat(&m, &[w]),
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
            assess_read_compat(&m, &[w]),
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
            assess_read_compat(&m, &[w]),
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
            assess_read_compat(&m, &[w]),
            MoverReadCompat::LikelyIncompatible { .. }
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
            assess_read_compat(&m, &[w]),
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
        let forward = assess_read_compat(&m, &[a.clone(), b.clone()]);
        let reversed = assess_read_compat(&m, &[b, a]);
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
