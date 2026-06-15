//! Security-context invariants for the resolved mover pod.
//!
//! The mover's effective container + pod security contexts are produced by the field-wise
//! merge `hardened ⊂ moverDefaults ⊂ recipe`/`inheritSecurityContextFrom`
//! ([`crate::common::resolve_mover`]). Because each field is taken from whichever layer set
//! it, the merge can assemble a *combination* no single layer intended — and some such
//! combinations are rejected by the kubelet or the API server. A rejected pod never reaches
//! a terminal phase, so the Job's `backoffLimit` never trips and only the (long, hours-scale)
//! `activeDeadlineSeconds` would ever stop it: the mover hangs, hammering the API, exactly
//! the production failure that motivated this module.
//!
//! This is the single place that normalizes a resolved `(SecurityContext,
//! PodSecurityContext)` pair into a spec the kubelet/apiserver always accept. Each invariant
//! is a small, pure, individually-tested function; [`enforce_security_context_invariants`]
//! composes them. Normalization only ever *relaxes a self-contradiction into the intent the
//! merge clearly expressed* (e.g. an inherited root UID means "run as root") — it never
//! grants privilege the layers didn't ask for, and elevated results are still caught by the
//! privileged-mover gate ([`crate::common::requires_privilege_resolved`]).
//!
//! Adding a new invariant: write a `fn(SecurityContext[, PodSecurityContext]) -> …` that is
//! idempotent (applying it twice equals applying it once) and a no-op on already-valid
//! input, chain it in [`enforce_security_context_invariants`], and add a focused test plus a
//! case to the `all_invariants_are_idempotent` test.

use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};

/// Capability whose presence in `capabilities.add` is, like `privileged: true`, incompatible
/// with `allowPrivilegeEscalation: false` (API-server validation).
const CAP_SYS_ADMIN: &str = "CAP_SYS_ADMIN";

/// Enforce every security-context invariant on a fully-resolved mover `(container, pod)`
/// context pair, returning a kubelet/apiserver-valid pair. Pure; the composition order is
/// irrelevant because the invariants touch disjoint fields. Idempotent.
pub fn enforce_security_context_invariants(
    sc: SecurityContext,
    psc: Option<PodSecurityContext>,
) -> (SecurityContext, Option<PodSecurityContext>) {
    // INV-1: a root effective UID is incompatible with `runAsNonRoot: true`.
    let (sc, psc) = root_uid_excludes_run_as_non_root(sc, psc);
    // INV-2: a privileged / CAP_SYS_ADMIN container is incompatible with
    // `allowPrivilegeEscalation: false`.
    let sc = privileged_excludes_no_privilege_escalation(sc);
    (sc, psc)
}

/// **INV-1 — `runAsUser == 0` ⟹ `runAsNonRoot != true`.**
///
/// The kubelet rejects a container whose effective `runAsUser` is `0` while `runAsNonRoot`
/// is `true` (*"container's runAsUser breaks non-root policy"*) and parks it in
/// `CreateContainerConfigError`. This is produced when `inheritSecurityContextFrom` copies
/// `runAsUser: 0` off a **root** workload while the hardened base
/// ([`crate::common::hardened_security_context`]) still carries `runAsNonRoot: true`.
///
/// The effective UID follows kubelet precedence (`container.runAsUser ?? pod.runAsUser`), so
/// a pod-level root UID also clears the container's `runAsNonRoot: true`. We flip
/// `Some(true)` → `Some(false)` (not `None`) so the resulting root mover is *explicitly*
/// root and is still recognized as elevated by the privileged-mover gate.
fn root_uid_excludes_run_as_non_root(
    mut sc: SecurityContext,
    psc: Option<PodSecurityContext>,
) -> (SecurityContext, Option<PodSecurityContext>) {
    let effective_run_as_user = sc
        .run_as_user
        .or_else(|| psc.as_ref().and_then(|p| p.run_as_user));
    if effective_run_as_user != Some(0) {
        return (sc, psc);
    }
    if sc.run_as_non_root == Some(true) {
        sc.run_as_non_root = Some(false);
    }
    let psc = psc.map(|mut p| {
        if p.run_as_non_root == Some(true) {
            p.run_as_non_root = Some(false);
        }
        p
    });
    (sc, psc)
}

/// **INV-2 — `privileged: true` (or `capabilities.add: [CAP_SYS_ADMIN]`) ⟹
/// `allowPrivilegeEscalation != false`.**
///
/// The API server rejects a container that sets `allowPrivilegeEscalation: false` together
/// with `privileged: true` or an added `CAP_SYS_ADMIN` (*"cannot set
/// `allowPrivilegeEscalation` to false and `privileged` to true"*) — the Job's pod template
/// is invalid, so no pod is ever created and the run hangs. This is produced when a workload
/// or `moverDefaults` requests privilege while the hardened base
/// ([`crate::common::hardened_security_context`]) still carries
/// `allowPrivilegeEscalation: false`.
///
/// Privilege already implies escalation, so we clear the contradictory `Some(false)` →
/// `Some(true)`, matching the privilege the layers asked for. The result is still caught by
/// the privileged-mover gate.
fn privileged_excludes_no_privilege_escalation(mut sc: SecurityContext) -> SecurityContext {
    let demands_escalation = sc.privileged == Some(true)
        || sc
            .capabilities
            .as_ref()
            .and_then(|c| c.add.as_ref())
            .is_some_and(|add| add.iter().any(|c| c == CAP_SYS_ADMIN));
    if demands_escalation && sc.allow_privilege_escalation == Some(false) {
        sc.allow_privilege_escalation = Some(true);
    }
    sc
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::Capabilities;

    /// Helper: a container SC with the hardened-relevant fields set.
    fn sc(run_as_user: Option<i64>, run_as_non_root: Option<bool>) -> SecurityContext {
        SecurityContext {
            run_as_user,
            run_as_non_root,
            ..Default::default()
        }
    }

    // --- INV-1 ---

    #[test]
    fn inv1_container_root_uid_clears_run_as_non_root() {
        let (out, _) = enforce_security_context_invariants(sc(Some(0), Some(true)), None);
        assert_eq!(out.run_as_user, Some(0));
        assert_eq!(out.run_as_non_root, Some(false));
    }

    #[test]
    fn inv1_pod_root_uid_clears_container_run_as_non_root() {
        // Effective UID = container.runAsUser ?? pod.runAsUser → the pod's 0 wins.
        let psc = PodSecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        };
        let (out_sc, out_psc) =
            enforce_security_context_invariants(sc(None, Some(true)), Some(psc));
        assert_eq!(
            out_sc.run_as_non_root,
            Some(false),
            "pod-level root UID clears the container's runAsNonRoot:true"
        );
        // The pod context had no runAsNonRoot to clear — left as-is, never Some(true).
        assert_ne!(out_psc.unwrap().run_as_non_root, Some(true));
    }

    #[test]
    fn inv1_pod_level_run_as_non_root_true_with_pod_root_uid_is_cleared() {
        let psc = PodSecurityContext {
            run_as_user: Some(0),
            run_as_non_root: Some(true),
            ..Default::default()
        };
        let (_, out_psc) =
            enforce_security_context_invariants(SecurityContext::default(), Some(psc));
        assert_eq!(out_psc.unwrap().run_as_non_root, Some(false));
    }

    #[test]
    fn inv1_nonroot_uid_is_untouched() {
        let (out, _) = enforce_security_context_invariants(sc(Some(2000), Some(true)), None);
        assert_eq!(
            out.run_as_non_root,
            Some(true),
            "non-root UID keeps the hardening"
        );
    }

    #[test]
    fn inv1_unset_run_as_non_root_with_root_uid_stays_unset() {
        // Nothing to fix: runAsUser:0 with runAsNonRoot unset is already kubelet-valid.
        let (out, _) = enforce_security_context_invariants(sc(Some(0), None), None);
        assert_eq!(out.run_as_non_root, None);
    }

    // --- INV-2 ---

    #[test]
    fn inv2_privileged_clears_no_privilege_escalation() {
        let input = SecurityContext {
            privileged: Some(true),
            allow_privilege_escalation: Some(false),
            ..Default::default()
        };
        let (out, _) = enforce_security_context_invariants(input, None);
        assert_eq!(
            out.allow_privilege_escalation,
            Some(true),
            "privileged:true is incompatible with allowPrivilegeEscalation:false"
        );
    }

    #[test]
    fn inv2_cap_sys_admin_clears_no_privilege_escalation() {
        let input = SecurityContext {
            allow_privilege_escalation: Some(false),
            capabilities: Some(Capabilities {
                add: Some(vec!["CAP_SYS_ADMIN".to_string()]),
                drop: Some(vec!["ALL".to_string()]),
            }),
            ..Default::default()
        };
        let (out, _) = enforce_security_context_invariants(input, None);
        assert_eq!(out.allow_privilege_escalation, Some(true));
        // Unrelated capabilities are untouched.
        assert_eq!(out.capabilities.unwrap().drop.unwrap(), vec!["ALL"]);
    }

    #[test]
    fn inv2_unprivileged_keeps_hardened_no_escalation() {
        // The common case: no privilege requested → the hardened allowPrivilegeEscalation:false
        // must survive (we must never relax hardening for a normal mover).
        let input = SecurityContext {
            allow_privilege_escalation: Some(false),
            capabilities: Some(Capabilities {
                add: Some(vec!["NET_BIND_SERVICE".to_string()]),
                drop: Some(vec!["ALL".to_string()]),
            }),
            ..Default::default()
        };
        let (out, _) = enforce_security_context_invariants(input, None);
        assert_eq!(out.allow_privilege_escalation, Some(false));
    }

    // --- composition / general properties ---

    #[test]
    fn combined_root_and_privileged_fixes_both() {
        let input = SecurityContext {
            run_as_user: Some(0),
            run_as_non_root: Some(true),
            privileged: Some(true),
            allow_privilege_escalation: Some(false),
            ..Default::default()
        };
        let (out, _) = enforce_security_context_invariants(input, None);
        assert_eq!(out.run_as_non_root, Some(false));
        assert_eq!(out.allow_privilege_escalation, Some(true));
    }

    #[test]
    fn a_fully_hardened_nonroot_context_is_unchanged() {
        // The default mover: runAsNonRoot:true, ape:false, no root UID, no privilege →
        // every invariant is a no-op.
        let input = crate::common::hardened_security_context();
        let (out, _) = enforce_security_context_invariants(input.clone(), None);
        assert_eq!(
            out, input,
            "invariants must not touch an already-valid hardened context"
        );
    }

    #[test]
    fn all_invariants_are_idempotent() {
        // Applying the full set twice equals applying it once — a property every invariant
        // must preserve so repeated reconciles never oscillate.
        let cases = [
            (sc(Some(0), Some(true)), None),
            (
                SecurityContext {
                    privileged: Some(true),
                    allow_privilege_escalation: Some(false),
                    ..Default::default()
                },
                None,
            ),
            (
                sc(None, Some(true)),
                Some(PodSecurityContext {
                    run_as_user: Some(0),
                    run_as_non_root: Some(true),
                    ..Default::default()
                }),
            ),
        ];
        for (sc_in, psc_in) in cases {
            let once = enforce_security_context_invariants(sc_in.clone(), psc_in.clone());
            let twice = enforce_security_context_invariants(once.0.clone(), once.1.clone());
            assert_eq!(once, twice, "invariants must be idempotent");
        }
    }
}
