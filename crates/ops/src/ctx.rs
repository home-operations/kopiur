//! The connected-client context every operation is threaded through: a kube
//! client plus the resolved namespace scope. Building it is the caller's job
//! (the CLI reads kubeconfig; a server reads its in-cluster credentials).

use kube::Client;
use kube::api::PatchParams;

/// Where a list-style command looks. Exhaustive — every list call site
/// `match`es this, so the `-A`/`-n` decision can never be half-applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// One namespace (from `-n` or the kubeconfig context default).
    Namespace(String),
    /// Every namespace (`-A`).
    All,
}

impl Scope {
    /// The namespace to use for namespaced *single-object* operations, and the
    /// `kubectl` flag suffix matching this scope (for error remediation text).
    pub fn flag_suffix(&self) -> String {
        match self {
            Scope::Namespace(ns) => format!(" -n {ns}"),
            Scope::All => " -A".to_string(),
        }
    }
}

/// A connected client plus the resolved namespace scope.
#[derive(Clone)]
pub struct OpsCtx {
    /// The kube client, ready for `Api` construction.
    pub client: Client,
    /// The single namespace single-object commands operate in.
    pub namespace: String,
    /// The list scope (`-A` vs the resolved namespace).
    pub scope: Scope,
    /// The Kubernetes field manager stamped on every patch/create this context
    /// performs, so `kubectl get -o yaml --show-managed-fields` names the tool
    /// that made the change. The CLI sets `"kubectl-kopiur"`; a server names
    /// itself, so the two front ends are distinguishable in an audit trail.
    pub field_manager: String,
}

/// **Pure.** The `PatchParams` every kopiur client-side MERGE patch uses.
///
/// Merge, never apply: the annotations these operations stamp are requests the
/// operator consumes, not fields this tool owns — a server-side apply would
/// fight the controller for ownership of the whole annotations map. `force` is
/// therefore left off; only the attribution is set.
pub fn merge_patch_params(field_manager: &str) -> PatchParams {
    PatchParams {
        field_manager: Some(field_manager.to_string()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_flag_suffix_matches_kubectl_flags() {
        assert_eq!(Scope::Namespace("media".into()).flag_suffix(), " -n media");
        assert_eq!(Scope::All.flag_suffix(), " -A");
    }

    #[test]
    fn merge_patch_params_carries_the_managers_name_and_never_forces() {
        let params = merge_patch_params("kubectl-kopiur");
        assert_eq!(params.field_manager.as_deref(), Some("kubectl-kopiur"));
        assert!(!params.force, "a merge patch must not force-take ownership");
        assert!(!params.dry_run);
    }
}
