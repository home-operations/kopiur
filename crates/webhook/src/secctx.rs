//! Best-effort securityContext-compatibility **admission warnings** — the earliest possible
//! surface (at `kubectl apply`). Non-blocking and fail-open: the authoritative checks are the
//! reconcile-time condition and the mover's runtime readability preflight.
//!
//! The webhook sees only the *recipe* securityContext (no `moverDefaults`), so it warns only
//! when the recipe **explicitly pins** a `runAsUser` — otherwise the mover UID is undetermined
//! and we stay silent. We warn only on a near-certain `LikelyIncompatible` verdict.

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
    let sc = m.security_context.as_ref()?;
    // Only when the recipe pins a UID (container or pod) — else the resolved UID is unknown.
    let id = secctx_compat::mover_identity(sc, m.pod_security_context.as_ref());
    id.uid.map(|_| id)
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
    let claims: Vec<&str> = sources
        .iter()
        .filter_map(|s| s.pvc.as_ref().map(|p| p.name.as_str()))
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
    for claim in claims {
        // `workload_identities` mounts-the-claim + excludes kopiur mover pods (shared core).
        let identities = secctx_compat::workload_identities(&pods, claim);
        if let MoverReadCompat::LikelyIncompatible { .. } =
            secctx_compat::assess_read_compat(&mover_id, &identities)
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
}
