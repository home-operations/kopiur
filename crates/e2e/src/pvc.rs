//! PVC teardown that actually finishes.
//!
//! Deleting a PVC only marks it `Terminating`: the `kubernetes.io/pvc-protection`
//! finalizer holds it for as long as ANY non-terminal Pod in the namespace
//! references it in `spec.volumes` — including a Pod that never started. So a
//! helper that deletes "the PVC and the one Pod I know about" and then waits for
//! the PVC to go is only correct while it really does know every consumer.
//!
//! That assumption is exactly what poisoned the `populator_fanout` retries: a
//! reader Pod left `Pending` by a failed first try still mounted the claim, the
//! retry's re-seed deleted the claim and its seed Pod, and then waited the full
//! timeout for a PVC the protection finalizer would never release. Every retry
//! failed the same way, before reaching what it was retrying.
//!
//! [`delete_pvc_and_consumers`] finds the consumers from the cluster instead of
//! from the caller's memory, so a leftover from ANY earlier attempt is cleared.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::api::{DeleteParams, ListParams};
use kube::{Api, Client};

use crate::{default_timeout, poll_interval, wait_until};

/// Names of the Pods in `pods` that reference PVC `claim` through a
/// `persistentVolumeClaim` volume — i.e. every Pod whose existence (while
/// non-terminal) keeps `claim` from being deleted. Pure.
///
/// Ephemeral (`ephemeral.volumeClaimTemplate`) volumes are not matched: their
/// PVC is named `<pod>-<volume>` and owned by the Pod, so it is not a claim a
/// test deletes by name.
pub fn pods_mounting_claim(pods: &[Pod], claim: &str) -> BTreeSet<String> {
    pods.iter()
        .filter(|pod| {
            pod.spec.as_ref().is_some_and(|spec| {
                spec.volumes.iter().flatten().any(|v| {
                    v.persistent_volume_claim
                        .as_ref()
                        .is_some_and(|pvc| pvc.claim_name == claim)
                })
            })
        })
        .filter_map(|pod| pod.metadata.name.clone())
        .collect()
}

/// Delete PVC `name` in `ns` together with EVERY Pod that mounts it, and wait
/// until all of them are gone.
///
/// The consumers are discovered by listing the namespace (see
/// [`pods_mounting_claim`]), so a Pod left behind by a panicked earlier attempt
/// — which the caller has no record of — cannot pin the claim `Terminating`.
/// Pods created while waiting are picked up too: the list is re-taken on every
/// poll.
///
/// On timeout the error names what is still standing (the PVC's finalizers and
/// the Pods still mounting it), so a stuck teardown says WHY it is stuck.
pub async fn delete_pvc_and_consumers(client: &Client, ns: &str, name: &str) -> Result<()> {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let _ = pvcs.delete(name, &DeleteParams::default()).await;
    let outcome = wait_until(
        &format!("PVC {ns}/{name} and every Pod mounting it are gone"),
        default_timeout(),
        poll_interval(),
        || async {
            let consumers =
                pods_mounting_claim(&pods.list(&ListParams::default()).await?.items, name);
            for pod in &consumers {
                let _ = pods.delete(pod, &DeleteParams::default()).await;
            }
            let pvc_gone = pvcs.get_opt(name).await?.is_none();
            Ok((pvc_gone && consumers.is_empty()).then_some(()))
        },
    )
    .await;
    if let Err(e) = outcome {
        let finalizers = pvcs
            .get_opt(name)
            .await
            .ok()
            .flatten()
            .map(|p| p.metadata.finalizers.unwrap_or_default());
        let consumers = pods
            .list(&ListParams::default())
            .await
            .map(|l| pods_mounting_claim(&l.items, name))
            .unwrap_or_default();
        return Err(anyhow!(
            "{e}; PVC finalizers still set: {finalizers:?}; Pods still mounting it: {consumers:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, claims: &[&str]) -> Pod {
        let volumes: Vec<serde_json::Value> = claims
            .iter()
            .enumerate()
            .map(|(i, c)| {
                serde_json::json!({ "name": format!("v{i}"), "persistentVolumeClaim": { "claimName": c } })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": name },
            "spec": { "containers": [{ "name": "c", "image": "busybox" }], "volumes": volumes },
        }))
        .expect("pod JSON")
    }

    /// The `populator_fanout` retry poisoning, as a pure rule: the leftover
    /// reader of a failed try mounts the claim the retry re-seeds, so it MUST be
    /// among the Pods the teardown deletes — not only the seed Pod the caller
    /// happens to know by name. Before the fix, `drop_csi_pvc` deleted exactly
    /// `<claim>-seed` and waited forever on the reader's pvc-protection hold.
    #[test]
    fn a_leftover_reader_of_an_earlier_try_is_a_consumer_the_teardown_must_delete() {
        let pods = vec![
            pod("e2e-popfan-alpha-seed", &["e2e-popfan-alpha"]),
            pod("e2e-popfan-alpha-reader", &["e2e-popfan-alpha"]),
            pod("e2e-popfan-bravo-seed", &["e2e-popfan-bravo"]),
            pod("unrelated", &[]),
        ];
        assert_eq!(
            pods_mounting_claim(&pods, "e2e-popfan-alpha"),
            BTreeSet::from([
                "e2e-popfan-alpha-reader".to_string(),
                "e2e-popfan-alpha-seed".to_string(),
            ])
        );
    }

    #[test]
    fn a_pod_mounting_several_claims_is_a_consumer_of_each() {
        let pods = vec![pod("multi", &["a", "b"])];
        assert_eq!(
            pods_mounting_claim(&pods, "b"),
            BTreeSet::from(["multi".to_string()])
        );
        assert!(pods_mounting_claim(&pods, "c").is_empty());
    }

    #[test]
    fn a_pod_without_a_spec_or_volumes_mounts_nothing() {
        let mut bare = pod("bare", &[]);
        bare.spec = None;
        assert!(pods_mounting_claim(&[bare, pod("novol", &[])], "a").is_empty());
    }
}
