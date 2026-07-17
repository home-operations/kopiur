//! Best-effort securityContext-compatibility **admission warnings** — the earliest possible
//! surface (at `kubectl apply`). Non-blocking and fail-open: the authoritative checks are the
//! reconcile-time condition and the mover's runtime readability preflight.
//!
//! The webhook sees only the *recipe* securityContext (resolved over the hardened base, so
//! the mover's default `fsGroup` is accounted for — it always has one), and warns only when
//! the recipe **explicitly pins** a `runAsUser` — otherwise the mover UID is undetermined
//! and we stay silent. We warn only on a near-certain `LikelyIncompatible` verdict.
//!
//! Two things it is blind to, both of which can only make a warning WRONG, never missing:
//!
//! - **`moverDefaults`** — a repository-level `runAsUser`/`fsGroup` it cannot see.
//! - **`inheritSecurityContextFrom`** — resolved from a live pod at reconcile time, which
//!   admission cannot do. Its inherited `fsGroup`/`supplementalGroups` land in the mover's
//!   group set and can soften a UID mismatch to `Unknown`, so judging an inherit recipe from
//!   its explicit context alone would warn about a mismatch that does not exist.

use api::common::MoverSpec;
use api::secctx_compat::{self, MoverReadCompat, MoverWriteIdentity, RestoreWriteCompat};
use api::snapshot_policy::Source;
use k8s_openapi::api::core::v1::Pod;
use kopiur_api as api;
use kube::api::ListParams;
use kube::{Api, Client};

/// The mover's recipe-level read identity, but ONLY when `runAsUser` is explicitly pinned in
/// the recipe (so the webhook can actually decide). `None` ⇒ stay silent.
fn pinned_mover_identity(mover: Option<&MoverSpec>) -> Option<secctx_compat::MoverIdentity> {
    let m = mover?;
    // An inherit recipe's real identity is only known once a live workload pod is resolved at
    // reconcile time. Judging it from the explicit context alone would miss the inherited
    // groups that soften a mismatch, producing a warning that contradicts what the operator
    // actually does. (The two were mutually exclusive before, so this path never spoke; the
    // guard keeps that silence now that they combine.)
    if m.inherit_security_context_from.is_some() {
        return None;
    }
    // Resolve the hardened base into the recipe rather than reading the raw spec: the
    // mover ALWAYS runs with `fsGroup: 65532` (hardened_pod_security_context) even when a
    // recipe pins nothing but `runAsUser`, so the raw spec reports no fsGroup for a mover
    // that certainly has one. That mattered little while fsGroup only softened a group
    // comparison; with a writable source (#254) it decides the verdict outright, and the
    // raw view would warn `LikelyIncompatible` for exactly the configuration the flag
    // exists to enable — while the controller, which resolves properly, says
    // `FsGroupMayApply`. Two layers contradicting each other is worse than either.
    //
    // `defaults: None` keeps this hardened ⊂ recipe: the repository's `moverDefaults` is
    // still invisible here (see the module docs), so this is strictly closer to runtime,
    // not equal to it. Fail-open still applies.
    let resolved = kopiur_api::common::resolve_mover(
        None,
        m.security_context.as_ref(),
        m.pod_security_context.as_ref(),
        None,
        None,
        None,
    );
    let id = secctx_compat::mover_identity(
        &resolved.security_context,
        resolved.pod_security_context.as_ref(),
    );
    // Only when the recipe pins a UID (container or pod) — else the resolved UID is
    // unknown. The hardened base sets `runAsNonRoot` but NOT `runAsUser`, so a recipe that
    // pins no UID still resolves to `None` here and stays silent, as before.
    id.uid.map(|_| id)
}

/// One checkable backup source: the PVC to assess, and how the mover will mount it.
struct ClaimMount<'a> {
    /// The `source.pvc` name to look for consuming pods of.
    claim: &'a str,
    /// The source's effective `readOnly`. A read-write mount lets the kubelet apply the
    /// mover's `fsGroup` to the tree, which changes the verdict (see `assess_read_compat`).
    read_only: bool,
}

/// Best-effort admission warnings for a backup config's `source.pvc` entries. Fails open:
/// no client, no pods, or any error → no warning.
pub async fn backup_warnings(
    client: Option<&Client>,
    namespace: Option<&str>,
    mover: Option<&MoverSpec>,
    sources: &[Source],
) -> Vec<String> {
    let (Some(client), Some(ns)) = (client, namespace) else {
        return Vec::new();
    };
    let Some(mover_id) = pinned_mover_identity(mover) else {
        return Vec::new();
    };
    // Only PVC sources can be checked at admission (pvcSelector is dynamic; NFS has no pod).
    // Each carries its own mount mode: `readOnly` is per-source, and it decides whether
    // the mover's fsGroup can reach the tree at all — a bare bool in a tuple here would
    // be unreadable at the use site 20 lines down.
    let claims: Vec<ClaimMount<'_>> = sources
        .iter()
        .filter_map(|s| {
            s.pvc.as_ref().map(|p| ClaimMount {
                claim: p.name.as_str(),
                read_only: kopiur_api::snapshot_policy::source_read_only(s),
            })
        })
        .collect();
    if claims.is_empty() {
        return Vec::new();
    }
    let pods = match Api::<Pod>::namespaced(client.clone(), ns)
        .list(&ListParams::default())
        .await
    {
        Ok(list) => list.items,
        Err(_) => return Vec::new(),
    };

    let mut warnings = Vec::new();
    for ClaimMount { claim, read_only } in claims {
        // `workload_identities` mounts-the-claim + excludes kopiur mover pods (shared core).
        let identities = secctx_compat::workload_identities(&pods, claim);
        if let MoverReadCompat::LikelyIncompatible { .. } =
            secctx_compat::assess_read_compat(&mover_id, &identities, read_only)
        {
            warnings.push(format!(
                "securityContext: the mover's UID likely cannot read the source PVC `{claim}` \
                 (no shared UID or group with the workload that mounts it) — the backup may fail \
                 with permission denied or silently skip unreadable files. Match the mover via \
                 mover.inheritSecurityContextFrom.pvcConsumer, or a matching runAsUser/fsGroup."
            ));
        }
    }
    warnings
}

/// Best-effort restore-direction admission warning. Fails open.
pub async fn restore_warnings(
    client: Option<&Client>,
    namespace: Option<&str>,
    mover: Option<&MoverSpec>,
    target_pvc: Option<&str>,
) -> Vec<String> {
    let (Some(client), Some(ns), Some(claim)) = (client, namespace, target_pvc) else {
        return Vec::new();
    };
    let Some(m) = mover else {
        return Vec::new();
    };
    let Some(sc) = m.security_context.as_ref() else {
        return Vec::new();
    };
    let write_id: MoverWriteIdentity =
        secctx_compat::mover_write_identity(sc, m.pod_security_context.as_ref());
    // Need a pinned write UID to say anything confident.
    if write_id.uid.is_none() {
        return Vec::new();
    }
    let pods = match Api::<Pod>::namespaced(client.clone(), ns)
        .list(&ListParams::default())
        .await
    {
        Ok(list) => list.items,
        Err(_) => return Vec::new(),
    };
    let consumer = secctx_compat::workload_identities(&pods, claim)
        .into_iter()
        .next();
    if let RestoreWriteCompat::LikelyIncompatible { .. } =
        secctx_compat::assess_restore_compat(&write_id, consumer.as_ref())
    {
        vec![format!(
            "securityContext: the future workload consuming the restore target `{claim}` likely \
             cannot read what the mover writes (no shared UID or fsGroup). Set \
             mover.inheritSecurityContextFrom.workloadSelector, or a matching runAsUser/fsGroup."
        )]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};

    #[test]
    fn webhook_stays_silent_unless_run_as_user_is_pinned() {
        // No mover → silent.
        assert!(pinned_mover_identity(None).is_none());

        // Mover with only a pod fsGroup (no pinned UID) → silent (UID is image-determined,
        // and the webhook lacks moverDefaults to resolve it).
        let unpinned = MoverSpec {
            security_context: Some(SecurityContext {
                run_as_non_root: Some(true),
                ..Default::default()
            }),
            pod_security_context: Some(PodSecurityContext {
                fs_group: Some(2000),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(pinned_mover_identity(Some(&unpinned)).is_none());

        // Mover with an explicit runAsUser → assessable.
        let pinned = MoverSpec {
            security_context: Some(SecurityContext {
                run_as_user: Some(65532),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            pinned_mover_identity(Some(&pinned)).and_then(|i| i.uid),
            Some(65532)
        );
    }

    /// #254: the mover ALWAYS gets `fsGroup: 65532` from the hardened base, so a recipe
    /// that pins only `runAsUser` still has one — the raw spec just cannot see it. Reading
    /// the raw spec made admission warn `LikelyIncompatible` for the very configuration
    /// `readOnly: false` exists to enable, while the controller (which resolves properly)
    /// said `FsGroupMayApply`.
    #[test]
    fn the_admission_identity_carries_the_hardened_default_fs_group() {
        let uid_only = MoverSpec {
            security_context: Some(SecurityContext {
                run_as_user: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let id = pinned_mover_identity(Some(&uid_only)).expect("a pinned UID yields an identity");
        assert_eq!(id.uid, Some(1000));
        assert_eq!(
            id.fs_group,
            Some(65532),
            "the resolved mover runs with the hardened fsGroup even though the recipe \
             never mentions one — the raw spec reports None and contradicts the controller"
        );
        assert!(
            id.groups.contains(&65532),
            "and it lands in the process group set too"
        );
    }

    /// The resolve must NOT start warning about every default policy: the hardened base
    /// sets `runAsNonRoot` but pins no `runAsUser`, so an unpinned recipe still resolves to
    /// no UID and stays silent.
    #[test]
    fn resolving_does_not_invent_a_uid_for_an_unpinned_recipe() {
        assert!(pinned_mover_identity(Some(&MoverSpec::default())).is_none());
        let psc_only = MoverSpec {
            pod_security_context: Some(PodSecurityContext {
                fs_group: Some(2000),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            pinned_mover_identity(Some(&psc_only)).is_none(),
            "an fsGroup alone must not make the mover's UID knowable"
        );
    }
}
