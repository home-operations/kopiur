//! The connected-client context every operation is threaded through: a kube
//! client plus the resolved namespace scope. Building it is the caller's job
//! (the CLI reads kubeconfig; a server reads its in-cluster credentials).

use kube::Client;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_flag_suffix_matches_kubectl_flags() {
        assert_eq!(Scope::Namespace("media".into()).flag_suffix(), " -n media");
        assert_eq!(Scope::All.flag_suffix(), " -A");
    }
}
