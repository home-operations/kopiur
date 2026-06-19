//! Controller glue for the securityContext-compatibility heuristic
//! ([`kopiur_api::secctx_compat`]): turn a resolved mover + the live workload pods into a
//! `SecurityContextCompatible` / `RestoreSecurityContextCompatible` status condition and an
//! optional once-per-transition advisory Warning event. Warn-only — never blocks a run, and
//! the mover's runtime preflight remains the authoritative check for backups.

use k8s_openapi::api::core::v1::{Pod, PodSecurityContext, SecurityContext};

use kopiur_api::secctx_compat::{
    self, CompatBasis, MoverReadCompat, RestoreBasis, RestoreWriteCompat, WorkloadIdentity,
};

use crate::consts::{
    MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION, SECURITY_CONTEXT_COMPATIBLE_REASON,
    SECURITY_CONTEXT_LIKELY_INCOMPATIBLE_REASON, SECURITY_CONTEXT_UNDETERMINED_REASON,
};

/// A condition decision derived from a compatibility verdict: the tri-state `status`,
/// machine-readable `reason`, human `message`, and whether to emit an advisory event.
pub struct CompatVerdict {
    /// Condition `status`: `"True"` | `"Unknown"` | `"False"`.
    pub status: &'static str,
    /// Condition `reason`.
    pub reason: &'static str,
    /// Deterministic condition message (no volatile content).
    pub message: String,
    /// Whether to emit a once-per-transition advisory Warning event (only on a near-certain
    /// mismatch).
    pub warn: bool,
}

impl CompatVerdict {
    /// The event `action` (remediation hint) for the advisory warning.
    pub const ACTION: &'static str = MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION;
}

/// Render an effective UID for messages: the number, or `"<image-determined>"`.
fn uid_render(uid: Option<i64>) -> String {
    uid.map(|u| u.to_string())
        .unwrap_or_else(|| "<image-determined>".to_string())
}

/// Map a backup read-compatibility verdict to a condition decision.
pub fn backup_verdict(mover_uid: Option<i64>, v: &MoverReadCompat) -> CompatVerdict {
    match v {
        MoverReadCompat::Compatible { basis } => CompatVerdict {
            status: "True",
            reason: SECURITY_CONTEXT_COMPATIBLE_REASON,
            message: match basis {
                CompatBasis::RootMover => format!(
                    "mover runs as root (UID {}) and can read all source files",
                    uid_render(mover_uid)
                ),
                CompatBasis::ExactUidMatch => format!(
                    "mover UID {} matches the workload's UID; it can read the source",
                    uid_render(mover_uid)
                ),
            },
            warn: false,
        },
        MoverReadCompat::Unknown { why } => CompatVerdict {
            status: "Unknown",
            reason: SECURITY_CONTEXT_UNDETERMINED_REASON,
            message: format!(
                "source readability cannot be determined from securityContext alone ({}); the \
                 mover verifies it at runtime",
                why.as_str()
            ),
            warn: false,
        },
        MoverReadCompat::LikelyIncompatible { .. } => CompatVerdict {
            status: "False",
            reason: SECURITY_CONTEXT_LIKELY_INCOMPATIBLE_REASON,
            // Reuse the enum's deterministic summary (sorted UID list, no volatile content).
            message: v.summary(&uid_render(mover_uid)),
            warn: true,
        },
    }
}

/// Map a restore write-compatibility verdict to a condition decision.
pub fn restore_verdict(v: &RestoreWriteCompat) -> CompatVerdict {
    match v {
        RestoreWriteCompat::Compatible { basis } => CompatVerdict {
            status: "True",
            reason: SECURITY_CONTEXT_COMPATIBLE_REASON,
            message: match basis {
                RestoreBasis::WorkloadOwnsFiles => {
                    "the future workload's UID owns the restored files; it can read them"
                        .to_string()
                }
                RestoreBasis::FsGroupMatch => "the mover's fsGroup matches the future workload's \
                     fsGroup; the restored files are group-readable by it"
                    .to_string(),
            },
            warn: false,
        },
        RestoreWriteCompat::Unknown { .. } => CompatVerdict {
            status: "Unknown",
            reason: SECURITY_CONTEXT_UNDETERMINED_REASON,
            message: "whether the future workload can read the restored files cannot be \
                      determined from securityContext alone (no running consumer pod, or unpinned \
                      UID); set mover.inheritSecurityContextFrom.workloadSelector or a matching \
                      runAsUser/fsGroup to be sure"
                .to_string(),
            warn: false,
        },
        RestoreWriteCompat::LikelyIncompatible {
            mover_uid,
            workload_uid,
        } => CompatVerdict {
            status: "False",
            reason: SECURITY_CONTEXT_LIKELY_INCOMPATIBLE_REASON,
            message: format!(
                "restore mover writes as UID {} with an fsGroup the future workload (UID {}) \
                 shares neither of — it may not be able to read the restored files; set \
                 mover.inheritSecurityContextFrom.workloadSelector, or a matching runAsUser/fsGroup",
                uid_render(*mover_uid),
                uid_render(*workload_uid)
            ),
            warn: true,
        },
    }
}

/// Build the [`WorkloadIdentity`] list for the pods mounting `claim` (excluding none here;
/// the caller filters kopiur movers when relevant). Convenience wrapper over the pure core.
pub fn workload_identities(pods: &[Pod], claim: &str) -> Vec<WorkloadIdentity> {
    secctx_compat::pods_mounting_pvc(pods, claim)
        .into_iter()
        .map(secctx_compat::workload_identity)
        .collect()
}

/// Build the mover read identity from its resolved contexts.
pub fn mover_read_identity(
    sc: &SecurityContext,
    psc: Option<&PodSecurityContext>,
) -> secctx_compat::MoverIdentity {
    secctx_compat::mover_identity(sc, psc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::secctx_compat::{CompatBasis, UnknownReason};

    #[test]
    fn compatible_is_true_no_warn() {
        let v = backup_verdict(
            Some(0),
            &MoverReadCompat::Compatible {
                basis: CompatBasis::RootMover,
            },
        );
        assert_eq!(v.status, "True");
        assert!(!v.warn);
    }

    #[test]
    fn unknown_is_unknown_no_warn() {
        let v = backup_verdict(
            None,
            &MoverReadCompat::Unknown {
                why: UnknownReason::MoverUidUnpinned,
            },
        );
        assert_eq!(v.status, "Unknown");
        assert!(!v.warn);
    }

    #[test]
    fn likely_incompatible_is_false_and_warns_with_remedy() {
        let v = backup_verdict(
            Some(65532),
            &MoverReadCompat::LikelyIncompatible {
                mover_uid: 65532,
                workload_uids: vec![999],
            },
        );
        assert_eq!(v.status, "False");
        assert!(v.warn);
        assert!(v.message.contains("pvcConsumer"));
        assert!(v.message.contains("999"));
    }
}
