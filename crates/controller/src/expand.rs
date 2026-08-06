//! Cluster-side half of selector expansion (#346).
//!
//! The decisions — which source governs a run, what its kopia path is, what a
//! fanned-out child is called, and whether two matched PVCs would collide onto
//! one path — are pure and live in [`kopiur_api::expand`], because the CLI
//! expands selectors too (`kubectl kopiur snapshot now`) and cannot depend on
//! this crate. Only the listing lives here.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Namespace, PersistentVolumeClaim};
use kopiur_api::SnapshotPolicy;
use kopiur_api::snapshot::PvcTargetRef;
use kopiur_api::snapshot_policy;
use kube::ResourceExt;
use kube::api::{Api, ListParams};

use crate::error::{Error, Result};

pub use kopiur_api::expand::{
    EffectiveSource, ExpandedMember, effective_source, expand_sources, fanout_child_name,
    strategy_for,
};

/// Match every `pvcSelector` source of `policy` against live PVCs.
///
/// Returns `source index -> matched PVCs`, sorted for determinism (the child
/// names are derived from it, and a reordering would look like churn).
///
/// Namespace scoping:
/// * no `namespaceSelector` → the policy's own namespace only;
/// * `namespaceSelector.matchNames` → exactly those namespaces;
/// * under a namespaced install, always clamped to the install namespace — a
///   Role cannot list PVCs elsewhere, and returning a partial match silently
///   would back up a subset while looking like success.
pub async fn match_pvcs(
    client: &kube::Client,
    policy: &SnapshotPolicy,
) -> Result<BTreeMap<usize, Vec<PvcTargetRef>>> {
    let policy_ns = policy.namespace().unwrap_or_default();
    let mut out: BTreeMap<usize, Vec<PvcTargetRef>> = BTreeMap::new();

    for (index, source) in policy.spec.sources.iter().enumerate() {
        let Some(selector) = source.pvc_selector.as_ref() else {
            continue;
        };
        let namespaces = selector_namespaces(policy, selector, &policy_ns)?;
        let label_selector = selector
            .label_selector
            .as_ref()
            .map(kopiur_api::expand::label_selector_string)
            .unwrap_or_default();

        let mut found: Vec<PvcTargetRef> = Vec::new();
        for ns in &namespaces {
            let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
            let mut lp = ListParams::default();
            if !label_selector.is_empty() {
                lp = lp.labels(&label_selector);
            }
            for pvc in api.list(&lp).await?.items {
                let Some(name) = pvc.metadata.name.clone() else {
                    continue;
                };
                found.push(PvcTargetRef {
                    namespace: ns.clone(),
                    name,
                });
            }
        }
        // Deterministic order: the child names derive from this.
        found.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
        found.dedup();
        out.insert(index, found);
    }
    Ok(out)
}

/// Which namespaces one selector covers: always exactly the policy's own.
///
/// `namespaceSelector` is refused at admission ([`validate_pvc_selector`]) and
/// again here, because a mover Pod can only mount PersistentVolumeClaims in its
/// OWN namespace — a Kubernetes invariant, not a kopiur limitation. The Job runs
/// in the `Snapshot`'s namespace, which is the policy's, so a PVC matched
/// elsewhere could never be mounted. The failure mode of accepting it is not a
/// clean error either: with a SAME-NAMED PVC present locally, the mover would
/// silently snapshot the wrong volume under the matched one's identity.
fn selector_namespaces(
    policy: &SnapshotPolicy,
    selector: &snapshot_policy::PvcSelector,
    policy_ns: &str,
) -> Result<Vec<String>> {
    let requested: Vec<&String> = selector
        .namespace_selector
        .as_ref()
        .map(|n| n.match_names.iter().filter(|n| *n != policy_ns).collect())
        .unwrap_or_default();
    if !requested.is_empty() {
        return Err(Error::Validation(format!(
            "SnapshotPolicy `{}`'s pvcSelector asks for namespace(s) {:?}, but a backup's mover \
             Pod can only mount PersistentVolumeClaims in its own namespace (`{policy_ns}`). Use \
             one SnapshotPolicy per namespace — each may point at the same repository.",
            policy.name_any(),
            requested,
        )));
    }
    Ok(vec![policy_ns.to_string()])
}

/// Best-effort check that a namespace exists, for a clearer message than an
/// empty match. Only used on the cluster-scoped path.
pub async fn namespace_exists(client: &kube::Client, ns: &str) -> bool {
    let api: Api<Namespace> = Api::all(client.clone());
    api.get_opt(ns).await.ok().flatten().is_some()
}
