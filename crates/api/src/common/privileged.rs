//! What `mover.privilegedMode: true` actually grants a mover (#552).
//!
//! The hardened base layer ([`super::hardened_security_context`]) drops **every** Linux
//! capability, and the field-wise merge keeps that `drop: [ALL]` unless a layer sets its
//! own `capabilities`. So a mover running as `runAsUser: 0` with nothing else is root
//! *without* `CAP_DAC_OVERRIDE`: the kernel applies normal owner/group/other permission
//! checks to it, and a `0660` file owned by another uid/gid is unreadable. Before #552,
//! `privilegedMode` only tripped the namespace gate and granted nothing on top.
//!
//! This module is the single, pure definition of the grant. It runs on the RESOLVED
//! mover context (after the `hardened ⊂ moverDefaults ⊂ inherited ⊂ explicit` merge),
//! so the webhook and the controller compute it identically.
//!
//! # The contract
//!
//! `privilegedMode` **never changes the UID**. Who the mover runs as decides who owns
//! the repository blobs it writes; silently switching an existing `65532` mover to root
//! would leave root-owned blobs on a filesystem repository that every other (non-root)
//! mover then cannot read, and breaks outright on an NFS export with `root_squash`.
//!
//! When `privilegedMode` is `true`:
//!
//! - If the effective (merged container/pod) `runAsUser` is **`0`**, the capabilities the
//!   [`MoverOperation`] needs are **added** on top of `drop: [ALL]`, unioned with any
//!   `capabilities.add` a layer already set (never removing one).
//! - Otherwise — a pinned non-zero UID, or no UID at all (the image's unprivileged
//!   `65532`) — it is inert: Kubernetes sets no ambient capabilities, so added
//!   capabilities would do nothing for a non-root process. Nothing is granted and the
//!   case is reported ([`PrivilegedModeEffect::InertNonRootUid`] /
//!   [`PrivilegedModeEffect::InertUnsetUid`]) so the webhook and the controller can tell
//!   the user to set `runAsUser: 0`.
//!
//! Every capability granted here is in the Pod Security Standards **baseline** allow
//! list ("Adding additional capabilities beyond those listed below must be disallowed:
//! AUDIT_WRITE, CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, MKNOD, NET_BIND_SERVICE,
//! SETFCAP, SETGID, SETPCAP, SETUID, SYS_CHROOT" —
//! <https://kubernetes.io/docs/concepts/security/pod-security-standards/#baseline>).
//! That is why a backup gets `DAC_OVERRIDE` and not the narrower-sounding
//! `DAC_READ_SEARCH`: the latter is NOT on the baseline list, so it would make a
//! privileged mover fail admission in a `baseline`-enforced namespace. (A root mover
//! already violates `restricted`, which requires `runAsNonRoot`.)

use k8s_openapi::api::core::v1::{Capabilities, PodSecurityContext, SecurityContext};

use super::effective_run_as_user;

/// `CAP_DAC_OVERRIDE`: bypass file read/write/execute permission checks.
pub const CAP_DAC_OVERRIDE: &str = "DAC_OVERRIDE";
/// `CAP_DAC_READ_SEARCH`: bypass file read and directory search checks. Never granted
/// by kopiur (not PSS-baseline), but honored when a user adds it themselves.
pub const CAP_DAC_READ_SEARCH: &str = "DAC_READ_SEARCH";
/// `CAP_CHOWN`: change a file's owner and group to any uid/gid.
pub const CAP_CHOWN: &str = "CHOWN";
/// `CAP_FOWNER`: bypass owner-only checks (chmod, utimes, …) on files it doesn't own.
pub const CAP_FOWNER: &str = "FOWNER";
/// `CAP_FSETID`: keep setuid/setgid bits when changing a file's owner or mode.
pub const CAP_FSETID: &str = "FSETID";

/// What a mover Job does, for deciding which capabilities `privilegedMode` grants.
/// Exhaustive on purpose: a new mover operation does not compile until it states what
/// privilege it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoverOperation {
    /// Snapshot a source volume (reads arbitrary files).
    Backup,
    /// Restore into a target volume (writes files and reproduces their owner and mode).
    Restore,
    /// Repository maintenance.
    Maintenance,
    /// Snapshot verification.
    Verify,
    /// Repository create/connect.
    Bootstrap,
    /// Repository-to-repository replication.
    RepositoryReplicate,
    /// Snapshot replication.
    SnapshotReplicate,
    /// Batched kopia snapshot deletion.
    SnapshotDelete,
    /// Pin/unpin a kopia snapshot.
    SnapshotPin,
    /// Browse a snapshot's contents.
    Browse,
}

impl MoverOperation {
    /// The capabilities `privilegedMode: true` adds for this operation. Empty means
    /// `privilegedMode` grants nothing beyond tripping the namespace gate.
    pub fn privileged_capabilities(self) -> &'static [&'static str] {
        match self {
            // Read every file in the source regardless of owner and mode.
            MoverOperation::Backup => &[CAP_DAC_OVERRIDE],
            // Write into directories it doesn't own (DAC_OVERRIDE), give each file its
            // recorded owner (CHOWN), then set mode/times on a file it no longer owns
            // (FOWNER) without the kernel clearing setuid/setgid bits (FSETID).
            MoverOperation::Restore => &[CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_FOWNER, CAP_FSETID],
            // These only talk to the repository (or the kopia cache the operator owns
            // through fsGroup). None of them mounts a user source/target volume — browse
            // reads snapshot content through kopia, not from a PVC — so there is no file
            // owned by a foreign uid for a capability to unlock.
            MoverOperation::Maintenance
            | MoverOperation::Verify
            | MoverOperation::Bootstrap
            | MoverOperation::RepositoryReplicate
            | MoverOperation::SnapshotReplicate
            | MoverOperation::SnapshotDelete
            | MoverOperation::SnapshotPin
            | MoverOperation::Browse => &[],
        }
    }
}

/// What [`apply_privileged_mode`] did to a resolved mover context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegedModeEffect {
    /// `privilegedMode` is not `true`; the context is unchanged.
    NotRequested,
    /// `privilegedMode` is `true` but the operation needs no capability; unchanged.
    NothingToGrant,
    /// The mover runs as root, so the operation's capabilities were added.
    Granted,
    /// A layer pins a non-zero UID, so capabilities would be inert; nothing was granted.
    InertNonRootUid {
        /// The pinned effective UID.
        uid: i64,
    },
    /// No layer pins a UID, so the mover runs as the image's non-root user and
    /// capabilities would be inert; nothing was granted and the UID is left alone.
    InertUnsetUid,
}

impl PrivilegedModeEffect {
    /// A short user-facing message for the inert cases (`None` otherwise).
    pub fn warning(&self) -> Option<String> {
        match self {
            PrivilegedModeEffect::InertNonRootUid { uid } => Some(format!(
                "privilegedMode grants capabilities only to a root mover; this one runs as \
                 UID {uid}. Fix: set securityContext.runAsUser: 0."
            )),
            PrivilegedModeEffect::InertUnsetUid => Some(
                "privilegedMode grants capabilities only to a root mover. Fix: set \
                 securityContext.runAsUser: 0."
                    .to_string(),
            ),
            PrivilegedModeEffect::NotRequested
            | PrivilegedModeEffect::NothingToGrant
            | PrivilegedModeEffect::Granted => None,
        }
    }
}

/// Decide what `privilegedMode` does to a resolved `(container, pod)` context without
/// changing it. Pure; the decision half of [`apply_privileged_mode`].
pub fn privileged_mode_effect(
    sc: &SecurityContext,
    psc: Option<&PodSecurityContext>,
    privileged_mode: Option<bool>,
    op: MoverOperation,
) -> PrivilegedModeEffect {
    if privileged_mode != Some(true) {
        return PrivilegedModeEffect::NotRequested;
    }
    if op.privileged_capabilities().is_empty() {
        return PrivilegedModeEffect::NothingToGrant;
    }
    match effective_run_as_user(Some(sc), psc) {
        None => PrivilegedModeEffect::InertUnsetUid,
        Some(0) => PrivilegedModeEffect::Granted,
        Some(uid) => PrivilegedModeEffect::InertNonRootUid { uid },
    }
}

/// Apply `privilegedMode` to a resolved mover context (see the module docs for the
/// contract). Pure and idempotent. Only ever touches `capabilities` — never the UID.
pub fn apply_privileged_mode(
    mut sc: SecurityContext,
    psc: Option<PodSecurityContext>,
    privileged_mode: Option<bool>,
    op: MoverOperation,
) -> (
    SecurityContext,
    Option<PodSecurityContext>,
    PrivilegedModeEffect,
) {
    let effect = privileged_mode_effect(&sc, psc.as_ref(), privileged_mode, op);
    match &effect {
        PrivilegedModeEffect::Granted => {
            sc.capabilities = Some(add_capabilities(
                sc.capabilities.take(),
                op.privileged_capabilities(),
            ));
        }
        PrivilegedModeEffect::NotRequested
        | PrivilegedModeEffect::NothingToGrant
        | PrivilegedModeEffect::InertNonRootUid { .. }
        | PrivilegedModeEffect::InertUnsetUid => {}
    }
    (sc, psc, effect)
}

/// Union `grant` into `caps.add`, keeping `drop` and every capability already added.
/// A capability already present (with or without the `CAP_` prefix, any case) is not
/// duplicated.
fn add_capabilities(caps: Option<Capabilities>, grant: &[&str]) -> Capabilities {
    let mut caps = caps.unwrap_or_default();
    let mut add = caps.add.take().unwrap_or_default();
    for cap in grant {
        if !add.iter().any(|have| same_capability(have, cap)) {
            add.push((*cap).to_string());
        }
    }
    caps.add = Some(add);
    caps
}

/// Whether two capability names refer to the same capability. Kubernetes accepts both
/// `DAC_OVERRIDE` and `CAP_DAC_OVERRIDE`.
pub fn same_capability(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        let up = s.trim().to_ascii_uppercase();
        up.strip_prefix("CAP_").map(str::to_string).unwrap_or(up)
    }
    norm(a) == norm(b)
}

/// Whether a container context can read any file regardless of owner and mode: it is
/// `privileged: true`, or it adds `DAC_OVERRIDE` or `DAC_READ_SEARCH`. Only meaningful
/// for a root process (capabilities are inert for any other UID).
pub fn bypasses_read_permission_checks(sc: &SecurityContext) -> bool {
    sc.privileged == Some(true)
        || sc
            .capabilities
            .as_ref()
            .and_then(|c| c.add.as_ref())
            .is_some_and(|add| {
                add.iter().any(|c| {
                    same_capability(c, CAP_DAC_OVERRIDE) || same_capability(c, CAP_DAC_READ_SEARCH)
                })
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{ResolvedMover, resolve_mover};

    fn root_sc() -> SecurityContext {
        SecurityContext {
            run_as_user: Some(0),
            run_as_non_root: Some(false),
            ..Default::default()
        }
    }

    fn resolved(
        sc: Option<&SecurityContext>,
        privileged: Option<bool>,
        op: MoverOperation,
    ) -> ResolvedMover {
        resolve_mover(None, sc, None, None, None, None).with_privileged_mode(privileged, op)
    }

    fn adds(m: &ResolvedMover) -> Vec<String> {
        m.security_context
            .capabilities
            .as_ref()
            .and_then(|c| c.add.clone())
            .unwrap_or_default()
    }

    fn drops(m: &ResolvedMover) -> Vec<String> {
        m.security_context
            .capabilities
            .as_ref()
            .and_then(|c| c.drop.clone())
            .unwrap_or_default()
    }

    /// #552: the reported configuration. A privileged root backup must be able to read a
    /// 0660 file owned by someone else, which takes DAC_OVERRIDE — while still dropping
    /// every other capability.
    #[test]
    fn privileged_root_backup_gets_dac_override_and_still_drops_all() {
        let m = resolved(Some(&root_sc()), Some(true), MoverOperation::Backup);
        assert_eq!(adds(&m), vec!["DAC_OVERRIDE"]);
        assert_eq!(drops(&m), vec!["ALL"]);
        assert_eq!(m.security_context.run_as_user, Some(0));
        assert_eq!(m.security_context.run_as_non_root, Some(false));
        assert_eq!(m.security_context.allow_privilege_escalation, Some(false));
    }

    #[test]
    fn privileged_root_restore_gets_the_four_ownership_capabilities() {
        let m = resolved(Some(&root_sc()), Some(true), MoverOperation::Restore);
        assert_eq!(adds(&m), vec!["CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID"]);
        assert_eq!(drops(&m), vec!["ALL"]);
    }

    /// Root alone stays capability-less: privilege is an explicit opt-in.
    #[test]
    fn non_privileged_root_gets_no_capabilities() {
        for op in [MoverOperation::Backup, MoverOperation::Restore] {
            for pm in [None, Some(false)] {
                let m = resolved(Some(&root_sc()), pm, op);
                assert!(adds(&m).is_empty(), "{op:?} {pm:?}");
                assert_eq!(drops(&m), vec!["ALL"]);
            }
        }
    }

    #[test]
    fn user_added_capabilities_are_kept_and_unioned() {
        let mut sc = root_sc();
        sc.capabilities = Some(Capabilities {
            add: Some(vec!["SETUID".into(), "cap_dac_override".into()]),
            drop: None,
        });
        let m = resolved(Some(&sc), Some(true), MoverOperation::Restore);
        assert_eq!(
            adds(&m),
            vec!["SETUID", "cap_dac_override", "CHOWN", "FOWNER", "FSETID"],
            "nothing the user asked for is removed and DAC_OVERRIDE is not duplicated"
        );
        assert_eq!(
            drops(&m),
            vec!["ALL"],
            "the hardened drop:[ALL] survives an add-only override"
        );
    }

    /// No layer pins a UID: privilegedMode never picks one (it never changes who writes
    /// repository blobs), so the mover keeps the image's non-root user and is granted
    /// nothing — and the user is told.
    #[test]
    fn privileged_with_no_pinned_uid_keeps_the_uid_grants_nothing_and_warns() {
        for op in [MoverOperation::Backup, MoverOperation::Restore] {
            let base = resolve_mover(None, None, None, None, None, None);
            let (m, effect) = base.clone().with_privileged_mode_effect(Some(true), op);
            assert_eq!(m, base, "{op:?}: uid/runAsNonRoot/caps unchanged");
            assert_eq!(m.security_context.run_as_user, None);
            assert_eq!(m.security_context.run_as_non_root, Some(true));
            assert!(adds(&m).is_empty());
            assert_eq!(effect, PrivilegedModeEffect::InertUnsetUid);
            let msg = effect.warning().expect("inert privilegedMode warns");
            assert!(
                msg.contains("root mover") && msg.contains("Fix: set securityContext.runAsUser: 0"),
                "{msg}"
            );
        }
    }

    /// A pinned non-zero UID is respected, and nothing inert is added.
    #[test]
    fn privileged_with_a_pinned_non_root_uid_grants_nothing_and_warns() {
        let sc = SecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        };
        let base = resolve_mover(None, Some(&sc), None, None, None, None);
        let (out, _, effect) = apply_privileged_mode(
            base.security_context.clone(),
            base.pod_security_context.clone(),
            Some(true),
            MoverOperation::Backup,
        );
        assert_eq!(out, base.security_context);
        assert_eq!(effect, PrivilegedModeEffect::InertNonRootUid { uid: 1000 });
        let msg = effect.warning().expect("inert privilegedMode warns");
        assert!(msg.contains("UID 1000") && msg.contains("Fix:"), "{msg}");
    }

    /// A pod-level root UID is root too (kubelet precedence).
    #[test]
    fn pod_level_root_is_granted() {
        let psc = PodSecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        };
        let m = resolve_mover(None, None, Some(&psc), None, None, None)
            .with_privileged_mode(Some(true), MoverOperation::Backup);
        assert_eq!(adds(&m), vec!["DAC_OVERRIDE"]);
        assert_eq!(m.security_context.run_as_user, Some(0));
    }

    #[test]
    fn repository_only_operations_grant_nothing() {
        for op in [
            MoverOperation::Maintenance,
            MoverOperation::Verify,
            MoverOperation::Bootstrap,
            MoverOperation::RepositoryReplicate,
            MoverOperation::SnapshotReplicate,
            MoverOperation::SnapshotDelete,
            MoverOperation::SnapshotPin,
            MoverOperation::Browse,
        ] {
            let base = resolve_mover(None, Some(&root_sc()), None, None, None, None);
            let m = base.clone().with_privileged_mode(Some(true), op);
            assert_eq!(m, base, "{op:?}");
        }
    }

    #[test]
    fn admission_warning_unless_the_spec_pins_root() {
        use crate::common::MoverSpec;
        let non_root = MoverSpec {
            security_context: Some(SecurityContext {
                run_as_user: Some(1000),
                ..Default::default()
            }),
            privileged_mode: Some(true),
            ..Default::default()
        };
        assert!(
            non_root
                .privileged_mode_warning(MoverOperation::Backup)
                .is_some()
        );
        // Not for an operation privilegedMode grants nothing to anyway.
        assert!(
            non_root
                .privileged_mode_warning(MoverOperation::Maintenance)
                .is_none()
        );
        let root = MoverSpec {
            security_context: Some(root_sc()),
            privileged_mode: Some(true),
            ..Default::default()
        };
        assert!(
            root.privileged_mode_warning(MoverOperation::Restore)
                .is_none()
        );
        let unset = MoverSpec {
            privileged_mode: Some(true),
            ..Default::default()
        };
        // No uid on the object itself: the repository's moverDefaults may still pin
        // root, which admission cannot see, so it stays silent rather than warn on a
        // config that may be correct. The controller's PrivilegedModeInert Event,
        // which sees the merged context, reports the truly-inert case.
        assert!(
            unset
                .privileged_mode_warning(MoverOperation::Backup)
                .is_none(),
            "admission must not guess about a uid moverDefaults may pin"
        );
        let off = MoverSpec {
            privileged_mode: Some(false),
            ..non_root.clone()
        };
        assert!(
            off.privileged_mode_warning(MoverOperation::Backup)
                .is_none()
        );
    }

    #[test]
    fn apply_is_idempotent() {
        for op in [MoverOperation::Backup, MoverOperation::Restore] {
            let once = resolved(Some(&root_sc()), Some(true), op);
            let twice = once.clone().with_privileged_mode(Some(true), op);
            assert_eq!(once, twice, "{op:?}");
        }
    }

    /// Every granted capability is on the PSS baseline allow list.
    #[test]
    fn every_grant_is_pss_baseline() {
        const BASELINE: &[&str] = &[
            "AUDIT_WRITE",
            "CHOWN",
            "DAC_OVERRIDE",
            "FOWNER",
            "FSETID",
            "KILL",
            "MKNOD",
            "NET_BIND_SERVICE",
            "SETFCAP",
            "SETGID",
            "SETPCAP",
            "SETUID",
            "SYS_CHROOT",
        ];
        for op in [MoverOperation::Backup, MoverOperation::Restore] {
            for cap in op.privileged_capabilities() {
                assert!(BASELINE.contains(cap), "{cap} is not PSS baseline");
            }
        }
    }

    #[test]
    fn read_bypass_detection() {
        let mut sc = root_sc();
        assert!(!bypasses_read_permission_checks(&sc));
        for cap in ["DAC_OVERRIDE", "CAP_DAC_READ_SEARCH", "dac_override"] {
            sc.capabilities = Some(Capabilities {
                add: Some(vec![cap.into()]),
                drop: Some(vec!["ALL".into()]),
            });
            assert!(bypasses_read_permission_checks(&sc), "{cap}");
        }
        sc.capabilities = Some(Capabilities {
            add: Some(vec!["CHOWN".into()]),
            drop: None,
        });
        assert!(!bypasses_read_permission_checks(&sc));
        let privileged = SecurityContext {
            privileged: Some(true),
            ..Default::default()
        };
        assert!(bypasses_read_permission_checks(&privileged));
    }
}
