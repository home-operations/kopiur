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

use crate::config::WatchScope;
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
    scope: &WatchScope,
    policy: &SnapshotPolicy,
) -> Result<BTreeMap<usize, Vec<PvcTargetRef>>> {
    let policy_ns = policy.namespace().unwrap_or_default();
    let mut out: BTreeMap<usize, Vec<PvcTargetRef>> = BTreeMap::new();

    for (index, source) in policy.spec.sources.iter().enumerate() {
        let Some(selector) = source.pvc_selector.as_ref() else {
            continue;
        };
        let namespaces = selector_namespaces(scope, policy, selector, &policy_ns)?;
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

/// Which namespaces one selector covers. See [`match_pvcs`].
fn selector_namespaces(
    scope: &WatchScope,
    policy: &SnapshotPolicy,
    selector: &snapshot_policy::PvcSelector,
    policy_ns: &str,
) -> Result<Vec<String>> {
    let requested: Vec<String> = match selector.namespace_selector.as_ref() {
        Some(ns_sel) if !ns_sel.match_names.is_empty() => ns_sel.match_names.clone(),
        _ => vec![policy_ns.to_string()],
    };
    if let WatchScope::Namespaced(install_ns) = scope {
        let outside: Vec<&String> = requested.iter().filter(|n| *n != install_ns).collect();
        if !outside.is_empty() {
            return Err(Error::Validation(format!(
                "SnapshotPolicy `{}`'s pvcSelector asks for namespace(s) {:?}, but this is a \
                 namespaced install watching only `{install_ns}` — its Role cannot list \
                 PersistentVolumeClaims elsewhere. Backing up only the reachable subset would \
                 look like success, so the run is refused. Drop `namespaceSelector`, or reinstall \
                 with installScope=cluster.",
                policy.name_any(),
                outside,
            )));
        }
    }
    Ok(requested)
}

/// Best-effort check that a namespace exists, for a clearer message than an
/// empty match. Only used on the cluster-scoped path.
pub async fn namespace_exists(client: &kube::Client, ns: &str) -> bool {
    let api: Api<Namespace> = Api::all(client.clone());
    api.get_opt(ns).await.ok().flatten().is_some()
}
